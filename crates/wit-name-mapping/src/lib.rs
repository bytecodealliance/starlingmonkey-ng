// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The mapping between WIT names and the JavaScript names of a componentized
//! application's imports and exports, used by the WIT bridge and the componentizer.
//!
//! WIT names are kebab-case. [`mangle_name`] converts a function or member name
//! to lowerCamelCase, and [`mangle_resource_name`] converts a resource name to
//! an UpperCamelCase class name.
//!
//! The application imports each WIT interface as an ES module. The module's
//! specifier is the interface's WIT name with its version shortened by
//! [`module_specifier`], such as `wasi:http/types@0.3`. World-level imported
//! functions are imported from the module [`WORLD_MODULE_NAME`].
//!
//! The application's main module exports a JavaScript implementation of each
//! function and resource class of the world's exported interfaces.
//! [`item_js_name`] returns the JS name of the item that implements a WIT
//! function, and [`FunctionName`] splits a WIT function name into its parts.
//! Each item can be exported in one of four shapes ([`Shape`]). For the function
//! `greet` of the interface `test:app/greeter@1.0.0`, the shapes are:
//!
//! ```js
//! // The package layer: the package name `test:app` in lowerCamelCase, then
//! // the interface name. The version is not part of the name.
//! export const testApp = { greeter: { greet } };
//! // The interface layer.
//! export const greeter = { greet };
//! // A bare export.
//! export function greet() {}
//! // The versioned layer: an export named by the interface's full WIT name.
//! export { greeterV1 as "test:app/greeter@1.0.0" };
//! ```
//!
//! [`possible_export_names`] computes, for each item, the shapes in which the
//! main module may export it. A shape is excluded if its name collides with the
//! name of another export. The main module exports each item in exactly one of
//! its possible shapes.
//!
//! An interface named without a package, such as `local`, has no package
//! layer. The versioned layer is used only when the world exports two versions
//! of one interface. Those versions have the same package-layer and
//! interface-layer names, so each is exported only in the versioned layer. A
//! world-level exported function is always a bare export.

use std::collections::{HashMap, HashSet};

use heck::{ToLowerCamelCase, ToUpperCamelCase};
pub use semver::Version;
use wasmparser::names;

/// The module specifier the world-level imports are provided under.
pub const WORLD_MODULE_NAME: &str = "external";

/// The characters of a WIT name that a JS identifier cannot contain, which
/// [`mangle_name`] and [`mangle_resource_name`] replace with `_`.
const PUNCTUATION: [char; 7] = ['@', ':', '/', '-', '[', ']', '.'];

/// Mangle a WIT identifier into its JavaScript form.
///
/// Punctuation that is illegal in a JS identifier is replaced with `_`, then the
/// result is lower-camel-cased.
pub fn mangle_name(s: &str) -> String {
    s.replace(PUNCTUATION, "_").to_lower_camel_case()
}

/// Mangle a WIT resource-type name into its JavaScript class name.
///
/// Resource classes are UpperCamelCase (`resource-type` becomes `ResourceType`),
/// unlike functions and members, which [`mangle_name`] lower-camel-cases.
pub fn mangle_resource_name(s: &str) -> String {
    s.replace(PUNCTUATION, "_").to_upper_camel_case()
}

