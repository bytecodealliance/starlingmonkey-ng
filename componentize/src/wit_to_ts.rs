// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Generate a TypeScript declaration (`.d.ts`) describing the guest JavaScript
//! module that this componentize pipeline expects an author to write for a given
//! WIT world.
//!
//! The output is one declaration file with nothing exported at file level, so
//! every `declare module` in it is an ambient module declaration. It declares:
//!
//! - `starling:types/<world>`: every named WIT type the world reaches, one class per
//!   resource, and one error class per `err` type that has one. Names that
//!   several types share are qualified (see [`Generator::assign_names`]).
//! - `starling:guest`: what the guest implements, in every naming shape the
//!   runtime accepts for the world (see [`export_layout`]).
//! - One module per imported interface, named by its WIT name, plus `external`
//!   for the world-level imports. Each declares the interface's functions and
//!   re-exports its types, resource classes and error classes from
//!   that module under their plain WIT names. This matches the module
//!   synthesis in the sibling `component-model` crate's `imports.rs`.
//!
//! A world exporting `wasi:http/handler` also gets file-level declarations for
//! the `fetch` event the runtime dispatches to listeners.
//!
//! The type mapping follows the WIT-to-JS-value mapping in `component-model`'s
//! `value.rs`, and each rendering site names the conversion it tracks.
//!
//! A payload-less `stream` or `future`, `error-context` and any
//! otherwise-unsupported WIT kind render as `unknown`. Each gets an inline
//! `// TODO`, the header lists which deferrals the world actually hit, and the
//! generator never fails on them.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;

use anyhow::{bail, Result};
use heck::{ToLowerCamelCase, ToUpperCamelCase};
use wit_parser::{
    Function, FunctionKind, Handle, InterfaceId, Resolve, Type, TypeDefKind, TypeId, TypeOwner,
    World, WorldId, WorldItem, WorldKey,
};

/// The global `ComponentError` class the runtime installs, which error classes
/// extend. Declared as an interface and a variable, as the DOM library declares
/// its classes, so two generated files can both declare it.
const COMPONENT_ERROR_DECL: &str = "\
// `ComponentError` holds the `err` payload of a WIT `result`. A string payload
// is also its `message`. The error classes in the types module extend it.
interface ComponentError extends Error {
  readonly payload: unknown;
}
declare var ComponentError: {
  new (payload: unknown): ComponentError;
  readonly prototype: ComponentError;
};

";

/// The globals the runtime provides to a world exporting `wasi:http/handler`,
/// which the DOM library lacks. Declared by interface merging only, so the file
/// also compiles against the `webworker` library, which declares `FetchEvent`
/// itself. Adding `fetch` to `WindowEventMap` types the event of an
/// `addEventListener('fetch', …)` listener.
const FETCH_EVENT_DECL: &str = "\
// The event the runtime dispatches to `addEventListener('fetch', …)` listeners.
interface FetchEvent extends Event {
  readonly request: Request;
  respondWith(response: Response | PromiseLike<Response>): void;
  waitUntil(promise: PromiseLike<unknown>): void;
}
interface WindowEventMap {
  fetch: FetchEvent;
}

";

/// The `wasi:http/types` interface whose `request` the runtime's `fetch` takes:
/// the one of the `wasi:http` version the runtime links, or of a version
/// [`same_interface`](crate::same_interface) matches with it.
const WASI_HTTP_TYPES: &str = "wasi:http/types@0.3.0";

/// The module specifier the import synthesizer registers world-level imports
/// under (`imports.rs`'s `WORLD_MODULE_NAME`).
const WORLD_MODULE_NAME: &str = "external";

/// The interface the runtime special-cases as the `wasi:cli/run` command
/// entry: the guest exports a bare `run` function (see `component_model::run`).
const CLI_RUN_INTERFACE: &str = "wasi:cli/run";

/// The interface the runtime serves either through `fetch` listeners or through
/// the guest's own handler, which the componentizer exports under
/// [`RAW_HTTP_HANDLER_EXPORT`](crate::RAW_HTTP_HANDLER_EXPORT).
const HTTP_HANDLER_INTERFACE: &str = "wasi:http/handler";

/// The module specifier the guest's own exports are declared under.
///
/// The whole file stays free of top-level `export`s, which makes every
/// `declare module` in it an ambient module declaration rather than an
/// augmentation of a module that cannot be resolved. Only an ambient one lets
/// the guest's `import … from '<wit-interface>'` resolve, so the guest's
/// obligations go in a module of their own rather than at file level.
const GUEST_MODULE_NAME: &str = "starling:guest";

/// The prefix of the module specifier the world's WIT types are declared
/// under, which [`types_module_name`] completes with the world's name.
///
/// They cannot be file-level declarations: those are global in a file with no
/// top-level `export`, and a WIT type named `permissions`, `response` or `event`
/// would then collide with the standard library.
const TYPES_MODULE_PREFIX: &str = "starling:types";

/// The module specifier the WIT types of `world` are declared under:
/// `starling:types/` followed by the world's qualified name, as in
/// `starling:types/wasi:http/proxy@0.3.0`. Declarations generated for different
/// worlds can then be part of one program.
pub fn types_module_name(resolve: &Resolve, world: WorldId) -> String {
    let world = &resolve.worlds[world];
    match world.package {
        Some(package) => format!(
            "{TYPES_MODULE_PREFIX}/{}",
            resolve.id_of_name(package, &world.name)
        ),
        None => format!("{TYPES_MODULE_PREFIX}/{}", world.name),
    }
}

/// The namespace every other module block imports the types module as. A WIT
/// identifier mangles to letters and digits only, so no generated name can
/// shadow it.
const TYPES_NAMESPACE: &str = "$t";

/// Mangle a WIT identifier to its JavaScript member form (lowerCamelCase),
/// matching `value.rs`'s `mangle_name`.
fn mangle_name(s: &str) -> String {
    s.replace(['@', ':', '/', '-', '[', ']', '.'], "_")
        .to_lower_camel_case()
}

/// Mangle a WIT resource-type name to its JavaScript class name (UpperCamelCase),
/// matching `value.rs`'s `mangle_resource_name`.
fn mangle_resource_name(s: &str) -> String {
    s.replace(['@', ':', '/', '-', '[', ']', '.'], "_")
        .to_upper_camel_case()
}

/// Indent every non-empty line of `text` by two spaces, for nesting a rendered
/// block inside a `declare module`.
fn indent(text: &str) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line.is_empty() {
            out.push('\n');
        } else {
            let _ = writeln!(out, "  {line}");
        }
    }
    out
}

/// The names a mangled WIT identifier cannot be bound to in a strict-mode
/// module: JavaScript's reserved words, the strict-mode reserved words,
/// `arguments` and `eval`, plus `globalThis`, which declarations here refer to.
/// A WIT name escaped with `%`, such as `%new` or `%for`, mangles to one of
/// these.
///
/// The runtime looks an export up under its mangled name, so an exported name
/// itself cannot change: the guest really does have to define a member under it.
/// Only how the declaration spells it changes. A parameter name is not looked
/// up, so a parameter is renamed instead (see [`param_name`]).
const RESERVED_BINDINGS: &[&str] = &[
    "arguments",
    "await",
    "break",
    "case",
    "catch",
    "class",
    "const",
    "continue",
    "debugger",
    "default",
    "delete",
    "do",
    "else",
    "enum",
    "eval",
    "export",
    "extends",
    "false",
    "finally",
    "for",
    "function",
    "globalThis",
    "if",
    "implements",
    "import",
    "in",
    "instanceof",
    "interface",
    "let",
    "new",
    "null",
    "package",
    "private",
    "protected",
    "public",
    "return",
    "static",
    "super",
    "switch",
    "this",
    "throw",
    "true",
    "try",
    "typeof",
    "var",
    "void",
    "while",
    "with",
    "yield",
];

fn is_reserved_binding(name: &str) -> bool {
    RESERVED_BINDINGS.contains(&name)
}

/// A member name for an object type literal. `new` is quoted, since `new(x): T`
/// in a type literal is a construct signature rather than a method. Every other
/// reserved word is an ordinary property name and needs nothing.
fn member_name(js_name: &str) -> String {
    if js_name == "new" {
        format!("'{js_name}'")
    } else {
        js_name.to_string()
    }
}

/// The TypeScript name of a WIT parameter: its mangled name, with `_` appended
/// when that is a reserved binding. A parameter named `this` would otherwise
/// declare the type of the receiver rather than an argument.
fn param_name(wit_name: &str) -> String {
    let name = mangle_name(wit_name);
    if is_reserved_binding(&name) {
        format!("{name}_")
    } else {
        name
    }
}

/// Declare the bare export `js_name` with `decl`, a declaration missing only its
/// name, e.g. `function {}(): void` or `const {}: number`.
///
/// A reserved word cannot be a binding, so it is declared under a local with
/// `_` appended and exported under the real name. An export clause accepts any
/// identifier name, and the guest's module exports it the same way, as
/// `export { make as new }`. A WIT `%default` becomes the `default` export,
/// which is the property the runtime looks up on the namespace.
fn bare_export(js_name: &str, decl: &str) -> Vec<String> {
    if is_reserved_binding(js_name) {
        let local = format!("{js_name}_");
        vec![
            format!("{};", decl.replace("{}", &local)),
            format!("export {{ {local} as {js_name} }};"),
        ]
    } else {
        vec![format!("export {};", decl.replace("{}", js_name))]
    }
}

/// Whether the world key name `wit_name` names `interface`, at any version.
fn names_interface(wit_name: &str, interface: &str) -> bool {
    wit_name
        .strip_prefix(interface)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('@'))
}

/// The resource a function is a constructor, method or static of, or `None` for
/// a freestanding function.
fn resource_member_of(func: &Function) -> Option<TypeId> {
    match func.kind {
        FunctionKind::Constructor(id)
        | FunctionKind::Method(id)
        | FunctionKind::AsyncMethod(id)
        | FunctionKind::Static(id)
        | FunctionKind::AsyncStatic(id) => Some(id),
        FunctionKind::Freestanding | FunctionKind::AsyncFreestanding => None,
    }
}

fn is_async(func: &Function) -> bool {
    matches!(
        func.kind,
        FunctionKind::AsyncFreestanding
            | FunctionKind::AsyncMethod(_)
            | FunctionKind::AsyncStatic(_)
    )
}

/// Generate the `.d.ts` source for `world` in `resolve`.
///
/// The serve declarations are emitted for every world exporting
/// `wasi:http/handler`, or the [`RAW_HTTP_HANDLER_EXPORT`](crate::RAW_HTTP_HANDLER_EXPORT)
/// that [`load_world_for_types`](crate::load_world_for_types) replaces it with.
///
/// Fails when the world's exports cannot all be given a JS name (see
/// [`export_layout`]).
pub fn generate(resolve: &Resolve, world: WorldId) -> Result<String> {
    Generator::new(resolve, world).run()
}

/// The module specifiers under which the runtime serves `world`'s imports: the
/// WIT name of each imported interface, and `external` for the world-level
/// imports. A module is listed only if the runtime registers it, which it does
/// if the module exports a function, a resource class, an error class, or an
/// enum or flags object.
pub fn import_modules(resolve: &Resolve, world: WorldId) -> Vec<String> {
    let mut generator = Generator::new(resolve, world);
    generator.collect();
    generator
        .import_module_groups()
        .into_iter()
        .filter(|module| generator.registers_module(module))
        .map(|module| module.spec)
        .collect()
}

/// Who implements a function or resource.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Side {
    /// The host: an imported function, or a resource of an imported interface.
    Import,
    /// The guest: an exported function, or a resource of an exported interface.
    Export,
}

