// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

#![cfg(not(target_arch = "wasm32"))]

use {
    anyhow::Context as _,
    bytes::Bytes,
    std::{
        borrow::Cow,
        path::{Path, PathBuf},
    },
    wasm_encoder::{CustomSection, Section as _},
    wasmtime::{
        component::{Component, Linker, ResourceTable, ResourceType},
        format_err, Config, Engine, Store,
    },
    wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe},
    wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView},
    wasmtime_wizer::{WasmtimeWizerComponent, Wizer},
    wit_component::metadata,
    wit_dylib::{AsyncFilterSet, DylibOpts, StackPointer},
    wit_parser::{Resolve, WorldId, WorldKey},
};

/// Host bindings of the runtime's `init` interface, `starling/wit/init.wit`.
mod bindings {
    wasmtime::component::bindgen!({
        path: "../starling/wit/init.wit",
        world: "runtime",
        exports: { default: async },
    });
}

/// `starling/wit/init.wit`, which declares the interface between the runtime
/// and the componentizer.
const INIT_WIT: &str = include_str!("../../starling/wit/init.wit");

/// The name of the interface the runtime's `init` export implements, without
/// its version.
pub(crate) const INIT_INTERFACE_PREFIX: &str = "starling:componentize/init";

/// The name of the interface the runtime's `init` export implements, with the
/// version of `starling/wit/init.wit` this componentizer calls, such as
/// `starling:componentize/init@0.1.0`.
pub fn init_interface_name() -> String {
    let mut resolve = Resolve::default();
    let package = resolve
        .push_str("init.wit", INIT_WIT)
        .expect("`starling/wit/init.wit` parses");
    let world = resolve
        .select_world(&[package], Some("runtime"))
        .expect("`starling/wit/init.wit` declares the world `runtime`");
    let key = resolve.worlds[world]
        .exports
        .keys()
        .next()
        .expect("the world `runtime` exports `init`");
    resolve.name_world_key(key)
}

/// The version of the `init` interface `runtime` implements, as the name of its
/// export, such as `starling:componentize/init@0.1.0`, or `None` for a runtime
/// that exports none.
pub fn runtime_init_interface(runtime: &Runtime) -> anyhow::Result<Option<String>> {
    Ok(builtin_export(runtime.module(), INIT_INTERFACE_PREFIX)?.map(|(_, name)| name))
}

/// Fail unless `runtime` implements the version of the `init` interface this
/// componentizer calls, or one that differs from it only in the patch number.
/// The interface's version changes whenever the interface does.
pub fn check_runtime_interface(runtime: &Runtime) -> anyhow::Result<()> {
    let expected = init_interface_name();
    match runtime_init_interface(runtime)? {
        Some(name) if same_interface(&name, &expected) => Ok(()),
        Some(name) => anyhow::bail!(
            "the runtime implements `{name}`, but this componentizer calls `{expected}`. \
             Use a runtime whose version of the interface differs from it at most in the patch \
             number, such as one of the componentizer's release."
        ),
        None => anyhow::bail!(
            "the runtime exports no `{INIT_INTERFACE_PREFIX}` interface, which this \
             componentizer calls as `{expected}`. It is not a StarlingMonkey runtime, or one that \
             predates the interface's versioning."
        ),
    }
}

pub mod command;
pub mod finalize;
pub mod static_link;
mod supported;
pub mod wit_to_ts;

pub struct Ctx {
    wasi: WasiCtx,
    table: ResourceTable,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

pub enum Wit<'a, P = PathBuf> {
    String(&'a str),
    Paths(&'a [P]),
}

/// The prefix `scripts/preserve-component-type.py` gives the copies of the
/// runtime's `component-type` sections.
const PRESERVED_SECTION_PREFIX: &str = "starling:";

/// The runtime build a componentization links the world's bindings against.
///
/// Both hold the full StarlingMonkey runtime and its `init` export. They differ
/// in how the bindings module reaches the runtime, see [`componentize`].
pub enum Runtime {
    /// The core module of `starling.wasm`, the statically linked runtime
    /// component `scripts/build-runtime.sh` builds, with the runtime's
    /// `component-type` sections. [`Runtime::static_from`] makes it from the
    /// component. Linked by [`static_link::link`].
    Static(Vec<u8>),
    /// The position-independent runtime dylib and the shared libraries it
    /// needs, linked by `wit_component::Linker`.
    Dynamic(Libraries),
}

impl Runtime {
    /// A [`Runtime::Static`] from `bytes`: the runtime component
    /// `scripts/build-runtime.sh` builds, or a core module with the runtime's
    /// `component-type` sections.
    ///
    /// A component's main core module, its largest, is taken out of it, with the
    /// `starling:component-type*` copies `build-runtime.sh` adds renamed back to
    /// `component-type*`. A component without them is an error.
    pub fn static_from(bytes: Vec<u8>) -> anyhow::Result<Runtime> {
        if !wasmparser::Parser::is_component(&bytes) {
            return Ok(Runtime::Static(bytes));
        }
        let mut main: Option<&[u8]> = None;
        for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
            if let wasmparser::Payload::ModuleSection {
                unchecked_range, ..
            } = payload.context("parsing the runtime component")?
            {
                let module = &bytes[unchecked_range.start as usize..unchecked_range.end as usize];
                if main.is_none_or(|main| module.len() > main.len()) {
                    main = Some(module);
                }
            }
        }
        let main = main.context("the runtime component contains no core module")?;

        let mut module = main[..8].to_vec();
        let mut restored = 0;
        for payload in wasmparser::Parser::new(0).parse_all(main) {
            let payload = payload.context("parsing the runtime's core module")?;
            if let wasmparser::Payload::CustomSection(section) = &payload {
                if let Some(name) = section.name().strip_prefix(PRESERVED_SECTION_PREFIX) {
                    CustomSection {
                        name: Cow::Borrowed(name),
                        data: Cow::Borrowed(section.data()),
                    }
                    .append_to(&mut module);
                    restored += 1;
                    continue;
                }
            }
            if let Some((id, range)) = payload.as_section() {
                wasm_encoder::RawSection {
                    id,
                    data: &main[range.start as usize..range.end as usize],
                }
                .append_to(&mut module);
            }
        }
        if restored == 0 {
            anyhow::bail!(
                "the runtime component has no `{PRESERVED_SECTION_PREFIX}component-type` \
                 sections. Build it with `just build-runtime`, which adds them."
            );
        }
        Ok(Runtime::Static(module))
    }

    /// The module holding the runtime's own `component-type` sections: the
    /// static runtime's core module, or the runtime dylib.
    pub fn module(&self) -> &[u8] {
        match self {
            Runtime::Static(module) => module,
            Runtime::Dynamic(libraries) => &libraries.runtime,
        }
    }
}

/// Pre-built libraries the dynamic link mode combines into the output
/// component.
///
/// They are supplied by path. See the CLI defaults.
pub struct Libraries {
    /// `libstarling_rt.so`, the StarlingMonkey runtime dylib built by
    /// `scripts/build-dylib.sh`.
    pub runtime: Vec<u8>,
    /// `libc.so` from the wasi-sdk wasm32-wasip2 sysroot.
    pub libc: Vec<u8>,
    /// `noeh/libc++.so` from the wasi-sdk sysroot.
    pub libcxx: Vec<u8>,
    /// `noeh/libc++abi.so` from the wasi-sdk sysroot.
    pub libcxxabi: Vec<u8>,
    /// `libwasi-emulated-getpid.so` from the wasi-sdk sysroot.
    pub wasi_emulated_getpid: Vec<u8>,
}

/// The `wasi:cli/run` export, dropped from the user's world so the runtime's
/// builtin serves it.
///
/// A world that declares `wasi:cli/run` as well makes the component encoder
/// abort on the duplicate component export, so [`remove_world_export`] drops it
/// from the world before the bindings are generated. The output component then
/// exports the runtime's builtin under the world's name for it, so a world
/// naming a version of `wasi:cli/run` whose major or minor number differs from
/// the builtin's is rejected.
const CLI_RUN_INTERFACE_PREFIX: &str = "wasi:cli/run";

/// The serve counterpart of [`CLI_RUN_INTERFACE_PREFIX`]: the runtime's builtin
/// `wasi:http/handler` export, which a world declaring it gets in the same way.
const HTTP_HANDLER_INTERFACE_PREFIX: &str = "wasi:http/handler";

/// Whether the interface names `a` and `b`, such as `wasi:http/types@0.3.0`,
/// name compatible versions of one interface: their names before the `@` are
/// equal, and so are their versions' major and minor numbers and pre-release
/// suffixes, or neither has a version. The patch number and build metadata are
/// ignored. A version that doesn't parse matches only an equal one.
///
/// This is a copy of `component_model::naming::same_interface`. Both are tested
/// against `crates/component-model/tests/interface_versions.txt`.
pub(crate) fn same_interface(a: &str, b: &str) -> bool {
    fn split(name: &str) -> (&str, Option<&str>) {
        match name.split_once('@') {
            Some((name, version)) => (name, Some(version)),
            None => (name, None),
        }
    }
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
    let ((a_name, a_version), (b_name, b_version)) = (split(a), split(b));
    a_name == b_name
        && match (a_version, b_version) {
            (Some(a), Some(b)) => match (key(a), key(b)) {
                (Some(a), Some(b)) => a == b,
                _ => a == b,
            },
            (None, None) => true,
            _ => false,
        }
}

/// The name of the interface the world key name `name` names, without its
/// version.
fn unversioned(name: &str) -> &str {
    name.split_once('@').map_or(name, |(name, _)| name)
}

/// The plain name under which the world exports the interface of the runtime's
/// builtin `wasi:http/handler` a second time, for the application to implement
/// itself. The main module provides its `handle` under the export names of
/// `wasi:http/handler`: `wasiHttp.handler.handle`, `handler.handle` or `handle`.
pub(crate) const RAW_HTTP_HANDLER_EXPORT: &str = "wasi-http-handler";

/// The worlds [`load_world`] targets: `worlds`, or the default world of the
/// last package loaded if `worlds` is empty, and every world of `builtins`. A
/// single world is the target itself, and the worlds of `resolved` are merged
/// into it. Several are merged, together with the worlds of `resolved`, into a
/// new world, [`MERGED_WORLD_NAME`] in the package [`MERGED_PACKAGE_NAME`],
/// which imports and exports everything they do.
#[derive(Clone, Debug, Default)]
pub struct WorldSelection<'a> {
    /// The worlds to target. Each is a world name, looked up in the last
    /// package loaded, or a qualified `namespace:package/world@version`.
    pub worlds: Vec<&'a str>,
    /// Worlds to target in addition to `worlds`: for each, the WIT source
    /// defining it, and its name in that source's package.
    pub builtins: Vec<(&'a str, &'a str)>,
    /// Worlds whose items the target world gets as well, each in a `Resolve`
    /// of its own, such as the one [`runtime_imports_world`] returns.
    pub resolved: Vec<(Resolve, WorldId)>,
}

