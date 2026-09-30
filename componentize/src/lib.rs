// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Building WebAssembly components from JavaScript applications.
//!
//! [`componentize`] links bindings for a WIT world with the StarlingMonkey
//! runtime, evaluates the application's main module, and snapshots the result
//! with Wizer. [`command`] implements the `starling-componentize` command-line
//! tool, and [`wit_to_ts`] generates TypeScript declarations for a world.

use {
    anyhow::Context as _,
    bytes::Bytes,
    std::{
        borrow::Cow,
        path::{Path, PathBuf},
    },
    versions::canonicalize_compatible_versions,
    wasm_encoder::{CustomSection, Section as _},
    wasmparser::names::ComponentName,
    wasmtime::{
        component::{Component, Linker, ResourceTable, ResourceType},
        format_err, Config, Engine, Store,
    },
    wasmtime_wasi::p2::pipe::{MemoryInputPipe, MemoryOutputPipe},
    wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView},
    wasmtime_wizer::{WasmtimeWizerComponent, Wizer},
    wit_component::metadata,
    wit_dylib::{AsyncFilterSet, DylibOpts, StackPointer},
    wit_name_mapping::{same_interface, unversioned},
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

/// The name of the interface implemented by the runtime's `init` export,
/// without its version.
pub(crate) const INIT_INTERFACE_PREFIX: &str = "starling:componentize/init";

/// The name of the interface implemented by the runtime's `init` export, with
/// the version of `starling/wit/init.wit` this componentizer calls, such as
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

/// Return the name of `runtime`'s export of the `init` interface, such as
/// `starling:componentize/init@0.1.0`, or `None` if `runtime` doesn't export
/// the interface.
pub fn runtime_init_interface(runtime: &Runtime) -> anyhow::Result<Option<String>> {
    builtin_export_name(runtime, INIT_INTERFACE_PREFIX)
}

/// Fail unless `runtime` implements the version of the `init` interface this
/// componentizer calls, or a compatible one (see [`same_interface`]). The
/// interface's version changes whenever the interface does.
pub fn check_runtime_interface(runtime: &Runtime) -> anyhow::Result<()> {
    let expected = init_interface_name();
    match runtime_init_interface(runtime)? {
        Some(name) if same_interface(&name, &expected) => Ok(()),
        Some(name) => anyhow::bail!(
            "the runtime implements `{name}`, but this componentizer calls `{expected}`. \
             Use a runtime with a compatible version of the interface, such as the runtime \
             released with this componentizer."
        ),
        None => anyhow::bail!(
            "the runtime exports no `{INIT_INTERFACE_PREFIX}` interface, which this \
             componentizer calls as `{expected}`. It is not a StarlingMonkey runtime."
        ),
    }
}

pub mod command;
pub mod finalize;
mod static_link;
mod supported;
mod versions;
pub mod wit_to_ts;

