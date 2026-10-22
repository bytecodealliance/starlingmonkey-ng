/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use rustc_hir::{self as hir, intravisit as visit, AmbigArg, ExprKind};
use rustc_lint::{LateContext, LateLintPass, Lint, LintPass, LintStore};
use rustc_middle::ty;
use rustc_session::declare_tool_lint;
use rustc_span::def_id::{DefId, LocalDefId};
use rustc_span::symbol::{sym, Symbol};

use crate::common::{alias_def_id, TyCtxtExt as _};
use crate::common::{in_derive_expn, match_def_path, LateContextExt};
use crate::symbols;

declare_tool_lint! {
    pub crown::UNROOTED_MUST_ROOT,
    Deny,
    "Warn and report usage of unrooted jsmanaged objects"
}

pub fn register(lint_store: &mut LintStore) {
    let symbols = Symbols::new();
    lint_store.register_lints(&[UNROOTED_MUST_ROOT]);
    lint_store.register_late_lint_pass(Box::new(move |_| {
        Box::new(UnrootedPass::new(symbols.clone()))
    }));
}

/// Lint for ensuring safe usage of unrooted pointers
///
/// This lint (disable with `-A unrooted-must-root`/`#[allow(unrooted_must_root)]`) ensures that
/// `#[js::must_root]` values are used correctly.
///
/// "Incorrect" usage includes:
///
///  - Not being used in a struct/enum field which is not `#[js::must_root]` itself
///  - Not being used as an argument to a function (Except onces named `new` and `new_inherited`)
///  - Not being bound locally in a `let` statement, assignment, `for` loop, or `match` statement.
///
/// This helps catch most situations where pointers like `JS<T>` are used in a way that they can be invalidated by a
/// GC pass.
///
/// Structs which have their own mechanism of rooting their unrooted contents (e.g. `ScriptThread`)
/// can be marked as `#[js::allow_unrooted]`. Smart pointers which root their interior type
/// can be marked as `#[js::allow_unrooted_interior]`
pub(crate) struct UnrootedPass {
    symbols: Symbols,
}

impl UnrootedPass {
    pub(crate) fn new(symbols: Symbols) -> UnrootedPass {
        UnrootedPass { symbols }
    }
}

/// For a given associated type for a trait implementation, checks if a given crown annotation
/// is present on that type.
fn associated_type_has_attr<'tcx>(
    sym: &'_ Symbols,
    cx: &LateContext<'tcx>,
    ty: ty::Ty<'tcx>,
    attr: Symbol,
) -> bool {
    let mut walker = ty.walk();
    while let Some(generic_arg) = walker.next() {
        let t = match generic_arg.kind() {
            rustc_middle::ty::GenericArgKind::Type(t) => t,
            _ => {
                walker.skip_current_subtree();
                continue;
            },
        };
        match t.kind() {
            ty::Adt(did, _substs) => {
                return cx.tcx.has_attrs_with_path(
                    did.did(),
                    &[sym.crown, sym.unrooted_must_root_lint, attr],
                );
            },
            ty::Alias(_, alias)
                if matches!(
                    alias.kind,
                    ty::AliasTyKind::Projection { .. }
                        | ty::AliasTyKind::Inherent { .. }
                        | ty::AliasTyKind::Free { .. }
                ) =>
            {
                return cx.tcx.has_attrs_with_path(
                    alias_def_id(alias.kind),
                    &[sym.crown, sym.unrooted_must_root_lint, attr],
                )
            },
            _ => {},
        }
    }
    false
}

/// Checks whether `did`'s `#[js::must_root]` attribute carries the
/// `allow_self_return` parameter, i.e. was written as
/// `#[js::must_root(allow_self_return)]`.
///
/// The parameter permits associated fns of the marked type (inherent or
/// trait-impl) to return the bare type: such fns are constructor-shaped, and
/// their callers are still checked (bindings of the returned value are
/// flagged as usual).
fn must_root_allows_self_return(sym: &Symbols, cx: &LateContext, did: DefId) -> bool {
    cx.tcx
        .get_attrs_by_path(
            did,
            &[sym.crown, sym.unrooted_must_root_lint, sym.must_root],
        )
        .any(|attr| {
            attr.meta_item_list().is_some_and(|list| {
                list.iter()
                    .any(|item| item.ident().map(|i| i.name) == Some(sym.allow_self_return))
            })
        })
}