impl<'a> From<Option<&'a str>> for WorldSelection<'a> {
    fn from(world: Option<&'a str>) -> Self {
        WorldSelection {
            worlds: world.into_iter().collect(),
            builtins: Vec::new(),
            resolved: Vec::new(),
        }
    }
}

/// The package of the world [`load_world`] merges several worlds into.
pub const MERGED_PACKAGE_NAME: &str = "starling:componentize";

/// The name of the world [`load_world`] merges several worlds into.
pub const MERGED_WORLD_NAME: &str = "merged";

/// Build a `Resolve` from the WIT source and select the world to target.
///
/// Shared by [`componentize`] and the `types` and `imports` subcommands, so
/// they always describe the world the component would be built for. `features`
/// are comma- or whitespace-separated WIT feature names, and `all_features`
/// enables every gated item regardless.
///
/// Each path of a [`Wit::Paths`], and the source of each of
/// [`WorldSelection::builtins`], is parsed on its own and then merged into the
/// result. A package several of them define is merged into one, whose
/// interfaces have the types and functions of every definition. The
/// definitions must agree on the functions they have in common.
pub fn load_world<'a>(
    wit: Wit<'_, impl AsRef<Path>>,
    world: impl Into<WorldSelection<'a>>,
    features: &[String],
    all_features: bool,
) -> anyhow::Result<(Resolve, WorldId)> {
    let selection = world.into();
    let new_resolve = || {
        let mut resolve = Resolve {
            all_features,
            ..Default::default()
        };
        for features in features {
            for feature in features
                .split(',')
                .flat_map(|s| s.split_whitespace())
                .filter(|f| !f.is_empty())
            {
                resolve.features.insert(feature.to_string());
            }
        }
        resolve
    };

    // Parse one source into a `Resolve` of its own with `parse`, and merge it
    // into `resolve`. Returns the id in `resolve` of the package `parse`
    // returned. A parse or resolve error names its file, line and column only
    // when rendered against the `Resolve` that holds the sources.
    let push = |resolve: &mut Resolve,
                source: &dyn std::fmt::Display,
                parse: &dyn Fn(&mut Resolve) -> anyhow::Result<wit_parser::PackageId>|
     -> anyhow::Result<wit_parser::PackageId> {
        let mut parsed = new_resolve();
        let package =
            parse(&mut parsed).map_err(|err| anyhow::anyhow!(parsed.render_error(&err)))?;
        let remap = resolve
            .merge(parsed)
            .with_context(|| format!("merging the WIT in {source} with the WIT loaded before"))?;
        // `Remap::packages` is indexed by the package's id in the merged-in
        // `Resolve`.
        Ok(remap.packages[package.index()])
    };

    let mut resolve = new_resolve();
    let last_package = match wit {
        Wit::String(wit) => push(&mut resolve, &"the WIT source", &|r: &mut Resolve| {
            r.push_str("wit", wit)
        })?,
        Wit::Paths(paths) => {
            let mut last_package = None;
            for path in paths.iter().map(AsRef::as_ref) {
                let source = format!("`{}`", path.display());
                last_package = Some(push(&mut resolve, &source, &|r: &mut Resolve| {
                    if path.is_dir() {
                        r.push_dir(path).map(|(package, _)| package)
                    } else {
                        r.push_file(path)
                    }
                })?);
            }
            last_package
                .ok_or_else(|| anyhow::anyhow!("Wit::Paths must contain at least one path"))?
        }
    };

    let mut worlds = Vec::new();
    for world in &selection.worlds {
        worlds.push(resolve.select_world(&[last_package], Some(world))?);
    }
    if worlds.is_empty() {
        worlds.push(resolve.select_world(&[last_package], None)?);
    }
    let mut additions = Vec::new();
    for (source, world) in selection.resolved {
        let remap = resolve
            .merge(source)
            .context("merging a world with the WIT loaded before")?;
        additions.push(remap.worlds[world.index()].expect("a merged world keeps its id"));
    }
    for (wit, name) in selection.builtins {
        let package = push(
            &mut resolve,
            &format!("the `{name}` world"),
            &|r: &mut Resolve| r.push_str(name, wit),
        )?;
        worlds.push(resolve.select_world(&[package], Some(name))?);
    }
    if let [world] = worlds[..] {
        let mut clone_maps = wit_parser::CloneMaps::default();
        for addition in additions {
            resolve
                .merge_worlds(addition, world, &mut clone_maps)
                .context("merging a world into the target world")?;
        }
        return Ok((resolve, world));
    }
    worlds.extend(additions);

    if resolve
        .package_names
        .keys()
        .any(|name| name.version.is_none() && name.to_string() == MERGED_PACKAGE_NAME)
    {
        anyhow::bail!(
            "the WIT defines the package `{MERGED_PACKAGE_NAME}`, which merging several worlds \
             needs for the merged world"
        );
    }
    let package = resolve.push_str(
        "merged.wit",
        &format!("package {MERGED_PACKAGE_NAME};\n\nworld {MERGED_WORLD_NAME} {{}}\n"),
    )?;
    let merged = resolve.select_world(&[package], Some(MERGED_WORLD_NAME))?;
    let mut clone_maps = wit_parser::CloneMaps::default();
    for world in worlds {
        let name = resolve.id_of_name(
            resolve.worlds[world]
                .package
                .expect("a selected world has a package"),
            &resolve.worlds[world].name,
        );
        resolve
            .merge_worlds(world, merged, &mut clone_maps)
            .with_context(|| format!("merging the world `{name}` into the target world"))?;
    }
    Ok((resolve, merged))
}