/// A WIT function name, such as `greet`, `[constructor]counter`,
/// `[method]counter.bump` or `[static]counter.origin`, split into its parts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FunctionName<'a> {
    /// A function that doesn't belong to a resource.
    Plain(&'a str),
    /// The constructor of `resource`.
    Constructor { resource: &'a str },
    /// The method `name` of `resource`.
    Method { resource: &'a str, name: &'a str },
    /// The static function `name` of `resource`.
    Static { resource: &'a str, name: &'a str },
}

impl<'a> FunctionName<'a> {
    /// Parse the WIT function name `name`. A name that isn't a valid component
    /// function name is a [`FunctionName::Plain`].
    pub fn parse(name: &'a str) -> FunctionName<'a> {
        let valid = names::ComponentName::new(name, 0)
            .is_ok_and(|parsed| matches!(parsed.kind(), names::ComponentNameKind::Plain(_)));
        if !valid {
            return FunctionName::Plain(name);
        }
        let plain = names::PlainName::new(name);
        match (plain.resource_func, plain.resource()) {
            (Some(names::ResourceFuncKind::Constructor), Some(resource)) => {
                FunctionName::Constructor {
                    resource: resource.as_str(),
                }
            }
            (Some(names::ResourceFuncKind::Method), Some(resource)) => FunctionName::Method {
                resource: resource.as_str(),
                name: plain.name().as_str(),
            },
            (Some(names::ResourceFuncKind::Static), Some(resource)) => FunctionName::Static {
                resource: resource.as_str(),
                name: plain.name().as_str(),
            },
            _ => FunctionName::Plain(name),
        }
    }
}

/// Return the JS name of the item that implements the WIT function `function`:
/// the function's name, or the class name of the resource for a
/// `[constructor]`, `[method]` or `[static]` function. The main module exports
/// the item under one of the names returned by [`possible_export_names`].
pub fn item_js_name(function: &str) -> String {
    match FunctionName::parse(function) {
        FunctionName::Plain(name) => mangle_name(name),
        FunctionName::Constructor { resource }
        | FunctionName::Method { resource, .. }
        | FunctionName::Static { resource, .. } => mangle_resource_name(resource),
    }
}

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
        let unversioned = unversioned(wit_name);
        let (package, interface) = match unversioned.split_once('/') {
            Some((package, interface)) => (Some(mangle_name(package)), interface),
            None => (None, unversioned),
        };
        InterfaceNames {
            wit_name: wit_name.to_string(),
            package,
            interface: mangle_name(interface),
        }
    }
}

/// Return the interface name `name`, such as `wasi:http/types@0.3.0`, without
/// its version.
pub fn unversioned(name: &str) -> &str {
    name.split_once('@').map_or(name, |(name, _)| name)
}

/// Return the name of a WIT function for messages: `interface#function`, or
/// `function` for a function of the world.
pub fn function_wit_name(interface: Option<&str>, function: &str) -> String {
    match interface {
        Some(interface) => format!("{interface}#{function}"),
        None => function.to_string(),
    }
}

/// Whether the interface names `a` and `b`, such as `wasi:http/types@0.3.0`,
/// name compatible versions of one interface: their names before the `@` are
/// equal, and so are their versions' `canonical_version`s, or neither has a
/// version.
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

/// Whether the semver versions `a` and `b` have the same
/// `canonical_version`. A version that doesn't parse is compatible only with
/// an equal one.
fn compatible_versions(a: &str, b: &str) -> bool {
    match (canonical_version(a), canonical_version(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a == b,
    }
}

/// The canonical version of `version`, as the component model defines it for
/// interface names, or `None` if `version` is neither a full semver version
/// nor a canonical version. Compatible versions have the same canonical
/// version, which is:
///
/// - `major`, for a release whose major number isn't 0, such as `1` for
///   `1.2.3`;
/// - `0.minor`, for a release whose major number is 0 and whose minor number
///   isn't, such as `0.3` for `0.3.1`;
/// - the version without its build metadata otherwise, such as `0.0.1` for
///   `0.0.1` and `0.3.0-rc.1` for `0.3.0-rc.1+build.1`;
/// - `version` itself if it is already canonical, such as `0.3`.
fn canonical_version(version: &str) -> Option<&str> {
    names::split_canonical_version(version)
        .map(|(canonical, _)| canonical)
        .or_else(|| names::is_canonical_version(version).then_some(version))
}

/// The module specifier under which a guest imports the interface named
/// `name`: `name` with its version replaced by its `canonical_version`, such
/// as `wasi:http/types@0.3` for `wasi:http/types@0.3.0`. A name without a
/// version, or with one that doesn't parse, is returned unchanged. The runtime
/// resolves the specifier to the interface's module.
pub fn module_specifier(name: &str) -> std::borrow::Cow<'_, str> {
    let Some((unversioned, version)) = name.split_once('@') else {
        return name.into();
    };
    match canonical_version(version) {
        Some(canonical) => format!("{unversioned}@{canonical}").into(),
        None => name.into(),
    }
}

