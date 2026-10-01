// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The pass over the wizened component that gives it exactly the surface
//! the user asked for.
//!
//! The snapshot exports everything the runtime exports (the
//! `starling:componentize/init` interface, `wizer-initialize`, and the
//! `wasi:cli/run` and `wasi:http/handler` builtins) next to the world's own exports,
//! and imports every WASI interface the runtime links against, whether or not the
//! application uses it.
//! [`finalize`] wraps it in a composition that exports only the names in
//! `keep_exports` and satisfies the imports of every disabled [`WASIAPISet`] from a
//! stub component whose functions trap, so the result imports none of a disabled
//! feature's interfaces.
//!
//! The stub component is generated from the snapshot's own import types, so the
//! resource types the remaining imports share with the stubbed ones stay the
//! same types. An interface whose types a remaining interface uses cannot be
//! stubbed at all: the composition would have to import it twice, once for each
//! side, as two distinct types. Such a conflict between a requested feature and
//! a remaining interface is an error naming both. An interface no feature owns
//! is stubbed only once nothing left uses it.
//!
//! The `now` function of each `wasi:clocks` interface returns zero instead of
//! trapping, because the runtime calls it whether or not the application does.
//! The event loop, for example, reads the monotonic clock on its first step.
//! `Date.now()` and `performance.now()` then return constant values, while all
//! other `wasi:clocks` functions still trap.
//!
//! SpiderMonkey also seeds its hash tables from `wasi:random/insecure-seed`,
//! once per task on wasm32-wasip3, so no feature owns that interface. With an
//! `insecure_seed`, the stub component exports it instead, with a
//! `get-insecure-seed` that always returns the seed.

use std::collections::HashMap;

use anyhow::{bail, Context as _};
use indexmap::IndexSet;
use wac_graph::{types::Package, CompositionGraph, EncodeOptions};
use wit_parser::{
    abi::{AbiVariant, WasmSignature, WasmType},
    decoding::DecodedWasm,
    Function, InterfaceId, ManglingAndAbi, PackageName, Resolve, Type, TypeDefKind, TypeOwner,
    WasmExport, WasmExportKind, World, WorldId, WorldItem, WorldKey,
};

/// Sets of WASI APIs the componentizer can disable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum WASIAPISet {
    /// `wasi:cli/std*` and `wasi:cli/terminal-*`: `console` output and the
    /// standard io streams.
    Stdio,
    /// `wasi:random/random`: `Math.random` seeding and `crypto.getRandomValues`.
    /// Note: does NOT disable `wasi:random/insecure-seed`, which is always called
    /// by SpiderMonkey. Use `--deterministic-insecure-random-seed` if you
    /// absolutely must disable it.
    Random,
    /// `wasi:clocks/*`: `Date.now`, `performance.now`, and timers.
    Clocks,
    /// `wasi:http/*`: `fetch`.
    Http,
    /// `wasi:filesystem/*`: module loading from disk.
    Filesystem,
    /// `wasi:sockets/*`: the debugger's TCP connection. Disabled unless the
    /// component is built for debugging.
    Sockets,
}

impl WASIAPISet {
    /// The interface-name prefixes of the feature's interfaces, without the
    /// package version.
    fn prefixes(self) -> &'static [&'static str] {
        match self {
            WASIAPISet::Stdio => &[
                "wasi:cli/stdin",
                "wasi:cli/stdout",
                "wasi:cli/stderr",
                "wasi:cli/terminal-",
            ],
            WASIAPISet::Random => &["wasi:random/random"],
            WASIAPISet::Clocks => &["wasi:clocks/"],
            WASIAPISet::Http => &["wasi:http/"],
            WASIAPISet::Filesystem => &["wasi:filesystem/"],
            WASIAPISet::Sockets => &["wasi:sockets/"],
        }
    }

    /// Whether the interface `name`, such as `wasi:http/types@0.3.0`, is one of
    /// the feature's.
    pub(crate) fn owns(self, name: &str) -> bool {
        self.prefixes()
            .iter()
            .any(|prefix| name.starts_with(prefix))
    }

    fn name(self) -> &'static str {
        match self {
            WASIAPISet::Stdio => "stdio",
            WASIAPISet::Random => "random",
            WASIAPISet::Clocks => "clocks",
            WASIAPISet::Http => "http",
            WASIAPISet::Filesystem => "filesystem",
            WASIAPISet::Sockets => "sockets",
        }
    }
}