/// The package of the world [`runtime_imports_world`] returns.
const RUNTIME_IMPORTS_PACKAGE: &str = "starling:runtime-imports";

/// A world that imports every interface the runtime imports, except those of
/// the `disabled` features, and the interfaces they `use`. Each has only the
/// functions the runtime's core module imports, so bindings generated from the
/// world import nothing the runtime does not.
///
/// The interfaces come from the runtime's `component-type` sections. An
/// interface of a disabled feature that a remaining one `use`s stays, for
/// [`finalize::finalize`] to report.
pub fn runtime_imports_world(
    runtime: &Runtime,
    disabled: &[finalize::WASIAPISet],
) -> anyhow::Result<(Resolve, WorldId)> {
    // The interfaces the runtime's core module imports functions of.
    let mut imported = std::collections::BTreeSet::new();
    for payload in wasmparser::Parser::new(0).parse_all(runtime.module()) {
        if let wasmparser::Payload::ImportSection(imports) =
            payload.context("parsing the runtime module")?
        {
            for import in imports.into_imports() {
                let import = import.context("parsing the runtime module's imports")?;
                if !import.module.starts_with('[') && import.module.contains(':') {
                    imported.insert(import.module.to_string());
                }
            }
        }
    }

    let mut resolve = Resolve::default();
    let mut known = std::collections::BTreeSet::new();
    for payload in wasmparser::Parser::new(0).parse_all(runtime.module()) {
        let wasmparser::Payload::CustomSection(section) =
            payload.context("parsing the runtime module")?
        else {
            continue;
        };
        if !section.name().starts_with("component-type") {
            continue;
        }
        let (decoded, world) = wit_parser::decoding::decode_world(section.data())
            .context("decoding a component-type section into a world")?;
        for key in decoded.worlds[world].imports.keys() {
            known.insert(decoded.name_world_key(key));
        }
        resolve
            .merge(decoded)
            .context("merging the runtime's component-type sections")?;
    }

    let imports: Vec<String> = imported
        .into_iter()
        .filter(|name| known.contains(name))
        .filter(|name| !disabled.iter().any(|feature| feature.owns(name)))
        .map(|name| format!("  import {name};\n"))
        .collect();
    let package = resolve.push_str(
        "runtime-imports.wit",
        &format!(
            "package {RUNTIME_IMPORTS_PACKAGE};\n\nworld imports {{\n{}}}\n",
            imports.concat()
        ),
    )?;
    let world = resolve.select_world(&[package], Some("imports"))?;
    retain_imported_functions(&mut resolve, world, runtime.module())?;
    Ok((resolve, world))
}

/// [`load_world`] for the declarations [`wit_to_ts::generate`] renders, failing
/// for a type [`componentize`] cannot lift or lower.
///
/// A world exporting `wasi:http/handler` has the export replaced as
/// [`componentize`] replaces it, by an export named [`RAW_HTTP_HANDLER_EXPORT`]
/// with the runtime's declaration of the interface, so the declarations describe
/// the `handle` the application implements. Without a `runtime`, the export is
/// kept as it is.
pub fn load_world_for_types<'a>(
    wit: Wit<'_, impl AsRef<Path>>,
    world: impl Into<WorldSelection<'a>>,
    features: &[String],
    all_features: bool,
    runtime: Option<&Runtime>,
) -> anyhow::Result<(Resolve, WorldId)> {
    let (mut resolve, world) = load_world(wit, world, features, all_features)?;
    supported::check_supported_types(&resolve, world)?;
    let exports_handler = resolve.worlds[world].exports.keys().any(|key| {
        resolve
            .name_world_key(key)
            .starts_with(HTTP_HANDLER_INTERFACE_PREFIX)
    });
    if !exports_handler {
        return Ok((resolve, world));
    }
    let Some(runtime) = runtime else {
        return Ok((resolve, world));
    };
    let Some(removed) = remove_world_export(&mut resolve, world, HTTP_HANDLER_INTERFACE_PREFIX)
        .into_iter()
        .next()
    else {
        return Ok((resolve, world));
    };
    match builtin_export(runtime.module(), HTTP_HANDLER_INTERFACE_PREFIX)? {
        Some((builtin, name)) if same_interface(&name, &removed) => {
            add_raw_http_handler_export(&mut resolve, world, builtin, &name)?;
            Ok((resolve, world))
        }
        builtin => anyhow::bail!(
            "the world exports `{removed}`, but the runtime provides {}",
            builtin.map_or("none".to_string(), |(_, name)| format!("`{name}`"))
        ),
    }
}

/// Build a component from `js` and the world selected from `wit`, initialized
/// under Wizer.
///
/// The world's bindings are generated with wit-dylib and linked against
/// `runtime`:
///
/// - [`Runtime::Static`] links the bindings as a library of a
///   `wit_component::ComponentEncoder` whose main module is the runtime's,
///   through [`static_link::link`].
/// - [`Runtime::Dynamic`] links the bindings, the runtime dylib and the
///   wasi-sdk shared libraries with `wit_component::Linker`, the wasm
///   dynamic-linking convention.
///
/// The component is then instantiated, its `init` export is called with `js`,
/// the resulting heap is wizened, and the snapshot is wrapped by
/// [`finalize::finalize`] so the output exports exactly the world's exports and
/// imports nothing from the interfaces of `disabled`.
///
/// The compiled component is cached in wasmtime's global compilation cache. See
/// [`componentize_with_output`] to leave it out, and for what the application
/// printed while it initialized.
#[expect(clippy::type_complexity)]
pub async fn componentize(
    wit: Wit<'_, impl AsRef<Path>>,
    world: impl Into<WorldSelection<'_>>,
    features: &[String],
    all_features: bool,
    js: impl Into<JsSource<'_>>,
    js_base_directory: Option<impl AsRef<Path>>,
    runtime: &Runtime,
    disabled: &[finalize::WASIAPISet],
    add_to_linker: Option<&dyn Fn(&mut Linker<Ctx>) -> anyhow::Result<()>>,
) -> anyhow::Result<Vec<u8>> {
    componentize_with_output(
        wit,
        world,
        features,
        all_features,
        js,
        js_base_directory,
        runtime,
        disabled,
        None,
        add_to_linker,
        None,
        true,
    )
    .await
    .map(|componentized| componentized.component)
}

/// What [`componentize_with_output`] produces.
pub struct Componentized {
    /// The component.
    pub component: Vec<u8>,
    /// What the application's top level printed on stdout and stderr while it
    /// initialized.
    pub init_output: String,
}