/// The store data of the instance [`componentize`] initializes.
struct Ctx {
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

/// The WIT source of a world: a string, or the paths of WIT files and
/// directories.
pub enum Wit<'a, P = PathBuf> {
    String(&'a str),
    Paths(&'a [P]),
}

/// The runtime with which [`componentize`] links a world's bindings. It holds
/// the core module of the runtime component `starling.wasm`, which is built by
/// `scripts/build-runtime.sh`, including the runtime's `component-type`
/// sections.
pub struct Runtime {
    module: Vec<u8>,
}

impl Runtime {
    /// Create a runtime from `bytes`, which hold either the runtime component
    /// built by `scripts/build-runtime.sh`, or a core module with the runtime's
    /// `component-type` sections.
    ///
    /// For a component, its largest core module is used. `build-runtime.sh`
    /// stores copies of that module's `component-type*` sections under other
    /// names, which [`preserve_component_type::restore`] renames back. Fails if
    /// a component has no such copies.
    pub fn new(bytes: Vec<u8>) -> anyhow::Result<Runtime> {
        if !wasmparser::Parser::is_component(&bytes) {
            return Ok(Runtime { module: bytes });
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
        let (module, restored) =
            preserve_component_type::restore(main).context("parsing the runtime's core module")?;
        if restored == 0 {
            anyhow::bail!(
                "the runtime component has no `{}component-type` sections. Build it with \
                 `just build-runtime`, which adds them.",
                preserve_component_type::PREFIX
            );
        }
        Ok(Runtime { module })
    }

    /// The runtime's core module.
    pub fn module(&self) -> &[u8] {
        &self.module
    }
}

/// The name of the runtime's own `wasi:cli/run` export, without its version.
///
/// If the world exports `wasi:cli/run` too, the export is removed from the
/// world before the bindings are generated, because the component encoder
/// fails on duplicate exports. The component then exports the runtime's
/// implementation under the world's name for the interface, so the world must
/// name a version compatible with the runtime's.
pub(crate) const CLI_RUN_INTERFACE_PREFIX: &str = "wasi:cli/run";

/// Like [`CLI_RUN_INTERFACE_PREFIX`], for the runtime's own `wasi:http/handler`
/// export. The application can also implement the interface itself (see
/// [`RAW_HTTP_HANDLER_EXPORT`]).
pub(crate) const HTTP_HANDLER_INTERFACE_PREFIX: &str = "wasi:http/handler";

/// The name of a second export of the runtime's `wasi:http/handler` interface,
/// which is added to a world that exports `wasi:http/handler`, so that the
/// application can implement the interface itself. The main module exports its
/// `handle` under the names for `wasi:http/handler`: `wasiHttp.handler.handle`,
/// `handler.handle` or `handle`.
pub(crate) const RAW_HTTP_HANDLER_EXPORT: &str = "wasi-http-handler";

/// The worlds selected by [`load_world`]: the worlds in `worlds`, or the default
/// world of the last package loaded if `worlds` is empty, and the worlds in
/// `builtins`.
///
/// If one world is selected, the worlds in `resolved` are merged into it, and it
/// is the target world. If several are selected, they are merged with the
/// worlds in `resolved` into a new world, [`MERGED_WORLD_NAME`] in the package
/// [`MERGED_PACKAGE_NAME`], which is the target world.
#[derive(Clone, Debug, Default)]
pub struct WorldSelection<'a> {
    /// The worlds to target. Each is a world name, looked up in the last
    /// package loaded, or a qualified `namespace:package/world@version`.
    pub worlds: Vec<&'a str>,
    /// Worlds selected in addition to `worlds`, each given as the WIT source
    /// that defines it and its name in that source's package.
    pub builtins: Vec<(&'a str, &'a str)>,
    /// Worlds whose imports and exports are merged into the target world, each
    /// in its own `Resolve`, such as the one returned by
    /// [`runtime_imports_world`].
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

/// Parse `wit` into a `Resolve`, and select the target world (see
/// [`WorldSelection`]). `features` holds WIT feature names separated by commas
/// or whitespace. `all_features` enables every gated item.
///
/// Each path of a [`Wit::Paths`], and the source of each of
/// [`WorldSelection::builtins`], is parsed on its own and then merged into the
/// result. A package defined by several of them is merged into one, whose
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

    // Parse one source with `parse` into a separate `Resolve`, and merge it into
    // `resolve`. Returns the id in `resolve` of the package returned by `parse`.
    // Errors are rendered against `parsed`, the `Resolve` that holds the source,
    // because only that shows their file, line and column.
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

/// Return a world that imports every interface imported by the runtime, except
/// those of the `disabled` features. The interfaces have only the functions
/// that the runtime's core module imports, so bindings generated from the world
/// import nothing that the runtime doesn't.
///
/// The interfaces are taken from the runtime's `component-type` sections. The
/// world also imports the interfaces `use`d by these, including interfaces of
/// disabled features, which [`finalize::finalize`] reports.
///
/// The core module can import an interface at several compatible versions (see
/// [`same_interface`]), which the encoded component imports as one. The world
/// imports each of these versions, with the functions imported from any of
/// them.
pub fn runtime_imports_world(
    runtime: &Runtime,
    disabled: &[finalize::WasiApiSet],
) -> anyhow::Result<(Resolve, WorldId)> {
    // The interfaces from which the runtime's core module imports functions.
    let imported: std::collections::BTreeSet<&str> = module_imports(runtime.module())?
        .into_iter()
        .map(|(module, _)| module)
        .filter(|module| !module.starts_with('[') && module.contains(':'))
        .collect();

    let mut resolve = Resolve::default();
    let mut known = std::collections::BTreeSet::new();
    for (decoded, world) in component_type_worlds(runtime.module())? {
        for key in decoded.worlds[world].imports.keys() {
            known.insert(decoded.name_world_key(key));
        }
        resolve
            .merge(decoded)
            .context("merging the runtime's component-type sections")?;
    }

    let imports: Vec<String> = imported
        .into_iter()
        .filter(|name| known.contains(*name))
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

/// Like [`load_world`], but for [`wit_to_ts::generate`]. Fails for a type that
/// [`componentize`] can't lift or lower. An interface imported at several
/// compatible versions is imported only once (see
/// `canonicalize_compatible_versions`).
///
/// If `runtime` is `Some` and the world exports `wasi:http/handler`, the export
/// is replaced as in [`componentize`], by an export named
/// `RAW_HTTP_HANDLER_EXPORT` with the runtime's declaration of the interface.
/// The declarations then describe the `handle` implemented by the application.
pub fn load_world_for_types<'a>(
    wit: Wit<'_, impl AsRef<Path>>,
    world: impl Into<WorldSelection<'a>>,
    features: &[String],
    all_features: bool,
    runtime: Option<&Runtime>,
) -> anyhow::Result<(Resolve, WorldId)> {
    let (mut resolve, world) = load_world(wit, world, features, all_features)?;
    supported::check_supported_types(&resolve, world)?;
    let Some(runtime) = runtime else {
        return canonicalize_compatible_versions(resolve, world);
    };
    let Some(removed) = remove_world_export(&mut resolve, world, HTTP_HANDLER_INTERFACE_PREFIX)
        .into_iter()
        .next()
    else {
        return canonicalize_compatible_versions(resolve, world);
    };
    match builtin_export(runtime.module(), HTTP_HANDLER_INTERFACE_PREFIX)? {
        Some((builtin, name)) if same_interface(&name, &removed) => {
            add_raw_http_handler_export(&mut resolve, world, builtin, &name)?;
            canonicalize_compatible_versions(resolve, world)
        }
        builtin => anyhow::bail!(
            "the world exports `{removed}`, but the runtime provides {}",
            builtin.map_or("none".to_string(), |(_, name)| format!("`{name}`"))
        ),
    }
}

/// The settings of a [`componentize`] call besides its inputs. The default
/// enables no WIT features, sets no base directory, disables no WASI APIs, and uses
/// the compilation cache.
#[derive(Clone, Copy, Debug)]
pub struct ComponentizeOptions<'a> {
    /// WIT feature names to enable, each entry comma- or whitespace-separated.
    pub features: &'a [String],
    /// Whether to enable every gated WIT item.
    pub all_features: bool,
    /// The directory within which the application's relative imports are
    /// resolved. It is preopened read-only at `/` while the application is
    /// initialized.
    pub base_directory: Option<&'a Path>,
    /// The WASI APIs from which the component doesn't import anything (see
    /// [`finalize::finalize`]).
    pub disabled: &'a [finalize::WasiApiSet],
    /// If set, the component doesn't import `wasi:random/insecure-seed`, and
    /// `get-insecure-seed` always returns this seed (see
    /// [`finalize::finalize`]).
    pub insecure_seed: Option<u128>,
    /// The URL `globalThis.location` reflects while the application's top level
    /// runs, if any.
    pub init_location: Option<&'a str>,
    /// Whether to cache the compiled component in wasmtime's global
    /// compilation cache.
    pub cache: bool,
}

impl Default for ComponentizeOptions<'_> {
    fn default() -> Self {
        ComponentizeOptions {
            features: &[],
            all_features: false,
            base_directory: None,
            disabled: &[],
            insecure_seed: None,
            init_location: None,
            cache: true,
        }
    }
}

/// What [`componentize`] produces.
pub struct Componentized {
    /// The component.
    pub component: Vec<u8>,
    /// The output that the application's top level printed on stdout and
    /// stderr while it was initialized.
    pub init_output: String,
}

/// Build a component from the JavaScript application `js` and the world
/// selected from `wit`:
///
/// 1. The world's bindings are generated with wit-dylib, and linked with the
///    runtime's core module by `static_link::link`.
/// 2. The component is instantiated, and its `init` export is called with
///    `js`, which evaluates the application's main module.
/// 3. The instance is snapshotted with Wizer.
/// 4. [`finalize::finalize`] wraps the snapshot, so that the component exports
///    exactly the world's exports, and doesn't import from the interfaces of
///    `options.disabled`.
pub async fn componentize(
    wit: Wit<'_, impl AsRef<Path>>,
    world: impl Into<WorldSelection<'_>>,
    js: impl Into<JsSource<'_>>,
    runtime: &Runtime,
    options: &ComponentizeOptions<'_>,
) -> anyhow::Result<Componentized> {
    let ComponentizeOptions {
        features,
        all_features,
        base_directory,
        disabled,
        insecure_seed,
        init_location,
        cache,
    } = *options;
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

    // The world's name for its `wasi:http/handler` export, if it has one.
    let mut http_handler = None;
    let mut cli = false;
    // A world's `wasi:cli/run` and `wasi:http/handler` exports are implemented
    // by the runtime's own exports. They are removed from the world before the
    // bindings and the metadata section below are generated, because they would
    // otherwise be duplicate exports.
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
                     A world exporting `{prefix}` must name a version compatible with the one \
                     the runtime provides.",
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
            if let (HTTP_HANDLER_INTERFACE_PREFIX, Some((builtin, name))) = (prefix, builtin) {
                add_raw_http_handler_export(&mut resolve, world, builtin, &name)?;
                http_handler = Some(removed.clone());
            }
        }
    }

    // This runs after the runtime's declaration of `wasi:http/handler` is merged
    // in, so that its packages are canonicalized with the others.
    let (resolve, world) = canonicalize_compatible_versions(resolve, world)?;
    // The bindings use the runtime's shadow stack pointer.
    let stack_pointer = stack_pointer_location(runtime.module())?;
    let (mut bindings, _metadata) = wit_dylib::create_with_metadata(
        &resolve,
        world,
        Some(&mut DylibOpts {
            interpreter: None,
            // With the default filter set, functions declared `async` in WIT
            // are bound as async, and all others as sync.
            async_: AsyncFilterSet::default(),
            stack_pointer: stack_pointer.clone(),
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
            false,
        )?),
    }
    .append_to(&mut bindings);

    let component = static_link::link(runtime.module(), &bindings, &stack_pointer)?;

    // The pipes are uncapped. They collect whatever the application's top level
    // logs during `init`, and `MemoryOutputPipe::write` traps rather than
    // truncating once a write would exceed the capacity.
    let stdout = MemoryOutputPipe::new(usize::MAX);
    let stderr = MemoryOutputPipe::new(usize::MAX);

    let js = js.into();
    let main_path = base_directory.and_then(|base| path_in(Path::new(js.name), base));
    let mut wasi = WasiCtxBuilder::new();
    if let Some(dir) = base_directory {
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
    // We don't register the compiled code's unwind information with the system unwinder, which
    // wasmtime's own backtraces don't use. On macOS, registering and unregistering it takes one
    // process-wide lock, which serializes every thread that compiles or drops a component.
    config.native_unwind_info(false);
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
    add_wasi(&mut linker)?;

    // The runtime imports more than the user's world declares, such as the
    // `wasi:http` interfaces `fetch` uses, which `add_wasi` does not cover.
    // `init` never calls those, so each import not defined by anything else is
    // defined as a trap. The component therefore instantiates without an http
    // host.
    trap_unsatisfied_imports(&engine, &component, &mut linker, &[])?;

    let instance = linker.instantiate_async(&mut store, &component).await?;
    let init_output;
    {
        let instance = bindings::Runtime::new(&mut store, &instance)?;
        // `init` builds the import modules from the wit-dylib `Wit` metadata, so
        // no module sources are passed to it. It reads the application's own
        // dependency modules through the preopened `base_directory`, and the
        // snapshot includes them.
        let raw_http_handler = http_handler
            .as_ref()
            .map(|_| RAW_HTTP_HANDLER_EXPORT.to_string());
        // `init` is an async export, so it runs as a concurrent task, which
        // lets the application's top-level `await` complete.
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
/// `wizer-initialize`, or [`RAW_HTTP_HANDLER_EXPORT`]. Names are compared with
/// the equality of [`ComponentName`], which the Component Model uses for the
/// uniqueness of export names: ignoring case and `-`.
fn check_export_names(resolve: &Resolve, world: WorldId) -> anyhow::Result<()> {
    let reserved = [
        INIT_INTERFACE_PREFIX,
        "wizer-initialize",
        RAW_HTTP_HANDLER_EXPORT,
    ]
    .map(|name| ComponentName::new(name, 0).expect("a reserved name is a valid export name"));
    for key in resolve.worlds[world].exports.keys() {
        let name = resolve.name_world_key(key);
        let parsed = ComponentName::new(unversioned(&name), 0)
            .with_context(|| format!("the world's export name `{name}` is invalid"))?;
        if reserved.contains(&parsed) {
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

/// A JavaScript module's source, and the name shown for it in stack traces and
/// error messages. A `&str` converts to a source named `main.js`.
///
/// If the name is the path of a file in the base directory, a module that
/// imports that file gets this module.
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
/// `builtin`'s packages are merged into `resolve` first, so that the export uses
/// the `wasi:http/types` resources imported by the runtime. If `resolve`
/// already has one of these packages, the two must agree on every item that
/// both declare.
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

/// Remove from `world` every export of the interface `prefix` at any version,
/// returning the names removed. `prefix` is an interface name without a
/// version, such as `wasi:cli/run`. Returns an empty `Vec` if `world` doesn't
/// export the interface.
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
    let task_context = module_imports(module)?.contains(&("env", "__wasm_get_stack_pointer"));
    Ok(if task_context {
        StackPointer::TaskContext
    } else {
        StackPointer::Global
    })
}

/// The module and name of every import of the runtime's core module `module`.
fn module_imports(module: &[u8]) -> anyhow::Result<Vec<(&str, &str)>> {
    let mut imports = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        let payload = payload.context("parsing the runtime module")?;
        let wasmparser::Payload::ImportSection(reader) = payload else {
            continue;
        };
        for import in reader.into_imports() {
            let import = import.context("parsing the runtime module's imports")?;
            imports.push((import.module, import.name));
        }
    }
    Ok(imports)
}

/// Return the world of `module` that exports the interface `prefix` at any
/// version, decoded into its own `Resolve`, and the export's name. `prefix` is
/// an interface name without a version. Returns `None` if `module` doesn't
/// export the interface.
///
/// The interfaces imported by the world keep only the functions that `module`
/// imports, so bindings generated from the `Resolve` import nothing that the
/// runtime doesn't.
fn builtin_export(module: &[u8], prefix: &str) -> anyhow::Result<Option<(Resolve, String)>> {
    let Some((mut resolve, world, name)) = find_builtin_export(module, prefix)? else {
        return Ok(None);
    };
    retain_imported_functions(&mut resolve, world, module)?;
    Ok(Some((resolve, name)))
}

/// Return the name of the runtime's export of the interface `prefix`, such as
/// `wasi:cli/run@0.3.0` for `wasi:cli/run`, or `None` if the runtime doesn't
/// export the interface.
pub(crate) fn builtin_export_name(
    runtime: &Runtime,
    prefix: &str,
) -> anyhow::Result<Option<String>> {
    Ok(find_builtin_export(runtime.module(), prefix)?.map(|(_, _, name)| name))
}

/// Like [`builtin_export`], but without removing any functions, and also
/// returning the world's id.
///
/// The worlds of `module` are searched in the order of their sections.
fn find_builtin_export(
    module: &[u8],
    prefix: &str,
) -> anyhow::Result<Option<(Resolve, WorldId, String)>> {
    for (resolve, world) in component_type_worlds(module)? {
        let found = resolve.worlds[world].exports.keys().find(|key| {
            matches!(key, WorldKey::Interface(_))
                && unversioned(&resolve.name_world_key(key)) == prefix
        });
        if let Some(key) = found {
            let name = resolve.name_world_key(key);
            return Ok(Some((resolve, world, name)));
        }
    }
    Ok(None)
}

/// Decode each `component-type*` custom section of the core module `module`,
/// which holds one world, into its own `Resolve`.
fn component_type_worlds(module: &[u8]) -> anyhow::Result<Vec<(Resolve, WorldId)>> {
    let mut worlds = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(module) {
        let wasmparser::Payload::CustomSection(section) =
            payload.context("parsing the runtime module")?
        else {
            continue;
        };
        if section.name().starts_with("component-type") {
            worlds.push(
                wit_parser::decoding::decode_world(section.data())
                    .context("decoding a component-type section into a world")?,
            );
        }
    }
    Ok(worlds)
}

/// From each interface imported by `world`, remove the functions that `module`
/// doesn't import, either synchronously or as an `[async-lower]` import, from
/// any version of the interface compatible with the world's (see
/// [`same_interface`]).
fn retain_imported_functions(
    resolve: &mut Resolve,
    world: WorldId,
    module: &[u8],
) -> anyhow::Result<()> {
    // The modules importing each function, by the function's name.
    let mut imported: std::collections::HashMap<&str, Vec<&str>> = Default::default();
    for (module, name) in module_imports(module)? {
        let name = name.strip_prefix("[async-lower]").unwrap_or(name);
        imported.entry(name).or_default().push(module);
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
        resolve.interfaces[id].functions.retain(|function, _| {
            imported
                .get(function.as_str())
                .is_some_and(|modules| modules.iter().any(|module| same_interface(module, &name)))
        });
    }
    Ok(())
}

/// Define every import of `component` that isn't provided by the WASI host or
/// by `provided` as a stub that traps.
///
/// `Linker::define_unknown_imports_as_traps` does almost the same, but gives
/// every imported resource a placeholder type. That fails for a resource that a
/// world's own interface `use`s from a provided interface. This function stubs
/// each remaining function, with `func_new_concurrent` for async functions and
/// `func_new` for sync ones, and each resource with the type a provider defines
/// for it (see `provided_resource_types`), or a placeholder if none does.
///
/// The WASI host added by [`add_wasi`] provides compatible versions of its
/// interfaces under its own version string, so its instances (see
/// `is_wasi_host_instance`) are skipped without looking them up in the
/// linker.
///
/// `provided` holds the interface name prefixes of other hosts that the caller
/// added to the linker, such as `wasi:http`. Their instances are skipped as
/// well. An interface's type aliases appear in the component's imports as
/// separate resources, such as `headers`, an alias of `fields` in
/// `wasi:http/types`, while the host defines only the underlying resource. A
/// stub would define the alias as an unrelated type, and instantiation would
/// fail.
pub fn trap_unsatisfied_imports<T: Send + 'static>(
    engine: &Engine,
    component: &Component,
    linker: &mut Linker<T>,
    provided: &[&str],
) -> anyhow::Result<()> {
    use wasmtime::component::types::ComponentItem;

    // Define each of `items` in `instance` as a stub, named `{prefix}#{name}` in
    // the trap's message, or `{name}` if `prefix` is `None`. Defining a name
    // that the linker already has fails, and the failure is ignored.
    // `instance()` reopens an instance that a provider already defined, so the
    // provider's items are kept, and only the missing items get stubs.
    fn stub_items<T: Send + 'static>(
        engine: &Engine,
        instance: &mut wasmtime::component::LinkerInstance<'_, T>,
        items: impl Iterator<Item = (String, ComponentItem)>,
        prefix: Option<&str>,
        host_types: &[(ResourceType, ResourceType)],
    ) -> anyhow::Result<()> {
        for (name, item) in items {
            let qualified = match prefix {
                Some(prefix) => format!("{prefix}#{name}"),
                None => name.clone(),
            };
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
                        stub_items::<T>(
                            engine,
                            &mut sub,
                            exports.into_iter(),
                            Some(&qualified),
                            host_types,
                        )?;
                    }
                }
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
    let imports: Vec<(String, ComponentItem)> = component
        .component_type()
        .imports(engine)
        .filter(|(name, item)| {
            !(matches!(item.ty, ComponentItem::ComponentInstance(_)) && is_provided(name))
        })
        .map(|(name, item)| (name.to_string(), item.ty))
        .collect();
    stub_items::<T>(
        engine,
        &mut linker.root(),
        imports.into_iter(),
        None,
        &host_types,
    )
}

/// For each resource of a provided import instance (see `is_provided`) that is
/// used by a stubbed import, return the resource's type in `component`'s
/// imports, paired with the type defined for it in `linker`.
///
/// Such a resource, such as `descriptor` of `wasi:filesystem/types` used by a
/// world's own interface, has the provider's type in the stubbed import too,
/// so its stub must have that type (see [`probe_resource_type`]). A resource
/// whose type isn't found isn't returned, and gets a placeholder type.
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

/// Add the WASI p2 and p3 hosts to `linker`. A runtime built for wasm32-wasip2
/// imports the p2 interfaces for most of WASI, and the p3 interfaces for the
/// monotonic clock and `wasi:http`. A runtime built for wasm32-wasip3 imports
/// only p3 interfaces.
pub fn add_wasi<T: WasiView + 'static>(linker: &mut Linker<T>) -> anyhow::Result<()> {
    wasmtime_wasi::p2::add_to_linker_async(linker)?;
    wasmtime_wasi::p3::add_to_linker(linker)?;
    Ok(())
}