/// Where a type is rendered.
#[derive(Clone, Copy)]
struct Cx {
    /// The side of the function or declaration the type is part of. A named
    /// type referring to a resource of an exported interface has a separate
    /// declaration for the export side (see [`Generator::side_of`]).
    side: Side,
    /// Whether the guest hands the value to the runtime, rather than receiving
    /// it. A `stream` or `future` the guest hands over accepts more than the
    /// `ReadableStream` or `Promise` it receives.
    produces: bool,
    /// Whether the type is rendered inside the types module, which names its own
    /// declarations directly and every other block names through
    /// [`TYPES_NAMESPACE`].
    in_types: bool,
}

impl Cx {
    /// The parameters of a function implemented on `side`, which the guest
    /// hands over when it calls an import.
    fn params(side: Side, in_types: bool) -> Cx {
        Cx {
            side,
            produces: side == Side::Import,
            in_types,
        }
    }

    /// The result of a function implemented on `side`, which the guest hands
    /// over when it implements an export.
    fn result(side: Side, in_types: bool) -> Cx {
        Cx {
            side,
            produces: side == Side::Export,
            in_types,
        }
    }
}

/// A declaration in the types module.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Decl {
    /// A named type by its canonical id (see [`Generator::canonical`]), and the
    /// side whose resources it refers to.
    Type(TypeId, Side),
    /// The error class of an `err` type, by the id the `result` names it with.
    /// A `use`d or renamed type has an id of its own, owned by the interface
    /// that `use`s it, and the runtime keys the class on that id's interface
    /// and name.
    ErrorClass(TypeId),
}

/// The running state of one `.d.ts` generation.
struct Generator<'a> {
    resolve: &'a Resolve,
    world: WorldId,
    /// The interfaces the world exports, whose resources the guest implements.
    exported_interfaces: HashSet<InterfaceId>,
    /// The interfaces the world imports.
    imported_interfaces: HashSet<InterfaceId>,
    /// The world key name of every interface the world imports or exports.
    interface_keys: HashMap<InterfaceId, String>,
    /// The module specifier the world's types are declared under (see
    /// [`types_module_name`]).
    types_module: String,
    /// The declarations of the types module, in discovery order.
    decls: Vec<Decl>,
    /// The position of each declaration in `decls`.
    decl_index: HashMap<Decl, usize>,
    /// The name of each declaration in `decls`, set by
    /// [`Generator::assign_names`].
    names: Vec<String>,
    /// Every name the types module declares.
    declared_names: HashSet<String>,
    /// The `err` types with error classes, each with the interface whose module
    /// exports the class, or `None` for `external`.
    error_classes: Vec<(TypeId, Option<InterfaceId>)>,
    /// Whether any `unknown`-placeholder shape (payload-less stream or future,
    /// error-context, or an otherwise-unsupported WIT kind) was rendered, so the
    /// header can note it.
    used_unknown_placeholder: Cell<bool>,
}

impl<'a> Generator<'a> {
    fn new(resolve: &'a Resolve, world: WorldId) -> Self {
        let w = &resolve.worlds[world];
        let mut interface_keys = HashMap::new();
        let mut imported_interfaces = HashSet::new();
        let mut exported_interfaces = HashSet::new();
        for (key, item) in &w.imports {
            if let WorldItem::Interface { id, .. } = item {
                imported_interfaces.insert(*id);
                interface_keys.insert(*id, resolve.name_world_key(key));
            }
        }
        for (key, item) in &w.exports {
            if let WorldItem::Interface { id, .. } = item {
                exported_interfaces.insert(*id);
                interface_keys.insert(*id, resolve.name_world_key(key));
            }
        }
        Generator {
            resolve,
            world,
            exported_interfaces,
            imported_interfaces,
            interface_keys,
            types_module: types_module_name(resolve, world),
            decls: Vec::new(),
            decl_index: HashMap::new(),
            names: Vec::new(),
            declared_names: HashSet::new(),
            error_classes: Vec::new(),
            used_unknown_placeholder: Cell::new(false),
        }
    }