/// [`componentize`], also returning what the application printed while it
/// initialized. `init_location` is the URL `globalThis.location` reflects while
/// the application's top level runs, if any. With `insecure_seed`, the
/// component imports no `wasi:random/insecure-seed`, and `get-insecure-seed`
/// always returns the seed (see [`finalize::finalize`]). The compiled component
/// is cached in wasmtime's global compilation cache only with `cache`.
#[expect(clippy::type_complexity)]
pub async fn componentize_with_output(
    wit: Wit<'_, impl AsRef<Path>>,
    world: impl Into<WorldSelection<'_>>,
    features: &[String],
    all_features: bool,
    js: impl Into<JsSource<'_>>,
    js_base_directory: Option<impl AsRef<Path>>,
    runtime: &Runtime,
    disabled: &[finalize::WASIAPISet],
    insecure_seed: Option<u128>,
    add_to_linker: Option<&dyn Fn(&mut Linker<Ctx>) -> anyhow::Result<()>>,
    init_location: Option<&str>,
    cache: bool,
) -> anyhow::Result<Componentized> {
    check_runtime_interface(runtime)?;
    let (mut resolve, world) = load_world(wit, world, features, all_features)?;
    supported::check_supported_types(&resolve, world)?;
    supported::reject_world_level_borrows(&resolve, world)?;
    check_export_names(&resolve, world)?;

    // The finalized component exports what the world declares, including the
    // two builtins removed from the world below, under the names the world gave
    // them.
    let mut keep_exports: Vec<finalize::KeptExport> = finalize::world_export_names(&resolve, world)
        .into_iter()
        .map(finalize::KeptExport::unrenamed)
        .collect();

    // The runtime's builtin `wasi:cli/run` serves a componentized CLI tool and
    // its builtin `wasi:http/handler` a serve component, so a world that exports
    // either has it dropped here, before both the wit-dylib bindings and the
    // metadata section below are generated from this world. Left in, the two
    // collide as duplicate component exports.
    let mut http_handler = None;
    let mut cli = false;
    for prefix in [CLI_RUN_INTERFACE_PREFIX, HTTP_HANDLER_INTERFACE_PREFIX] {
        if let Some(removed) = remove_world_export(&mut resolve, world, prefix).first() {
            cli |= prefix == CLI_RUN_INTERFACE_PREFIX;
            // The output component exports the runtime's builtin, so the world
            // must name a version the builtin is compatible with. The export
            // keeps the world's name for it.
            let builtin = builtin_export(runtime.module(), prefix)?;
            let builtin_name = builtin.as_ref().map(|(_, name)| name.as_str());
            if !builtin_name.is_some_and(|builtin| same_interface(builtin, removed)) {
                anyhow::bail!(
                    "the world exports `{removed}`, but the runtime provides {}. \
                     A world exporting `{prefix}` must name a version with the major and minor \
                     numbers and the pre-release suffix of the one the runtime provides.",
                    builtin_name
                        .map(|b| format!("`{b}`"))
                        .unwrap_or_else(|| format!("no `{prefix}`"))
                );
            }
            if let Some(builtin_name) = builtin_name {
                for kept in &mut keep_exports {
                    if kept.name == *removed {
                        kept.snapshot_name = builtin_name.to_string();
                    }
                }
            }
            if prefix == HTTP_HANDLER_INTERFACE_PREFIX {
                http_handler = builtin.map(|(builtin, name)| (builtin, name, removed.clone()));
            }
        }
    }
    // The builtin's name, for a component that exports `wasi:http/handler`,
    // and the world's name for the export.
    let http_handler = match http_handler {
        Some((builtin, name, world_name)) => {
            add_raw_http_handler_export(&mut resolve, world, builtin, &name)?;
            Some(world_name)
        }
        None => None,
    };

    let (mut bindings, _metadata) = wit_dylib::create_with_metadata(
        &resolve,
        world,
        Some(&mut DylibOpts {
            // The `dylink.0` "needed" entry the dynamic linker resolves. The
            // static link drops the section.
            interpreter: match runtime {
                Runtime::Static(_) => None,
                Runtime::Dynamic(_) => Some("libstarling_rt.so".into()),
            },
            // Respect each WIT function's declared bindings mode: an `async`
            // function is bound async and everything else sync. This is
            // `AsyncFilterSet`'s default, where an empty filter set falls through
            // to the WIT attribute. Async-declared exports route through the
            // runtime's `export_call_async`.
            async_: AsyncFilterSet::default(),
            // Where the runtime keeps its own shadow stack pointer, so the bindings
            // reach it the same way.
            stack_pointer: stack_pointer_location(runtime.module())?,
        }),
    )
    .context("generating the world's wit-dylib bindings")?;

    CustomSection {
        name: Cow::Borrowed("component-type:starling"),
        data: Cow::Owned(metadata::encode(
            &resolve,
            world,
            wit_component::StringEncoding::UTF8,
            None,
        )?),
    }
    .append_to(&mut bindings);

    let component = match runtime {
        Runtime::Static(module) => static_link::link(module, &bindings)?,
        Runtime::Dynamic(libraries) => {
            // Stubbing is off so an import nothing defines is an error here
            // rather than a trap at run time. The dylib's stream and future
            // intrinsics are ordinary component imports the host satisfies, so
            // the graph resolves and only a genuine gap is left.
            let mut linker = wit_component::Linker::default();
            linker
                .use_built_in_libdl(true)
                .stub_missing_functions(false);

            linker.library("libstarling_rt.so", &libraries.runtime, false)?;

            linker.library("libstarlingmonkey_bindings.so", &bindings, false)?;

            linker.library("libc.so", &libraries.libc, false)?;

            linker.library("libc++.so", &libraries.libcxx, false)?;

            linker.library("libc++abi.so", &libraries.libcxxabi, false)?;

            linker.library(
                "libwasi-emulated-getpid.so",
                &libraries.wasi_emulated_getpid,
                false,
            )?;

            linker.encode().map_err(|e| anyhow::anyhow!(e))?
        }
    };

    // The pipes are uncapped. They collect whatever the application's top level
    // logs during `init`, and `MemoryOutputPipe::write` traps rather than
    // truncating once a write would exceed the capacity.
    let stdout = MemoryOutputPipe::new(usize::MAX);
    let stderr = MemoryOutputPipe::new(usize::MAX);

    let js = js.into();
    let main_path = js_base_directory
        .as_ref()
        .and_then(|base| path_in(Path::new(js.name), base.as_ref()));
    let mut wasi = WasiCtxBuilder::new();
    if let Some(dir) = js_base_directory {
        // `init` only reads the application's dependency modules.
        wasi.preopened_dir(dir, "/", FsPerms::ReadOnly)?;
    }
    let wasi = wasi
        .stdin(MemoryInputPipe::new(Bytes::new()))
        .stdout(stdout.clone())
        .stderr(stderr.clone())
        .build();
    let table = ResourceTable::new();

    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    // This engine runs the component's `init` export exactly once, then the heap is wizened
    // and the compiled code discarded. Optimizing it would take several seconds per
    // world on a component of tens to hundreds of megabytes, so it is compiled
    // without optimization.
    config.cranelift_opt_level(wasmtime::OptLevel::None);
    // Compiling the component dominates a componentization. Reuse the compilation across
    // runs whenever the component and the engine's configuration are unchanged. The world's
    // bindings library is linked in before this point, so the cache hits for a rerun of the same
    // world against the same runtime build and misses for any other.
    if cache {
        config.cache(wasmtime::Cache::from_file(None).ok());
    }

    let engine = Engine::new(&config)?;
    let mut store = Store::new(&engine, Ctx { wasi, table });

    let wizer = Wizer::new();
    let (cx, component) = wizer.instrument_component(&component)?;
    let component = Component::new(&engine, &component)?;

    let mut linker = Linker::new(&engine);
    if let Some(add_to_linker) = add_to_linker {
        add_to_linker(&mut linker)?;
    } else {
        add_wasi(&mut linker)?;
    }

    // The runtime imports more than the user's world declares, such as the
    // `wasi:http` interfaces `fetch` uses, which neither `add_wasi` nor a
    // caller-supplied `add_to_linker` necessarily covers. `init` never calls
    // those, so each import nothing else defined is defined as a trap, and the
    // component instantiates without an http host.
    trap_unsatisfied_imports(&engine, &component, &mut linker, &[])?;

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let init_output;
    {
        let instance = bindings::Runtime::new(&mut store, &instance)?;
        // `init` builds the import modules from the wit-dylib `Wit` metadata, so
        // no module sources are passed to it. It reads the application's own
        // dependency modules through the preopened `js_base_directory`, and the
        // snapshot includes them.
        let raw_http_handler = http_handler
            .as_ref()
            .map(|_| RAW_HTTP_HANDLER_EXPORT.to_string());
        // `init` is an async export, so it runs as a concurrent task and can
        // drive the application's top-level `await` to completion.
        let result = store
            .run_concurrent(async |accessor| {
                instance
                    .starling_componentize_init()
                    .call_init(
                        accessor,
                        js.text.to_string(),
                        js.name.to_string(),
                        main_path,
                        cli,
                        raw_http_handler,
                        init_location.map(str::to_string),
                    )
                    .await
            })
            .await
            .and_then(|call| call);
        let printed = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout.contents()),
            String::from_utf8_lossy(&stderr.contents())
        );
        let with_printed = |message: String| {
            if printed.is_empty() {
                message
            } else {
                format!("{message}\n\nThe application printed:\n{printed}")
            }
        };
        let raw_serves = match result {
            Ok(Ok(raw_serves)) => raw_serves,
            Ok(Err(message)) => anyhow::bail!(with_printed(message)),
            Err(trap) => {
                return Err(anyhow::Error::from(trap).context(with_printed(
                    "initializing the application trapped".to_string(),
                )));
            }
        };
        init_output = printed;
        // The application implements `wasi:http/handler` itself, so its own
        // export replaces the runtime's builtin.
        if let (true, Some(name)) = (raw_serves, &http_handler) {
            for kept in &mut keep_exports {
                if kept.name == *name {
                    kept.snapshot_name = RAW_HTTP_HANDLER_EXPORT.to_string();
                }
            }
        }
    }

    let snapshot = wizer
        .snapshot_component(
            &cx,
            &mut WasmtimeWizerComponent {
                store: &mut store,
                instance,
            },
        )
        .await?;

    let component = finalize::finalize(&snapshot, &keep_exports, disabled, insecure_seed)?;
    Ok(Componentized {
        component,
        init_output,
    })
}

