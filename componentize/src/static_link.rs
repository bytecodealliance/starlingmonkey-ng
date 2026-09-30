// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Linking a statically built runtime with wit-dylib bindings.
//!
//! [`link`] produces the component that is then initialized with Wizer. The
//! runtime is the non-PIC core module of the runtime component built by
//! `scripts/build-runtime.sh`, and the bindings are the wit-dylib module for
//! the user's world. They become the main module and one library of a
//! [`wit_component::ComponentEncoder`].
//!
//! The bindings module imports from `env`: the memory, the function table, the
//! immutable `__memory_base` and `__table_base` globals, and the `wit_dylib_*`
//! intrinsic functions. Bindings for a wasm32-wasip2 runtime also import the
//! mutable `__stack_pointer` global from `env`. Bindings for a wasm32-wasip3
//! runtime instead import the `[context-get-0]` and `[context-set-0]` builtins
//! from `$root`, which reach the stack pointer in the task context, and their
//! start function runs on a dedicated stack (see [`START_STACK_SIZE`]). The
//! bindings module's only data segment, which holds the WIT metadata, is placed
//! at `__memory_base`, and its only element segment is placed at `__table_base`.
//! Two rewrites satisfy the `env` imports from the runtime:
//!
//! - [`reserve_in_runtime`] raises the runtime's memory and table minimums by
//!   the sizes the bindings' `dylink.0` section declares, and appends two
//!   constant globals holding the old ends, exported as `__memory_base` and
//!   `__table_base`. The reserved memory lies past the end of wasi-libc's
//!   initial heap, the link-time constant `__heap_end`. `sbrk` grows memory
//!   from its current size, so `malloc` never allocates the reserved memory.
//! - [`rewrite_bindings`] renames the bindings' `env` imports other than
//!   `memory` to `__main_module__`, which the encoder resolves to the main
//!   module's exports of the same name. It drops the `dylink.0` section and
//!   adds a start function that runs `__wasm_apply_data_relocs`, which
//!   relocates the metadata's pointers by `__memory_base`, and then
//!   `__wasm_call_ctors`, which passes the metadata to `wit_dylib_initialize`.
//!   Both therefore run at instantiation, before any export is called. Wizer
//!   runs the start function during initialization and omits it from the
//!   snapshot.

use {
    anyhow::{anyhow, bail, Context as _},
    wasm_encoder::{
        reencode::{Reencode as _, RoundtripReencoder},
        CodeSection, ConstExpr, ExportKind, ExportSection, Function, FunctionSection,
        GlobalSection, GlobalType, ImportSection, Instruction, MemorySection, MemoryType, Module,
        RawSection, StartSection, TableSection, TableType, TypeSection, ValType,
    },
    wasmparser::{
        Dylink0Subsection, ExternalKind, KnownCustom, MemInfo, Parser, Payload, TableInit, TypeRef,
    },
    wit_component::{ComponentEncoder, LibraryInfo},
    wit_dylib::StackPointer,
};

/// The library name the bindings module is registered under.
const BINDINGS_LIBRARY: &str = "starling-bindings";

/// The import module the encoder resolves against the main module's exports.
const MAIN_MODULE: &str = "__main_module__";

/// The canonical builtin the bindings call to set task context slot 0, imported when they were
/// generated for a runtime that keeps its stack pointer there.
const CONTEXT_SET: &str = "[context-set-0]";

/// The global the bindings import for the base of their reserved memory region.
const MEMORY_BASE: &str = "__memory_base";

/// The global the bindings import for the base of their reserved table region.
const TABLE_BASE: &str = "__table_base";

/// Size in bytes of one wasm memory page.
const PAGE_SIZE: u64 = 65536;

/// Shadow stack the start function runs on when the runtime keeps its stack pointer in the task
/// context.
///
/// The start function runs at instantiation, before any task exists, so context slot 0 holds 0 and
/// the runtime's `wit_dylib_initialize` would address memory below zero. This much is reserved
/// immediately below `__memory_base`, and the start function points the stack at `__memory_base` so
/// it grows down through the reservation. The reservation lies below the bindings' region rather
/// than above it, so an overrun runs off the bottom of the reservation instead of into the metadata
/// the same start function is about to read.
///
/// The size isn't measured. Pages that the start function doesn't touch stay zero, so the
/// snapshot only grows by the pages it uses.
const START_STACK_SIZE: u32 = 16 * PAGE_SIZE as u32;