/// Interfaces no feature owns, stubbed once every interface using their types
/// is stubbed.
///
/// `wasi:io` defines the p2 streams and pollables. `wasi:cli/types` defines the
/// `error-code` the p3 standard streams return. Neither is reached except
/// through an interface some feature owns.
const DEPENDENCY_ONLY: &[&str] = &["wasi:io/", "wasi:cli/types"];

/// The package the generated stub world lives in.
const STUB_PACKAGE: (&str, &str) = ("starling", "stubs");

/// The address of the zeroed return area a zero-returning stub returns a
/// pointer to.
const ZERO_AREA: i32 = 8;

/// The address of the return area the deterministic `get-insecure-seed` writes
/// its seed to and returns a pointer to.
const SEED_AREA: i32 = 16;

/// The interface `insecure_seed` replaces, without the package version.
const INSECURE_SEED_INTERFACE: &str = "wasi:random/insecure-seed";

/// An export of the snapshot that the finalized component keeps.
///
/// Both names are component export names: an interface's WIT name
/// (`starling:bench/api`) or a plain name, such as a world-level function's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeptExport {
    /// The snapshot's name for the export.
    pub snapshot_name: String,
    /// The finalized component's name for the export.
    pub name: String,
}

impl KeptExport {
    /// An export kept under the snapshot's name for it.
    pub fn unrenamed(name: String) -> Self {
        KeptExport {
            snapshot_name: name.clone(),
            name,
        }
    }
}

/// Wrap `snapshot` so the result exports exactly `keep_exports` and imports
/// nothing from the interfaces of `disabled`, except those named in
/// `kept_imports`. With `insecure_seed`, it imports no
/// `wasi:random/insecure-seed` either, and `get-insecure-seed` returns the
/// seed's high and low 64 bits, in that order.
///
/// `kept_imports` are interface names as the snapshot imports them, such as
/// `wasi:sockets/types@0.3.0`. The interfaces of the same feature they use are
/// kept as well.
///
/// `required` are the features a debuggable component needs. A conflict with
/// one of them is reported as unsupported rather than with a hint to disable it.
///
/// A [`KeptExport::snapshot_name`] the snapshot does not export is an error.
pub fn finalize(
    snapshot: &[u8],
    keep_exports: &[KeptExport],
    disabled: &[WASIAPISet],
    insecure_seed: Option<u128>,
    kept_imports: &[String],
    required: &[WASIAPISet],
) -> anyhow::Result<Vec<u8>> {
    let DecodedWasm::Component(mut resolve, world) =
        wit_parser::decoding::decode(snapshot).context("decoding the snapshot's world")?
    else {
        bail!("the snapshot is not a component");
    };

    let stubbed = stub_set(
        &resolve,
        world,
        keep_exports,
        disabled,
        insecure_seed.is_some(),
        kept_imports,
        required,
    )?;
    let stub_names: Vec<String> = stubbed
        .iter()
        .map(|id| interface_name(&resolve, world, *id))
        .collect();

    let stub = if stubbed.is_empty() {
        None
    } else {
        Some(stub_component(&mut resolve, &stubbed, insecure_seed)?)
    };

    let mut graph = CompositionGraph::new();
    let main = Package::from_bytes("main", None, snapshot.to_vec(), graph.types_mut())
        .context("registering the snapshot")?;
    let main = graph.register_package(main)?;
    let main_instance = graph.instantiate(main);
    if let Some(stub) = stub {
        let stub = Package::from_bytes("stubs", None, stub, graph.types_mut())
            .context("registering the stub component")?;
        let stub = graph.register_package(stub)?;
        // Instantiated before the main instance, so its own imports, the
        // interfaces the stubbed ones use, come first among the composition's.
        let stub_instance = graph.instantiate(stub);
        for name in &stub_names {
            let export = graph.alias_instance_export(stub_instance, name)?;
            graph.set_instantiation_argument(main_instance, name, export)?;
        }
    }
    for kept in keep_exports {
        let snapshot_name = &kept.snapshot_name;
        let export = graph
            .alias_instance_export(main_instance, snapshot_name)
            .with_context(|| format!("the snapshot does not export `{snapshot_name}`"))?;
        graph.export(export, &kept.name)?;
    }
    graph
        .encode(EncodeOptions::default())
        .context("encoding the finalized component")
}