/// Fail if `world` exports something under a name the componentizer's own
/// exports take: the `init` interface, at any version, the runtime's
/// `wizer-initialize`, or [`RAW_HTTP_HANDLER_EXPORT`]. Names are compared as the
/// Component Model compares export names for uniqueness: ignoring case and `-`.
fn check_export_names(resolve: &Resolve, world: WorldId) -> anyhow::Result<()> {
    let canonical = |name: &str| -> String {
        name.chars()
            .filter(|c| *c != '-')
            .map(|c| c.to_ascii_lowercase())
            .collect()
    };
    let reserved = [
        INIT_INTERFACE_PREFIX,
        "wizer-initialize",
        RAW_HTTP_HANDLER_EXPORT,
    ]
    .map(canonical);
    for key in resolve.worlds[world].exports.keys() {
        let name = resolve.name_world_key(key);
        if reserved.contains(&canonical(unversioned(&name))) {
            anyhow::bail!(
                "the world exports `{name}`, a name the componentizer uses for an export of its \
                 own. Rename the world's export."
            );
        }
    }
    Ok(())
}

/// The path of the file `file` names, relative to the directory `base`, if the
/// file exists inside `base`, with `/` separating its components.
fn path_in(file: &Path, base: &Path) -> Option<String> {
    let file = std::fs::canonicalize(file).ok()?;
    let base = std::fs::canonicalize(base).ok()?;
    let relative = file.strip_prefix(&base).ok()?;
    let components: Option<Vec<&str>> = relative.iter().map(|part| part.to_str()).collect();
    Some(components?.join("/"))
}

/// A JavaScript module's source, and the name stack traces and error messages
/// show for it. A plain `&str` converts to a source named `main.js`.
///
/// A name that is the path of a file inside the base directory makes the
/// source that file's module, so a module importing the file gets it.
#[derive(Clone, Copy, Debug)]
pub struct JsSource<'a> {
    pub name: &'a str,
    pub text: &'a str,
}

impl<'a> From<&'a str> for JsSource<'a> {
    fn from(text: &'a str) -> Self {
        JsSource {
            name: "main.js",
            text,
        }
    }
}

/// Add the export [`RAW_HTTP_HANDLER_EXPORT`] to `world`: an interface with the
/// same types and function as the runtime's builtin `wasi:http/handler`, which
/// `builtin` exports as `handler`.
///
/// `builtin`'s packages are merged into `resolve` first, so the export uses the
/// `wasi:http/types` resources the runtime imports. A package `resolve` already
/// has must agree with the runtime's on every item both declare.
fn add_raw_http_handler_export(
    resolve: &mut Resolve,
    world: WorldId,
    builtin: Resolve,
    handler: &str,
) -> anyhow::Result<()> {
    resolve.merge(builtin).with_context(|| {
        format!(
            "merging the runtime's `{handler}` into the world. A world declaring \
             `{HTTP_HANDLER_INTERFACE_PREFIX}` must declare it as the runtime does, or leave \
             the interface empty."
        )
    })?;
    let version = handler
        .split_once('@')
        .map(|(_, version)| format!("@{version}"))
        .unwrap_or_default();
    let raw = resolve
        .push_str(
            "raw-http-handler.wit",
            &format!(
                "package starling:raw-http-handler;\n\
                 world raw {{\n\
                   export {RAW_HTTP_HANDLER_EXPORT}: interface {{\n\
                     use wasi:http/types{version}.{{request, response, error-code}};\n\
                     handle: async func(request: request) -> result<response, error-code>;\n\
                   }}\n\
                 }}\n"
            ),
        )
        .context("declaring the raw `wasi:http/handler` export")?;
    let raw = resolve.select_world(&[raw], Some("raw"))?;
    resolve
        .merge_worlds(raw, world, &mut wit_parser::CloneMaps::default())
        .context("adding the raw `wasi:http/handler` export to the world")
}

/// Remove every export of the interface `prefix`, an interface name without a
/// version such as `wasi:cli/run`, at any version, from `world`, returning the
/// names removed. A world exporting no such interface is left unchanged and
/// gives an empty `Vec`.
fn remove_world_export(resolve: &mut Resolve, world: WorldId, prefix: &str) -> Vec<String> {
    let to_remove: Vec<(WorldKey, String)> = resolve.worlds[world]
        .exports
        .keys()
        .filter_map(|key| {
            let name = resolve.name_world_key(key);
            (matches!(key, WorldKey::Interface(_)) && unversioned(&name) == prefix)
                .then(|| (key.clone(), name))
        })
        .collect();

    for (key, _) in &to_remove {
        resolve.worlds[world].exports.shift_remove(key);
    }

    to_remove.into_iter().map(|(_, name)| name).collect()
}

/// Where `module` keeps its shadow stack pointer.
///
/// A wasm32-wasip2 build keeps it in a `__stack_pointer` global. A wasm32-wasip3 build gives each
/// task its own in the task context, reached through the `env` accessors wit-component lowers to
/// `[context-get-0]` and `[context-set-0]`, and has no such global.
fn stack_pointer_location(module: &[u8]) -> anyhow::Result<StackPointer> {
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        let payload = payload.context("parsing the runtime module")?;
        let wasmparser::Payload::ImportSection(imports) = payload else {
            continue;
        };
        for import in imports.into_imports() {
            let import = import.context("parsing the runtime module's imports")?;
            if import.module == "env" && import.name == "__wasm_get_stack_pointer" {
                return Ok(StackPointer::TaskContext);
            }
        }
    }
    Ok(StackPointer::Global)
}