/// Looks through a `Result<T, E>` return type to `T`, leaving any other type
/// alone. A constructor is allowed to be fallible, and `Result` hands its `Ok`
/// value straight on to the caller just as a bare return does.
fn peel_result<'tcx>(cx: &LateContext<'tcx>, ty: ty::Ty<'tcx>) -> ty::Ty<'tcx> {
    match ty.kind() {
        ty::Adt(def, args) if cx.tcx.is_diagnostic_item(sym::Result, def.did()) => args.type_at(0),
        _ => ty,
    }
}

/// Checks if a type is unrooted or contains any owned unrooted types
fn is_unrooted_ty<'tcx>(
    sym: &'_ Symbols,
    cx: &LateContext<'tcx>,
    ty: ty::Ty<'tcx>,
    in_new_function: bool,
    // Whether a bare `JS::Value` should be treated as unrooted. A `Value` only
    // becomes a rooting hazard when it is *held* — as a parameter, a `let`
    // binding, or a struct/enum field — across an allocation. In return position
    // it is produced and handed straight to the caller (typically a native
    // callback handing its result to the engine), which consumes it
    // immediately; flagging that would be a false positive on every callback, so
    // callers pass `false` for return-type checks.
    flag_value: bool,
) -> bool {
    // Structures which are semantically similar to an &ptr.
    let ref_like: &[&[Symbol]] = &[
        &[sym::core, sym.cell, sym.Ref],
        &[sym::core, sym.cell, sym.RefMut],
        &[sym::core, sym::slice, sym::iter, sym.Iter],
        &[sym::core, sym::slice, sym::iter, sym.IterMut],
        &[sym.accountable_refcell, sym.Ref],
        &[sym.accountable_refcell, sym.RefMut],
        &[sym.mozjs, sym.gc, sym.root, sym.Handle],
        &[sym.js, sym.gc, sym.handle, sym.Stack],
        &[sym.mozjs_sys, sym.generated, sym.root, sym.JS, sym.Handle],
        &[sym::std, sym.collections, sym.hash, sym.map, sym.Entry],
        &[
            sym::std,
            sym.collections,
            sym.hash,
            sym.map,
            sym.OccupiedEntry,
        ],
        &[
            sym::std,
            sym.collections,
            sym.hash,
            sym.map,
            sym.VacantEntry,
        ],
        &[sym::std, sym.collections, sym.hash, sym.map, sym.Iter],
        &[sym::std, sym.collections, sym.hash, sym.set, sym.Iter],
    ];
    let mut ret = false;
    let mut walker = ty.walk();
    while let Some(generic_arg) = walker.next() {
        let t = match generic_arg.kind() {
            rustc_middle::ty::GenericArgKind::Type(t) => t,
            _ => {
                walker.skip_current_subtree();
                continue;
            },
        };
        let has_attr = |did, name| {
            cx.tcx
                .has_attrs_with_path(did, &[sym.crown, sym.unrooted_must_root_lint, name])
        };
        let recur_into_subtree = match t.kind() {
            ty::Adt(did, substs) => {
                if has_attr(did.did(), sym.must_root) {
                    ret = true;
                    false
                } else if has_attr(did.did(), sym.allow_unrooted_interior) {
                    false
                } else if match_def_path(cx, did.did(), &[sym.alloc, sym.rc, sym.Rc]) {
                    // Rc<Promise> is okay
                    let inner = substs.type_at(0);
                    match inner.kind() {
                        ty::Adt(did, _) => !has_attr(did.did(), sym.allow_unrooted_in_rc),
                        ty::Alias(_, alias)
                            if matches!(
                                alias.kind,
                                ty::AliasTyKind::Projection { .. }
                                    | ty::AliasTyKind::Inherent { .. }
                                    | ty::AliasTyKind::Free { .. }
                            ) =>
                        {
                            !has_attr(alias_def_id(alias.kind), sym.allow_unrooted_in_rc)
                        },
                        _ => true,
                    }
                } else if ref_like
                    .iter()
                    .any(|path| match_def_path(cx, did.did(), path))
                {
                    false
                } else if match_def_path(
                    cx,
                    did.did(),
                    &[sym.mozjs_sys, sym.generated, sym.root, sym.JS, sym.Value],
                ) {
                    // Allow unrooted `JS::Value` in `mozjs` and `js` crates, but
                    // not downstream.
                    let krate = cx.tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE);
                    let name = krate.as_str();
                    if flag_value && name != "js" && !name.starts_with("mozjs") {
                        ret = true;
                    }
                    false
                } else if did.is_box() && in_new_function {
                    // box in new() is okay
                    false
                } else {
                    true
                }
            },
            ty::Ref(..) => false, // don't recurse down &ptrs
            ty::RawPtr(inner_ty, _) => {
                // Flag *mut JSObject / *const JSObject as unrooted GC
                // pointers in downstream crates. The `js` crate and
                // `mozjs` crates legitimately handle raw pointers as
                // the interface to SpiderMonkey's C++ API.
                if let ty::Adt(did, _) = inner_ty.kind() {
                    if cx.tcx.item_name(did.did()) == sym.JSObject {
                        let krate = cx.tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE);
                        let name = krate.as_str();
                        if name != "js" && !name.starts_with("mozjs") {
                            ret = true;
                        }
                    }
                }
                false
            },
            ty::FnDef(..) | ty::FnPtr(..) => false,
            ty::Alias(_, alias)
                if matches!(
                    alias.kind,
                    ty::AliasTyKind::Projection { .. }
                        | ty::AliasTyKind::Inherent { .. }
                        | ty::AliasTyKind::Free { .. }
                ) =>
            {
                let def_id = alias_def_id(alias.kind);
                if has_attr(def_id, sym.must_root) {
                    ret = true;
                    false
                } else if has_attr(def_id, sym.allow_unrooted_interior) {
                    false
                } else {
                    // If this is a projection (i.e. Self::FOO), recursing will
                    // make us consider Self, which is overly conservative for
                    // this analysys.
                    !matches!(alias.kind, ty::AliasTyKind::Projection { .. })
                }
            },
            _ => true,
        };
        if !recur_into_subtree {
            walker.skip_current_subtree();
        }
    }
    ret
}