    fn world(&self) -> &'a World {
        &self.resolve.worlds[self.world]
    }

    fn run(mut self) -> Result<String> {
        let groups = export_groups(self.resolve, self.world);
        let world_funcs: Vec<String> = world_export_functions(self.world())
            .map(|func| mangle_name(&func.name))
            .collect();
        let layout = export_layout(&groups, &world_funcs)?;
        self.collect();
        self.assign_names();

        let types = self.render_types_module();
        let guest = self.render_guest_module(&groups, &layout);
        let imports = self.render_import_modules();

        let mut out = String::new();
        self.write_header(&mut out);
        out.push_str(COMPONENT_ERROR_DECL);
        if groups.iter().any(|group| group.http_handler) {
            out.push_str(FETCH_EVENT_DECL);
        }
        if let Some(request) = self.wit_request_type() {
            let name = self.decl_name(Decl::Type(request, Side::Import));
            let _ = write!(
                out,
                "// `fetch` also takes a `wasi:http/types` `request`, such as the one a\n\
                 // `wasi:http/handler` implementation receives, and sends it.\n\
                 declare function fetch(\n  \
                   input: import('{types}').{name},\n  \
                   init?: RequestInit,\n\
                 ): Promise<Response>;\n\n",
                types = self.types_module,
            );
        }
        out.push_str(&types);
        out.push_str(&guest);
        out.push_str(&imports);
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Collection: every declaration the types module needs
    // -----------------------------------------------------------------------

    /// Collect every declaration the rendered blocks refer to, and the error
    /// classes the runtime creates.
    fn collect(&mut self) {
        let world = self.world();
        for item in world.imports.values() {
            match item {
                WorldItem::Function(func) => self.visit_func(func, Side::Import),
                WorldItem::Interface { id, .. } => {
                    let iface = &self.resolve.interfaces[*id];
                    for func in iface.functions.values() {
                        self.visit_func(func, Side::Import);
                    }
                    // Every type is re-exported from the interface's module.
                    for &ty in iface.types.values() {
                        self.visit_id(ty, Side::Import);
                    }
                }
                WorldItem::Type { id, .. } => self.visit_id(*id, Side::Import),
            }
        }
        for (key, item) in &world.exports {
            match item {
                WorldItem::Function(func) => self.visit_func(func, Side::Export),
                WorldItem::Interface { id, .. } => {
                    // `run` is declared without WIT types.
                    if names_interface(&self.resolve.name_world_key(key), CLI_RUN_INTERFACE) {
                        continue;
                    }
                    let iface = &self.resolve.interfaces[*id];
                    for func in iface.functions.values() {
                        self.visit_func(func, Side::Export);
                    }
                    for res in self.interface_resources(*id) {
                        self.visit_id(res, Side::Export);
                    }
                }
                WorldItem::Type { id, .. } => self.visit_id(*id, Side::Import),
            }
        }
        for err in self.world_error_types() {
            let def = &self.resolve.types[err];
            let module = match def.owner {
                TypeOwner::World(_) => Some(None),
                TypeOwner::Interface(iface) if self.imported_interfaces.contains(&iface) => {
                    Some(Some(iface))
                }
                // A type an exported interface defines has no module to export
                // its class from.
                _ => None,
            };
            if let Some(module) = module {
                self.visit_id(err, Side::Import);
                self.add_decl(Decl::ErrorClass(err));
                self.error_classes.push((err, module));
            }
        }
    }

    /// Add `decl` to the types module, returning whether it was new.
    fn add_decl(&mut self, decl: Decl) -> bool {
        if self.decl_index.contains_key(&decl) {
            return false;
        }
        self.decl_index.insert(decl, self.decls.len());
        self.decls.push(decl);
        true
    }

    /// Collect the types a function implemented on `side` refers to.
    fn visit_func(&mut self, func: &Function, side: Side) {
        for param in &func.params {
            self.visit_type(&param.ty, side);
        }
        if let Some(result) = &func.result {
            self.visit_type(result, side);
        }
    }

    fn visit_type(&mut self, ty: &Type, side: Side) {
        if let Type::Id(id) = ty {
            self.visit_id(*id, side);
        }
    }

    /// Collect the declarations type `id` refers to where it is part of a
    /// function or declaration of `side`.
    fn visit_id(&mut self, id: TypeId, side: Side) {
        let root = self.canonical(id);
        let def = &self.resolve.types[root];
        if def.name.is_none() {
            self.visit_structure(root, side);
            return;
        }
        let side = self.side_of(root, side);
        if !self.add_decl(Decl::Type(root, side)) {
            return;
        }
        if matches!(def.kind, TypeDefKind::Resource) {
            for func in self.resource_funcs(root) {
                self.visit_func(func, side);
            }
        } else {
            self.visit_structure(root, side);
        }
    }

    fn visit_structure(&mut self, id: TypeId, side: Side) {
        match &self.resolve.types[id].kind {
            TypeDefKind::Type(inner)
            | TypeDefKind::List(inner)
            | TypeDefKind::Option(inner)
            | TypeDefKind::Future(Some(inner))
            | TypeDefKind::Stream(Some(inner)) => self.visit_type(inner, side),
            TypeDefKind::Result(result) => {
                for arm in [&result.ok, &result.err].into_iter().flatten() {
                    self.visit_type(arm, side);
                }
            }
            TypeDefKind::Tuple(tuple) => {
                for element in &tuple.types {
                    self.visit_type(element, side);
                }
            }
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    self.visit_type(&field.ty, side);
                }
            }
            TypeDefKind::Variant(variant) => {
                for payload in variant.cases.iter().filter_map(|case| case.ty.as_ref()) {
                    self.visit_type(payload, side);
                }
            }
            TypeDefKind::Handle(Handle::Own(res) | Handle::Borrow(res)) => {
                self.visit_id(*res, side)
            }
            _ => {}
        }
    }

    /// The side of the declaration a reference to the named type `root` from a
    /// function or declaration of `side` resolves to.
    ///
    /// An interface the world both imports and exports has one set of types but
    /// two classes per resource, the host's and the guest's. A type referring to
    /// a resource of an exported interface is therefore declared once per side,
    /// and every other type once, on the import side.
    fn side_of(&self, root: TypeId, side: Side) -> Side {
        if side == Side::Export && self.refers_to_exported_resource(root) {
            Side::Export
        } else {
            Side::Import
        }
    }

    /// Whether type `id` is, or contains, a resource of an interface the world
    /// exports.
    fn refers_to_exported_resource(&self, id: TypeId) -> bool {
        let id = self.canonical(id);
        let refers = |ty: &Type| match ty {
            Type::Id(id) => self.refers_to_exported_resource(*id),
            _ => false,
        };
        match &self.resolve.types[id].kind {
            TypeDefKind::Resource => matches!(
                self.resolve.types[id].owner,
                TypeOwner::Interface(iface) if self.exported_interfaces.contains(&iface)
            ),
            TypeDefKind::Handle(Handle::Own(res) | Handle::Borrow(res)) => {
                self.refers_to_exported_resource(*res)
            }
            TypeDefKind::Type(inner)
            | TypeDefKind::List(inner)
            | TypeDefKind::Option(inner)
            | TypeDefKind::Future(Some(inner))
            | TypeDefKind::Stream(Some(inner)) => refers(inner),
            TypeDefKind::Result(result) => {
                [&result.ok, &result.err].into_iter().flatten().any(refers)
            }
            TypeDefKind::Tuple(tuple) => tuple.types.iter().any(refers),
            TypeDefKind::Record(record) => record.fields.iter().any(|field| refers(&field.ty)),
            TypeDefKind::Variant(variant) => variant
                .cases
                .iter()
                .filter_map(|case| case.ty.as_ref())
                .any(refers),
            _ => false,
        }
    }

    /// The `err` types with error classes in every function the world imports or
    /// exports, in first-use order. This follows the runtime's
    /// `collect_err_classes`.
    fn world_error_types(&self) -> Vec<TypeId> {
        let world = self.world();
        let mut types = Vec::new();
        let funcs = world
            .imports
            .values()
            .chain(world.exports.values())
            .flat_map(|item| match item {
                WorldItem::Function(func) => vec![func],
                WorldItem::Interface { id, .. } => {
                    self.resolve.interfaces[*id].functions.values().collect()
                }
                WorldItem::Type { .. } => Vec::new(),
            });
        for func in funcs {
            for param in &func.params {
                self.collect_error_types(&param.ty, false, &mut types);
            }
            if let Some(result) = &func.result {
                self.collect_error_types(result, true, &mut types);
            }
        }
        types
    }

    /// Add to `types` the `err` type of every `result` whose payloads cross into
    /// JS as errors and have an error class: the one a `future` in `ty` holds,
    /// and, with `direct`, `ty` itself, as a function's result.
    fn collect_error_types(&self, ty: &Type, direct: bool, types: &mut Vec<TypeId>) {
        let Type::Id(id) = ty else {
            return;
        };
        if direct {
            if let Some(err) = self.error_class_type(ty) {
                if !types.contains(&err) {
                    types.push(err);
                }
            }
        }
        match &self.resolve.types[*id].kind {
            TypeDefKind::Type(inner) => self.collect_error_types(inner, direct, types),
            TypeDefKind::Future(Some(payload)) => self.collect_error_types(payload, true, types),
            TypeDefKind::List(inner) | TypeDefKind::Option(inner) => {
                self.collect_error_types(inner, false, types)
            }
            TypeDefKind::Result(result) => {
                for arm in [&result.ok, &result.err].into_iter().flatten() {
                    self.collect_error_types(arm, false, types);
                }
            }
            TypeDefKind::Tuple(tuple) => {
                for element in &tuple.types {
                    self.collect_error_types(element, false, types);
                }
            }
            TypeDefKind::Record(record) => {
                for field in &record.fields {
                    self.collect_error_types(&field.ty, false, types);
                }
            }
            TypeDefKind::Variant(variant) => {
                for payload in variant.cases.iter().filter_map(|case| case.ty.as_ref()) {
                    self.collect_error_types(payload, false, types);
                }
            }
            _ => {}
        }
    }

    /// The `err` type of `ty` if `ty` is a `result` whose `err` arm has an error
    /// class: a named type other than a resource. The id is the one the `result`
    /// names, not its canonical definition, as in the runtime's `err_class_of`,
    /// except that an alias in an interface the world only exports is followed
    /// to the type it names, since that interface has no module to export a
    /// class from.
    fn error_class_type(&self, ty: &Type) -> Option<TypeId> {
        let (_, Some(Type::Id(mut err))) = self.as_result(ty)? else {
            return None;
        };
        while let (TypeOwner::Interface(iface), TypeDefKind::Type(Type::Id(target))) =
            (self.resolve.types[err].owner, &self.resolve.types[err].kind)
        {
            if !self.exported_interfaces.contains(&iface)
                || self.imported_interfaces.contains(&iface)
            {
                break;
            }
            err = *target;
        }
        self.resolve.types[err].name.as_ref()?;
        let root = &self.resolve.types[self.canonical(err)];
        if matches!(root.kind, TypeDefKind::Resource | TypeDefKind::Handle(_)) {
            return None;
        }
        Some(err)
    }

    // -----------------------------------------------------------------------
    // Naming the types module
    // -----------------------------------------------------------------------

    /// Name every declaration of the types module.
    ///
    /// A declaration is named after its WIT type in UpperCamelCase, and an error
    /// class after its type with `Error` appended. The export-side declaration of
    /// a type that also has an import-side one is prefixed with `Exported`. Where
    /// several declarations have the same name, each of them is qualified with
    /// its interface's name, then with its package's namespace and name, then
    /// with its package's version, and finally has `_` and its position appended.
    /// A flags type also occupies its name with `Set` appended.
    fn assign_names(&mut self) {
        let candidates: Vec<Vec<String>> = self
            .decls
            .iter()
            .map(|&decl| self.name_candidates(decl))
            .collect();
        let is_flags: Vec<bool> = self
            .decls
            .iter()
            .map(|decl| match decl {
                Decl::Type(id, _) => matches!(self.resolve.types[*id].kind, TypeDefKind::Flags(_)),
                Decl::ErrorClass(_) => false,
            })
            .collect();
        let name_at = |i: usize, level: usize| match candidates[i].get(level) {
            Some(name) => name.clone(),
            None => format!("{}_{i}", candidates[i].last().expect("a name")),
        };
        let mut levels = vec![0; self.decls.len()];
        loop {
            let mut owners: HashMap<String, Vec<usize>> = HashMap::new();
            for (i, &level) in levels.iter().enumerate() {
                let name = name_at(i, level);
                if is_flags[i] {
                    owners.entry(flags_set_name(&name)).or_default().push(i);
                }
                owners.entry(name).or_default().push(i);
            }
            let mut colliding: Vec<usize> = owners
                .into_values()
                .filter(|owners| owners.len() > 1)
                .flatten()
                .collect();
            colliding.sort_unstable();
            colliding.dedup();
            // A name with `_` and a position appended is unique, since no
            // mangled name contains `_`.
            colliding.retain(|&i| levels[i] < candidates[i].len());
            if colliding.is_empty() {
                break;
            }
            for i in colliding {
                levels[i] += 1;
            }
        }
        self.names = (0..self.decls.len())
            .map(|i| name_at(i, levels[i]))
            .collect();
        for (i, name) in self.names.iter().enumerate() {
            if is_flags[i] {
                self.declared_names.insert(flags_set_name(name));
            }
            self.declared_names.insert(name.clone());
        }
    }

    /// The names of `decl` from least to most qualified.
    fn name_candidates(&self, decl: Decl) -> Vec<String> {
        let (id, stem) = match decl {
            Decl::Type(id, _) => (id, mangle_resource_name(self.type_name(id))),
            Decl::ErrorClass(id) => (
                id,
                format!("{}Error", mangle_resource_name(self.type_name(id))),
            ),
        };
        let prefix = match decl {
            Decl::Type(id, Side::Export)
                if self.decl_index.contains_key(&Decl::Type(id, Side::Import)) =>
            {
                "Exported"
            }
            _ => "",
        };
        let mut candidates = vec![format!("{prefix}{stem}")];
        for qualifier in self.owner_qualifiers(self.resolve.types[id].owner) {
            let name = format!("{prefix}{qualifier}{stem}");
            if !candidates.contains(&name) {
                candidates.push(name);
            }
        }
        candidates
    }

    /// The qualifiers of a type owned by `owner`: its interface's (or world's)
    /// name, that name with its package's namespace and name, and that with its
    /// package's version, each in UpperCamelCase.
    fn owner_qualifiers(&self, owner: TypeOwner) -> Vec<String> {
        let (short, package) = match owner {
            TypeOwner::Interface(iface) => {
                let interface = &self.resolve.interfaces[iface];
                let short = interface
                    .name
                    .clone()
                    .or_else(|| self.interface_keys.get(&iface).cloned())
                    .unwrap_or_default();
                (short, interface.package)
            }
            TypeOwner::World(world) => {
                let world = &self.resolve.worlds[world];
                (world.name.clone(), world.package)
            }
            TypeOwner::None => return Vec::new(),
        };
        let mut qualifiers = vec![mangle_resource_name(&short)];
        if let Some(package) = package {
            let name = &self.resolve.packages[package].name;
            let qualified = format!("{}-{}-{short}", name.namespace, name.name);
            qualifiers.push(mangle_resource_name(&qualified));
            if let Some(version) = &name.version {
                qualifiers.push(mangle_resource_name(&format!("{qualified}-{version}")));
            }
        }
        qualifiers
    }

    /// The name of declaration `decl`.
    fn decl_name(&self, decl: Decl) -> &str {
        let index = self.decl_index[&decl];
        &self.names[index]
    }

    /// `name`, a global the rendered type refers to, qualified with
    /// `globalThis.` where a declaration in the types module shadows it.
    fn global(&self, name: &str, cx: Cx) -> String {
        if cx.in_types && self.declared_names.contains(name) {
            format!("globalThis.{name}")
        } else {
            name.to_string()
        }
    }

    /// A reference to the declaration of the named type `id`, as a value of it.
    /// A flags type is named by its set alias.
    fn type_ref(&self, id: TypeId, cx: Cx) -> String {
        let root = self.canonical(id);
        let name = self.decl_name(Decl::Type(root, self.side_of(root, cx.side)));
        let name = if matches!(self.resolve.types[root].kind, TypeDefKind::Flags(_)) {
            flags_set_name(name)
        } else {
            name.to_string()
        };
        self.qualify(name, cx)
    }

    /// `name`, a declaration of the types module, as a block rendered in `cx`
    /// refers to it.
    fn qualify(&self, name: String, cx: Cx) -> String {
        if cx.in_types {
            name
        } else {
            format!("{TYPES_NAMESPACE}.{name}")
        }
    }

    // -----------------------------------------------------------------------
    // Blocks
    // -----------------------------------------------------------------------

    fn write_header(&self, out: &mut String) {
        let _ = writeln!(
            out,
            "// Generated by `starling-componentize types`. Do not edit by hand.\n//\n\
             // The guest JavaScript module a `{world}` componentize target implements, and\n\
             // the modules it may import. Add this file to the program (a `files` or\n\
             // `include` entry in `tsconfig.json`, or a `/// <reference path=…/>`).\n\
             //\n\
             // Nothing here is exported at file level, which makes every `declare\n\
             // module` below an ambient module declaration. An import then\n\
             // resolves:\n\
             //\n\
             //   import {{ log }} from 'test:example/logger';\n\
             //\n\
             // The guest's own obligations are `{guest}`. Reference it to have the\n\
             // compiler check what the module exports:\n\
             //\n\
             //   import type * as Guest from '{guest}';\n\
             //   export const example: typeof Guest.example = {{ … }};\n\
             //\n\
             // Every WIT type the world uses is declared in the module\n\
             //\n\
             //   {types}\n\
             //\n\
             // Each interface's module re-exports the types the interface defines or\n\
             // `use`s under their WIT names. A name that several types share is\n\
             // qualified there.",
            world = self.world().name,
            guest = GUEST_MODULE_NAME,
            types = self.types_module,
        );
        if self.used_unknown_placeholder.get() {
            out.push_str(
                "//\n// Deferred shapes (placeholder types, see the TODOs below):\n\
                 //   - payload-less stream/future, error-context and other unsupported types\n\
                 //     render as `unknown`.\n",
            );
        }
        out.push('\n');
    }

    fn render_types_module(&self) -> String {
        if self.decls.is_empty() {
            return String::new();
        }
        let decls: Vec<String> = self
            .decls
            .iter()
            .map(|&decl| self.render_decl(decl))
            .collect();
        format!(
            "declare module '{}' {{\n{}}}\n\n",
            self.types_module,
            indent(decls.join("\n").trim_end())
        )
    }

    fn render_decl(&self, decl: Decl) -> String {
        let name = self.decl_name(decl);
        match decl {
            Decl::ErrorClass(err) => self.render_error_class(name, err),
            Decl::Type(id, side) => match &self.resolve.types[id].kind {
                TypeDefKind::Resource => self.render_resource_class(name, id, side),
                TypeDefKind::Enum(e) => declare_enum(name, e, self.has_runtime_object(id)),
                TypeDefKind::Flags(f) => declare_flags(name, f, self.has_runtime_object(id)),
                _ => {
                    // A declaration is shared by every position that names the
                    // type, and a value the guest receives is also one it may
                    // hand over.
                    let cx = Cx {
                        side,
                        produces: false,
                        in_types: true,
                    };
                    format!("export type {name} = {};\n", self.render_structure(id, cx))
                }
            },
        }
    }

    /// Whether the runtime creates an object for the enum or flags type `id`:
    /// the module of the interface defining it does, unless the world only
    /// exports the interface, and so does `external` for a type of the world.
    fn has_runtime_object(&self, id: TypeId) -> bool {
        match self.resolve.types[id].owner {
            TypeOwner::Interface(interface) => self.imported_interfaces.contains(&interface),
            TypeOwner::World(_) => true,
            TypeOwner::None => false,
        }
    }

    /// Declare the error class of the `err` type `err`: a class extending
    /// `ComponentError` whose `payload` has the type.
    fn render_error_class(&self, name: &str, err: TypeId) -> String {
        let cx = Cx {
            side: Side::Import,
            produces: false,
            in_types: true,
        };
        let payload = self.type_ref(err, cx);
        let base = self.global("ComponentError", cx);
        format!(
            "export class {name} extends {base} {{\n  \
             constructor(payload: {payload});\n  \
             readonly payload: {payload};\n\
             }}\n"
        )
    }

    /// Declare a resource's class, with the constructor, methods and statics its
    /// WIT declares, rendered for the side implementing them.
    ///
    /// The host's class has a private member, so no other object type-checks as
    /// one of its handles, and a private constructor when the WIT declares none.
    /// The runtime installs `[Symbol.dispose]` on it. A guest class may define
    /// `[Symbol.dispose]`, which the runtime calls when the host drops the
    /// resource. A guest class whose WIT declares no constructor may take any
    /// constructor arguments, since the runtime never constructs it.
    fn render_resource_class(&self, name: &str, res: TypeId, side: Side) -> String {
        let funcs = self.resource_funcs(res);
        let mut out = format!("export class {name} {{\n");
        let host = side == Side::Import;
        let has_constructor = funcs
            .iter()
            .any(|func| matches!(func.kind, FunctionKind::Constructor(_)));
        if !has_constructor {
            out.push_str(if host {
                "  private constructor();\n"
            } else {
                "  constructor(...args: any[]);\n"
            });
        }
        for func in funcs {
            let line = match &func.kind {
                FunctionKind::Constructor(_) => {
                    format!("constructor({})", self.render_params(func, side, true))
                }
                FunctionKind::Static(_) | FunctionKind::AsyncStatic(_) => format!(
                    "static {}",
                    self.render_signature(&mangle_name(func.item_name()), func, side, true)
                ),
                _ => self.render_signature(&mangle_name(func.item_name()), func, side, true),
            };
            let _ = writeln!(out, "  {line};");
        }
        let cx = Cx::result(side, true);
        let symbol = self.global("Symbol", cx);
        if host {
            let _ = writeln!(out, "  private $brand: unknown;");
            let _ = writeln!(out, "  [{symbol}.dispose](): void;");
        } else {
            let _ = writeln!(out, "  [{symbol}.dispose]?(): void;");
        }
        out.push_str("}\n");
        out
    }

    fn render_guest_module(&self, groups: &[ExportGroup<'_>], layout: &Layout) -> String {
        let mut sections: Vec<String> = Vec::new();

        if groups.iter().any(|group| group.http_handler) {
            sections.push(self.render_http_handler_guidance(groups));
        }
        if layout.versioned.iter().any(|versioned| *versioned) {
            sections.push(
                "// Each item of an exported interface is implemented through exactly one\n\
                 // of the shapes below: a member of its package's object, a member of its\n\
                 // interface's object, a bare export, or, for one of several versions of an\n\
                 // interface, a member of the export named by its full WIT name. A shape is\n\
                 // declared only where the world allows it for the item."
                    .to_string(),
            );
        } else if groups.iter().any(|group| !group.items.is_empty()) {
            sections.push(
                "// Each item of an exported interface is implemented through exactly one\n\
                 // of the shapes below: a member of its package's object, a member of its\n\
                 // interface's object, or a bare export. A shape is declared only where the\n\
                 // world allows it for the item."
                    .to_string(),
            );
        }

        let members: Vec<Vec<String>> = groups
            .iter()
            .map(|group| {
                group
                    .items
                    .iter()
                    .map(|item| self.render_item_member(item))
                    .collect()
            })
            .collect();
        let object = |g: usize, depth: usize| {
            let pad = "  ".repeat(depth);
            let mut out = String::from("{\n");
            for member in &members[g] {
                let _ = writeln!(out, "{pad}  {member};");
            }
            let _ = write!(out, "{pad}}}");
            out
        };

        // Package objects, one per package, in first-use order.
        let mut packages: Vec<&str> = Vec::new();
        for (g, group) in groups.iter().enumerate() {
            if group.items.is_empty() || layout.versioned[g] {
                continue;
            }
            if let Some(package) = group.package.as_deref() {
                if !packages.contains(&package) {
                    packages.push(package);
                }
            }
        }
        for package in packages {
            let mut body = String::from("{\n");
            for (g, group) in groups.iter().enumerate() {
                if group.items.is_empty()
                    || layout.versioned[g]
                    || group.package.as_deref() != Some(package)
                {
                    continue;
                }
                let _ = writeln!(
                    body,
                    "  // The exported interface `{}`.\n  {}: {};",
                    group.wit_name,
                    member_name(&group.interface),
                    object(g, 1)
                );
            }
            body.push('}');
            sections.push(bare_export(package, &format!("const {{}}: {body}")).join("\n"));
        }

        for (g, group) in groups.iter().enumerate() {
            if layout.versioned[g] && !group.items.is_empty() {
                let local = format!("versioned{g}_");
                sections.push(format!(
                    "// The exported interface `{0}`, one of several versions.\n\
                     const {local}: {1};\n\
                     export {{ {local} as \"{0}\" }};",
                    group.wit_name,
                    object(g, 0)
                ));
            }
        }

        for (g, group) in groups.iter().enumerate() {
            if layout.interface_layer[g] {
                let decl = format!("const {{}}: {}", object(g, 0));
                sections.push(format!(
                    "// The exported interface `{}`.\n{}",
                    group.wit_name,
                    bare_export(&group.interface, &decl).join("\n")
                ));
            }
        }

        for (g, group) in groups.iter().enumerate() {
            let bare: Vec<String> = group
                .items
                .iter()
                .enumerate()
                .filter(|&(i, _)| layout.bare[g][i])
                .flat_map(|(_, item)| self.render_item_bare(item))
                .collect();
            if !bare.is_empty() {
                sections.push(format!(
                    "// Items of the exported interface `{}`.\n{}",
                    group.wit_name,
                    bare.join("\n")
                ));
            }
        }

        let world_funcs: Vec<String> = world_export_functions(self.world())
            .flat_map(|func| self.render_func_lines(func, Side::Export))
            .collect();
        if !world_funcs.is_empty() {
            sections.push(world_funcs.join("\n"));
        }
        if groups.iter().any(|group| group.cli_run) {
            sections.push(
                "// The `wasi:cli/run` command entry point. It may be synchronous or\n\
                 // `async`. Throwing, or a rejected promise, maps to a failed run.\n\
                 export function run(): void | Promise<void>;"
                    .to_string(),
            );
        }

        if sections.is_empty() {
            return String::new();
        }
        self.render_module_block(GUEST_MODULE_NAME, &sections.join("\n\n"))
    }

    /// The comment describing the two ways a `wasi:http/handler` export is
    /// served.
    fn render_http_handler_guidance(&self, groups: &[ExportGroup<'_>]) -> String {
        let Some(group) = groups.iter().find(|group| group.http_handler) else {
            return String::new();
        };
        let wit_response = group.items.iter().any(|item| {
            matches!(item.kind, ExportItemKind::HttpHandle(func) if self.http_response_type(func).is_some())
        });
        let returns = if wit_response {
            "a `Response` or the WIT `response`, or a promise of either"
        } else {
            "a `Response` or a promise of one"
        };
        let mut paragraph = format!(
            "Or implement `handle` below, which receives the WIT request and returns \
             {returns}. `fetch(request)` sends the request on."
        );
        let types = self
            .world()
            .imports
            .keys()
            .map(|key| self.resolve.name_world_key(key))
            .find(|name| names_interface(name, "wasi:http/types"));
        if let Some(types) = types {
            let _ = write!(
                paragraph,
                " It fails with an `error-code` by throwing an `ErrorCode` from `{types}`."
            );
        }
        let mut out = format!(
            "// The `{HTTP_HANDLER_INTERFACE}` export is served in one of two ways. Either\n\
             // register a listener on the global, and the runtime dispatches each\n\
             // request to it as a `FetchEvent`:\n\
             //\n\
             //   addEventListener('fetch', (event) => {{\n\
             //     event.respondWith(new Response('hello'));\n\
             //   }});\n\
             //"
        );
        // The paragraph, wrapped to lines of at most 76 characters after `// `.
        let mut line = String::new();
        for word in paragraph.split(' ') {
            if !line.is_empty() && line.len() + 1 + word.len() > 76 {
                let _ = write!(out, "\n// {line}");
                line.clear();
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        let _ = write!(out, "\n// {line}");
        out.push_str(
            "\n//\n\
             // An application that does both, or neither, fails to componentize.",
        );
        out
    }

    /// An item's member of its interface object, without the trailing `;`.
    fn render_item_member(&self, item: &ExportItem<'_>) -> String {
        match item.kind {
            ExportItemKind::Func(func) => {
                self.render_signature(&member_name(&item.js_name), func, Side::Export, false)
            }
            ExportItemKind::Resource(res) => {
                let class = self.qualify(
                    self.decl_name(Decl::Type(res, Side::Export)).to_string(),
                    Cx::result(Side::Export, false),
                );
                format!("{}: typeof {class}", item.js_name)
            }
            ExportItemKind::HttpHandle(func) => {
                format!("{}{}", item.js_name, self.render_http_handle(func))
            }
        }
    }

    /// An item's bare export statements.
    fn render_item_bare(&self, item: &ExportItem<'_>) -> Vec<String> {
        match item.kind {
            ExportItemKind::Func(func) => self.render_func_lines(func, Side::Export),
            ExportItemKind::Resource(res) => {
                vec![self.reexport(
                    "export",
                    &[(
                        self.decl_name(Decl::Type(res, Side::Export)).to_string(),
                        item.js_name.clone(),
                    )],
                )]
            }
            ExportItemKind::HttpHandle(func) => bare_export(
                &item.js_name,
                &format!("function {{}}{}", self.render_http_handle(func)),
            ),
        }
    }

    /// The `(request): …` signature of the guest's `wasi:http/handler`
    /// implementation. `func` is the WIT `handle`, which is absent from the
    /// built-in serve world's empty `wasi:http/handler`, whose request is then
    /// `unknown`.
    ///
    /// The runtime passes the WIT request, and accepts a `Response` or the
    /// resource [`Generator::http_response_type`] finds.
    fn render_http_handle(&self, func: Option<&Function>) -> String {
        let request = func
            .and_then(|func| func.params.first())
            .map(|param| self.render_type(&param.ty, Cx::params(Side::Export, false)))
            .unwrap_or_else(|| "unknown".to_string());
        let mut returned = "Response".to_string();
        if let Some(response) = self.http_response_type(func) {
            let _ = write!(
                returned,
                " | {}",
                self.render_type(&response, Cx::result(Side::Export, false))
            );
        }
        format!("(request: {request}): {returned} | Promise<{returned}>")
    }

    /// The `request` resource of [`WASI_HTTP_TYPES`], or of an interface
    /// [`same_interface`](crate::same_interface) matches with it, if the world
    /// imports it and the declarations name it.
    fn wit_request_type(&self) -> Option<TypeId> {
        let id = self
            .world()
            .imports
            .iter()
            .find_map(|(key, item)| match item {
                WorldItem::Interface { id, .. }
                    if crate::same_interface(
                        &self.resolve.name_world_key(key),
                        WASI_HTTP_TYPES,
                    ) =>
                {
                    self.resolve.interfaces[*id].types.get("request").copied()
                }
                _ => None,
            })?;
        let id = self.canonical(id);
        self.decl_index
            .contains_key(&Decl::Type(id, Side::Import))
            .then_some(id)
    }

    /// The `ok` type of the WIT `handle`'s `result`, if it is a resource.
    fn http_response_type(&self, func: Option<&Function>) -> Option<Type> {
        let (ok, _) = self.as_result(&func?.result?)?;
        let Some(Type::Id(id)) = ok else {
            return None;
        };
        match self.resolve.types[self.canonical(id)].kind {
            TypeDefKind::Resource | TypeDefKind::Handle(Handle::Own(_)) => ok,
            _ => None,
        }
    }

    /// The modules the runtime may register for the world's imports: one per
    /// imported interface, and `external` for the world-level imports.
    fn import_module_groups(&self) -> Vec<ImportModule<'a>> {
        let world = self.world();

        // World-level imports, grouped into `external`.
        let mut funcs: Vec<&Function> = Vec::new();
        let mut types: Vec<(&str, TypeId)> = Vec::new();
        let mut member_funcs = false;
        for (key, item) in &world.imports {
            match item {
                WorldItem::Function(func) if resource_member_of(func).is_none() => funcs.push(func),
                WorldItem::Function(_) => member_funcs = true,
                WorldItem::Type { id, .. } => {
                    if let WorldKey::Name(name) = key {
                        types.push((name, *id));
                    }
                }
                WorldItem::Interface { .. } => {}
            }
        }
        let mut modules = vec![ImportModule {
            spec: WORLD_MODULE_NAME.to_string(),
            owner: TypeOwner::World(self.world),
            funcs,
            types,
            member_funcs,
        }];

        for (key, item) in &world.imports {
            let WorldItem::Interface { id, .. } = item else {
                continue;
            };
            let iface = &self.resolve.interfaces[*id];
            let funcs: Vec<&Function> = iface
                .functions
                .values()
                .filter(|func| resource_member_of(func).is_none())
                .collect();
            let member_funcs = iface.functions.len() > funcs.len();
            let types: Vec<(&str, TypeId)> = iface
                .types
                .iter()
                .map(|(name, id)| (name.as_str(), *id))
                .collect();
            modules.push(ImportModule {
                spec: self.resolve.name_world_key(key),
                owner: TypeOwner::Interface(*id),
                funcs,
                types,
                member_funcs,
            });
        }
        modules
    }

    fn render_import_modules(&self) -> String {
        self.import_module_groups()
            .iter()
            .map(|module| self.render_import_module(module))
            .collect()
    }

    /// The `err` types whose error classes the module of `owner` exports.
    fn module_error_classes(&self, owner: TypeOwner) -> impl Iterator<Item = TypeId> + '_ {
        let interface = match owner {
            TypeOwner::Interface(iface) => Some(iface),
            _ => None,
        };
        self.error_classes
            .iter()
            .filter(move |(_, module)| *module == interface)
            .map(|(err, _)| *err)
    }

    /// Whether the runtime registers `module`: it does if the module exports a
    /// function, a resource class, an error class, or an enum or flags object.
    fn registers_module(&self, module: &ImportModule<'_>) -> bool {
        if !module.funcs.is_empty() || module.member_funcs {
            return true;
        }
        let error_names: HashSet<String> = self
            .module_error_classes(module.owner)
            .map(|err| mangle_resource_name(self.type_name(err)))
            .collect();
        if !error_names.is_empty() {
            return true;
        }
        module.types.iter().any(|&(wit_name, id)| {
            let root = self.canonical(id);
            match self.resolve.types[root].kind {
                // The runtime creates a resource's class in the module of the
                // interface defining it, under the resource's own name, whether
                // or not the resource has functions. A `use`d or renamed
                // resource is a type only.
                TypeDefKind::Resource => {
                    id == root && self.resolve.types[root].owner == module.owner
                }
                // The error class of the same name is exported instead.
                TypeDefKind::Enum(_) | TypeDefKind::Flags(_) => {
                    !error_names.contains(&mangle_resource_name(wit_name))
                }
                _ => false,
            }
        })
    }

    /// Render the `declare module '<spec>'` block of `module`. It declares the
    /// module's freestanding functions, and re-exports its types (each with its
    /// WIT name) and the error classes the module exports.
    fn render_import_module(&self, module: &ImportModule<'_>) -> String {
        let ImportModule {
            spec,
            owner,
            funcs,
            types,
            ..
        } = module;
        let mut values: Vec<(String, String)> = Vec::new();
        let mut type_only: Vec<(String, String)> = Vec::new();
        // The declared names of the flags types' set aliases. Each is re-exported unless another
        // type of the interface has its name, such as a record `perms-set` next to flags `perms`.
        let mut sets: HashSet<String> = HashSet::new();
        let mut error_names = HashSet::new();
        for err in self.module_error_classes(*owner) {
            let plain = mangle_resource_name(self.type_name(err));
            values.push((
                self.decl_name(Decl::ErrorClass(err)).to_string(),
                plain.clone(),
            ));
            error_names.insert(plain);
        }
        for &(wit_name, id) in types {
            let plain = mangle_resource_name(wit_name);
            let root = self.canonical(id);
            let decl = self.decl_name(Decl::Type(root, Side::Import)).to_string();
            match &self.resolve.types[root].kind {
                // The runtime creates a resource's class in the module of the
                // interface defining it, under the resource's own name, whether
                // or not the resource has functions. A `use`d or renamed
                // resource is a type only.
                TypeDefKind::Resource => {
                    if id == root && self.resolve.types[root].owner == *owner {
                        values.push((decl, plain));
                    } else {
                        type_only.push((decl, plain));
                    }
                }
                kind => {
                    if let TypeDefKind::Flags(_) = kind {
                        sets.insert(flags_set_name(&decl));
                        values.push((flags_set_name(&decl), flags_set_name(&plain)));
                    }
                    // The error class of the same name is exported instead. Its
                    // `payload` property has this type.
                    if !error_names.contains(&plain) {
                        values.push((decl, plain));
                    }
                }
            }
        }

        let taken: HashSet<String> = values
            .iter()
            .chain(&type_only)
            .filter(|(decl, _)| !sets.contains(decl))
            .map(|(_, name)| name.clone())
            .collect();
        values.retain(|(decl, name)| !sets.contains(decl) || !taken.contains(name));

        let mut lines: Vec<String> = Vec::new();
        for func in funcs {
            lines.extend(self.render_func_lines(func, Side::Import));
        }
        let mut reexports: Vec<String> = Vec::new();
        if !values.is_empty() {
            reexports.push(self.reexport("export", &values));
        }
        if !type_only.is_empty() {
            reexports.push(self.reexport("export type", &type_only));
        }
        if lines.is_empty() && reexports.is_empty() {
            return String::new();
        }

        let mut body = String::new();
        if !self.registers_module(module) {
            body.push_str("// The runtime registers no module under this name, so only its types\n// are usable.\n");
        }
        let lines = lines.join("\n");
        if !lines.is_empty() {
            let _ = writeln!(body, "{lines}");
        }
        if !reexports.is_empty() {
            if !lines.is_empty() {
                body.push('\n');
            }
            body.push_str(&reexports.join("\n"));
        }
        self.render_module_block(spec, body.trim_end())
    }

    // -----------------------------------------------------------------------
    // Functions
    // -----------------------------------------------------------------------

    /// Declare a freestanding function implemented on `side` as a bare export,
    /// e.g. `export function foo(a: number): bigint;`.
    fn render_func_lines(&self, func: &Function, side: Side) -> Vec<String> {
        let js_name = mangle_name(&func.name);
        let signature = self.render_signature("{}", func, side, false);
        bare_export(&js_name, &format!("function {signature}"))
    }

    /// Render a function's `name(params): return` signature (no keyword, no
    /// trailing `;`), used for bare functions, interface-object members and
    /// resource members.
    fn render_signature(
        &self,
        js_name: &str,
        func: &Function,
        side: Side,
        in_types: bool,
    ) -> String {
        let params = self.render_params(func, side, in_types);
        let ret = self.render_func_return(func, side, in_types);
        format!("{js_name}({params}): {ret}")
    }

    /// Render a function's parameter list, without the surrounding parentheses.
    ///
    /// A method's first WIT parameter is its `borrow<resource>` receiver, which
    /// reaches JavaScript as `this` rather than as an argument, so it is dropped.
    fn render_params(&self, func: &Function, side: Side, in_types: bool) -> String {
        let skip = usize::from(matches!(
            func.kind,
            FunctionKind::Method(_) | FunctionKind::AsyncMethod(_)
        ));
        let cx = Cx::params(side, in_types);
        let params: Vec<String> = func
            .params
            .iter()
            .skip(skip)
            .map(|param| {
                format!(
                    "{}: {}",
                    param_name(&param.name),
                    self.render_type(&param.ty, cx)
                )
            })
            .collect();
        params.join(", ")
    }

    /// Render the return type of a function implemented on `side`, applying the
    /// two return-position rules:
    ///
    /// - A `result<T, E>` return is unwrapped to its `ok` payload, `T`, or to
    ///   `void` for a `result` with no `ok` payload. The `err` arm is thrown
    ///   rather than returned, as `handle_import_result` and `call_export` do.
    /// - An `async` function wraps its return in `Promise<…>`. An exported one is
    ///   `T | Promise<T>`, since `export_call_async` accepts a settled value from
    ///   a plain `function`. An import stays `Promise<T>`, since the host always
    ///   returns one.
    fn render_func_return(&self, func: &Function, side: Side, in_types: bool) -> String {
        let cx = Cx::result(side, in_types);
        let returned = func.result.and_then(|ty| match self.as_result(&ty) {
            Some((ok, _err)) => ok,
            None => Some(ty),
        });
        let inner = match &returned {
            None => "void".to_string(),
            Some(ty) => self.render_type(ty, cx),
        };
        // A `future<T>` result is already a promise, which an `async` function's
        // own promise adopts rather than wrapping.
        let returns_future = returned.is_some_and(|ty| self.is_future(&ty));
        let promise = self.global("Promise", cx);
        if returns_future || !is_async(func) {
            inner
        } else if side == Side::Export {
            format!("{inner} | {promise}<{inner}>")
        } else {
            format!("{promise}<{inner}>")
        }
    }

    // -----------------------------------------------------------------------
    // Types
    // -----------------------------------------------------------------------

    /// Render a WIT type as its TypeScript form in a non-return position,
    /// meaning parameters and nested types, where `result<T, E>` is the
    /// `{tag, val?}` union.
    fn render_type(&self, ty: &Type, cx: Cx) -> String {
        match ty {
            Type::Bool => "boolean".to_string(),
            // JS numbers, per `value.rs`'s `push_u8` family and `push_f32`.
            Type::U8 | Type::U16 | Type::U32 | Type::S8 | Type::S16 | Type::S32 => {
                "number".to_string()
            }
            // `push_u64` and `push_s64` lift a BigInt.
            Type::U64 | Type::S64 => "bigint".to_string(),
            Type::F32 | Type::F64 => "number".to_string(),
            // `char` is a one-code-point string, per `push_char`.
            Type::Char => "string".to_string(),
            Type::String => "string".to_string(),
            Type::ErrorContext => {
                self.used_unknown_placeholder.set(true);
                "unknown /* TODO: error-context, deferred */".to_string()
            }
            Type::Id(id) => {
                let root = self.canonical(*id);
                if self.resolve.types[root].name.is_some() {
                    self.type_ref(root, cx)
                } else {
                    self.render_structure(root, cx)
                }
            }
        }
    }

    /// Render a type definition's structural form: the body of a named type's
    /// declaration, or an anonymous type inline. An enum, a flags type and a
    /// resource have a declaration of their own and are never rendered here.
    fn render_structure(&self, id: TypeId, cx: Cx) -> String {
        match &self.resolve.types[id].kind {
            TypeDefKind::Type(inner) => self.render_type(inner, cx),
            TypeDefKind::List(elem) => self.render_list(elem, cx),
            TypeDefKind::Option(inner) => self.render_option(inner, cx),
            TypeDefKind::Result(r) => self.render_result_union(r.ok.as_ref(), r.err.as_ref(), cx),
            TypeDefKind::Tuple(t) => {
                let elems: Vec<String> =
                    t.types.iter().map(|ty| self.render_type(ty, cx)).collect();
                format!("[{}]", elems.join(", "))
            }
            TypeDefKind::Record(r) => self.render_record(r, cx),
            TypeDefKind::Variant(v) => self.render_variant(v, cx),
            TypeDefKind::Handle(Handle::Own(res) | Handle::Borrow(res)) => self.type_ref(*res, cx),
            // `readable.rs` in `component-model`: a `stream<u8>` the guest
            // receives is a byte stream of `Uint8Array`s, and one it hands over
            // takes `ArrayBufferView` and `ArrayBuffer` chunks.
            TypeDefKind::Stream(Some(elem)) if self.unaliased(elem) == Type::U8 => {
                let stream = self.global("ReadableStream", cx);
                if cx.produces {
                    let chunk = format!(
                        "{} | {}",
                        self.global("ArrayBufferView", cx),
                        self.global("ArrayBuffer", cx)
                    );
                    format!(
                        "{stream}<{chunk}> | {}<{chunk}> | {}<{chunk}>",
                        self.global("AsyncIterable", cx),
                        self.global("Iterable", cx)
                    )
                } else {
                    format!("{stream}<{}>", self.global("Uint8Array", cx))
                }
            }
            TypeDefKind::Stream(Some(elem)) => {
                let elem = self.render_type(elem, cx);
                let stream = self.global("ReadableStream", cx);
                if cx.produces {
                    format!(
                        "{stream}<{elem}> | {}<{elem}> | {}<{elem}>",
                        self.global("AsyncIterable", cx),
                        self.global("Iterable", cx)
                    )
                } else {
                    format!("{stream}<{elem}>")
                }
            }
            // `promises.rs` in `component-model`: a `future<result<T, E>>`
            // settles with `T` or rejects with the `err` payload's error.
            TypeDefKind::Future(Some(value)) => {
                let value = match self.as_result(value) {
                    Some((Some(ok), _)) => self.render_type(&ok, cx),
                    Some((None, _)) => "void".to_string(),
                    None => self.render_type(value, cx),
                };
                if cx.produces {
                    format!("{value} | {}<{value}>", self.global("PromiseLike", cx))
                } else {
                    format!("{}<{value}>", self.global("Promise", cx))
                }
            }
            TypeDefKind::Stream(None) | TypeDefKind::Future(None) => {
                self.used_unknown_placeholder.set(true);
                "unknown /* TODO: stream/future without a payload type, deferred */".to_string()
            }
            _ => {
                self.used_unknown_placeholder.set(true);
                "unknown /* TODO: unsupported WIT type, deferred */".to_string()
            }
        }
    }

    /// Render `list<T>`. Numeric element types map to the matching typed array,
    /// following `value.rs`'s `list_uses_typed_array`, which a list the guest
    /// hands over may also be a plain array of instead, and a `list<u8>` it
    /// hands over a `Uint8ClampedArray`. Everything else is `T[]`.
    fn render_list(&self, elem: &Type, cx: Cx) -> String {
        let (typed_array, element) = match self.unaliased(elem) {
            Type::U8 => ("Uint8Array", "number"),
            Type::S8 => ("Int8Array", "number"),
            Type::U16 => ("Uint16Array", "number"),
            Type::S16 => ("Int16Array", "number"),
            Type::U32 => ("Uint32Array", "number"),
            Type::S32 => ("Int32Array", "number"),
            Type::F32 => ("Float32Array", "number"),
            Type::F64 => ("Float64Array", "number"),
            Type::U64 => ("BigUint64Array", "bigint"),
            Type::S64 => ("BigInt64Array", "bigint"),
            _ => {
                let inner = self.render_type(elem, cx);
                // Wrap a union/object element so `(T)[]` parses unambiguously.
                return if inner.contains('|') || inner.starts_with('{') {
                    format!("({inner})[]")
                } else {
                    format!("{inner}[]")
                };
            }
        };
        let typed_array = self.global(typed_array, cx);
        if cx.produces && self.unaliased(elem) == Type::U8 {
            let clamped = self.global("Uint8ClampedArray", cx);
            format!("{typed_array} | {clamped} | {element}[]")
        } else if cx.produces {
            format!("{typed_array} | {element}[]")
        } else {
            typed_array
        }
    }

    /// Render `option<T>`. `none` is `undefined` and `some(v)` is `v`, except
    /// for a nested `option<option<T>>`, where `some` wraps the inner option in
    /// `{ val }` so `some(none)` is distinguishable from `none`. See
    /// `push_option`. An option the guest hands over may also be `null` for
    /// `none`, which `pop_option` accepts.
    fn render_option(&self, inner: &Type, cx: Cx) -> String {
        let inner_ts = self.render_type(inner, cx);
        let none = if cx.produces {
            "undefined | null"
        } else {
            "undefined"
        };
        if self.is_option(inner) {
            format!("{none} | {{ val: {inner_ts} }}")
        } else if inner_ts.contains('|') {
            format!("({inner_ts}) | {none}")
        } else {
            format!("{inner_ts} | {none}")
        }
    }

    /// Render `result<T, E>` as the `{tag, val?}` union, the shape `push_result`
    /// and `pop_result` build in non-return positions. The `val` field is present
    /// only on an arm that has a payload (see [`Self::render_tagged`]).
    fn render_result_union(&self, ok: Option<&Type>, err: Option<&Type>, cx: Cx) -> String {
        format!(
            "{} | {}",
            self.render_tagged("ok", ok, cx),
            self.render_tagged("err", err, cx)
        )
    }

    /// Render one arm of a result or case of a variant: `{ tag }`, with `val`
    /// when it has a payload. An `option` payload's `val` is optional, since
    /// `pop_result` and `pop_variant` read a missing `val` as `none`.
    fn render_tagged(&self, tag: &str, payload: Option<&Type>, cx: Cx) -> String {
        match payload {
            Some(ty) => {
                let optional = if self.is_option(ty) { "?" } else { "" };
                format!(
                    "{{ tag: '{tag}'; val{optional}: {} }}",
                    self.render_type(ty, cx)
                )
            }
            None => format!("{{ tag: '{tag}' }}"),
        }
    }

    /// Render a record as a TypeScript object type. Field names are mangled
    /// (`push_record` uses the mangled names verbatim). An `option` field is
    /// optional, since `pop_record` reads a missing key as `none`.
    fn render_record(&self, r: &wit_parser::Record, cx: Cx) -> String {
        let mut out = String::from("{ ");
        for field in &r.fields {
            let optional = if self.is_option(&field.ty) { "?" } else { "" };
            let _ = write!(
                out,
                "{}{optional}: {}; ",
                mangle_name(&field.name),
                self.render_type(&field.ty, cx)
            );
        }
        out.push('}');
        out
    }

    /// Render a variant as a discriminated union keyed by `tag`. A case with a
    /// payload also has `val`, and a payloadless case has only `tag`. See
    /// `push_variant`.
    fn render_variant(&self, v: &wit_parser::Variant, cx: Cx) -> String {
        let arms: Vec<String> = v
            .cases
            .iter()
            .map(|case| self.render_tagged(&mangle_name(&case.name), case.ty.as_ref(), cx))
            .collect();
        arms.join(" | ")
    }

    // -----------------------------------------------------------------------
    // WIT helpers
    // -----------------------------------------------------------------------

    /// The resource types an interface declares, in declaration order.
    fn interface_resources(&self, id: InterfaceId) -> Vec<TypeId> {
        self.resolve.interfaces[id]
            .types
            .values()
            .copied()
            .filter(|id| matches!(self.resolve.types[*id].kind, TypeDefKind::Resource))
            .collect()
    }

    /// The constructor, methods and statics of resource `res`, in the order its
    /// owning interface or world declares them.
    fn resource_funcs(&self, res: TypeId) -> Vec<&'a Function> {
        let resolve = self.resolve;
        let is_member = |func: &&Function| resource_member_of(func) == Some(res);
        match resolve.types[res].owner {
            TypeOwner::Interface(iface) => resolve.interfaces[iface]
                .functions
                .values()
                .filter(is_member)
                .collect(),
            TypeOwner::World(world) => resolve.worlds[world]
                .imports
                .values()
                .chain(resolve.worlds[world].exports.values())
                .filter_map(|item| match item {
                    WorldItem::Function(func) => Some(func),
                    _ => None,
                })
                .filter(is_member)
                .collect(),
            TypeOwner::None => Vec::new(),
        }
    }

    /// The declared name of a type def, or a synthetic name for an anonymous one.
    fn type_name(&self, id: TypeId) -> &str {
        self.resolve.types[id]
            .name
            .as_deref()
            .unwrap_or("Anonymous")
    }

    /// Follow `use`-alias links (`type x = root`, encoded as
    /// `TypeDefKind::Type(Type::Id(root))`) to the canonical definition.
    ///
    /// A type pulled into several interfaces with `use` appears as a distinct
    /// [`TypeId`] per interface, each an alias to one root. Collapsing to the root
    /// gives it exactly one declaration in the types module, which each
    /// interface's module re-exports under the name it uses.
    fn canonical(&self, id: TypeId) -> TypeId {
        let mut current = id;
        while let TypeDefKind::Type(Type::Id(next)) = self.resolve.types[current].kind {
            current = next;
        }
        current
    }

    /// `ty` with aliases resolved, so an alias of a primitive type is that type.
    /// The runtime resolves them the same way when it picks a typed array for a
    /// list or a byte stream for a stream.
    fn unaliased(&self, ty: &Type) -> Type {
        match ty {
            Type::Id(id) => match self.resolve.types[self.canonical(*id)].kind {
                TypeDefKind::Type(primitive) => primitive,
                _ => Type::Id(self.canonical(*id)),
            },
            primitive => *primitive,
        }
    }

    /// If `ty` is a `result<T, E>`, return its `(ok, err)` arms, each `Some(Type)`
    /// iff that arm has a payload. Used by return-position unwrapping.
    ///
    /// `use`-aliases are resolved first, since the runtime's `type_shape` follows
    /// `Type::Alias` through to the underlying `result`. Without that step an
    /// aliased `result` in return position would render as the `{tag, val?}` union
    /// instead, diverging from what the runtime lowers.
    fn as_result(&self, ty: &Type) -> Option<(Option<Type>, Option<Type>)> {
        if let Type::Id(id) = ty {
            if let TypeDefKind::Result(r) = &self.resolve.types[self.canonical(*id)].kind {
                return Some((r.ok, r.err));
            }
        }
        None
    }

    /// Whether `ty` is a `future<_>`, after resolving `use`-aliases.
    fn is_future(&self, ty: &Type) -> bool {
        match ty {
            Type::Id(id) => matches!(
                self.resolve.types[self.canonical(*id)].kind,
                TypeDefKind::Future(_)
            ),
            _ => false,
        }
    }

    /// Whether `ty` is an `option<_>`, after resolving `use`-aliases. The runtime
    /// follows aliases through to the `option`, so the nested-option `{ val }`
    /// wrap applies to an aliased `option<option<_>>` the same way.
    fn is_option(&self, ty: &Type) -> bool {
        match ty {
            Type::Id(id) => matches!(
                self.resolve.types[self.canonical(*id)].kind,
                TypeDefKind::Option(_)
            ),
            _ => false,
        }
    }
}