/// Link the runtime's core module (`runtime`) and the target world's wit-dylib
/// module (`bindings`) into a component. `stack_pointer` is the location of the
/// shadow stack pointer for which the bindings were generated.
pub fn link(
    runtime: &[u8],
    bindings: &[u8],
    stack_pointer: &StackPointer,
) -> anyhow::Result<Vec<u8>> {
    let mut mem_info = bindings_mem_info(bindings)?;
    let start_stack = matches!(stack_pointer, StackPointer::TaskContext);
    if start_stack {
        mem_info.memory_size += START_STACK_SIZE;
    }
    let runtime = reserve_in_runtime(runtime, &mem_info, start_stack)?;
    let bindings = rewrite_bindings(bindings, start_stack)?;
    ComponentEncoder::default()
        .validate(true)
        .module(&runtime)
        .context("registering the runtime as the main module")?
        .library(
            BINDINGS_LIBRARY,
            &bindings,
            LibraryInfo {
                arguments: Vec::new(),
            },
        )
        .context("registering the bindings library")?
        .encode()
        .context("encoding the component")
}

/// The memory and table reservation the bindings module declares in its
/// `dylink.0` section.
fn bindings_mem_info(bindings: &[u8]) -> anyhow::Result<MemInfo> {
    for payload in Parser::new(0).parse_all(bindings) {
        let Payload::CustomSection(section) = payload.context("parsing the bindings module")?
        else {
            continue;
        };
        let KnownCustom::Dylink0(reader) = section.as_known() else {
            continue;
        };
        for subsection in reader {
            if let Dylink0Subsection::MemInfo(info) =
                subsection.context("parsing the bindings' `dylink.0` section")?
            {
                return Ok(info);
            }
        }
    }
    bail!("the bindings module has no `dylink.0` memory info")
}

/// Reserve room for the bindings in the runtime's memory and table, and export
/// the reserved regions' start offsets as `__memory_base` and `__table_base`.
///
/// The runtime must define its memory and table rather than import them, and
/// the memory must be 32-bit. Every section other than the table, memory,
/// global and export sections is copied unchanged.
fn reserve_in_runtime(
    runtime: &[u8],
    mem_info: &MemInfo,
    start_stack: bool,
) -> anyhow::Result<Vec<u8>> {
    let mut module = Module::new();
    let mut imported_globals = 0u32;
    let mut defined_globals = 0u32;
    let mut memory_base = None;
    let mut table_base = None;
    let mut global_section_seen = false;

    for payload in Parser::new(0).parse_all(runtime) {
        let payload = payload.context("parsing the runtime module")?;
        match &payload {
            Payload::ImportSection(reader) => {
                for import in reader.clone().into_imports() {
                    let import = import?;
                    match import.ty {
                        wasmparser::TypeRef::Global(_) => imported_globals += 1,
                        wasmparser::TypeRef::Memory(_) | wasmparser::TypeRef::Table(_) => bail!(
                            "the runtime imports `{}::{}`, but must define its memory and table",
                            import.module,
                            import.name
                        ),
                        _ => {}
                    }
                }
                copy_raw(&mut module, &payload, runtime);
            }
            Payload::TableSection(reader) => {
                let mut tables = TableSection::new();
                for (index, table) in reader.clone().into_iter().enumerate() {
                    let table = table?;
                    let mut ty = RoundtripReencoder.table_type(table.ty)?;
                    if index == 0 {
                        let (base, minimum) = reserve(
                            ty.minimum,
                            u64::from(mem_info.table_size),
                            mem_info.table_alignment,
                        )?;
                        table_base = Some(base);
                        ty.minimum = minimum;
                        ty.maximum = ty.maximum.map(|max| max.max(minimum));
                    }
                    push_table(&mut tables, ty, table.init)?;
                }
                module.section(&tables);
            }
            Payload::MemorySection(reader) => {
                let mut memories = MemorySection::new();
                for (index, memory) in reader.clone().into_iter().enumerate() {
                    let mut ty: MemoryType = RoundtripReencoder.memory_type(memory?)?;
                    if index == 0 {
                        if ty.memory64 {
                            bail!("the runtime's memory is 64-bit, which is unsupported");
                        }
                        let (base, minimum) = reserve(
                            ty.minimum * PAGE_SIZE,
                            u64::from(mem_info.memory_size),
                            mem_info.memory_alignment,
                        )?;
                        // The bindings' own region starts above the start function's stack, whose
                        // size is already included in the reservation.
                        memory_base = Some(if start_stack {
                            base + u64::from(START_STACK_SIZE)
                        } else {
                            base
                        });
                        ty.minimum = minimum.div_ceil(PAGE_SIZE);
                        ty.maximum = ty.maximum.map(|max| max.max(ty.minimum));
                    }
                    memories.memory(ty);
                }
                module.section(&memories);
            }
            Payload::GlobalSection(reader) => {
                global_section_seen = true;
                let mut globals = GlobalSection::new();
                RoundtripReencoder.parse_global_section(&mut globals, reader.clone())?;
                defined_globals = globals.len();
                push_base_globals(&mut globals, memory_base, table_base)?;
                module.section(&globals);
            }
            Payload::ExportSection(reader) => {
                if !global_section_seen {
                    let mut globals = GlobalSection::new();
                    push_base_globals(&mut globals, memory_base, table_base)?;
                    module.section(&globals);
                }
                let mut exports = ExportSection::new();
                RoundtripReencoder.parse_export_section(&mut exports, reader.clone())?;
                let first_base = imported_globals + defined_globals;
                exports.export(MEMORY_BASE, ExportKind::Global, first_base);
                exports.export(TABLE_BASE, ExportKind::Global, first_base + 1);
                module.section(&exports);
            }
            _ => copy_raw(&mut module, &payload, runtime),
        }
    }

    if memory_base.is_none() || table_base.is_none() {
        bail!("the runtime defines no memory or no table");
    }
    Ok(module.finish())
}