impl LintPass for UnrootedPass {
    fn name(&self) -> &'static str {
        "ServoUnrootedPass"
    }

    fn get_lints(&self) -> Vec<&'static Lint> {
        vec![UNROOTED_MUST_ROOT]
    }
}

impl<'tcx> LateLintPass<'tcx> for UnrootedPass {
    /// All structs containing #[js::must_root] types
    /// must be #[js::must_root] themselves
    fn check_item(&mut self, cx: &LateContext<'tcx>, item: &'tcx hir::Item) {
        let sym = &self.symbols;
        let has_attr = |symbol| {
            cx.tcx.has_attrs_with_path(
                item.hir_id().expect_owner(),
                &[sym.crown, sym.unrooted_must_root_lint, symbol],
            )
        };
        if has_attr(sym.must_root) || has_attr(sym.allow_unrooted_interior) {
            return;
        }
        if let hir::ItemKind::Struct(_, _, variant_data) = &item.kind {
            for field in variant_data.fields() {
                let field_type = cx.tcx.type_of(field.def_id);
                if is_unrooted_ty(&self.symbols, cx, field_type.skip_binder(), false, true) {
                    cx.lint(UNROOTED_MUST_ROOT, |lint| {
                        lint.primary_message(
                            "Type must be rooted, use #[js::must_root] \
                               on the struct definition to propagate.",
                        );
                        lint.span(field.span);
                    })
                }
            }
        }
    }

    /// All enums containing #[js::must_root] types
    /// must be #[js::must_root] themselves
    fn check_variant(&mut self, cx: &LateContext, var: &hir::Variant) {
        let parent = cx.tcx.hir_get_parent_item(var.hir_id).def_id;
        let parent_item = cx.tcx.hir_expect_item(parent);
        let sym = &self.symbols;
        if !cx.tcx.has_attrs_with_path(
            parent_item.hir_id().expect_owner(),
            &[sym.crown, sym.unrooted_must_root_lint, sym.must_root],
        ) {
            // `VariantData::fields()` yields the fields of both tuple-like
            // (`V(Heap<T>)`) and struct-like (`V { f: Heap<T> }`) variants, and
            // is empty for unit variants. Checking every field here closes a gap
            // where an unrooted field in a struct-like variant went unflagged.
            for field in var.data.fields() {
                let field_type = cx.tcx.type_of(field.def_id);
                if is_unrooted_ty(&self.symbols, cx, field_type.skip_binder(), false, true) {
                    cx.lint(UNROOTED_MUST_ROOT, |lint| {
                        lint.primary_message(
                            "Type must be rooted, \
                              use #[js::must_root] \
                              on the enum definition to propagate.",
                        );
                        lint.span(field.ty.span);
                    })
                }
            }
        }
    }