/// The version of the interface name `name`, such as `0.2.12` for
/// `wasi:cli/run@0.2.12`, or `None` for a name without a version or with one
/// that isn't a full semver version.
pub fn interface_version(name: &str) -> Option<Version> {
    let (_, version) = name.split_once('@')?;
    Version::parse(version).ok()
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

/// Returns the names under which the main module may export each of `items`.
///
/// `items` lists every function and resource class the world exports,
/// including world-level functions. A possible export name of an item is a
/// property path in the main module's namespace object where the
/// implementation of the item may be found, such as `testApp.greeter.greet`
/// or a bare `greet`. The result has one entry per item, in the order of
/// `items`. Each entry holds the [`Shape`]s of the item's possible export
/// names, in the order the shapes are listed on [`Shape`]. [`Shape::path`]
/// turns a shape into the name. The main module exports each item under
/// exactly one of its possible names.
///
/// A shape is excluded if its name collides with the name of another export,
/// so the possible names of one item depend on all of `items` and `fixed`:
///
/// - A world-level function is always a bare export.
/// - The package layer is possible for every interface with a package, unless
///   another version of the interface has the same package-layer and
///   interface-layer names. Such an interface is exported in the versioned
///   layer only.
/// - The interface layer is possible unless another exported interface has the
///   same interface-layer name, or the name is a package-layer name or the name
///   of a world-level function or of an entry in `fixed`.
/// - A bare export is possible unless another item or a world-level function
///   has the same name, or the name is a package-layer name, a possible
///   interface-layer name, or an entry in `fixed`.
///
/// `fixed` lists the JS names the main module exports for other purposes,
/// such as a CLI tool's `run`. An item may appear more than once, and gets the
/// same shapes each time.
///
/// Fails when a world-level function or an entry in `fixed` has a
/// package-layer name.
pub fn possible_export_names(items: &[Item], fixed: &[&str]) -> Result<Vec<Vec<Shape>>, String> {
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
        let shapes = possible_export_names(&items, fixed)?;
        Ok(items
            .iter()
            .zip(shapes)
            .map(|((_, name), shapes)| shapes.iter().map(|shape| shape.path(name)).collect())
            .collect())
    }

    fn expect(items: &[(Option<&str>, &str)], fixed: &[&str], expected: &[&[&str]]) {
        let actual = paths(items, fixed).expect("possible export names");
        assert_eq!(actual, expected, "for {items:?}");
    }

    #[test]
    fn function_names_are_split_into_their_parts() {
        let cases = [
            ("greet", FunctionName::Plain("greet")),
            (
                "[constructor]counter",
                FunctionName::Constructor {
                    resource: "counter",
                },
            ),
            (
                "[method]http-request.set-body",
                FunctionName::Method {
                    resource: "http-request",
                    name: "set-body",
                },
            ),
            (
                "[static]counter.origin",
                FunctionName::Static {
                    resource: "counter",
                    name: "origin",
                },
            ),
            // Not valid component names, so they are plain.
            ("[method]no-dot", FunctionName::Plain("[method]no-dot")),
            ("notKebab", FunctionName::Plain("notKebab")),
        ];
        for (name, expected) in cases {
            assert_eq!(FunctionName::parse(name), expected, "{name}");
        }
    }

    #[test]
    fn item_js_names_are_the_function_or_the_class() {
        assert_eq!(item_js_name("do-thing"), "doThing");
        assert_eq!(item_js_name("[constructor]http-request"), "HttpRequest");
        assert_eq!(item_js_name("[method]http-request.set-body"), "HttpRequest");
        assert_eq!(
            item_js_name("[static]http-request.new-empty"),
            "HttpRequest"
        );
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
    fn unrepresentable_worlds_are_rejected() {
        let world_function = paths(&[(Some("test:v/api"), "greet"), (None, "testV")], &[]);
        assert!(world_function
            .unwrap_err()
            .contains("the world-level export `testV` has the same JS name"));
    }
}
