// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Canonicalizing the interfaces a world imports at several compatible versions, such as
//! `wasi:io/poll@0.2.0` and `wasi:io/poll@0.2.12` (see [`same_interface`]).

use anyhow::Context as _;
use wit_name_mapping::same_interface;
use wit_parser::{Resolve, WorldId, WorldKey};

/// Return `resolve` with `world` changed to import each interface only once if it imports the
/// interface at several compatible versions, so that the interface gets one module, and each of
/// its types one name. Returns `resolve` unchanged if `world` imports no interface at several
/// compatible versions.
///
/// The interface is imported at the highest of the versions, and every `use` of a type from a
/// lower version names the highest version instead (see
/// `Resolve::merge_world_imports_based_on_semver`). A version that lacks a type or function of a
/// lower compatible version isn't rejected.
pub(crate) fn canonicalize_compatible_versions(
    mut resolve: Resolve,
    world: WorldId,
) -> anyhow::Result<(Resolve, WorldId)> {
    if !imports_compatible_versions(&resolve, world) {
        return Ok((resolve, world));
    }
    resolve
        .merge_world_imports_based_on_semver(world)
        .context("importing each interface at one of its compatible versions")?;

    // TODO: Remove the round trip through WIT once `merge_world_imports_based_on_semver` keeps
    // `Resolve::types` topologically sorted.
    // `merge_world_imports_based_on_semver` changes a `use` of a type from a lower version to
    // refer to the type in the highest version, which can come after it in `Resolve::types`.
    // Code that walks the types in order, such as `SizeAlign::fill`, requires each type to come
    // after the types it refers to, so we print the merged `resolve` as WIT and parse it again.
    let main = resolve.worlds[world]
        .package
        .context("the world has a package")?;
    let nested: Vec<_> = resolve
        .packages
        .iter()
        .map(|(id, _)| id)
        .filter(|id| *id != main)
        .collect();
    let mut printer = wit_component::WitPrinter::default();
    printer
        .print(&resolve, main, &nested)
        .context("printing the world's WIT")?;
    let mut canonical = Resolve {
        features: resolve.features.clone(),
        all_features: resolve.all_features,
        ..Resolve::default()
    };
    let package = canonical
        .push_str("canonical.wit", &printer.output.to_string())
        .context("parsing the world's WIT with each interface at one version")?;
    let world = canonical
        .select_world(&[package], Some(&resolve.worlds[world].name))
        .context("selecting the world in its canonicalized WIT")?;
    Ok((canonical, world))
}