impl Generator<'_> {
    /// A `declare module '<spec>' { … }` block around `body`, which imports
    /// the types module as [`TYPES_NAMESPACE`] when `body` refers to it.
    fn render_module_block(&self, spec: &str, body: &str) -> String {
        let import = if body.contains(&format!("{TYPES_NAMESPACE}.")) {
            format!(
                "import type * as {TYPES_NAMESPACE} from '{}';\n\n",
                self.types_module
            )
        } else {
            String::new()
        };
        format!(
            "declare module '{spec}' {{\n{}}}\n\n",
            indent(&format!("{import}{body}"))
        )
    }

    /// An `export { a as b, … } from '<types module>';` statement re-exporting
    /// each `(declaration, name)` pair. `keyword` is `export` or `export type`.
    fn reexport(&self, keyword: &str, names: &[(String, String)]) -> String {
        let specifiers: Vec<String> = names
            .iter()
            .map(|(decl, name)| {
                if decl == name {
                    format!("  {name},\n")
                } else {
                    format!("  {decl} as {name},\n")
                }
            })
            .collect();
        format!(
            "{keyword} {{\n{}}} from '{}';",
            specifiers.concat(),
            self.types_module
        )
    }
}

/// The name of the `number` alias declared next to a flags enum. A set
/// of its members has that type.
fn flags_set_name(name: &str) -> String {
    format!("{name}Set")
}