/// Rewrite the bindings module to be linked against the main module directly:
/// `env` imports other than `memory` move to `__main_module__`, the `dylink.0`
/// section is dropped, and a start function is added that calls
/// `__wasm_apply_data_relocs`, when the module exports one, and then
/// `__wasm_call_ctors`.
///
/// If `start_stack` is set, the start function first installs `__memory_base` in task context
/// slot 0, so it runs on the shadow stack reserved immediately below the bindings' region. Both the
/// slot and the global are already imported by such a bindings module, so this adds no import and
/// shifts no index.
///
/// The start function is appended after the module's own functions, with a
/// new `[] -> []` type appended to the type section, so no existing index
/// changes.
fn rewrite_bindings(bindings: &[u8], start_stack: bool) -> anyhow::Result<Vec<u8>> {
    let mut module = Module::new();
    let mut imported_funcs = 0u32;
    let mut imported_globals = 0u32;
    let mut context_set = None;
    let mut memory_base = None;
    let mut start_type = None;
    let mut start_index = None;
    let mut start_body = None;
    // The code section under construction while its entries are being read.
    // It is emitted, with the start body appended, at the first payload after
    // the last entry.
    let mut code: Option<CodeSection> = None;

    for payload in Parser::new(0).parse_all(bindings) {
        let payload = payload.context("parsing the bindings module")?;
        if !matches!(payload, Payload::CodeSectionEntry(_)) {
            if let Some(mut code) = code.take() {
                let body = start_body
                    .take()
                    .ok_or_else(|| anyhow!("the bindings module's code precedes its exports"))?;
                code.function(&body);
                module.section(&code);
            }
        }
        match &payload {
            Payload::TypeSection(reader) => {
                let mut types = TypeSection::new();
                RoundtripReencoder.parse_type_section(&mut types, reader.clone())?;
                start_type = Some(types.len());
                types.ty().function([], []);
                module.section(&types);
            }
            Payload::ImportSection(reader) => {
                let mut imports = ImportSection::new();
                for import in reader.clone().into_imports() {
                    let import = import?;
                    match import.ty {
                        TypeRef::Func(_) => {
                            if import.name == CONTEXT_SET {
                                context_set = Some(imported_funcs);
                            }
                            imported_funcs += 1;
                        }
                        TypeRef::Global(_) => {
                            if import.name == MEMORY_BASE {
                                memory_base = Some(imported_globals);
                            }
                            imported_globals += 1;
                        }
                        _ => {}
                    }
                    let module_name = if import.module == "env" && import.name != "memory" {
                        MAIN_MODULE
                    } else {
                        import.module
                    };
                    imports.import(
                        module_name,
                        import.name,
                        RoundtripReencoder.entity_type(import.ty)?,
                    );
                }
                module.section(&imports);
            }
            Payload::FunctionSection(reader) => {
                let mut functions = FunctionSection::new();
                RoundtripReencoder.parse_function_section(&mut functions, reader.clone())?;
                start_index = Some(imported_funcs + functions.len());
                let start_type =
                    start_type.ok_or_else(|| anyhow!("the bindings module has no type section"))?;
                functions.function(start_type);
                module.section(&functions);
            }
            Payload::ExportSection(reader) => {
                copy_raw(&mut module, &payload, bindings);
                let mut apply_relocs = None;
                let mut ctors = None;
                for export in reader.clone() {
                    let export = export?;
                    if export.kind != ExternalKind::Func {
                        continue;
                    }
                    match export.name {
                        "__wasm_apply_data_relocs" => apply_relocs = Some(export.index),
                        "__wasm_call_ctors" => ctors = Some(export.index),
                        _ => {}
                    }
                }
                let ctors = ctors
                    .ok_or_else(|| anyhow!("the bindings module exports no `__wasm_call_ctors`"))?;
                let mut body = Function::new([]);
                if start_stack {
                    let context_set = context_set
                        .ok_or_else(|| anyhow!("the bindings module imports no `{CONTEXT_SET}`"))?;
                    let memory_base = memory_base
                        .ok_or_else(|| anyhow!("the bindings module imports no `{MEMORY_BASE}`"))?;
                    body.instruction(&Instruction::GlobalGet(memory_base));
                    body.instruction(&Instruction::Call(context_set));
                }
                if let Some(apply_relocs) = apply_relocs {
                    body.instruction(&Instruction::Call(apply_relocs));
                }
                body.instruction(&Instruction::Call(ctors));
                body.instruction(&Instruction::End);
                start_body = Some(body);
                let function_index = start_index
                    .ok_or_else(|| anyhow!("the bindings module has no function section"))?;
                module.section(&StartSection { function_index });
            }
            Payload::StartSection { .. } => {
                bail!("the bindings module already has a start function")
            }
            Payload::CodeSectionStart { .. } => code = Some(CodeSection::new()),
            Payload::CodeSectionEntry(body) => {
                code.as_mut()
                    .ok_or_else(|| anyhow!("a code entry outside the code section"))?
                    .raw(&bindings[usize_range(body.range())]);
            }
            Payload::CustomSection(section) if section.name() == "dylink.0" => {}
            _ => copy_raw(&mut module, &payload, bindings),
        }
    }
    Ok(module.finish())
}

