/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use rustc_errors::{Diag, DiagDecorator};
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{CrateNum, DefId};
use rustc_infer::infer::TyCtxtInferExt;
use rustc_infer::traits::{EvaluationResult, Obligation, ObligationCause};
use rustc_lint::{LateContext, Lint, LintContext};
use rustc_middle::ty::{self, GenericArg, TraitRef, Ty, TyCtxt, TypeVisitableExt};
use rustc_span::hygiene::{ExpnKind, MacroKind};
use rustc_span::symbol::Symbol;
use rustc_span::Span;
use rustc_trait_selection::traits::query::evaluate_obligation::InferCtxtExt as _;
use rustc_type_ir::Upcast as _;

/// Emit a lint whose diagnostic `decorate` builds, including its span.
pub trait LateContextExt {
    fn lint(&self, lint: &'static Lint, decorate: impl FnOnce(&mut Diag<'_, ()>));
}

impl<'tcx> LateContextExt for LateContext<'tcx> {
    fn lint(&self, lint: &'static Lint, decorate: impl FnOnce(&mut Diag<'_, ()>)) {
        self.opt_span_lint(lint, None::<Span>, DiagDecorator(decorate));
    }
}

/// Attribute lookups on [`TyCtxt`].
pub trait TyCtxtExt {
    /// Whether the item `did` has an attribute whose path is `attrs`.
    fn has_attrs_with_path(self, did: impl Into<DefId>, attrs: &[Symbol]) -> bool;
}

impl TyCtxtExt for TyCtxt<'_> {
    fn has_attrs_with_path(self, did: impl Into<DefId>, attrs: &[Symbol]) -> bool {
        self.get_attrs_by_path(did.into(), attrs).next().is_some()
    }
}

/// The item an alias type names: the associated type, opaque type or type alias.
pub fn alias_def_id(kind: ty::AliasTyKind<'_>) -> DefId {
    match kind {
        ty::AliasTyKind::Projection { def_id }
        | ty::AliasTyKind::Inherent { def_id }
        | ty::AliasTyKind::Opaque { def_id }
        | ty::AliasTyKind::Free { def_id } => def_id,
    }
}

/// Check if a DefId's path matches the given absolute type path, e.g. with
/// `match_def_path(cx, id, &["core", "option", "Option"])`.
pub fn match_def_path(cx: &LateContext, def_id: DefId, path: &[Symbol]) -> bool {
    let def_path = cx.tcx.def_path(def_id);
    let krate = &cx.tcx.crate_name(def_path.krate);
    if krate != &path[0] {
        return false;
    }

    let path = &path[1..];
    let other = def_path.data;

    if other.len() != path.len() {
        return false;
    }

    other
        .into_iter()
        .zip(path)
        .all(|(e, p)| e.data.get_opt_name().as_ref() == Some(p))
}

/// Check if a DefId's intra-crate path suffix matches the given segments,
/// ignoring the crate name.  Matches both external crates and local modules.
///
/// For example, `match_def_path_suffix(cx, id, &["jsapi", "JS", "Value"])`
/// matches `mozjs_sys::jsapi::JS::Value` in real code and also matches a
/// local `mod mozjs_sys { pub mod jsapi { pub mod JS { pub struct Value ... } } }` in tests.
pub fn match_def_path_suffix(cx: &LateContext, def_id: DefId, suffix: &[Symbol]) -> bool {
    let def_path = cx.tcx.def_path(def_id);
    let segments: Vec<_> = def_path
        .data
        .iter()
        .filter_map(|d| d.data.get_opt_name())
        .collect();
    if segments.len() < suffix.len() {
        return false;
    }
    segments[segments.len() - suffix.len()..]
        .iter()
        .zip(suffix)
        .all(|(seg, expected)| seg == expected)
}

pub fn in_derive_expn(span: Span) -> bool {
    matches!(
        span.ctxt().outer_expn_data().kind,
        ExpnKind::Macro(MacroKind::Derive, ..)
    )
}

#[macro_export]
macro_rules! symbols {
    ($($s: ident)+) => {
        #[derive(Clone)]
        #[allow(non_snake_case)]
        pub(crate) struct Symbols {
            $( $s: Symbol, )+
        }

        impl Symbols {
            fn new() -> Self {
                Symbols {
                    $( $s: Symbol::intern(stringify!($s)), )+
                }
            }
        }
    }
}

pub fn find_first_crate<'tcx>(tcx: &TyCtxt<'tcx>, crate_name: Symbol) -> Option<CrateNum> {
    tcx.crates(())
        .iter()
        .find(|c| tcx.crate_name(**c) == crate_name)
        .copied()
}

pub fn trait_in_crate<'tcx>(
    tcx: &TyCtxt<'tcx>,
    krate: CrateNum,
    trait_sym: Symbol,
) -> Option<DefId> {
    tcx.traits(krate)
        .iter()
        .find(|id| tcx.opt_item_name(**id) == Some(trait_sym))
        .copied()
}

/*
Stuff copied from clippy:
*/

/// Checks whether a type implements a trait. Panics if the type contains an inference variable.
///
/// See [Common tools for writing lints] for an example how to use this function and other options.
///
/// [Common tools for writing lints]: https://github.com/rust-lang/rust-clippy/blob/master/book/src/development/common_tools_writing_lints.md#checking-if-a-type-implements-a-specific-trait
pub fn implements_trait<'tcx>(cx: &LateContext<'tcx>, ty: Ty<'tcx>, trait_id: DefId) -> bool {
    let tcx = cx.tcx;
    // Clippy shouldn't have infer types
    assert!(!ty.has_infer());

    let ty = tcx.erase_and_anonymize_regions(ty);
    if ty.has_escaping_bound_vars() {
        return false;
    }

    let (infcx, param_env) = tcx.infer_ctxt().build_with_typing_env(cx.typing_env());
    let trait_ref = TraitRef::new(tcx, trait_id, [GenericArg::from(ty)]);

    debug_assert!(
        matches!(tcx.def_kind(trait_id), DefKind::Trait | DefKind::TraitAlias),
        "`DefId` must belong to a trait or trait alias"
    );

    let obligation = Obligation {
        cause: ObligationCause::dummy(),
        param_env,
        recursion_depth: 0,
        predicate: trait_ref.upcast(tcx),
    };
    infcx
        .evaluate_obligation(&obligation)
        .is_ok_and(EvaluationResult::must_apply_modulo_regions)
}