/// The keyword declaring an enum or flags type: `enum` when the runtime creates
/// an object for it (see [`Generator::has_runtime_object`]), which the guest
/// imports from the interface's module, and `const enum` otherwise, whose
/// members `tsc` inlines at each use, since there is no object to import.
fn enum_keyword(runtime_object: bool) -> &'static str {
    if runtime_object {
        "enum"
    } else {
        "const enum"
    }
}

/// Declare an enum numbered from zero, which is the case index `push_enum`
/// lifts and `pop_enum` lowers. See [`enum_keyword`] on `runtime_object`.
fn declare_enum(name: &str, e: &wit_parser::Enum, runtime_object: bool) -> String {
    let members: Vec<String> = e
        .cases
        .iter()
        .enumerate()
        .map(|(i, case)| format!("  {} = {i},\n", mangle_name(&case.name)))
        .collect();
    format!(
        "export {} {name} {{\n{}}}\n",
        enum_keyword(runtime_object),
        members.concat()
    )
}

/// Declare a flags set as an enum of its bits, plus the `number` alias a set of
/// them has. `push_flags` lifts and `pop_flags` lowers that bit set as an
/// int32, matching the result of `|`, so the member for bit 31 is `1 << 31`, a
/// negative number. See [`enum_keyword`] on `runtime_object`.
///
/// The alias is needed because a bitwise-or of two members is a `number` and
/// not a member, so the enum type itself cannot be the parameter type.
fn declare_flags(name: &str, f: &wit_parser::Flags, runtime_object: bool) -> String {
    let members: Vec<String> = f
        .flags
        .iter()
        .enumerate()
        .map(|(i, flag)| {
            let value = if i == 31 {
                "1 << 31".to_string()
            } else {
                (1u64 << i).to_string()
            };
            format!("  {} = {value},\n", mangle_name(&flag.name))
        })
        .collect();
    let set = flags_set_name(name);
    format!(
        "export {} {name} {{\n{}}}\n\n\
         // A set of `{name}` members, combined with `|`.\n\
         export type {set} = number;\n",
        enum_keyword(runtime_object),
        members.concat()
    )
}