/// Append the region-start globals to a runtime global section.
fn push_base_globals(
    globals: &mut GlobalSection,
    memory_base: Option<u64>,
    table_base: Option<u64>,
) -> anyhow::Result<()> {
    let (Some(memory_base), Some(table_base)) = (memory_base, table_base) else {
        bail!("the runtime's global section precedes its memory or table section");
    };
    let i32_const = GlobalType {
        val_type: ValType::I32,
        mutable: false,
        shared: false,
    };
    let offset = |base: u64| -> anyhow::Result<i32> {
        u32::try_from(base)
            .map(|v| v as i32)
            .map_err(|_| anyhow!("region base {base} exceeds the 32-bit address space"))
    };
    globals.global(i32_const, &ConstExpr::i32_const(offset(memory_base)?));
    globals.global(i32_const, &ConstExpr::i32_const(offset(table_base)?));
    Ok(())
}

/// Place a region of `size` units aligned to `2^alignment` units at or past
/// `end`, returning the region's start and the new end.
fn reserve(end: u64, size: u64, alignment: u32) -> anyhow::Result<(u64, u64)> {
    let align = 1u64
        .checked_shl(alignment)
        .ok_or_else(|| anyhow!("unsupported region alignment 2^{alignment}"))?;
    let base = end.next_multiple_of(align);
    Ok((base, base + size))
}

fn push_table(tables: &mut TableSection, ty: TableType, init: TableInit<'_>) -> anyhow::Result<()> {
    match init {
        TableInit::RefNull => tables.table(ty),
        TableInit::Expr(expr) => tables.table_with_init(ty, &RoundtripReencoder.const_expr(expr)?),
    };
    Ok(())
}