/// Whether the WASI host added by [`add_wasi`] provides `interface_name`: an
/// interface of the p2 host at a version compatible with `0.2.0`, or of the p3
/// host at a version compatible with `0.3.0` (see [`same_interface`]). The p3
/// host provides the same packages as the p2 host, except `wasi:io`.
/// Pre-release versions aren't provided.
fn is_wasi_host_instance(interface_name: &str) -> bool {
    let Some((interface, _)) = interface_name.split_once('@') else {
        return false;
    };
    let both_generations = interface.starts_with("wasi:cli/")
        || interface.starts_with("wasi:clocks/")
        || interface.starts_with("wasi:random/")
        || interface.starts_with("wasi:filesystem/")
        || interface.starts_with("wasi:sockets/");
    let compatible =
        |version: &str| same_interface(interface_name, &format!("{interface}@{version}"));
    (both_generations || interface.starts_with("wasi:io/")) && compatible("0.2.0")
        || both_generations && compatible("0.3.0")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names of the componentizer's own exports are refused. All other names
    /// are accepted.
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

    /// The WASI host provides compatible versions of its interfaces, and not
    /// their pre-releases or other versions.
    #[test]
    fn wasi_host_instances_match_compatible_versions() {
        for (name, owned) in [
            ("wasi:io/poll@0.2.0", true),
            ("wasi:io/poll@0.2.12", true),
            ("wasi:cli/environment@0.3.0", true),
            ("wasi:cli/environment@0.3.4", true),
            ("wasi:io/poll@0.3.0", false),
            ("wasi:cli/environment@0.3.0-rc.1", false),
            ("wasi:cli/environment@0.4.0", false),
            ("wasi:http/types@0.3.0", false),
            ("wasi:cli/environment", false),
        ] {
            assert_eq!(is_wasi_host_instance(name), owned, "{name}");
        }
    }

    /// A runtime is accepted if its `init` interface is compatible with the
    /// componentizer's, and refused otherwise, or if it exports none.
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
                    metadata::encode(
                        &resolve,
                        world,
                        wit_component::StringEncoding::UTF8,
                        None,
                        false,
                    )
                    .unwrap(),
                ),
            }
            .append_to(&mut module);
            Runtime::new(module).unwrap()
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

    /// If definitions of a package disagree on a function, the error names the
    /// path that disagrees.
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