/// The runtime's own export of the interface `prefix`, an interface name without
/// a version, at any version, as the `Resolve` decoded from the world that
/// exports it and the export's name, or `None` if it exports none.
///
/// The runtime's worlds live one per `component-type*` custom section, so each
/// is decoded and searched in turn. The interfaces the world imports keep only
/// the functions `module` imports, so bindings generated from the `Resolve`
/// import nothing the runtime does not.
fn builtin_export(module: &[u8], prefix: &str) -> anyhow::Result<Option<(Resolve, String)>> {
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        let payload = payload.context("parsing the runtime module")?;
        let wasmparser::Payload::CustomSection(section) = payload else {
            continue;
        };
        if !section.name().starts_with("component-type") {
            continue;
        }

        let (resolve, world) = wit_parser::decoding::decode_world(section.data())
            .context("decoding a component-type section into a world")?;
        let found = resolve.worlds[world].exports.keys().find(|key| {
            matches!(key, WorldKey::Interface(_))
                && unversioned(&resolve.name_world_key(key)) == prefix
        });
        if let Some(key) = found {
            let name = resolve.name_world_key(key);
            let mut resolve = resolve;
            retain_imported_functions(&mut resolve, world, module)?;
            return Ok(Some((resolve, name)));
        }
    }
    Ok(None)
}

/// Remove every function of an interface `world` imports that `module` does not
/// import, either synchronously or as an `[async-lower]` import.
fn retain_imported_functions(
    resolve: &mut Resolve,
    world: WorldId,
    module: &[u8],
) -> anyhow::Result<()> {
    let mut imported = std::collections::HashSet::new();
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        let payload = payload.context("parsing the runtime module")?;
        let wasmparser::Payload::ImportSection(imports) = payload else {
            continue;
        };
        for import in imports.into_imports() {
            let import = import.context("parsing the runtime module's imports")?;
            let name = import
                .name
                .strip_prefix("[async-lower]")
                .unwrap_or(import.name);
            imported.insert((import.module, name));
        }
    }
    let interfaces: Vec<_> = resolve.worlds[world]
        .imports
        .iter()
        .filter_map(|(key, item)| match item {
            wit_parser::WorldItem::Interface { id, .. } => Some((resolve.name_world_key(key), *id)),
            _ => None,
        })
        .collect();
    for (name, id) in interfaces {
        resolve.interfaces[id]
            .functions
            .retain(|function, _| imported.contains(&(name.as_str(), function.as_str())));
    }
    Ok(())
}

/// Define every component import the configured WASI host does not provide as a
/// trapping stub.
///
/// `wasmtime::component::Linker::define_unknown_imports_as_traps` does almost
/// this, but defines every imported resource with a placeholder type, which a
/// resource a world's own interface `use`s from a provided interface cannot have.
/// This walks the component's import types, and stubs each remaining function
/// with `func_new_concurrent` for async functions and `func_new` for sync ones,
/// and each resource with the type a provider defines for it (see
/// [`provided_resource_types`]) or a placeholder if none does.
///
/// The WASI host added before this, either by [`add_wasi`] or by the caller's
/// `add_to_linker`, satisfies the `@0.2.x` WASI interfaces through
/// wasmtime's version-tolerant matching, but under its own version string.
/// Probing the linker by the component's exact `@0.2.9` import name would
/// therefore never see them, and would define traps for functions the host
/// provides. Instances
/// the WASI host owns ([`is_wasi_host_instance`]) are skipped outright instead.
/// What remains are the imports no host added, such as the runtime's
/// `wasi:http/*` without an http host.
///
/// `provided` names interface prefixes the caller has already registered on the
/// linker, which must be left alone. `is_wasi_host_instance` covers the WASI host,
/// and a caller that adds another such as `wasi:http` names it here. Stubbing over
/// one is not merely redundant: an interface's type aliases appear in the
/// component's import type as resources of their own, as `wasi:http/types`'
/// `headers` aliases `fields`, and the host registers only the underlying
/// resource, so a stub would define the alias as an unrelated host type and the
/// instantiation would fail on mismatched resource types.
pub fn trap_unsatisfied_imports<T: Send + 'static>(
    engine: &Engine,
    component: &Component,
    linker: &mut Linker<T>,
    provided: &[&str],
) -> anyhow::Result<()> {
    use wasmtime::component::types::ComponentItem;

    // Fill one instance's exports with traps, skipping any a provider already
    // defined. A whole instance the host owns is filtered out by the caller. Defining
    // a name the linker already holds is an error, which `let _ =` below ignores,
    // the same way the root-level arms tolerate an already-defined name.
    fn stub_instance_exports<T: Send + 'static>(
        engine: &Engine,
        instance: &mut wasmtime::component::LinkerInstance<'_, T>,
        exports: impl Iterator<Item = (String, ComponentItem)>,
        qualified_prefix: &str,
        host_types: &[(ResourceType, ResourceType)],
    ) -> anyhow::Result<()> {
        for (name, item) in exports {
            let qualified = format!("{qualified_prefix}#{name}");
            match item {
                ComponentItem::ComponentFunc(func) => {
                    let message = format!("called trapping stub: {qualified}");
                    let result = if func.async_() {
                        instance.func_new_concurrent(&name, move |_, _, _, _| {
                            let message = message.clone();
                            Box::pin(async move { Err(format_err!("{message}")) })
                        })
                    } else {
                        instance.func_new(&name, move |_, _, _, _| Err(format_err!("{message}")))
                    };
                    let _ = result;
                }
                ComponentItem::Resource(ty) => {
                    let ty = host_type(host_types, ty);
                    let _ = instance.resource(&name, ty, |_, _| Ok(()));
                }
                ComponentItem::ComponentInstance(inner) => {
                    let exports = inner
                        .exports(engine)
                        .map(|(n, i)| (n.to_string(), i.ty))
                        .collect::<Vec<_>>();
                    if let Ok(mut sub) = instance.instance(&name) {
                        stub_instance_exports::<T>(
                            engine,
                            &mut sub,
                            exports.into_iter(),
                            &qualified,
                            host_types,
                        )?;
                    }
                }
                // Core modules, core functions and bare types never appear as
                // component-instance imports here.
                _ => {}
            }
        }
        Ok(())
    }

    // The provider's type for `ty`, a resource type of the component's imports,
    // if a provider defines it, and a placeholder otherwise.
    fn host_type(host_types: &[(ResourceType, ResourceType)], ty: ResourceType) -> ResourceType {
        host_types
            .iter()
            .find(|(imported, _)| *imported == ty)
            .map_or_else(ResourceType::host::<()>, |(_, host)| *host)
    }

    let is_provided =
        |name: &str| is_wasi_host_instance(name) || provided.iter().any(|p| name.starts_with(p));
    let host_types = provided_resource_types(engine, component, linker, &is_provided);
    for (name, item) in component.component_type().imports(engine) {
        match item.ty {
            // The WASI host registers its interfaces under its own version
            // string, so `LinkerInstance::instance` below would succeed on the
            // component's differently versioned name and define traps for
            // functions the host provides. Such instances are skipped here. An
            // instance a provider registered under this very name is handled by
            // the next arm.
            ComponentItem::ComponentInstance(_) if is_provided(name) => {}
            ComponentItem::ComponentInstance(inner) => {
                let exports = inner
                    .exports(engine)
                    .map(|(n, i)| (n.to_string(), i.ty))
                    .collect::<Vec<_>>();
                // `instance` reopens an instance a provider already defined, such
                // as one the caller's `add_to_linker` registered. Defining a name
                // it already holds fails, and `stub_instance_exports` ignores the
                // failure, so the provider's items stay and only the ones it left
                // out get stubs. The function and resource arms below likewise
                // tolerate an already-defined name.
                let mut root = linker.root();
                if let Ok(mut sub) = root.instance(name) {
                    stub_instance_exports::<T>(
                        engine,
                        &mut sub,
                        exports.into_iter(),
                        name,
                        &host_types,
                    )?;
                }
            }
            ComponentItem::ComponentFunc(func) => {
                let message = format!("called trapping stub: {name}");
                let mut root = linker.root();
                let result = if func.async_() {
                    root.func_new_concurrent(name, move |_, _, _, _| {
                        let message = message.clone();
                        Box::pin(async move { Err(format_err!("{message}")) })
                    })
                } else {
                    root.func_new(name, move |_, _, _, _| Err(format_err!("{message}")))
                };
                // An error means a root-level function of this name is already
                // defined, for instance a world-level import stubbed earlier, so
                // it is skipped.
                let _ = result;
            }
            ComponentItem::Resource(ty) => {
                let ty = host_type(&host_types, ty);
                let _ = linker.root().resource(name, ty, |_, _| Ok(()));
            }
            _ => {}
        }
    }
    Ok(())
}

