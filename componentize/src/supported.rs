// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Rejects worlds that use WIT types the runtime's interpreter cannot lift or lower
//! yet, so they fail at componentization instead of trapping at instantiation or on
//! the first call.

use std::collections::HashSet;

use anyhow::bail;
use wit_parser::{Function, FunctionKind, Resolve, Type, TypeDefKind, TypeId, WorldId, WorldItem};

/// Fail if any function the world imports or exports uses a `map`, a fixed-length
/// list or `error-context`, or a synchronous function the world exports takes or
/// returns a `stream` or `future`, naming the first such function.
///
/// A synchronous export runs without an event loop to drive the reads and writes
/// of the streams and futures it takes and returns.
pub(crate) fn check_supported_types(resolve: &Resolve, world: WorldId) -> anyhow::Result<()> {
    let world = &resolve.worlds[world];
    for (key, item) in world.exports.iter() {
        let (interface, funcs): (Option<String>, Vec<&Function>) = match item {
            WorldItem::Function(func) => (None, vec![func]),
            WorldItem::Interface { id, .. } => (
                Some(resolve.name_world_key(key)),
                resolve.interfaces[*id].functions.values().collect(),
            ),
            WorldItem::Type { .. } => continue,
        };
        for func in funcs {
            let sync = matches!(
                func.kind,
                FunctionKind::Freestanding
                    | FunctionKind::Method(_)
                    | FunctionKind::Static(_)
                    | FunctionKind::Constructor(_)
            );
            let types = func.params.iter().map(|p| p.ty).chain(func.result);
            if sync
                && types
                    .into_iter()
                    .any(|ty| has_stream_or_future(resolve, ty))
            {
                let name = match &interface {
                    Some(interface) => format!("{interface}#{}", func.name),
                    None => func.name.clone(),
                };
                bail!(
                    "the synchronous export `{name}` takes or returns a `stream` or `future`, \
                     which componentize supports only for `async` functions. Declare it `async`."
                );
            }
        }
    }
    for (key, item) in world.imports.iter().chain(world.exports.iter()) {
        match item {
            WorldItem::Function(func) => check_function(resolve, None, func)?,
            WorldItem::Interface { id, .. } => {
                let interface = resolve.name_world_key(key);
                for func in resolve.interfaces[*id].functions.values() {
                    check_function(resolve, Some(&interface), func)?;
                }
            }
            WorldItem::Type { .. } => {}
        }
    }
    Ok(())
}

fn check_function(
    resolve: &Resolve,
    interface: Option<&str>,
    func: &Function,
) -> anyhow::Result<()> {
    let types = func.params.iter().map(|p| p.ty).chain(func.result);
    for ty in types {
        if let Some(unsupported) = unsupported_type(resolve, ty) {
            let name = match interface {
                Some(interface) => format!("{interface}#{}", func.name),
                None => func.name.clone(),
            };
            bail!("`{name}` uses a {unsupported}, which componentize does not support yet");
        }
    }
    Ok(())
}

/// Whether `ty` contains a `stream` or a `future`.
fn has_stream_or_future(resolve: &Resolve, ty: Type) -> bool {
    contains(resolve, ty, &|kind| {
        matches!(kind, TypeDefKind::Stream(_) | TypeDefKind::Future(_))
    })
}

/// Whether `ty` contains a `borrow`.
fn has_borrow(resolve: &Resolve, ty: Type) -> bool {
    contains(resolve, ty, &|kind| {
        matches!(kind, TypeDefKind::Handle(wit_parser::Handle::Borrow(_)))
    })
}

/// Whether `ty`, or a type it is made of, is a type whose kind `matches`. The
/// payloads of streams and futures are not searched.
fn contains(resolve: &Resolve, ty: Type, matches: &impl Fn(&TypeDefKind) -> bool) -> bool {
    contains_unvisited(resolve, ty, matches, &mut HashSet::new())
}