/// The imported interfaces to stub: every interface of a disabled feature
/// except the kept ones (see [`finalize`]), `wasi:random/insecure-seed` with
/// `seeded`, plus the dependency-only ones
/// nothing remaining uses.
fn stub_set(
    resolve: &Resolve,
    world: WorldId,
    keep_exports: &[KeptExport],
    disabled: &[WASIAPISet],
    seeded: bool,
    kept_imports: &[String],
    required: &[WASIAPISet],
) -> anyhow::Result<IndexSet<InterfaceId>> {
    let imports: Vec<InterfaceId> = resolve.worlds[world]
        .imports
        .values()
        .filter_map(|item| match item {
            WorldItem::Interface { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    let owner = |id: InterfaceId| -> Option<WASIAPISet> {
        let name = interface_name(resolve, world, id);
        disabled
            .iter()
            .copied()
            .find(|f| f.prefixes().iter().any(|p| name.starts_with(p)))
    };
    // The feature an interface belongs to, disabled or not.
    let feature_of = |id: InterfaceId| -> Option<WASIAPISet> {
        let name = interface_name(resolve, world, id);
        <WASIAPISet as clap::ValueEnum>::value_variants()
            .iter()
            .copied()
            .find(|f| f.prefixes().iter().any(|p| name.starts_with(p)))
    };
    let dependency_only = |id: InterfaceId| {
        let name = interface_name(resolve, world, id);
        DEPENDENCY_ONLY.iter().any(|p| name.starts_with(p))
    };

    let insecure_seed = |id: InterfaceId| {
        seeded && interface_name(resolve, world, id).starts_with(INSECURE_SEED_INTERFACE)
    };

    let mut kept: IndexSet<InterfaceId> = imports
        .iter()
        .copied()
        .filter(|id| kept_imports.contains(&interface_name(resolve, world, *id)))
        .collect();
    let mut pending: Vec<InterfaceId> = kept.iter().copied().collect();
    while let Some(id) = pending.pop() {
        for dep in resolve.interface_direct_deps(id) {
            if owner(dep).is_some() && owner(dep) == owner(id) && kept.insert(dep) {
                pending.push(dep);
            }
        }
    }

    let mut stubbed: IndexSet<InterfaceId> = imports
        .iter()
        .copied()
        .filter(|id| !kept.contains(id))
        .filter(|id| owner(*id).is_some() || dependency_only(*id) || insecure_seed(*id))
        .collect();

    // The interfaces whose types the world `use`s at its top level. A disabled
    // feature's interface among them is a conflict, and a dependency-only one stays.
    let world_deps: IndexSet<InterfaceId> = resolve.worlds[world]
        .imports
        .values()
        .chain(resolve.worlds[world].exports.values())
        .filter_map(|item| match item {
            WorldItem::Type { id, .. } => match resolve.types[*id].kind {
                TypeDefKind::Type(Type::Id(used)) => match resolve.types[used].owner {
                    TypeOwner::Interface(interface) => Some(interface),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect();
    let world_uses: Vec<String> = world_deps
        .iter()
        .filter_map(|dep| {
            owner(*dep).map(|feature| {
                format!(
                    "`{}` of `{}` is used by the world",
                    interface_name(resolve, world, *dep),
                    feature.name(),
                )
            })
        })
        .collect();
    stubbed.retain(|id| owner(*id).is_some() || !world_deps.contains(id));

    // The interfaces that stay: the remaining imports and the kept exports.
    // Anything they use stays too.
    let kept_exports: Vec<InterfaceId> = resolve.worlds[world]
        .exports
        .iter()
        .filter(|(key, _)| {
            let name = resolve.name_world_key(key);
            keep_exports.iter().any(|kept| kept.snapshot_name == name)
        })
        .filter_map(|(_, item)| match item {
            WorldItem::Interface { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    loop {
        let users: Vec<InterfaceId> = imports
            .iter()
            .copied()
            .filter(|id| !stubbed.contains(id))
            .chain(kept_exports.iter().copied())
            .collect();
        let mut unstubbed = None;
        // Each use of a disabled feature's interface by one that stays, as the
        // feature, the used interface and the user.
        let mut conflicts = Vec::new();
        for user in users {
            for dep in resolve.interface_direct_deps(user) {
                if !stubbed.contains(&dep) {
                    continue;
                }
                match owner(dep) {
                    Some(feature) if !conflicts.contains(&(feature, dep, user)) => {
                        conflicts.push((feature, dep, user))
                    }
                    Some(_) => {}
                    None => unstubbed = unstubbed.or(Some(dep)),
                }
            }
        }
        if !conflicts.is_empty() || !world_uses.is_empty() {
            let uses: Vec<String> = world_uses
                .iter()
                .cloned()
                .chain(conflicts.iter().map(|(feature, dep, user)| {
                    let user_is = if kept_exports.contains(user) {
                        ", which the component exports".to_string()
                    } else {
                        feature_of(*user)
                            .map(|f| format!(" (feature `{}`)", f.name()))
                            .unwrap_or_default()
                    };
                    format!(
                        "`{}` of `{}` is used by `{}`{user_is}",
                        interface_name(resolve, world, *dep),
                        feature.name(),
                        interface_name(resolve, world, *user)
                    )
                }))
                .collect();
            // The features of the remaining imports that use a disabled feature,
            // other than a feature that is disabled already. A required one makes
            // the combination unsupported, and the others are to be disabled as well.
            let mut users: Vec<WASIAPISet> = conflicts
                .iter()
                .filter(|(_, _, user)| !kept_exports.contains(user))
                .filter_map(|(_, _, user)| feature_of(*user))
                .filter(|feature| !disabled.contains(feature))
                .collect();
            users.sort_unstable_by_key(|feature| feature.name());
            users.dedup();
            let names = |features: &[&WASIAPISet]| {
                features
                    .iter()
                    .map(|feature| format!("`{}`", feature.name()))
                    .collect::<Vec<_>>()
                    .join(" and ")
            };
            let (needed, also): (Vec<&WASIAPISet>, Vec<&WASIAPISet>) =
                users.iter().partition(|feature| required.contains(feature));
            let hint = if !needed.is_empty() {
                format!(
                    " Debugging needs {}, so this combination is unsupported.",
                    names(&needed)
                )
            } else if !also.is_empty() {
                format!(" Disable {} as well.", names(&also))
            } else {
                String::new()
            };
            bail!(
                "cannot disable these features while interfaces that stay enabled use them: {}.{hint}",
                uses.join(", ")
            );
        }
        match unstubbed {
            Some(dep) => {
                stubbed.shift_remove(&dep);
            }
            None => return Ok(stubbed),
        }
    }
}

/// A stub component exporting `stubbed`, with every function trapping except a
/// clock's `now` and, with `insecure_seed`, `get-insecure-seed`.
// TODO: make this not allocate a `memory` once we have lazy lowering.
fn stub_component(
    resolve: &mut Resolve,
    stubbed: &IndexSet<InterfaceId>,
    insecure_seed: Option<u128>,
) -> anyhow::Result<Vec<u8>> {
    // The stub world imports what its exports use, transitively, so the types
    // line up with the snapshot's.
    let mut imports = IndexSet::new();
    let mut pending: Vec<InterfaceId> = stubbed.iter().copied().collect();
    while let Some(id) = pending.pop() {
        for dep in resolve.interface_direct_deps(id).collect::<Vec<_>>() {
            if !stubbed.contains(&dep) && imports.insert(dep) {
                pending.push(dep);
            }
        }
    }
    let item = |id: InterfaceId| {
        (
            WorldKey::Interface(id),
            WorldItem::Interface {
                id,
                stability: Default::default(),
                external_id: None,
                docs: Default::default(),
                span: Default::default(),
            },
        )
    };
    let package = resolve.packages.alloc(wit_parser::Package {
        name: PackageName {
            namespace: STUB_PACKAGE.0.into(),
            name: STUB_PACKAGE.1.into(),
            version: None,
        },
        docs: Default::default(),
        interfaces: Default::default(),
        worlds: Default::default(),
    });
    let world = resolve.worlds.alloc(World {
        name: STUB_PACKAGE.1.into(),
        imports: imports.iter().copied().map(item).collect(),
        exports: stubbed.iter().copied().map(item).collect(),
        package: Some(package),
        docs: Default::default(),
        stability: Default::default(),
        includes: Default::default(),
        span: Default::default(),
    });
    resolve.packages[package]
        .worlds
        .insert(STUB_PACKAGE.1.into(), world);

    let mut module = stub_module(resolve, world, insecure_seed)?;
    wit_component::embed_component_metadata(
        &mut module,
        resolve,
        world,
        wit_component::StringEncoding::UTF8,
    )?;
    wit_component::ComponentEncoder::default()
        .validate(true)
        .module(&module)
        .context("registering the stub module")?
        .encode()
        .context("encoding the stub component")
}

/// The body of a stub function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StubBody {
    /// Traps.
    Trap,
    /// Returns zeroes.
    Zero,
    /// Returns the high and low 64 bits of the seed, in that order.
    Seed(u128),
}

/// The body of the stub of `func` in the interface named `interface`: zeroes
/// for a clock's `now`, the seed for `get-insecure-seed` with `insecure_seed`,
/// and a trap for everything else.
fn stub_body(interface: &str, func: &Function, insecure_seed: Option<u128>) -> StubBody {
    match (func.name.as_str(), insecure_seed) {
        ("now", _) if interface.starts_with("wasi:clocks/") => StubBody::Zero,
        ("get-insecure-seed", Some(seed)) if interface.starts_with(INSECURE_SEED_INTERFACE) => {
            StubBody::Seed(seed)
        }
        _ => StubBody::Trap,
    }
}

/// The core module of the stub component for `world`, which exports the
/// stubbed interfaces, in the standard name mangling.
///
/// The module exports each function of the exported interfaces with the body
/// [`stub_body`] gives it, its empty post-return function, and an empty
/// destructor for each resource. It also exports the memory and the trapping
/// `cabi_realloc` that the canonical options of the lifted functions need. The
/// memory has a page for the return areas at [`ZERO_AREA`] and [`SEED_AREA`] if
/// a body returns through one, and none otherwise. The module imports nothing.
///
/// The module is named `disabled-features`, and each function
/// `<interface>#<function> (disabled)`, or `(deterministic)` for the seed, so the
/// backtrace of a trap in a stub names the disabled function.
fn stub_module(
    resolve: &Resolve,
    world: WorldId,
    insecure_seed: Option<u128>,
) -> anyhow::Result<Vec<u8>> {
    use wasm_encoder::{
        CodeSection, ExportKind, ExportSection, Function as Body, FunctionSection, Instruction,
        MemorySection, MemoryType, NameMap, NameSection, TypeSection, ValType,
    };

    const MANGLING: ManglingAndAbi = ManglingAndAbi::Standard32;

    let mut types = TypeSection::new();
    let mut type_indices: HashMap<(Vec<ValType>, Vec<ValType>), u32> = HashMap::new();
    let mut functions = FunctionSection::new();
    let mut code = CodeSection::new();
    let mut exports = ExportSection::new();
    let mut names = NameMap::new();
    let mut return_area = false;
    let mut add =
        |name: &str, debug_name: &str, params: &[ValType], results: &[ValType], body: &Body| {
            let next = type_indices.len() as u32;
            let ty = *type_indices
                .entry((params.to_vec(), results.to_vec()))
                .or_insert_with(|| {
                    types
                        .ty()
                        .function(params.iter().copied(), results.iter().copied());
                    next
                });
            let index = functions.len();
            functions.function(ty);
            code.function(body);
            exports.export(name, ExportKind::Func, index);
            names.append(index, debug_name);
        };
    let val_types = |types: &[WasmType]| -> Vec<ValType> {
        types
            .iter()
            .map(|ty| match ty {
                WasmType::I32 | WasmType::Pointer | WasmType::Length => ValType::I32,
                WasmType::I64 | WasmType::PointerOrI64 => ValType::I64,
                WasmType::F32 => ValType::F32,
                WasmType::F64 => ValType::F64,
            })
            .collect()
    };
    let empty = || {
        let mut body = Body::new([]);
        body.instruction(&Instruction::End);
        body
    };

    for (key, item) in &resolve.worlds[world].exports {
        let WorldItem::Interface { id, .. } = item else {
            continue;
        };
        let interface = resolve.name_world_key(key);
        for func in resolve.interfaces[*id].functions.values() {
            let sig = resolve.wasm_signature(AbiVariant::GuestExport, func);
            let (params, results) = (val_types(&sig.params), val_types(&sig.results));
            let body = stub_body(&interface, func, insecure_seed);
            return_area |= sig.retptr && body != StubBody::Trap;
            let export = |kind| {
                resolve.wasm_export_name(
                    MANGLING,
                    WasmExport::Func {
                        interface: Some(key),
                        func,
                        kind,
                    },
                )
            };
            let state = match body {
                StubBody::Seed(_) => "deterministic",
                StubBody::Trap | StubBody::Zero => "disabled",
            };
            add(
                &export(WasmExportKind::Normal),
                &format!("{interface}#{} ({state})", func.name),
                &params,
                &results,
                &stub_function(&sig, &results, body)?,
            );
            add(
                &export(WasmExportKind::PostReturn),
                &format!("{interface}#{} post-return", func.name),
                &results,
                &[],
                &empty(),
            );
        }
        for &resource in resolve.interfaces[*id].types.values() {
            if !matches!(resolve.types[resource].kind, TypeDefKind::Resource) {
                continue;
            }
            let name = resolve.wasm_export_name(
                MANGLING,
                WasmExport::ResourceDtor {
                    interface: key,
                    resource,
                },
            );
            let resource_name = resolve.types[resource]
                .name
                .as_deref()
                .unwrap_or("resource");
            add(
                &name,
                &format!("{interface}#{resource_name} destructor"),
                &[ValType::I32],
                &[],
                &empty(),
            );
        }
    }
    let mut realloc = Body::new([]);
    realloc.instruction(&Instruction::Unreachable);
    realloc.instruction(&Instruction::End);
    add(
        &resolve.wasm_export_name(MANGLING, WasmExport::Realloc),
        "cabi_realloc (disabled)",
        &[ValType::I32; 4],
        &[ValType::I32],
        &realloc,
    );

    let mut memories = MemorySection::new();
    memories.memory(MemoryType {
        minimum: u64::from(return_area),
        maximum: None,
        memory64: false,
        shared: false,
        page_size_log2: None,
    });
    exports.export(
        &resolve.wasm_export_name(MANGLING, WasmExport::Memory),
        ExportKind::Memory,
        0,
    );

    let mut name_section = NameSection::new();
    name_section.module("disabled-features");
    name_section.functions(&names);

    let mut module = wasm_encoder::Module::new();
    module
        .section(&types)
        .section(&functions)
        .section(&memories)
        .section(&exports)
        .section(&code)
        .section(&name_section);
    Ok(module.finish())
}

/// The body of the stub function with the core signature `sig`, whose results
/// are `results`, that `body` describes.
fn stub_function(
    sig: &WasmSignature,
    results: &[wasm_encoder::ValType],
    body: StubBody,
) -> anyhow::Result<wasm_encoder::Function> {
    use wasm_encoder::{Instruction, ValType};

    let mut f = wasm_encoder::Function::new([]);
    match body {
        StubBody::Trap => {
            f.instruction(&Instruction::Unreachable);
        }
        // The zeroed return area holds the zero value of every result type.
        StubBody::Zero if sig.retptr => {
            f.instruction(&Instruction::I32Const(ZERO_AREA));
        }
        StubBody::Zero => {
            for result in results {
                f.instruction(&match result {
                    ValType::I32 => Instruction::I32Const(0),
                    ValType::I64 => Instruction::I64Const(0),
                    ValType::F32 => Instruction::F32Const(0.0.into()),
                    ValType::F64 => Instruction::F64Const(0.0.into()),
                    other => bail!("a zero-returning stub has an unexpected {other:?} result"),
                });
            }
        }
        StubBody::Seed(seed) => {
            if !sig.params.is_empty() || !sig.retptr {
                bail!("`get-insecure-seed` has the unexpected core signature {sig:?}");
            }
            for (offset, half) in [(0, (seed >> 64) as u64), (8, seed as u64)] {
                f.instruction(&Instruction::I32Const(SEED_AREA));
                f.instruction(&Instruction::I64Const(half as i64));
                f.instruction(&Instruction::I64Store(wasm_encoder::MemArg {
                    offset,
                    align: 3,
                    memory_index: 0,
                }));
            }
            f.instruction(&Instruction::I32Const(SEED_AREA));
        }
    }
    f.instruction(&Instruction::End);
    Ok(f)
}

/// The WIT name of an interface the world imports or exports.
fn interface_name(resolve: &Resolve, world: WorldId, id: InterfaceId) -> String {
    let w = &resolve.worlds[world];
    let key = w
        .imports
        .iter()
        .chain(w.exports.iter())
        .find_map(|(key, item)| match item {
            WorldItem::Interface { id: i, .. } if *i == id => Some(key),
            _ => None,
        });
    match key {
        Some(key) => resolve.name_world_key(key),
        // An interface reached only through `use`, named by its package.
        None => resolve.id_of(id).expect("a used interface has a package"),
    }
}

/// The export names of `world`, the surface a component built for it keeps.
pub fn world_export_names(resolve: &Resolve, world: WorldId) -> Vec<String> {
    resolve.worlds[world]
        .exports
        .keys()
        .map(|key| resolve.name_world_key(key))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kinds of interface `stub_component` stubs: clocks, whose `now`
    /// returns zeroes directly or through the return area, the insecure seed,
    /// and ordinary randomness.
    const STUBBED_WIT: &str = "\
package test:stubbed;

package wasi:clocks@0.3.0 {
  interface monotonic-clock {
    now: func() -> u64;
    get-resolution: func() -> u64;
  }
  interface system-clock {
    record instant {
      seconds: s64,
      nanoseconds: u32,
    }
    now: func() -> instant;
  }
}

package wasi:random@0.3.0 {
  interface insecure-seed {
    get-insecure-seed: func() -> tuple<u64, u64>;
  }
  interface random {
    get-random-u64: func() -> u64;
    get-random-bytes: func(len: u64) -> list<u8>;
  }
}
";

    /// The stub component of every interface in [`STUBBED_WIT`], instantiated.
    fn stub_instance(
        insecure_seed: Option<u128>,
    ) -> (wasmtime::Store<()>, wasmtime::component::Instance) {
        let mut resolve = Resolve::default();
        resolve.push_str("stubbed.wit", STUBBED_WIT).unwrap();
        let stubbed: IndexSet<InterfaceId> = resolve.interfaces.iter().map(|(id, _)| id).collect();
        let component = stub_component(&mut resolve, &stubbed, insecure_seed).unwrap();

        let engine = wasmtime::Engine::default();
        let component = wasmtime::component::Component::new(&engine, &component).unwrap();
        let mut store = wasmtime::Store::new(&engine, ());
        let instance = wasmtime::component::Linker::new(&engine)
            .instantiate(&mut store, &component)
            .unwrap();
        (store, instance)
    }

    /// Call `function` of `interface` on `instance`, returning its result.
    fn call(
        store: &mut wasmtime::Store<()>,
        instance: &wasmtime::component::Instance,
        interface: &str,
        function: &str,
        params: &[wasmtime::component::Val],
    ) -> wasmtime::Result<wasmtime::component::Val> {
        let interface = instance
            .get_export_index(&mut *store, None, interface)
            .unwrap();
        let function = instance
            .get_export_index(&mut *store, Some(&interface), function)
            .unwrap();
        let function = instance.get_func(&mut *store, function).unwrap();
        let mut results = [wasmtime::component::Val::Bool(false)];
        function.call(&mut *store, params, &mut results)?;
        Ok(results.into_iter().next().unwrap())
    }

    /// A clock's `now` returns zeroes, `get-insecure-seed` returns the seed if
    /// there is one, and every other function traps.
    #[test]
    fn stub_component_bodies() {
        use wasmtime::component::Val;

        let (mut store, instance) = stub_instance(Some((1 << 64) | 2));
        assert_eq!(
            call(
                &mut store,
                &instance,
                "wasi:clocks/monotonic-clock@0.3.0",
                "now",
                &[]
            )
            .unwrap(),
            Val::U64(0)
        );
        assert_eq!(
            call(
                &mut store,
                &instance,
                "wasi:clocks/system-clock@0.3.0",
                "now",
                &[]
            )
            .unwrap(),
            Val::Record(vec![
                ("seconds".to_string(), Val::S64(0)),
                ("nanoseconds".to_string(), Val::U32(0)),
            ])
        );
        assert_eq!(
            call(
                &mut store,
                &instance,
                "wasi:random/insecure-seed@0.3.0",
                "get-insecure-seed",
                &[]
            )
            .unwrap(),
            Val::Tuple(vec![Val::U64(1), Val::U64(2)])
        );

        for (interface, function, params) in [
            (
                "wasi:clocks/monotonic-clock@0.3.0",
                "get-resolution",
                vec![],
            ),
            ("wasi:random/random@0.3.0", "get-random-u64", vec![]),
            (
                "wasi:random/random@0.3.0",
                "get-random-bytes",
                vec![Val::U64(4)],
            ),
        ] {
            let (mut store, instance) = stub_instance(None);
            let err = call(&mut store, &instance, interface, function, &params)
                .expect_err("a disabled function traps");
            assert!(
                format!("{err:?}").contains(&format!("{interface}#{function} (disabled)")),
                "{err:?}"
            );
        }
    }

    /// Without a seed, `get-insecure-seed` traps like any other stub.
    #[test]
    fn insecure_seed_traps_without_a_seed() {
        let (mut store, instance) = stub_instance(None);
        let err = call(
            &mut store,
            &instance,
            "wasi:random/insecure-seed@0.3.0",
            "get-insecure-seed",
            &[],
        )
        .expect_err("the stub traps");
        assert!(
            format!("{err:?}").contains("get-insecure-seed (disabled)"),
            "{err:?}"
        );
    }

    /// A world importing sockets interfaces whose types use a clock's.
    const SOCKETS_WIT: &str = "\
package test:sockets;

world uses {
  import wasi:sockets/ip-name-lookup@0.3.0;
  import wasi:sockets/types@0.3.0;
  import wasi:clocks/types@0.3.0;
}

package wasi:clocks@0.3.0 {
  interface types {
    type duration = u64;
  }
}

package wasi:sockets@0.3.0 {
  interface types {
    use wasi:clocks/types@0.3.0.{duration};
    type timeout = duration;
  }
  interface ip-name-lookup {
    use types.{timeout};
    lookup: func(t: timeout);
  }
}
";

    fn sockets_world() -> (Resolve, WorldId) {
        let mut resolve = Resolve::default();
        let package = resolve.push_str("sockets.wit", SOCKETS_WIT).unwrap();
        let world = resolve.select_world(&[package], Some("uses")).unwrap();
        (resolve, world)
    }

    fn names(resolve: &Resolve, world: WorldId, ids: &IndexSet<InterfaceId>) -> Vec<String> {
        ids.iter()
            .map(|id| interface_name(resolve, world, *id))
            .collect()
    }

    /// A kept import stays with the interfaces of its feature it uses, and the
    /// feature's other interfaces are stubbed.
    #[test]
    fn kept_imports_keep_what_they_use() {
        let (resolve, world) = sockets_world();
        let kept = ["wasi:sockets/ip-name-lookup@0.3.0".to_string()];
        let stubbed = stub_set(
            &resolve,
            world,
            &[],
            &[WASIAPISet::Sockets],
            false,
            &kept,
            &[],
        )
        .unwrap();
        assert!(stubbed.is_empty(), "{:?}", names(&resolve, world, &stubbed));

        let kept = ["wasi:sockets/types@0.3.0".to_string()];
        let stubbed = stub_set(
            &resolve,
            world,
            &[],
            &[WASIAPISet::Sockets],
            false,
            &kept,
            &[],
        )
        .unwrap();
        assert_eq!(
            names(&resolve, world, &stubbed),
            ["wasi:sockets/ip-name-lookup@0.3.0"]
        );

        let stubbed = stub_set(
            &resolve,
            world,
            &[],
            &[WASIAPISet::Sockets],
            false,
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(
            names(&resolve, world, &stubbed),
            [
                "wasi:sockets/types@0.3.0",
                "wasi:sockets/ip-name-lookup@0.3.0"
            ]
        );
    }

    /// A conflict with a required feature is reported as unsupported, and the
    /// feature is not suggested for disabling.
    #[test]
    fn conflicts_with_required_features_are_unsupported() {
        let (resolve, world) = sockets_world();
        let message = stub_set(&resolve, world, &[], &[WASIAPISet::Clocks], false, &[], &[])
            .unwrap_err()
            .to_string();
        assert!(message.contains("Disable `sockets` as well."), "{message}");

        let message = stub_set(
            &resolve,
            world,
            &[],
            &[WASIAPISet::Clocks],
            false,
            &[],
            &[WASIAPISet::Sockets],
        )
        .unwrap_err()
        .to_string();
        assert!(
            message.contains("Debugging needs `sockets`, so this combination is unsupported."),
            "{message}"
        );
        assert!(!message.contains("Disable"), "{message}");
    }

    #[test]
    fn every_feature_has_a_name_and_prefixes() {
        for feature in [
            WASIAPISet::Stdio,
            WASIAPISet::Random,
            WASIAPISet::Clocks,
            WASIAPISet::Http,
            WASIAPISet::Filesystem,
            WASIAPISet::Sockets,
        ] {
            assert!(!feature.name().is_empty());
            assert!(!feature.prefixes().is_empty());
        }
    }
}