/// The resource types a provider in `linker` defines for the resources of
/// `component`'s provided import instances (those `is_provided` names) that
/// another import `use`s, each paired with its type in `component`'s imports.
///
/// Such a resource, like `wasi:filesystem/types`' `descriptor` in a world's own
/// interface, has the provider's type in the other import too, so its stub must
/// be defined with that type. The linker holds it, and [`probe_resource_type`]
/// reads it. A resource no probe finds a type for keeps a placeholder stub.
fn provided_resource_types<T: Send + 'static>(
    engine: &Engine,
    component: &Component,
    linker: &Linker<T>,
    is_provided: &dyn Fn(&str) -> bool,
) -> Vec<(ResourceType, ResourceType)> {
    use wasmtime::component::types::ComponentItem;

    fn resources(engine: &Engine, item: &ComponentItem) -> Vec<(String, ResourceType)> {
        let ComponentItem::ComponentInstance(instance) = item else {
            return Vec::new();
        };
        instance
            .exports(engine)
            .filter_map(|(name, export)| match export.ty {
                ComponentItem::Resource(ty) => Some((name.to_string(), ty)),
                _ => None,
            })
            .collect()
    }

    let imports: Vec<(String, ComponentItem)> = component
        .component_type()
        .imports(engine)
        .map(|(name, item)| (name.to_string(), item.ty))
        .collect();
    let stubbed: Vec<ResourceType> = imports
        .iter()
        .filter(|(name, _)| !is_provided(name))
        .flat_map(|(_, item)| match item {
            ComponentItem::Resource(ty) => vec![*ty],
            item => resources(engine, item)
                .into_iter()
                .map(|(_, ty)| ty)
                .collect(),
        })
        .collect();
    let mut host_types: Vec<(ResourceType, ResourceType)> = Vec::new();
    for (name, item) in imports.iter().filter(|(name, _)| is_provided(name)) {
        for (resource, ty) in resources(engine, item) {
            if !stubbed.contains(&ty) || host_types.iter().any(|(known, _)| *known == ty) {
                continue;
            }
            // An instance can re-export another's resource, which the provider
            // defines only in the one that declares it, so each instance
            // exporting the resource is tried until a probe typechecks.
            if let Some(host) = probe_resource_type(engine, linker, name, &resource) {
                host_types.push((ty, host));
            }
        }
    }
    host_types
}

/// The type `linker` defines for the resource `resource` of the instance
/// `instance`, found by typechecking a component that imports only that
/// resource. `None` if the linker does not define it there.
fn probe_resource_type<T: Send + 'static>(
    engine: &Engine,
    linker: &Linker<T>,
    instance: &str,
    resource: &str,
) -> Option<ResourceType> {
    use wasmtime::component::types::ComponentItem;

    let mut instance_type = wasm_encoder::InstanceType::new();
    instance_type.export(
        resource,
        wasm_encoder::ComponentTypeRef::Type(wasm_encoder::TypeBounds::SubResource),
    );
    let mut types = wasm_encoder::ComponentTypeSection::new();
    types.instance(&instance_type);
    let mut imports = wasm_encoder::ComponentImportSection::new();
    imports.import(instance, wasm_encoder::ComponentTypeRef::Instance(0));
    let mut probe = wasm_encoder::Component::new();
    probe.section(&types);
    probe.section(&imports);
    let probe = Component::new(engine, probe.finish()).ok()?;
    let probe = linker.substituted_component_type(&probe).ok()?;
    let (_, item) = probe.imports(engine).next()?;
    let ComponentItem::ComponentInstance(imported) = item.ty else {
        return None;
    };
    let (_, export) = imported.exports(engine).next()?;
    match export.ty {
        ComponentItem::Resource(ty) => Some(ty),
        _ => None,
    }
}

/// Add the WASI host a componentized guest runs against.
///
/// Both generations are needed. A runtime built for wasm32-wasip2 imports the
/// p2 interfaces for most of WASI, and the p3 ones for the monotonic clock and
/// `wasi:http`. One built for wasm32-wasip3 imports only p3 interfaces.
///
/// Whatever the component imports beyond this is stubbed by
/// [`trap_unsatisfied_imports`], which walks the component's own import types
/// rather than the world's declared imports and so also covers what the runtime
/// pulls in on its own. The two go together: that function leaves every instance
/// [`is_wasi_host_instance`] names to this one.
pub fn add_wasi<T: WasiView + 'static>(linker: &mut Linker<T>) -> anyhow::Result<()> {
    wasmtime_wasi::p2::add_to_linker_async(linker)?;
    wasmtime_wasi::p3::add_to_linker(linker)?;
    Ok(())
}