/// [`contains`], skipping the types in `visited` and adding each type it walks.
/// A type walked before contains no match, since the walk ends at the first one.
fn contains_unvisited(
    resolve: &Resolve,
    ty: Type,
    matches: &impl Fn(&TypeDefKind) -> bool,
    visited: &mut HashSet<TypeId>,
) -> bool {
    let Type::Id(id) = ty else {
        return false;
    };
    if !visited.insert(id) {
        return false;
    }
    let kind = &resolve.types[id].kind;
    if matches(kind) {
        return true;
    }
    let mut walk = |t: Type| contains_unvisited(resolve, t, matches, visited);
    match kind {
        TypeDefKind::Record(r) => r.fields.iter().any(|f| walk(f.ty)),
        TypeDefKind::Tuple(t) => t.types.iter().any(|&t| walk(t)),
        TypeDefKind::Variant(v) => v.cases.iter().any(|c| c.ty.is_some_and(&mut walk)),
        TypeDefKind::Option(t) | TypeDefKind::List(t) | TypeDefKind::Type(t) => walk(*t),
        TypeDefKind::Result(r) => [r.ok, r.err].into_iter().flatten().any(walk),
        _ => false,
    }
}

/// Refuse a world-level function import taking a `borrow<T>`, anywhere in a
/// parameter's type, which wac-graph 0.11 cannot encode: its type encoder accepts
/// a borrow only inside an interface (`TypeEncoder::borrow`, an assertion), so
/// composing the snapshot would panic. A world-level resource's methods are such
/// imports.
pub(crate) fn reject_world_level_borrows(resolve: &Resolve, world: WorldId) -> anyhow::Result<()> {
    for (key, item) in &resolve.worlds[world].imports {
        let WorldItem::Function(func) = item else {
            continue;
        };
        if func
            .params
            .iter()
            .any(|param| has_borrow(resolve, param.ty))
        {
            bail!(
                "the world-level import `{}` takes a `borrow`, which the component composition \
                 (wac-graph) cannot encode outside an interface. Declare the resource in an \
                 interface the world imports instead.",
                resolve.name_world_key(key)
            );
        }
    }
    Ok(())
}

/// The kind of the first unsupported type `ty` contains, if any.
fn unsupported_type(resolve: &Resolve, ty: Type) -> Option<&'static str> {
    unsupported_unvisited(resolve, ty, &mut HashSet::new())
}

