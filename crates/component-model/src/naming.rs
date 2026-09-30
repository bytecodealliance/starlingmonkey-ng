// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The JS names under which the main module provides the world's exports.
//!
//! An item of an exported interface, a function or a resource class, can be
//! provided in up to four shapes:
//!
//! - the package layer, `nsPkg.iface.item`, for an interface named by package
//!   (`ns:pkg/iface@version`), where `nsPkg` is `ns-pkg` in lowerCamelCase and
//!   the version is dropped;
//! - the interface layer, `iface.item`;
//! - a bare export, `item`;
//! - the versioned layer, an export named by the interface's full WIT name
//!   (`export { impl as "ns:pkg/iface@1.0.0" }`), only for interfaces whose
//!   package and interface layers another version of the interface shares.
//!
//! [`plan`] computes the shapes each item allows. A world-level exported
//! function is always a bare export.

use std::collections::{HashMap, HashSet};

use heck::ToLowerCamelCase;

/// The JS names of an exported interface.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InterfaceNames {
    /// The interface's WIT name, for messages.
    pub wit_name: String,
    /// The package-layer name, or `None` for an interface the world exports
    /// under a plain name.
    pub package: Option<String>,
    /// The interface-layer name.
    pub interface: String,
}

impl InterfaceNames {
    /// The names of the interface the world exports as `wit_name`: either
    /// `ns:pkg/iface`, optionally followed by `@version`, or a plain name.
    pub fn of(wit_name: &str) -> InterfaceNames {
        let unversioned = wit_name.split_once('@').map_or(wit_name, |(name, _)| name);
        let (package, interface) = match unversioned.split_once('/') {
            Some((package, interface)) => {
                let package = package.replace(':', "-").to_lower_camel_case();
                (Some(package), interface)
            }
            None => (None, unversioned),
        };
        InterfaceNames {
            wit_name: wit_name.to_string(),
            package,
            interface: interface.to_lower_camel_case(),
        }
    }
}

/// Whether the interface names `a` and `b`, such as `wasi:http/types@0.3.0`,
/// name compatible versions of one interface: their names before the `@` are
/// equal, and so are their versions' major and minor numbers and pre-release
/// suffixes, or neither has a version. The patch number is ignored.
pub fn same_interface(a: &str, b: &str) -> bool {
    fn split(name: &str) -> (&str, Option<&str>) {
        match name.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (name, None),
        }
    }
    let ((a_name, a_version), (b_name, b_version)) = (split(a), split(b));
    a_name == b_name
        && match (a_version, b_version) {
            (Some(a), Some(b)) => compatible_versions(a, b),
            (None, None) => true,
            _ => false,
        }
}