/// Whether the WASI host [`add_wasi`] adds owns `interface_name`.
///
/// The p2 host covers its interfaces at any `0.2.x`, since wasmtime registers them under its own
/// patch version. The p3 host covers the same families at exactly `0.3.0`, minus `wasi:io`, which
/// has no p3 counterpart. Matched exactly, so a version the host does not register, a
/// release-candidate one among them, is left to the trapping stubs.
fn is_wasi_host_instance(interface_name: &str) -> bool {
    let Some((interface, version)) = interface_name.split_once('@') else {
        return false;
    };
    let both_generations = interface.starts_with("wasi:cli/")
        || interface.starts_with("wasi:clocks/")
        || interface.starts_with("wasi:random/")
        || interface.starts_with("wasi:filesystem/")
        || interface.starts_with("wasi:sockets/");
    if version.starts_with("0.2.") {
        return both_generations || interface.starts_with("wasi:io/");
    }
    version == "0.3.0" && both_generations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_versions_match_the_runtime_table() {
        let table = include_str!("../../crates/component-model/tests/interface_versions.txt");
        for line in table.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let [a, b, expected] = line.split(' ').collect::<Vec<_>>()[..] else {
                panic!("a table line has three fields: {line:?}");
            };
            let same = same_interface(a, b);
            assert_eq!(same, expected == "same", "{a} {b}");
            assert_eq!(same_interface(b, a), same, "{b} {a}");
        }
    }

    /// The names the componentizer's own exports take are refused, and others
    /// pass.
    #[test]
    fn reserved_export_names_are_refused() {
        for (export, reserved) in [
            ("wizer-initialize: func()", true),
            ("wizer-INITIALIZE: func()", true),
            (&format!("{RAW_HTTP_HANDLER_EXPORT}: func()"), true),
            ("starling:componentize/init@0.1.0", true),
            ("starling:componentize/init@9.0.0", true),
            ("init: func()", false),
            ("run: func()", false),
        ] {
            let mut resolve = Resolve::default();
            let package = resolve
                .push_str(
                    "w.wit",
                    &format!(
                        "package t:w; world w {{ export {export}; }}\n\
                         package starling:componentize@0.1.0 {{ interface init {{}} }}\n\
                         package starling:componentize@9.0.0 {{ interface init {{}} }}\n"
                    ),
                )
                .unwrap();
            let world = resolve.select_world(&[package], Some("w")).unwrap();
            let result = check_export_names(&resolve, world);
            assert_eq!(result.is_err(), reserved, "{export}: {result:?}");
        }
    }

    /// The componentizer is released together with the runtime, under the
    /// version of the runtime workspace, `[workspace.package]` in the
    /// repository's `Cargo.toml`. This crate is a workspace of its own and cannot
    /// inherit it, so its own version has to be kept equal.
    #[test]
    fn version_is_the_workspace_version() {
        let manifest =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../Cargo.toml"))
                .unwrap();
        let section = manifest
            .split("[workspace.package]")
            .nth(1)
            .expect("the repository's `Cargo.toml` has a `[workspace.package]` table");
        let version = section
            .lines()
            .find_map(|line| line.strip_prefix("version = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("`[workspace.package]` sets `version`");
        assert_eq!(
            env!("CARGO_PKG_VERSION"),
            version,
            "set `version` in `componentize/Cargo.toml` to the workspace's"
        );
    }

    /// The componentizer's version of the `init` interface comes from
    /// `starling/wit/init.wit`.
    #[test]
    fn init_interface_name_is_versioned() {
        let name = init_interface_name();
        assert!(
            name.starts_with(&format!("{INIT_INTERFACE_PREFIX}@")),
            "{name}"
        );
    }

    /// A runtime is accepted if its `init` interface differs from the
    /// componentizer's at most in the patch number, and refused otherwise, or
    /// if it exports none.
    #[test]
    fn runtime_init_interface_versions_are_checked() {
        let runtime = |wit: &str| {
            let mut resolve = Resolve::default();
            let package = resolve.push_str("runtime.wit", wit).unwrap();
            let world = resolve.select_world(&[package], Some("runtime")).unwrap();
            let mut module = wasm_encoder::Module::new().finish();
            CustomSection {
                name: Cow::Borrowed("component-type"),
                data: Cow::Owned(
                    metadata::encode(&resolve, world, wit_component::StringEncoding::UTF8, None)
                        .unwrap(),
                ),
            }
            .append_to(&mut module);
            Runtime::Static(module)
        };
        let with_version = |version: &str| {
            runtime(&format!(
                "package starling:componentize@{version};\n\
                 interface init {{ init: func(); }}\n\
                 world runtime {{ export init; }}\n"
            ))
        };
        let own = init_interface_name();
        let (major_minor, patch) = own.rsplit_once('@').unwrap().1.rsplit_once('.').unwrap();
        let patch: u32 = patch.parse().unwrap();

        check_runtime_interface(&with_version(&format!("{major_minor}.{patch}"))).unwrap();
        check_runtime_interface(&with_version(&format!("{major_minor}.{}", patch + 1))).unwrap();
        let error = check_runtime_interface(&with_version("99.0.0"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("implements `starling:componentize/init@99.0.0`"),
            "{error}"
        );
        let error = check_runtime_interface(&runtime(
            "package t:other;\ninterface i {}\nworld runtime { export i; }\n",
        ))
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("exports no `starling:componentize/init`"),
            "{error}"
        );
    }

    /// A fresh directory under the system temp directory, named after `name`
    /// and the process, holding `files` at their relative paths.
    fn wit_tree(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("starling-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (path, contents) in files {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }
        root
    }

    /// The names of `world`'s imports and exports.
    fn world_items(resolve: &Resolve, world: WorldId) -> (Vec<String>, Vec<String>) {
        let names = |items: &wit_parser::IndexMap<WorldKey, wit_parser::WorldItem>| {
            items
                .keys()
                .map(|key| resolve.name_world_key(key))
                .collect()
        };
        (
            names(&resolve.worlds[world].imports),
            names(&resolve.worlds[world].exports),
        )
    }

    /// A WIT package passed twice, directly or through a directory's `deps`,
    /// is loaded once.
    #[test]
    fn a_package_loaded_twice_is_merged() {
        let root = wit_tree(
            "wit-twice",
            &[
                (
                    "wit/deps/dep/dep.wit",
                    "package test:dep;\ninterface i {}\n",
                ),
                (
                    "wit/app.wit",
                    "package test:app;\nworld w { import test:dep/i; }\n",
                ),
            ],
        );
        let (wit, dep) = (root.join("wit"), root.join("wit/deps/dep"));
        for paths in [vec![wit.clone(), wit.clone()], vec![dep, wit]] {
            let (resolve, world) = load_world(Wit::Paths(&paths), None, &[], false).unwrap();
            assert_eq!(resolve.worlds[world].name, "w");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Partial definitions of one package are merged into a package with the
    /// interfaces and functions of each, and several worlds into one.
    #[test]
    fn partial_packages_and_several_worlds_are_merged() {
        let root = wit_tree(
            "wit-partial",
            &[
                (
                    "kv/kv.wit",
                    "package test:spin@1.0.0;\n\
                     interface kv { get: func() -> u32; }\n\
                     world kv-world { import kv; }\n",
                ),
                (
                    "vars/vars.wit",
                    "package test:spin@1.0.0;\n\
                     interface vars { get: func() -> string; }\n\
                     interface kv { set: func(v: u32); }\n\
                     world vars-world { import vars; export run: func(); }\n",
                ),
            ],
        );
        let paths = [root.join("kv"), root.join("vars")];
        let selection = WorldSelection {
            worlds: vec!["test:spin/kv-world@1.0.0", "vars-world"],
            builtins: Vec::new(),
            resolved: Vec::new(),
        };
        let (resolve, world) = load_world(Wit::Paths(&paths), selection, &[], false).unwrap();
        assert_eq!(resolve.worlds[world].name, MERGED_WORLD_NAME);
        assert_eq!(
            world_items(&resolve, world),
            (
                vec![
                    "test:spin/kv@1.0.0".to_string(),
                    "test:spin/vars@1.0.0".to_string()
                ],
                vec!["run".to_string()]
            )
        );
        let kv = resolve
            .interfaces
            .iter()
            .find(|(_, i)| i.name.as_deref() == Some("kv"))
            .unwrap()
            .1;
        let functions: Vec<&str> = kv.functions.keys().map(String::as_str).collect();
        assert_eq!(functions, ["get", "set"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A builtin world is merged into the selected one, and its partial
    /// definition of an interface into the WIT's full one.
    #[test]
    fn a_builtin_world_is_merged_into_the_selected_one() {
        const BUILTIN: &str = "package test:serve;\n\
             world serve { export test:http/handler@1.0.0; }\n\
             package test:http@1.0.0 { interface handler {} }\n";
        let root = wit_tree(
            "wit-builtin",
            &[(
                "app.wit",
                "package test:app;\n\
                 world app { import log: func(); }\n\
                 package test:http@1.0.0 { interface handler { handle: func(); } }\n",
            )],
        );
        let selection = WorldSelection {
            worlds: vec![],
            builtins: vec![(BUILTIN, "serve")],
            resolved: Vec::new(),
        };
        let (resolve, world) =
            load_world(Wit::Paths(&[root.join("app.wit")]), selection, &[], false).unwrap();
        assert_eq!(
            world_items(&resolve, world),
            (
                vec!["log".to_string()],
                vec!["test:http/handler@1.0.0".to_string()]
            )
        );
        let handler = resolve
            .interfaces
            .iter()
            .find(|(_, i)| i.name.as_deref() == Some("handler"))
            .unwrap()
            .1;
        assert!(handler.functions.contains_key("handle"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Definitions of a package that disagree on a function are an error
    /// naming the path that disagrees.
    #[test]
    fn disagreeing_package_definitions_are_refused() {
        let root = wit_tree(
            "wit-disagree",
            &[
                (
                    "a.wit",
                    "package test:p;\ninterface i { f: func(); }\nworld w {}\n",
                ),
                (
                    "b.wit",
                    "package test:p;\ninterface i { f: func(x: u32); }\nworld v {}\n",
                ),
            ],
        );
        let paths = [root.join("a.wit"), root.join("b.wit")];
        let err = load_world(Wit::Paths(&paths), Some("v"), &[], false).unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("b.wit"), "{message}");
        assert!(message.contains("mismatch in function `f`"), "{message}");
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A WIT syntax error names the file, line and column it was found at.
    #[test]
    fn wit_errors_name_their_location() {
        let err = load_world(
            Wit::<PathBuf>::String("package a:b;\nworld w {\n  export f: func() -> u32\n}\n"),
            None,
            &[],
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("wit:4:1"), "{err}");
    }
}