    /// for trait_type_impl_must_root test
    fn check_trait_item(
        &mut self,
        cx: &LateContext<'tcx>,
        trait_item: &'tcx rustc_hir::TraitItem<'tcx>,
    ) {
        let hir::TraitItemKind::Type(_, _) = trait_item.kind else {
            return;
        };

        let sym = &self.symbols;
        let has_attr = |did, name| {
            cx.tcx
                .has_attrs_with_path(did, &[sym.crown, sym.unrooted_must_root_lint, name])
        };

        let def_id: DefId = trait_item.hir_id().expect_owner().into();
        let must_root_present = has_attr(def_id, sym.must_root);

        let allow_unrooted_interior_present = has_attr(def_id, sym.allow_unrooted_interior);

        let allow_unrooted_in_rc_present = has_attr(def_id, sym.allow_unrooted_in_rc);

        let trait_id = cx
            .tcx
            .associated_item(trait_item.hir_id().expect_owner().to_def_id())
            .trait_container(cx.tcx)
            .unwrap();
        // we need to make sure that each impl has same crown attrs
        let impls = cx.tcx.trait_impls_of(trait_id);
        for (_ty, impl_def_ids) in impls.non_blanket_impls() {
            for impl_def_id in impl_def_ids {
                let type_impl = cx
                    .tcx
                    .associated_items(*impl_def_id)
                    .find_by_ident_and_kind(cx.tcx, trait_item.ident, ty::AssocTag::Type, trait_id)
                    .unwrap();

                let mir_ty = cx.tcx.type_of(type_impl.def_id).skip_binder();

                // A binding to a bare type parameter (e.g. the blanket
                // `impl<T: ClassDef> StackType for Stack<T> { type Inner = T; }`)
                // carries no attribute information of its own: the concrete
                // type is supplied by other impls and checked at their sites.
                // Treating it as unannotated would make an annotated
                // declaration unsatisfiable in any crate containing both a
                // parameter binding and a concrete annotated one.
                if matches!(mir_ty.kind(), ty::Param(_)) {
                    continue;
                }

                let impl_ty_must_root = associated_type_has_attr(sym, cx, mir_ty, sym.must_root);
                let impl_ty_allow_unrooted =
                    associated_type_has_attr(sym, cx, mir_ty, sym.allow_unrooted_interior);
                let impl_ty_allow_rc =
                    associated_type_has_attr(sym, cx, mir_ty, sym.allow_unrooted_in_rc);

                if impl_ty_must_root != must_root_present {
                    if !must_root_present && impl_ty_must_root {
                        cx.lint(UNROOTED_MUST_ROOT, |lint| {
                            lint.primary_message(
                                "Type trait declaration must be marked with \
                                 #[js::must_root] \
                                 to allow binding must_root types in associated types.",
                            );
                            lint.span(trait_item.span);
                        });
                    } else {
                        cx.lint(UNROOTED_MUST_ROOT, |lint| {
                            lint.primary_message(
                                "Mismatched use of \
                                 #[js::must_root] \
                                 between associated type declaration and impl definition.",
                            );
                            lint.span(trait_item.span);
                        });
                    }
                }

                if impl_ty_allow_unrooted != allow_unrooted_interior_present {
                    cx.lint(UNROOTED_MUST_ROOT, |lint| {
                        lint.primary_message(
                            "Mismatched use of \
                             #[js::allow_unrooted_interior] \
                             between associated type declaration and impl definition.",
                        );
                        lint.span(trait_item.span);
                    });
                }

                if impl_ty_allow_rc != allow_unrooted_in_rc_present {
                    cx.lint(UNROOTED_MUST_ROOT, |lint| {
                        lint.primary_message(
                            "Mismatched use of \
                             #[js::allow_unrooted_interior_in_rc] \
                             between associated type declaration and impl definition.",
                        );
                        lint.span(trait_item.span);
                    });
                }
            }
        }
    }