/// [`unsupported_type`], skipping the types in `visited` and adding each type it
/// walks. A type walked before contains no unsupported type, since the walk ends
/// at the first one.
fn unsupported_unvisited(
    resolve: &Resolve,
    ty: Type,
    visited: &mut HashSet<TypeId>,
) -> Option<&'static str> {
    let id = match ty {
        Type::ErrorContext => return Some("`error-context`"),
        Type::Id(id) => id,
        _ => return None,
    };
    if !visited.insert(id) {
        return None;
    }
    let mut walk = |t: Type| unsupported_unvisited(resolve, t, visited);
    match &resolve.types[id].kind {
        TypeDefKind::Map(..) => Some("`map`"),
        TypeDefKind::FixedLengthList(..) => Some("fixed-length `list`"),
        TypeDefKind::Record(r) => r.fields.iter().find_map(|f| walk(f.ty)),
        TypeDefKind::Tuple(t) => t.types.iter().find_map(|&t| walk(t)),
        TypeDefKind::Variant(v) => v.cases.iter().find_map(|c| c.ty.and_then(&mut walk)),
        TypeDefKind::Option(t) | TypeDefKind::List(t) | TypeDefKind::Type(t) => walk(*t),
        TypeDefKind::Result(r) => [r.ok, r.err].into_iter().flatten().find_map(walk),
        TypeDefKind::Future(t) | TypeDefKind::Stream(t) => t.and_then(walk),
        TypeDefKind::Resource
        | TypeDefKind::Handle(_)
        | TypeDefKind::Flags(_)
        | TypeDefKind::Enum(_)
        | TypeDefKind::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(wit: &str) -> anyhow::Result<()> {
        let (resolve, world) = crate::load_world(
            crate::Wit::<std::path::PathBuf>::String(wit),
            None,
            &[],
            true,
        )?;
        check_supported_types(&resolve, world)
    }

    #[test]
    fn supported_types_pass() {
        check(
            "package test:ok;
             world w {
               record r { a: list<u8>, b: option<string> }
               export f: func(x: r) -> result<u32, string>;
             }",
        )
        .unwrap();
    }

    #[test]
    fn a_world_level_import_with_a_nested_borrow_is_refused() {
        let (resolve, world) = crate::load_world(
            crate::Wit::<std::path::PathBuf>::String(
                "package test:bad;
                 world w {
                   resource r { constructor(); }
                   import f: func(x: option<borrow<r>>) -> u32;
                   export run: func() -> u32;
                 }",
            ),
            None,
            &[],
            true,
        )
        .unwrap();
        let err = reject_world_level_borrows(&resolve, world)
            .unwrap_err()
            .to_string();
        assert!(err.contains("`f` takes a `borrow`"), "{err}");
    }

    #[test]
    fn a_sync_export_with_a_stream_is_refused() {
        let err = check(
            "package test:bad;
             interface i {
               g: func(s: option<stream<u8>>);
             }
             world w { export i; }",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("test:bad/i#g") && err.contains("async"),
            "{err}"
        );
    }

    #[test]
    fn an_async_export_or_a_sync_import_with_a_future_passes() {
        check(
            "package test:ok;
             interface i {
               g: async func(f: future<u32>) -> stream<u8>;
             }
             interface h {
               s: func(f: future<u32>);
             }
             world w { export i; import h; }",
        )
        .unwrap();
    }

    #[test]
    fn a_map_or_a_fixed_length_list_is_refused() {
        for (ty, kind) in [
            ("map<string, u32>", "`map`"),
            ("list<u8, 4>", "fixed-length `list`"),
        ] {
            let err = check(&format!(
                "package test:bad;
                 interface i {{ g: func(x: {ty}); }}
                 world w {{ import i; }}"
            ))
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("`test:bad/i#g`") && err.contains(kind),
                "{err}"
            );
        }
    }

    /// A type that is `depth` levels of `tuple<t, t>` over `u32`, so a walk that
    /// visits a shared type once per path reaching it takes 2^`depth` steps.
    fn doubling_tuples(depth: usize) -> String {
        let mut wit = String::from("type t0 = u32;\n");
        for i in 1..=depth {
            let _ = std::fmt::Write::write_fmt(
                &mut wit,
                format_args!("type t{i} = tuple<t{0}, t{0}>;\n", i - 1),
            );
        }
        wit
    }

    /// A walk visiting each path to a shared type would not finish.
    #[test]
    fn shared_sub_types_are_checked_once() {
        const DEPTH: usize = 64;
        let types = doubling_tuples(DEPTH);
        check(&format!(
            "package test:ok;
             interface i {{
               {types}
               g: func(x: t{DEPTH}) -> t{DEPTH};
             }}
             world w {{ export i; import i; }}"
        ))
        .unwrap();
        let err = check(&format!(
            "package test:bad;
             interface i {{
               {types}
               g: func(x: tuple<t{DEPTH}, map<string, u32>>);
             }}
             world w {{ import i; }}"
        ))
        .unwrap_err()
        .to_string();
        assert!(err.contains("`map`"), "{err}");
    }

    #[test]
    fn a_nested_error_context_names_the_function() {
        let err = check(
            "package test:bad;
             interface i {
               record r { e: option<error-context> }
               g: func() -> r;
             }
             world w { export i; }",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("`test:bad/i#g`"), "{err}");
        assert!(err.contains("error-context"), "{err}");
    }
}