// ---------------------------------------------------------------------------
// Export naming
// ---------------------------------------------------------------------------

/// The imports the runtime may serve from one module: those of an imported
/// interface, or the world-level imports.
struct ImportModule<'a> {
    /// The module specifier.
    spec: String,
    /// The interface, or the world.
    owner: TypeOwner,
    /// The freestanding functions.
    funcs: Vec<&'a Function>,
    /// The types, each with its WIT name.
    types: Vec<(&'a str, TypeId)>,
    /// Whether `owner` has resource functions.
    member_funcs: bool,
}

/// The items of one exported interface, which the guest implements.
struct ExportGroup<'a> {
    /// The interface's world key name, e.g. `wasi:http/handler@0.3.0`.
    wit_name: String,
    /// The package-layer name, or `None` for an interface without one.
    package: Option<String>,
    /// The interface-layer name.
    interface: String,
    items: Vec<ExportItem<'a>>,
    /// Whether this is the `wasi:cli/run` export, which has no items and makes
    /// the guest export a bare `run`.
    cli_run: bool,
    /// Whether this is the `wasi:http/handler` export, whose one item is
    /// `handle`.
    http_handler: bool,
}

struct ExportItem<'a> {
    /// The item's JS name: lowerCamelCase for a function, UpperCamelCase for a
    /// resource class.
    js_name: String,
    kind: ExportItemKind<'a>,
}