/// Narrow a wasmparser byte range to index a slice. Ranges are `u64` for 64-bit
/// memories. Every module here is wasm32, so the bounds fit.
fn usize_range(range: std::ops::Range<u64>) -> std::ops::Range<usize> {
    range.start as usize..range.end as usize
}

/// Copy a section into `module` byte for byte.
///
/// A payload that is not a whole section (a code entry, the version header,
/// the end marker) is skipped, since the code section is copied from its
/// `CodeSectionStart` payload.
fn copy_raw(module: &mut Module, payload: &Payload<'_>, source: &[u8]) {
    if let Some((id, range)) = payload.as_section() {
        module.section(&RawSection {
            id,
            data: &source[usize_range(range)],
        });
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        wasm_encoder::{
            CodeSection, Function, FunctionSection, Instruction, Section as _, TypeSection,
        },
        wasmparser::{ExternalKind, TypeRef, Validator, WasmFeatures},
        wit_dylib::{AsyncFilterSet, DylibOpts},
        wit_parser::Resolve,
    };

    const WORLD: &str = "\
package test:static-link;

world w {
  import log: func(message: string);
  export greet: func(name: string) -> string;
}
";

    /// The bindings for `WORLD` and a wasm32-wasip2 runtime, with the world's
    /// `component-type` section appended as `componentize` does, so the encoder
    /// reads the world from the library.
    fn bindings() -> Vec<u8> {
        bindings_with(StackPointer::Global)
    }

    /// Like [`bindings`], for a runtime that keeps its stack pointer in `stack_pointer`.
    fn bindings_with(stack_pointer: StackPointer) -> Vec<u8> {
        let mut resolve = Resolve::default();
        let package = resolve.push_str("w.wit", WORLD).unwrap();
        let world = resolve.select_world(&[package], Some("w")).unwrap();
        let mut opts = DylibOpts {
            interpreter: None,
            async_: AsyncFilterSet::default(),
            stack_pointer,
        };
        let mut bindings = wit_dylib::create(&resolve, world, Some(&mut opts)).unwrap();
        wasm_encoder::CustomSection {
            name: "component-type:test".into(),
            data: wit_component::metadata::encode(
                &resolve,
                world,
                wit_component::StringEncoding::UTF8,
                None,
                false,
            )
            .unwrap()
            .into(),
        }
        .append_to(&mut bindings);
        bindings
    }

    /// A stand-in for the runtime: it exports what the runtime's core module exports,
    /// plus a trapping function for every function `bindings` imports from
    /// `env`, with the imported type.
    fn fake_runtime(bindings: &[u8], memory_pages: u64, table_size: u64) -> Vec<u8> {
        let mut types = TypeSection::new();
        let mut functions = FunctionSection::new();
        let mut code = CodeSection::new();
        let mut exports = ExportSection::new();

        let mut import_types = Vec::new();
        for payload in Parser::new(0).parse_all(bindings) {
            match payload.unwrap() {
                Payload::TypeSection(reader) => {
                    import_types = reader
                        .into_iter_err_on_gc_types()
                        .map(|t| t.unwrap())
                        .collect();
                }
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import.unwrap();
                        if import.module != "env" {
                            continue;
                        }
                        if let TypeRef::Func(ty) = import.ty {
                            let ty = &import_types[ty as usize];
                            let params = ty
                                .params()
                                .iter()
                                .map(|p| RoundtripReencoder.val_type(*p).unwrap());
                            let results = ty
                                .results()
                                .iter()
                                .map(|r| RoundtripReencoder.val_type(*r).unwrap());
                            let index = types.len();
                            types.ty().function(params, results);
                            functions.function(index);
                            let mut body = Function::new([]);
                            body.instruction(&Instruction::Unreachable);
                            body.instruction(&Instruction::End);
                            code.function(&body);
                            exports.export(import.name, ExportKind::Func, index);
                        }
                    }
                }
                _ => {}
            }
        }

        let mut tables = TableSection::new();
        tables.table(TableType {
            element_type: wasm_encoder::RefType::FUNCREF,
            table64: false,
            minimum: table_size,
            maximum: None,
            shared: false,
        });
        let mut memories = MemorySection::new();
        memories.memory(MemoryType {
            minimum: memory_pages,
            maximum: None,
            memory64: false,
            shared: false,
            page_size_log2: None,
        });
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(1024),
        );
        exports.export("memory", ExportKind::Memory, 0);
        exports.export("__indirect_function_table", ExportKind::Table, 0);
        exports.export("__stack_pointer", ExportKind::Global, 0);

        let mut module = Module::new();
        module
            .section(&types)
            .section(&functions)
            .section(&tables)
            .section(&memories)
            .section(&globals)
            .section(&exports)
            .section(&code);
        module.finish()
    }

    fn validate(bytes: &[u8]) {
        Validator::new_with_features(WasmFeatures::all())
            .validate_all(bytes)
            .expect("the output validates");
    }

    fn exported_global_value(module: &[u8], name: &str) -> i32 {
        let mut globals = Vec::new();
        let mut index = None;
        for payload in Parser::new(0).parse_all(module) {
            match payload.unwrap() {
                Payload::GlobalSection(reader) => {
                    for global in reader {
                        let global = global.unwrap();
                        let mut ops = global.init_expr.get_operators_reader();
                        let wasmparser::Operator::I32Const { value } = ops.read().unwrap() else {
                            panic!("a non-constant global initializer");
                        };
                        globals.push(value);
                    }
                }
                Payload::ExportSection(reader) => {
                    index = reader
                        .into_iter()
                        .map(|e| e.unwrap())
                        .find(|e| e.name == name && e.kind == ExternalKind::Global)
                        .map(|e| e.index);
                }
                _ => {}
            }
        }
        globals[index.expect("the global is exported") as usize]
    }

    fn limits(module: &[u8]) -> (u64, u64) {
        let mut memory = None;
        let mut table = None;
        for payload in Parser::new(0).parse_all(module) {
            match payload.unwrap() {
                Payload::MemorySection(reader) => {
                    memory = Some(reader.into_iter().next().unwrap().unwrap().initial)
                }
                Payload::TableSection(reader) => {
                    table = Some(reader.into_iter().next().unwrap().unwrap().ty.initial)
                }
                _ => {}
            }
        }
        (memory.unwrap(), table.unwrap())
    }

    #[test]
    fn runtime_reserves_regions_past_its_own() {
        let bindings = bindings();
        let info = bindings_mem_info(&bindings).unwrap();
        assert!(info.memory_size > 0, "the metadata segment is not empty");
        assert!(
            info.table_size > 0,
            "the export trampolines occupy table slots"
        );

        let runtime = fake_runtime(&bindings, 3, 5);
        let reserved = reserve_in_runtime(&runtime, &info, false).unwrap();
        validate(&reserved);

        assert_eq!(exported_global_value(&reserved, MEMORY_BASE), 3 * 65536);
        assert_eq!(exported_global_value(&reserved, TABLE_BASE), 5);
        let (pages, slots) = limits(&reserved);
        assert_eq!(pages, 3 + u64::from(info.memory_size).div_ceil(PAGE_SIZE));
        assert_eq!(slots, 5 + u64::from(info.table_size));
    }

    #[test]
    fn bindings_import_from_main_module_and_start_with_ctors() {
        let rewritten = rewrite_bindings(&bindings(), false).unwrap();
        validate(&rewritten);

        let mut env_imports = Vec::new();
        let mut main_imports = Vec::new();
        let mut imported_funcs = 0;
        let mut start = None;
        let mut apply_relocs = None;
        let mut ctors = None;
        let mut custom = Vec::new();
        let mut bodies = Vec::new();
        for payload in Parser::new(0).parse_all(&rewritten) {
            match payload.unwrap() {
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import.unwrap();
                        if let TypeRef::Func(_) = import.ty {
                            imported_funcs += 1;
                        }
                        match import.module {
                            "env" => env_imports.push(import.name.to_string()),
                            MAIN_MODULE => main_imports.push(import.name.to_string()),
                            _ => {}
                        }
                    }
                }
                Payload::ExportSection(reader) => {
                    for export in reader.into_iter().map(|e| e.unwrap()) {
                        match export.name {
                            "__wasm_apply_data_relocs" => apply_relocs = Some(export.index),
                            "__wasm_call_ctors" => ctors = Some(export.index),
                            _ => {}
                        }
                    }
                }
                Payload::StartSection { func, .. } => start = Some(func),
                Payload::CodeSectionEntry(body) => {
                    let ops = body
                        .get_operators_reader()
                        .unwrap()
                        .into_iter()
                        .map(|op| op.unwrap())
                        .collect::<Vec<_>>();
                    bodies.push(ops);
                }
                Payload::CustomSection(section) => custom.push(section.name().to_string()),
                _ => {}
            }
        }
        assert_eq!(env_imports, ["memory"]);
        for name in [
            "__indirect_function_table",
            "__stack_pointer",
            MEMORY_BASE,
            TABLE_BASE,
        ] {
            assert!(
                main_imports.iter().any(|n| n == name),
                "{name} is imported from the main module"
            );
        }
        assert!(main_imports.iter().any(|n| n.starts_with("wit_dylib_")));
        assert!(!custom.iter().any(|n| n == "dylink.0"));

        let start = start.expect("a start function was added");
        let body = &bodies[(start - imported_funcs) as usize];
        let expected = [
            wasmparser::Operator::Call {
                function_index: apply_relocs.expect("the metadata has pointers to relocate"),
            },
            wasmparser::Operator::Call {
                function_index: ctors.expect("the ctors are exported"),
            },
            wasmparser::Operator::End,
        ];
        assert_eq!(
            body, &expected,
            "the start function applies relocations, then runs the ctors"
        );
    }

    #[test]
    fn links_into_a_valid_component() {
        let bindings = bindings();
        let runtime = fake_runtime(&bindings, 2, 1);
        let component = link(&runtime, &bindings, &StackPointer::Global).unwrap();
        validate(&component);

        let wit_component::DecodedWasm::Component(resolve, world) =
            wit_component::decode(&component).unwrap()
        else {
            panic!("the output is a component");
        };
        let world = &resolve.worlds[world];
        let names = |keys: &mut dyn Iterator<Item = &wit_parser::WorldKey>| {
            keys.map(|key| resolve.name_world_key(key))
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&mut world.exports.keys()), ["greet"]);
        assert_eq!(names(&mut world.imports.keys()), ["log"]);
    }

    /// Bindings that reach the stack pointer through the task context get the
    /// start stack below their own region.
    #[test]
    fn task_context_bindings_reserve_the_start_stack() {
        let bindings = bindings_with(StackPointer::TaskContext);

        let mut info = bindings_mem_info(&bindings).unwrap();
        let region = info.memory_size;
        info.memory_size += START_STACK_SIZE;
        let runtime = fake_runtime(&bindings, 3, 5);
        let reserved = reserve_in_runtime(&runtime, &info, true).unwrap();
        validate(&reserved);

        assert_eq!(
            exported_global_value(&reserved, MEMORY_BASE),
            3 * 65536 + START_STACK_SIZE as i32
        );
        let (pages, _) = limits(&reserved);
        assert_eq!(
            pages,
            3 + u64::from(START_STACK_SIZE + region).div_ceil(PAGE_SIZE)
        );
    }

    /// The start function of bindings that reach the stack pointer through the
    /// task context first sets context slot 0 to `__memory_base`, the top of the
    /// start stack.
    #[test]
    fn task_context_start_function_sets_the_stack_pointer_first() {
        let rewritten = rewrite_bindings(&bindings_with(StackPointer::TaskContext), true).unwrap();
        validate(&rewritten);

        let (mut funcs, mut globals) = (Vec::new(), Vec::new());
        let mut start = None;
        let mut bodies = Vec::new();
        for payload in Parser::new(0).parse_all(&rewritten) {
            match payload.unwrap() {
                Payload::ImportSection(reader) => {
                    for import in reader.into_imports() {
                        let import = import.unwrap();
                        match import.ty {
                            TypeRef::Func(_) => funcs.push(import.name),
                            TypeRef::Global(_) => globals.push(import.name),
                            _ => {}
                        }
                    }
                }
                Payload::StartSection { func, .. } => start = Some(func),
                Payload::CodeSectionEntry(body) => bodies.push(
                    body.get_operators_reader()
                        .unwrap()
                        .into_iter()
                        .map(|op| op.unwrap())
                        .collect::<Vec<_>>(),
                ),
                _ => {}
            }
        }
        let start = start.expect("a start function was added") as usize;
        let body = &bodies[start - funcs.len()];
        let index =
            |names: &[&str], name: &str| names.iter().position(|n| *n == name).unwrap() as u32;
        assert_eq!(
            body[..2],
            [
                wasmparser::Operator::GlobalGet {
                    global_index: index(&globals, MEMORY_BASE),
                },
                wasmparser::Operator::Call {
                    function_index: index(&funcs, CONTEXT_SET),
                },
            ]
        );
    }
}