/// Returns whether `world` imports an interface at several compatible versions.
fn imports_compatible_versions(resolve: &Resolve, world: WorldId) -> bool {
    let names: Vec<String> = resolve.worlds[world]
        .imports
        .keys()
        .filter_map(|key| match key {
            WorldKey::Interface(id) => resolve.id_of(*id),
            WorldKey::Name(_) => None,
        })
        .collect();
    names.iter().enumerate().any(|(index, name)| {
        names[index + 1..]
            .iter()
            .any(|other| same_interface(name, other))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A world importing `test:cli/stdout@{older}`, which uses `test:io/poll@{older}`, and
    /// `test:cli/net@{newer}`, which uses `test:io/poll@{newer}`. `test:io/poll@{newer}` declares
    /// an additional function `extra`. `newer_first` is passed to [`with_older_app`].
    fn two_versions(older: &str, newer: &str, newer_first: bool) -> (Resolve, WorldId) {
        let newer = format!(
            "package test:newer;\n\
             world newer {{\n  import test:cli/net@{newer};\n}}\n\
             package test:io@{newer} {{\n  interface poll {{\n    resource pollable;\n    \
             extra: func();\n  }}\n}}\n\
             package test:cli@{newer} {{\n  interface net {{\n    use test:io/poll@{newer}.{{pollable}};\n    \
             get: func() -> pollable;\n  }}\n}}\n"
        );
        with_older_app(older, &newer, newer_first)
    }

    /// The world `newer` from the WIT `newer` merged into a world importing
    /// `test:cli/stdout@{older}`, which uses `test:io/poll@{older}`. The packages of `newer` are
    /// added to the `Resolve` before those of the older world if `newer_first` is set, and after
    /// them otherwise.
    fn with_older_app(older: &str, newer: &str, newer_first: bool) -> (Resolve, WorldId) {
        let older = format!(
            "package test:app;\n\
             world app {{\n  import test:cli/stdout@{older};\n}}\n\
             package test:io@{older} {{\n  interface poll {{\n    resource pollable;\n  }}\n}}\n\
             package test:cli@{older} {{\n  interface stdout {{\n    use test:io/poll@{older}.{{pollable}};\n    \
             get: func() -> pollable;\n  }}\n}}\n"
        );
        let mut resolve = Resolve::default();
        let mut push = |name: &str, wit: &str, world: &str| {
            let package = resolve.push_str(name, wit).unwrap();
            resolve.select_world(&[package], Some(world)).unwrap()
        };
        let (newer, app) = if newer_first {
            let newer = push("newer.wit", newer, "newer");
            (newer, push("app.wit", &older, "app"))
        } else {
            let app = push("app.wit", &older, "app");
            (push("newer.wit", newer, "newer"), app)
        };
        resolve
            .merge_worlds(newer, app, &mut wit_parser::CloneMaps::default())
            .unwrap();
        (resolve, app)
    }

    fn imports(resolve: &Resolve, world: WorldId) -> Vec<String> {
        let mut imports: Vec<String> = resolve.worlds[world]
            .imports
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect();
        imports.sort();
        imports
    }

    fn canonicalized((resolve, world): (Resolve, WorldId)) -> (Resolve, WorldId) {
        let (resolve, world) = canonicalize_compatible_versions(resolve, world).unwrap();
        resolve.assert_valid();
        // Computes the sizes of the types in their arena order, which fails on a type that uses
        // one defined after it.
        wit_parser::SizeAlign::default().fill(&resolve).unwrap();
        (resolve, world)
    }

    #[test]
    fn compatible_versions_are_imported_at_the_highest() {
        let (resolve, world) = canonicalized(two_versions("0.2.0", "0.2.1", true));
        assert_eq!(
            imports(&resolve, world),
            [
                "test:cli/net@0.2.1",
                "test:cli/stdout@0.2.0",
                "test:io/poll@0.2.1"
            ]
        );
    }

    /// The types of the highest version are defined after the types that use the lower version.
    #[test]
    fn types_are_defined_before_they_are_used() {
        let (resolve, world) = canonicalized(two_versions("0.2.0", "0.2.1", false));
        assert_eq!(
            imports(&resolve, world),
            [
                "test:cli/net@0.2.1",
                "test:cli/stdout@0.2.0",
                "test:io/poll@0.2.1"
            ]
        );
    }

    /// Compatible versions are ordered by semver, so `1.10.0` is higher than `1.2.0`.
    #[test]
    fn minor_versions_are_ordered_by_semver() {
        let (resolve, world) = canonicalized(two_versions("1.2.0", "1.10.0", true));
        assert_eq!(
            imports(&resolve, world),
            [
                "test:cli/net@1.10.0",
                "test:cli/stdout@1.2.0",
                "test:io/poll@1.10.0"
            ]
        );
    }

    #[test]
    fn a_world_without_compatible_versions_is_unchanged() {
        let (resolve, world) = two_versions("0.2.0", "0.3.0", true);
        let types = resolve.types.len();
        let (resolve, world) = canonicalize_compatible_versions(resolve, world).unwrap();
        assert_eq!(resolve.types.len(), types);
        assert_eq!(
            imports(&resolve, world),
            [
                "test:cli/net@0.3.0",
                "test:cli/stdout@0.2.0",
                "test:io/poll@0.2.0",
                "test:io/poll@0.3.0"
            ]
        );
    }

    /// An interface that `use`s a lower version from its own package uses the highest version.
    #[test]
    fn a_use_within_a_package_names_the_imported_version() {
        let newer = "package test:newer;\n\
             world newer {\n  import test:io/streams@0.2.1;\n}\n\
             package test:io@0.2.1 {\n  interface poll {\n    resource pollable;\n  }\n  \
             interface streams {\n    use poll.{pollable};\n    get: func() -> pollable;\n  }\n}\n";
        let (resolve, world) = canonicalized(with_older_app("0.2.0", newer, true));
        assert_eq!(
            imports(&resolve, world),
            [
                "test:cli/stdout@0.2.0",
                "test:io/poll@0.2.1",
                "test:io/streams@0.2.1"
            ]
        );
    }
}