#[derive(Clone, Copy)]
enum ExportItemKind<'a> {
    Func(&'a Function),
    Resource(TypeId),
    /// The `handle` of a `wasi:http/handler` export, with its WIT declaration if
    /// the world declares one.
    HttpHandle(Option<&'a Function>),
}

/// The freestanding functions `world` exports at world level.
fn world_export_functions(world: &World) -> impl Iterator<Item = &Function> {
    world.exports.values().filter_map(|item| match item {
        WorldItem::Function(func) if resource_member_of(func).is_none() => Some(func),
        _ => None,
    })
}

/// The package-layer name of package `ns:pkg`: `ns-pkg` in lowerCamelCase.
fn package_layer_name(namespace: &str, name: &str) -> String {
    format!("{namespace}-{name}").to_lower_camel_case()
}

/// The exported interfaces of `world`, with their JS names and items.
///
/// An interface the world names by package (`export ns:pkg/iface@ver;`) has the
/// package-layer name `nsPkg` (see [`package_layer_name`]) and the
/// interface-layer name `iface`, both lowerCamelCase and without the version. An
/// interface the world names with a plain name (`export foo: interface { … }`)
/// has no package layer and the interface-layer name `foo`.
///
/// Its items are its freestanding functions and the resources it declares a
/// constructor, method or static function of. Two interfaces are
/// special-cased. `wasi:cli/run` has no items, since the guest exports a bare
/// `run`. `wasi:http/handler`, which the componentizer exports as the
/// plain-named interface [`RAW_HTTP_HANDLER_EXPORT`](crate::RAW_HTTP_HANDLER_EXPORT),
/// keeps the names of `wasi:http/handler` and has the one item `handle`.
fn export_groups(resolve: &Resolve, world: WorldId) -> Vec<ExportGroup<'_>> {
    let mut groups = Vec::new();
    for (key, item) in &resolve.worlds[world].exports {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let wit_name = resolve.name_world_key(key);
        let iface = &resolve.interfaces[*id];
        if names_interface(&wit_name, CLI_RUN_INTERFACE) {
            groups.push(ExportGroup {
                wit_name,
                package: None,
                interface: String::new(),
                items: Vec::new(),
                cli_run: true,
                http_handler: false,
            });
            continue;
        }
        if names_interface(&wit_name, HTTP_HANDLER_INTERFACE)
            || wit_name == crate::RAW_HTTP_HANDLER_EXPORT
        {
            let handle = iface.functions.get("handle");
            // Comments name the raw export by its interface, `wasi:http/handler`.
            let wit_name = if wit_name == crate::RAW_HTTP_HANDLER_EXPORT {
                HTTP_HANDLER_INTERFACE.to_string()
            } else {
                wit_name
            };
            groups.push(ExportGroup {
                wit_name,
                package: Some(package_layer_name("wasi", "http")),
                interface: mangle_name("handler"),
                items: vec![ExportItem {
                    js_name: "handle".to_string(),
                    kind: ExportItemKind::HttpHandle(handle),
                }],
                cli_run: false,
                http_handler: true,
            });
            continue;
        }
        let (package, interface) = match key {
            WorldKey::Interface(_) => {
                let package = iface.package.map(|package| {
                    let name = &resolve.packages[package].name;
                    package_layer_name(&name.namespace, &name.name)
                });
                let name = iface.name.as_deref().unwrap_or_default();
                (package, mangle_name(name))
            }
            WorldKey::Name(name) => (None, mangle_name(name)),
        };
        let mut items: Vec<ExportItem<'_>> = iface
            .functions
            .values()
            .filter(|func| resource_member_of(func).is_none())
            .map(|func| ExportItem {
                js_name: mangle_name(&func.name),
                kind: ExportItemKind::Func(func),
            })
            .collect();
        items.extend(
            iface
                .types
                .iter()
                .filter(|(_, ty)| {
                    matches!(resolve.types[**ty].kind, TypeDefKind::Resource)
                        && iface
                            .functions
                            .values()
                            .any(|func| resource_member_of(func) == Some(**ty))
                })
                .map(|(name, ty)| ExportItem {
                    js_name: mangle_resource_name(name),
                    kind: ExportItemKind::Resource(*ty),
                }),
        );
        groups.push(ExportGroup {
            wit_name,
            package,
            interface,
            items,
            cli_run: false,
            http_handler: false,
        });
    }
    groups
}

/// Which naming shapes the world allows for each exported item.
struct Layout {
    /// Per group, whether another version of its interface has the same
    /// package-layer and interface-layer names. Such a group's items are
    /// provided only by the export named by its full WIT name.
    versioned: Vec<bool>,
    /// Per group, whether its interface-layer object may provide its items.
    interface_layer: Vec<bool>,
    /// Per group and item, whether a bare export may provide the item.
    bare: Vec<Vec<bool>>,
}