    /// Function arguments that are #[js::must_root] types are not allowed
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: visit::FnKind<'tcx>,
        decl: &'tcx hir::FnDecl,
        body: &'tcx hir::Body,
        span: rustc_span::Span,
        def_id: LocalDefId,
    ) {
        let in_new_function = match kind {
            visit::FnKind::ItemFn(n, _, _) | visit::FnKind::Method(n, _) => {
                let n = n.as_str();
                matches!(
                    n,
                    "new" | "from" | "default" | "Wrap" | "constructor" | "construct"
                ) || n.starts_with("new_")
                    || n.starts_with("from_")
            },
            visit::FnKind::Closure => false,
        };

        if !in_derive_expn(span) && !matches!(kind, visit::FnKind::Closure) {
            let sig = cx.tcx.type_of(def_id).skip_binder().fn_sig(cx.tcx);

            for (arg, ty) in decl.inputs.iter().zip(sig.inputs().skip_binder().iter()) {
                if is_unrooted_ty(&self.symbols, cx, *ty, in_new_function, true) {
                    cx.lint(UNROOTED_MUST_ROOT, |lint| {
                        lint.primary_message("Type must be rooted.");
                        lint.span(arg.span);
                    })
                }
            }

            // An associated fn returning its own `must_root(allow_self_return)`
            // type, bare or as the `Ok` of a `Result`, is constructor-shaped
            // and exempt from the return check; callers holding the returned
            // value are still flagged.
            let exempt_self_return = match peel_result(cx, sig.output().skip_binder()).kind() {
                ty::Adt(ret_def, _) => {
                    must_root_allows_self_return(&self.symbols, cx, ret_def.did())
                        && cx
                            .tcx
                            .impl_of_assoc(def_id.to_def_id())
                            .map(|impl_did| cx.tcx.type_of(impl_did).skip_binder())
                            .is_some_and(|self_ty| {
                                matches!(self_ty.kind(), ty::Adt(self_def, _) if self_def.did() == ret_def.did())
                            })
                },
                _ => false,
            };

            if !in_new_function
                && !exempt_self_return
                && is_unrooted_ty(&self.symbols, cx, sig.output().skip_binder(), false, false)
            {
                cx.lint(UNROOTED_MUST_ROOT, |lint| {
                    lint.primary_message("Type must be rooted.");
                    lint.span(decl.output.span());
                })
            }
        }

        let mut visitor = FnDefVisitor {
            symbols: &self.symbols,
            cx,
            in_new_function,
        };
        visit::walk_expr(&mut visitor, body.value);
    }
}

struct FnDefVisitor<'a, 'tcx: 'a> {
    symbols: &'a Symbols,
    cx: &'a LateContext<'tcx>,
    in_new_function: bool,
}

impl<'a, 'tcx> visit::Visitor<'tcx> for FnDefVisitor<'a, 'tcx> {
    fn visit_expr(&mut self, expr: &'tcx hir::Expr) {
        let cx = self.cx;

        // Trait casts from #[js::must_root] types are not allowed
        if let ExprKind::Cast(subexpr, _) = expr.kind {
            let ty = cx.typeck_results().expr_ty(subexpr);
            if is_unrooted_ty(self.symbols, cx, ty, self.in_new_function, true) {
                cx.lint(UNROOTED_MUST_ROOT, |lint| {
                    lint.primary_message(format!("Expression of type {:?} must be rooted.", ty));
                    lint.span(subexpr.span);
                })
            }
        }

        visit::walk_expr(self, expr);
    }

    fn visit_pat(&mut self, pat: &'tcx hir::Pat) {
        let cx = self.cx;

        // We want to detect pattern bindings that move a value onto the stack.
        // When "default binding modes" https://github.com/rust-lang/rust/issues/42640
        // are implemented, the `Unannotated` case could cause false-positives.
        // These should be fixable by adding an explicit `ref`.
        // Skip the compiler-generated binding of the `?` desugaring: the Ok
        // value is bound only to be immediately returned or passed on, with no
        // window for an allocation. Users cannot restructure it away.
        if matches!(
            pat.span.desugaring_kind(),
            Some(rustc_span::DesugaringKind::QuestionMark)
        ) {
            return;
        }

        match pat.kind {
            hir::PatKind::Binding(hir::BindingMode::NONE, ..)
            | hir::PatKind::Binding(hir::BindingMode::MUT, ..) => {
                let ty = cx.typeck_results().pat_ty(pat);
                if is_unrooted_ty(self.symbols, cx, ty, self.in_new_function, true) {
                    cx.lint(UNROOTED_MUST_ROOT, |lint| {
                        lint.primary_message(format!(
                            "Expression of type {:?} must be rooted.",
                            ty
                        ));
                        lint.span(pat.span);
                    })
                }
            },
            _ => {},
        }

        visit::walk_pat(self, pat);
    }

    fn visit_ty(&mut self, _: &'tcx rustc_hir::Ty<'tcx, AmbigArg>) {}
}

symbols! {
    crown
    unrooted_must_root_lint
    allow_unrooted_interior
    allow_unrooted_in_rc
    must_root
    alloc
    rc
    Rc
    cell
    accountable_refcell
    Ref
    RefMut
    Iter
    IterMut
    collections
    hash
    map
    set
    Entry
    OccupiedEntry
    VacantEntry
    JSObject
    Value
    mozjs
    mozjs_sys
    gc
    root
    Handle
    JS
    generated
    js
    handle
    Stack
    allow_self_return
}