/// Whether the semver versions `a` and `b` have the same major and minor
/// numbers and pre-release suffix. Build metadata and the patch number are
/// ignored. A version that doesn't parse is compatible only with an equal one.
fn compatible_versions(a: &str, b: &str) -> bool {
    // The major and minor numbers and the pre-release suffix.
    fn key(version: &str) -> Option<(&str, &str, &str)> {
        let version = version
            .split_once('+')
            .map_or(version, |(version, _)| version);
        let (numbers, pre) = version.split_once('-').unwrap_or((version, ""));
        let mut parts = numbers.split('.');
        let (major, minor, _patch) = (parts.next()?, parts.next()?, parts.next()?);
        parts.next().is_none().then_some((major, minor, pre))
    }
    match (key(a), key(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// One shape an item can be provided in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shape {
    /// `package.interface.item`.
    Package { package: String, interface: String },
    /// `interface.item`.
    Interface(String),
    /// `item`, a member of the main module's namespace.
    Bare,
    /// `item` of the export named by an interface's full WIT name.
    Versioned(String),
}

impl Shape {
    /// The JS path of `item` in this shape, such as `nsPkg.iface.item`.
    pub fn path(&self, item: &str) -> String {
        match self {
            Shape::Package { package, interface } => format!("{package}.{interface}.{item}"),
            Shape::Interface(interface) => format!("{interface}.{item}"),
            Shape::Bare => item.to_string(),
            Shape::Versioned(wit_name) => format!("[\"{wit_name}\"].{item}"),
        }
    }
}

/// One exported item: the names of its interface, or `None` for a world-level
/// function, and its JS name.
pub type Item = (Option<InterfaceNames>, String);

/// The shapes that may provide each of `items`, in the order the shapes
/// are listed on [`Shape`].
///
/// - The package layer is allowed for every interface with a package, unless
///   another version of the interface has the same package-layer and
///   interface-layer names. Such an interface is provided in the versioned
///   layer only.
/// - The interface layer is allowed unless another exported interface has the
///   same interface-layer name, or the name is a package-layer name or the name
///   of a world-level function or of an entry in `fixed`.
/// - A bare export is allowed unless another item or a world-level function has
///   the same name, or the name is a package-layer name, an allowed
///   interface-layer name, or an entry in `fixed`.
///
/// `fixed` lists the JS names the main module provides for other purposes,
/// such as a CLI tool's `run`. An item may appear more than once, and gets the
/// same shapes each time.
///
/// Fails when a world-level function or an entry in `fixed` has a
/// package-layer name.
pub fn plan(items: &[Item], fixed: &[&str]) -> Result<Vec<Vec<Shape>>, String> {
    let mut interfaces: Vec<&InterfaceNames> = Vec::new();
    let mut interface_items: HashSet<(&InterfaceNames, &str)> = HashSet::new();
    let mut world_items: HashSet<&str> = fixed.iter().copied().collect();
    for (interface, name) in items {
        match interface {
            Some(interface) => {
                if !interfaces.contains(&interface) {
                    interfaces.push(interface);
                }
                interface_items.insert((interface, name));
            }
            None => {
                world_items.insert(name);
            }
        }
    }

    let packages: HashSet<&str> = interfaces
        .iter()
        .filter_map(|interface| interface.package.as_deref())
        .collect();
    if let Some(name) = world_items.iter().find(|name| packages.contains(**name)) {
        return Err(format!(
            "the world-level export `{name}` has the same JS name as a package's exports"
        ));
    }
    let mut qualified: HashMap<(&str, &str), usize> = HashMap::new();
    for interface in &interfaces {
        if let Some(package) = &interface.package {
            *qualified
                .entry((package.as_str(), interface.interface.as_str()))
                .or_default() += 1;
        }
    }
    let versioned = |interface: &InterfaceNames| {
        interface
            .package
            .as_deref()
            .is_some_and(|package| qualified[&(package, interface.interface.as_str())] > 1)
    };

    let mut interface_counts: HashMap<&str, usize> = HashMap::new();
    for interface in &interfaces {
        *interface_counts.entry(&interface.interface).or_default() += 1;
    }
    let interface_layer = |interface: &InterfaceNames| {
        let name = interface.interface.as_str();
        interface_counts[name] == 1 && !packages.contains(name) && !world_items.contains(name)
    };
    let interface_layers: HashSet<&str> = interfaces
        .iter()
        .filter(|interface| interface_layer(interface))
        .map(|interface| interface.interface.as_str())
        .collect();

    let mut item_counts: HashMap<&str, usize> = HashMap::new();
    for name in interface_items
        .iter()
        .map(|(_, name)| *name)
        .chain(world_items.iter().copied())
    {
        *item_counts.entry(name).or_default() += 1;
    }

    Ok(items
        .iter()
        .map(|(interface, name)| {
            let Some(interface) = interface else {
                return vec![Shape::Bare];
            };
            if versioned(interface) {
                return vec![Shape::Versioned(interface.wit_name.clone())];
            }
            let mut shapes = Vec::new();
            if let Some(package) = &interface.package {
                shapes.push(Shape::Package {
                    package: package.clone(),
                    interface: interface.interface.clone(),
                });
            }
            if interface_layer(interface) {
                shapes.push(Shape::Interface(interface.interface.clone()));
            }
            let name = name.as_str();
            if item_counts[name] == 1
                && !packages.contains(name)
                && !interface_layers.contains(name)
            {
                shapes.push(Shape::Bare);
            }
            shapes
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The JS paths each of `items` is accepted under, for interfaces given by
    /// WIT name and world-level functions given with `None`.
    fn paths(items: &[(Option<&str>, &str)], fixed: &[&str]) -> Result<Vec<Vec<String>>, String> {
        let items: Vec<Item> = items
            .iter()
            .map(|(interface, name)| (interface.map(InterfaceNames::of), name.to_string()))
            .collect();
        let shapes = plan(&items, fixed)?;
        Ok(items
            .iter()
            .zip(shapes)
            .map(|((_, name), shapes)| shapes.iter().map(|shape| shape.path(name)).collect())
            .collect())
    }

    fn expect(items: &[(Option<&str>, &str)], fixed: &[&str], expected: &[&[&str]]) {
        let actual = paths(items, fixed).expect("a valid plan");
        assert_eq!(actual, expected, "for {items:?}");
    }

    #[test]
    fn every_shape_when_names_are_unique() {
        let api = Some("test:naming/api");
        expect(
            &[(api, "greet"), (api, "Counter")],
            &[],
            &[
                &["testNaming.api.greet", "api.greet", "greet"],
                &["testNaming.api.Counter", "api.Counter", "Counter"],
            ],
        );
    }

    #[test]
    fn interfaces_sharing_a_name_keep_only_the_package_layer() {
        expect(
            &[
                (Some("test:a/api"), "greet"),
                (Some("test:b/api@1.0.0"), "greet"),
            ],
            &[],
            &[&["testA.api.greet"], &["testB.api.greet"]],
        );
    }

    #[test]
    fn items_sharing_a_name_lose_the_bare_shape() {
        let (left, right) = (Some("test:naming/left"), Some("test:naming/right"));
        expect(
            &[(left, "greet"), (left, "onlyLeft"), (right, "greet")],
            &[],
            &[
                &["testNaming.left.greet", "left.greet"],
                &["testNaming.left.onlyLeft", "left.onlyLeft", "onlyLeft"],
                &["testNaming.right.greet", "right.greet"],
            ],
        );
    }

    #[test]
    fn a_plain_named_interface_has_no_package_layer() {
        expect(
            &[(Some("local"), "greet")],
            &[],
            &[&["local.greet", "greet"]],
        );
    }

    #[test]
    fn a_world_level_function_takes_its_name() {
        let api = Some("test:naming/api");
        expect(
            &[(api, "greet"), (api, "Counter"), (None, "greet")],
            &[],
            &[
                &["testNaming.api.greet", "api.greet"],
                &["testNaming.api.Counter", "api.Counter", "Counter"],
                &["greet"],
            ],
        );
        expect(
            &[(api, "greet"), (api, "Counter"), (None, "api")],
            &[],
            &[
                &["testNaming.api.greet", "greet"],
                &["testNaming.api.Counter", "Counter"],
                &["api"],
            ],
        );
    }

    #[test]
    fn no_item_is_bare_under_a_layer_name() {
        expect(
            &[(Some("test:naming/same"), "same")],
            &[],
            &[&["testNaming.same.same", "same.same"]],
        );
        expect(
            &[(Some("test:naming/pkg-named"), "testNaming")],
            &[],
            &[&["testNaming.pkgNamed.testNaming", "pkgNamed.testNaming"]],
        );
    }

    #[test]
    fn fixed_names_are_taken() {
        let api = Some("test:naming/api");
        expect(
            &[(api, "run"), (Some("test:naming/run"), "go")],
            &["run"],
            &[
                &["testNaming.api.run", "api.run"],
                &["testNaming.run.go", "go"],
            ],
        );
    }

    #[test]
    fn the_http_handler_has_every_shape() {
        expect(
            &[(Some("wasi:http/handler@0.3.0"), "handle")],
            &[],
            &[&["wasiHttp.handler.handle", "handler.handle", "handle"]],
        );
    }

    #[test]
    fn versions_of_one_interface_take_the_versioned_layer() {
        expect(
            &[
                (Some("test:v/api@1.0.0"), "greet"),
                (Some("test:v/api@2.0.0"), "greet"),
                (Some("test:v/other@1.0.0"), "wave"),
            ],
            &[],
            &[
                &["[\"test:v/api@1.0.0\"].greet"],
                &["[\"test:v/api@2.0.0\"].greet"],
                &["testV.other.wave", "other.wave", "wave"],
            ],
        );
    }

    #[test]
    fn interface_versions_match_on_minor() {
        assert!(same_interface(
            "wasi:http/types@0.3.0",
            "wasi:http/types@0.3.2"
        ));
        assert!(same_interface("a:b/c@1.2.3+build", "a:b/c@1.2.0"));
        assert!(same_interface("a:b/c", "a:b/c"));
        assert!(!same_interface(
            "wasi:http/types@0.3.0",
            "wasi:http/types@0.4.0"
        ));
        assert!(!same_interface("a:b/c@1.2.0", "a:b/c@2.2.0"));
        assert!(!same_interface("a:b/c@0.3.0-rc-1", "a:b/c@0.3.0"));
        assert!(!same_interface("a:b/c@0.3.0", "a:b/c"));
        assert!(!same_interface("a:b/c@0.3.0", "a:b/d@0.3.0"));
    }

    #[test]
    fn unrepresentable_worlds_are_rejected() {
        let world_function = paths(&[(Some("test:v/api"), "greet"), (None, "testV")], &[]);
        assert!(world_function
            .unwrap_err()
            .contains("the world-level export `testV` has the same JS name"));
    }
}