/// Decide which naming shapes may provide each item of `groups`. The runtime
/// accepts an item from exactly one allowed shape.
///
/// - The package layer, `nsPkg.iface.item`, is allowed for an interface with a
///   package, unless another version of the interface has the same package-layer
///   and interface-layer names. Such an interface is provided only by the export
///   named by its full WIT name, `export { impl as "ns:pkg/iface@1.0.0" }`.
/// - The interface layer, `iface.item`, is allowed unless another exported
///   interface with items has the same interface-layer name, or the name is a
///   package-layer name or the name of a world-level exported function.
/// - A bare export, `item`, is allowed unless another item of an exported
///   interface or a world-level exported function has the same name, or the name
///   is a package-layer name or an allowed interface-layer name.
///
/// A world-level exported function, named in `world_funcs`, and the `run` of
/// `wasi:cli/run` are always bare exports. Fails when one of them has a
/// package-layer name, or when `run` is also a world-level function.
fn export_layout(groups: &[ExportGroup<'_>], world_funcs: &[String]) -> Result<Layout> {
    let with_items = || groups.iter().filter(|group| !group.items.is_empty());
    let packages: HashSet<&str> = with_items()
        .filter_map(|group| group.package.as_deref())
        .collect();
    let mut fixed: Vec<&str> = world_funcs.iter().map(String::as_str).collect();
    if groups.iter().any(|group| group.cli_run) {
        fixed.push("run");
    }
    let mut seen = HashSet::new();
    for &name in &fixed {
        if packages.contains(name) {
            bail!("the world-level export `{name}` has the same JS name as a package's exports");
        }
        if !seen.insert(name) {
            bail!(
                "the world exports `{name}` both as a world-level function and as `wasi:cli/run`"
            );
        }
    }
    let mut qualified: HashMap<(&str, &str), usize> = HashMap::new();
    for group in with_items() {
        if let Some(package) = &group.package {
            *qualified
                .entry((package.as_str(), group.interface.as_str()))
                .or_default() += 1;
        }
    }
    let versioned: Vec<bool> = groups
        .iter()
        .map(|group| {
            group.package.as_deref().is_some_and(|package| {
                qualified
                    .get(&(package, group.interface.as_str()))
                    .is_some_and(|count| *count > 1)
            })
        })
        .collect();

    let mut interface_counts: HashMap<&str, usize> = HashMap::new();
    for group in with_items() {
        *interface_counts.entry(&group.interface).or_default() += 1;
    }
    let interface_layer: Vec<bool> = groups
        .iter()
        .zip(&versioned)
        .map(|(group, versioned)| {
            !versioned
                && !group.items.is_empty()
                && interface_counts[group.interface.as_str()] == 1
                && !packages.contains(group.interface.as_str())
                && !fixed.contains(&group.interface.as_str())
        })
        .collect();
    let interfaces: HashSet<&str> = groups
        .iter()
        .zip(&interface_layer)
        .filter(|(_, allowed)| **allowed)
        .map(|(group, _)| group.interface.as_str())
        .collect();

    let mut item_counts: HashMap<&str, usize> = HashMap::new();
    for name in groups
        .iter()
        .flat_map(|group| group.items.iter().map(|item| item.js_name.as_str()))
        .chain(fixed.iter().copied())
    {
        *item_counts.entry(name).or_default() += 1;
    }
    let bare = groups
        .iter()
        .zip(&versioned)
        .map(|(group, versioned)| {
            group
                .items
                .iter()
                .map(|item| {
                    let name = item.js_name.as_str();
                    !versioned
                        && item_counts[name] == 1
                        && !packages.contains(name)
                        && !interfaces.contains(name)
                })
                .collect()
        })
        .collect();
    Ok(Layout {
        versioned,
        interface_layer,
        bare,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An exported item and the JS paths it is accepted under.
    type Accepted = (&'static str, &'static [&'static str]);

    /// The export naming rules, as `(world, accepted)` cases. `world` is the
    /// body of `world w` in package `test:naming`, and `accepted` lists each
    /// exported item as `<wit interface>#<js item>` with every JS path from the
    /// main module's namespace that the runtime accepts it under. A world-level
    /// function is listed as `world#<js name>`.
    const NAMING_CASES: &[(&str, &[Accepted])] = &[
        // Every shape is allowed when the names are unique.
        (
            "export api;",
            &[
                (
                    "test:naming/api#greet",
                    &["testNaming.api.greet", "api.greet", "greet"],
                ),
                (
                    "test:naming/api#Counter",
                    &["testNaming.api.Counter", "api.Counter", "Counter"],
                ),
            ],
        ),
        // Two interfaces with the same name leave only the package layer.
        (
            "export test:a/api; export test:b/api;",
            &[
                ("test:a/api#greet", &["testA.api.greet"]),
                ("test:b/api#greet", &["testB.api.greet"]),
            ],
        ),
        // Two items with the same name lose the bare shape.
        (
            "export left; export right;",
            &[
                (
                    "test:naming/left#greet",
                    &["testNaming.left.greet", "left.greet"],
                ),
                (
                    "test:naming/left#onlyLeft",
                    &["testNaming.left.onlyLeft", "left.onlyLeft", "onlyLeft"],
                ),
                (
                    "test:naming/right#greet",
                    &["testNaming.right.greet", "right.greet"],
                ),
            ],
        ),
        // A plain-named interface has no package layer.
        (
            "export local: interface { greet: func(); }",
            &[("local#greet", &["local.greet", "greet"])],
        ),
        // A world-level function is always bare, so no interface item is bare
        // under its name.
        (
            "export api; export greet: func();",
            &[
                (
                    "test:naming/api#greet",
                    &["testNaming.api.greet", "api.greet"],
                ),
                (
                    "test:naming/api#Counter",
                    &["testNaming.api.Counter", "api.Counter", "Counter"],
                ),
                ("world#greet", &["greet"]),
            ],
        ),
        // No interface layer is allowed under a world-level function's name.
        (
            "export api; export api: func();",
            &[
                ("test:naming/api#greet", &["testNaming.api.greet", "greet"]),
                (
                    "test:naming/api#Counter",
                    &["testNaming.api.Counter", "Counter"],
                ),
                ("world#api", &["api"]),
            ],
        ),
        // No item is bare under an allowed interface-layer name.
        (
            "export same;",
            &[(
                "test:naming/same#same",
                &["testNaming.same.same", "same.same"],
            )],
        ),
        // No item is bare under a package-layer name.
        (
            "export pkg-named;",
            &[(
                "test:naming/pkg-named#testNaming",
                &["testNaming.pkgNamed.testNaming", "pkgNamed.testNaming"],
            )],
        ),
        // An interface without items has no object, and its name makes no other
        // interface-layer name ambiguous.
        (
            "export test:a/types-only; export test:b/api;",
            &[(
                "test:b/api#greet",
                &["testB.api.greet", "api.greet", "greet"],
            )],
        ),
        // `wasi:http/handler` keeps its names, though the componentizer
        // exports it as the plain-named `wasi-http-handler`.
        (
            "export wasi:http/handler@0.3.0;",
            &[(
                "wasi:http/handler@0.3.0#handle",
                &["wasiHttp.handler.handle", "handler.handle", "handle"],
            )],
        ),
        // Two versions of one interface are provided only by the exports named
        // by their full WIT names.
        (
            "export test:v/api@1.0.0; export test:v/api@2.0.0; export test:b/api;",
            &[
                ("test:v/api@1.0.0#greet", &["[\"test:v/api@1.0.0\"].greet"]),
                ("test:v/api@2.0.0#greet", &["[\"test:v/api@2.0.0\"].greet"]),
                ("test:b/api#greet", &["testB.api.greet"]),
            ],
        ),
        // `wasi:cli/run` is a bare `run`.
        ("export wasi:cli/run@0.3.0;", &[("world#run", &["run"])]),
    ];

    /// The packages the naming cases' worlds refer to.
    const NAMING_PACKAGES: &str = "
        package test:a { interface api { greet: func(); } interface types-only { type t = u32; } }
        package test:b { interface api { greet: func(); } }
        package test:v@1.0.0 { interface api { greet: func(); } }
        package test:v@2.0.0 { interface api { greet: func(); } }
        package wasi:http@0.3.0 { interface handler { handle: func(); } }
        package wasi:cli@0.3.0 { interface run { run: async func() -> result; } }
    ";

    /// The interfaces of `test:naming` the naming cases' worlds refer to.
    const NAMING_INTERFACES: &str = "
        interface api { greet: func(); resource counter { get: func() -> u32; } }
        interface left { greet: func(); only-left: func(); }
        interface right { greet: func(); }
        interface same { same: func(); }
        interface pkg-named { test-naming: func(); }
    ";

    fn resolve_naming_world(world: &str) -> (Resolve, WorldId) {
        let wit = format!(
            "package test:naming;\n{NAMING_INTERFACES}\nworld w {{ {world} }}\n{NAMING_PACKAGES}"
        );
        let mut resolve = Resolve::default();
        let package = resolve.push_str("naming.wit", &wit).expect("parsing");
        let world = resolve
            .select_world(&[package], Some("w"))
            .expect("selecting");
        (resolve, world)
    }

    /// Every exported item of `world` with the JS paths it is accepted under.
    fn accepted_paths(world: &str) -> Result<Vec<(String, Vec<String>)>> {
        let (resolve, world) = resolve_naming_world(world);
        let groups = export_groups(&resolve, world);
        let world_funcs: Vec<String> = world_export_functions(&resolve.worlds[world])
            .map(|func| mangle_name(&func.name))
            .collect();
        let layout = export_layout(&groups, &world_funcs)?;
        let mut accepted = Vec::new();
        for (g, group) in groups.iter().enumerate() {
            if group.cli_run {
                accepted.push(("world#run".to_string(), vec!["run".to_string()]));
            }
            for (i, item) in group.items.iter().enumerate() {
                let mut paths = Vec::new();
                if layout.versioned[g] {
                    paths.push(format!("[\"{}\"].{}", group.wit_name, item.js_name));
                } else if let Some(package) = &group.package {
                    paths.push(format!("{package}.{}.{}", group.interface, item.js_name));
                }
                if layout.interface_layer[g] {
                    paths.push(format!("{}.{}", group.interface, item.js_name));
                }
                if layout.bare[g][i] {
                    paths.push(item.js_name.clone());
                }
                accepted.push((format!("{}#{}", group.wit_name, item.js_name), paths));
            }
        }
        for name in world_funcs {
            accepted.push((format!("world#{name}"), vec![name]));
        }
        Ok(accepted)
    }

    #[test]
    fn export_naming_rules() {
        for (world, expected) in NAMING_CASES {
            let expected: Vec<(String, Vec<String>)> = expected
                .iter()
                .map(|(item, paths)| {
                    (
                        item.to_string(),
                        paths.iter().map(|path| path.to_string()).collect(),
                    )
                })
                .collect();
            let actual = accepted_paths(world).expect("a valid layout");
            assert_eq!(actual, expected, "for `{world}`");
        }
    }

    #[test]
    fn export_naming_rejects_unrepresentable_worlds() {
        // A world-level function with a package-layer name.
        assert!(accepted_paths("export api; export test-naming: func();").is_err());
        // `run` both as a world-level function and as `wasi:cli/run`.
        assert!(accepted_paths("export wasi:cli/run@0.3.0; export run: func();").is_err());
    }

    /// `import_modules` lists the modules that export a function, a resource
    /// class, an error class, or an enum or flags object, and no others.
    #[test]
    fn import_modules_lists_registered_modules() {
        let wit = r#"
            package test:modules;
            interface records { record point { x: u32 } }
            interface funcs { f: func(); }
            interface resources { resource handle; }
            interface enums { enum color { red } }
            interface flagged { flags perms { read } }
            interface errors { variant failure { bad } }
            interface uses-enum { use enums.{color}; }
            interface uses-record { use records.{point}; }
            interface api {
                use errors.{failure};
                g: func() -> result<_, failure>;
            }
            world types-only {
                import records;
                import uses-record;
                use records.{point};
            }
            world registered {
                import funcs;
                import resources;
                import enums;
                import flagged;
                import errors;
                import uses-enum;
                import records;
                export api;
            }
            world world-funcs { import h: func(); }
            world world-errors {
                use errors.{failure};
                export g: func() -> result<_, failure>;
            }
        "#;
        let mut resolve = Resolve::default();
        let package = resolve.push_str("modules.wit", wit).expect("parsing");
        let modules = |world: &str| {
            let world = resolve
                .select_world(&[package], Some(world))
                .expect("world");
            import_modules(&resolve, world)
        };
        assert_eq!(modules("types-only"), Vec::<String>::new());
        assert_eq!(
            modules("registered"),
            [
                "test:modules/funcs",
                "test:modules/resources",
                "test:modules/enums",
                "test:modules/flagged",
                "test:modules/errors",
                "test:modules/uses-enum",
            ]
        );
        assert_eq!(modules("world-funcs"), ["external"]);
        // The error class of a type the world `use`s is exported from
        // `external`, not from the interface defining the type.
        assert_eq!(modules("world-errors"), ["external"]);
    }

    /// Both functions against the table `component-model`'s own copies are tested
    /// against, so the two sets cannot drift apart.
    #[test]
    fn mangled_names_match_the_runtime_table() {
        let table = include_str!("../../crates/component-model/tests/mangled_names.txt");
        for line in table.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let [wit, member, class] = line.split(' ').collect::<Vec<_>>()[..] else {
                panic!("a table line has three names: {line:?}");
            };
            assert_eq!(mangle_name(wit), member, "{wit}");
            assert_eq!(mangle_resource_name(wit), class, "{wit}");
        }
    }
}
