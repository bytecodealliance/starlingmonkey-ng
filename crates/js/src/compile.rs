// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Script compilation and evaluation.
//!
//! This module wraps SpiderMonkey's compilation and evaluation APIs, with both
//! safe convenience functions (e.g. [`evaluate`]) and lower-level wrappers for
//! advanced use cases.
//!
//! # Quick Start
//!
//! ```ignore
//! use crate::compile;
//! let result = compile::evaluate(&scope, "2 + 2")?;
//! ```

use std::ffi::CString;
use std::ptr::NonNull;

use crate::gc::scope::Scope;
use mozjs::gc::{Handle, HandleFunction, HandleScript, HandleValue};
use mozjs::jsapi::mozilla::Utf8Unit;
use mozjs::jsapi::{
    DelazificationOption, EnvironmentChain, JSFunction, JSScript, ReadOnlyCompileOptions,
    SourceText,
};
use mozjs::rust::{transform_str_to_source_text, wrappers2, CompileOptionsWrapper};

use super::error::ExnThrown;

/// Evaluate a UTF-8 script string and return its completion value.
///
/// This is the primary entry point for evaluating JavaScript from Rust. It
/// handles compile options creation internally.
pub fn evaluate<'s>(scope: &'s Scope<'_>, script: &str) -> Result<HandleValue<'s>, ExnThrown> {
    evaluate_with_filename(scope, script, "<inline>", 1)
}

/// Evaluate a UTF-8 script string with a custom filename and starting line.
pub fn evaluate_with_filename<'s>(
    scope: &'s Scope<'_>,
    script: &str,
    filename: &str,
    lineno: u32,
) -> Result<HandleValue<'s>, ExnThrown> {
    evaluate_with_options(scope, script, filename, lineno, false)
}

/// Evaluate a UTF-8 script string in a non-syntactic scope.
///
/// This is equivalent to calling [`evaluate_with_filename`] but with the
/// `nonSyntacticScope` option set. In a non-syntactic scope, top-level `let` and
/// `const` bindings are placed on a shared scope object rather than creating a
/// new lexical scope, making them visible to subsequent `evalScript` calls.
///
/// This is primarily used by the WPT (Web Platform Tests) harness to emulate
/// the behavior of HTML `<script>` tags.
pub fn evaluate_non_syntactic<'s>(
    scope: &'s Scope<'_>,
    script: &str,
    filename: &str,
    lineno: u32,
) -> Result<HandleValue<'s>, ExnThrown> {
    evaluate_with_options(scope, script, filename, lineno, true)
}

/// Compile options for a script or module at `filename` and `lineno`. Every function is parsed
/// and compiled to bytecode when its script is compiled, instead of on its first call, so a
/// snapshot taken after evaluation holds the bytecode of every function.
pub fn options(scope: &Scope<'_>, filename: CString, lineno: u32) -> CompileOptionsWrapper {
    let options = CompileOptionsWrapper::new(scope.cx(), filename, lineno);
    // SAFETY: `options.ptr` is a valid pointer created by `NewCompileOptions`, owned by
    // `options`.
    unsafe {
        (*options.ptr)._base.eagerDelazificationStrategy_ =
            DelazificationOption::ParseEverythingEagerly;
    }
    options
}

fn evaluate_with_options<'s>(
    scope: &'s Scope<'_>,
    script: &str,
    filename: &str,
    lineno: u32,
    non_syntactic_scope: bool,
) -> Result<HandleValue<'s>, ExnThrown> {
    let options = options(scope, filename_or_unknown(filename), lineno);
    if non_syntactic_scope {
        // SAFETY: `options.ptr` is a valid pointer created by `NewCompileOptions`, owned by
        // `options`.
        unsafe {
            (*options.ptr)._base.nonSyntacticScope = non_syntactic_scope;
        }
    }
    let mut source = transform_str_to_source_text(script);
    // SAFETY: the options and the source are valid.
    unsafe { evaluate_utf8_raw(scope, options.ptr, &mut source) }
}

/// Returns `filename` as a C string, or `<unknown>` if it contains a NUL.
fn filename_or_unknown(filename: &str) -> CString {
    CString::new(filename).unwrap_or_else(|_| c"<unknown>".into())
}

/// Evaluate a UTF-16 script and return its completion value.
///
/// # Safety
///
/// `options` must be a valid pointer to `ReadOnlyCompileOptions`.
/// `src_buf` must be a valid pointer to a `SourceText<u16>`.
pub unsafe fn evaluate_utf16_raw<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<u16>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let mut rval = scope.root_value_mut(crate::value::undefined());
    let ok = wrappers2::Evaluate(scope.cx_mut(), options, src_buf, rval.reborrow());
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Evaluate a UTF-8 script with raw compile options.
///
/// # Safety
///
/// `options` must be a valid pointer to `ReadOnlyCompileOptions`.
/// `src_buf` must be a valid pointer to a `SourceText<Utf8Unit>`.
pub unsafe fn evaluate_utf8_raw<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<Utf8Unit>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let mut rval = scope.root_value_mut(crate::value::undefined());
    let ok = wrappers2::Evaluate2(scope.cx_mut(), options, src_buf, rval.reborrow());
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Evaluate a UTF-16 script with a custom environment chain.
///
/// # Safety
///
/// `env_chain`, `options`, and `src_buf` must all be valid pointers.
pub unsafe fn evaluate_with_env_raw<'s>(
    scope: &'s Scope<'_>,
    env_chain: *const EnvironmentChain,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<u16>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let mut rval = scope.root_value_mut(crate::value::undefined());
    let ok = wrappers2::Evaluate1(scope.cx_mut(), env_chain, options, src_buf, rval.reborrow());
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Evaluate a script from a UTF-8 file path.
///
/// # Safety
///
/// `options` must be valid. `filename` must be a valid C string.
pub unsafe fn evaluate_path_raw<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    filename: *const std::os::raw::c_char,
) -> Result<HandleValue<'s>, ExnThrown> {
    let mut rval = scope.root_value_mut(crate::value::undefined());
    let ok = wrappers2::EvaluateUtf8Path(scope.cx_mut(), options, filename, rval.reborrow());
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Compile a UTF-8 script string into a `JSScript` without executing it.
///
/// Uses `"<inline>"` as the filename. For a custom filename, use
/// [`compile_with_filename`].
pub fn compile<'s>(
    scope: &'s Scope<'_>,
    script: &str,
) -> Result<Handle<'s, *mut JSScript>, ExnThrown> {
    compile_with_filename(scope, script, "<inline>", 1)
}

/// Compile a UTF-8 script string with a custom filename and starting line.
pub fn compile_with_filename<'s>(
    scope: &'s Scope<'_>,
    script: &str,
    filename: &str,
    lineno: u32,
) -> Result<Handle<'s, *mut JSScript>, ExnThrown> {
    let options = options(scope, filename_or_unknown(filename), lineno);
    let mut source = transform_str_to_source_text(script);
    // SAFETY: the options and the source are valid.
    unsafe { compile_raw(scope, options.ptr, &mut source) }
}

/// Compile a UTF-16 script into a `JSScript` without executing it.
///
/// # Safety
///
/// `options` and `src_buf` must be valid pointers.
pub unsafe fn compile_utf16_raw<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<u16>,
) -> Result<Handle<'s, *mut JSScript>, ExnThrown> {
    let script = wrappers2::Compile(scope.cx_mut(), options, src_buf);
    NonNull::new(script)
        .map(|p| scope.root_script(p))
        .ok_or(ExnThrown)
}

/// Compile a UTF-8 script into a `JSScript` without executing it.
///
/// # Safety
///
/// `options` and `src_buf` must be valid pointers.
pub unsafe fn compile_raw<'s>(
    scope: &'s Scope<'_>,
    options: *const ReadOnlyCompileOptions,
    src_buf: *mut SourceText<Utf8Unit>,
) -> Result<Handle<'s, *mut JSScript>, ExnThrown> {
    let script = wrappers2::Compile1(scope.cx_mut(), options, src_buf);
    NonNull::new(script)
        .map(|p| scope.root_script(p))
        .ok_or(ExnThrown)
}

/// Execute a previously compiled script, returning its completion value.
pub fn execute_script<'s>(
    scope: &'s Scope<'_>,
    script: HandleScript,
) -> Result<HandleValue<'s>, ExnThrown> {
    // SAFETY: scope guarantees a realm is entered; script handle is rooted.
    let mut rval = scope.root_value_mut(crate::value::undefined());
    let ok = unsafe { wrappers2::JS_ExecuteScript(scope.cx_mut(), script, rval.reborrow()) };
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Execute a previously compiled script, discarding the result.
pub fn execute_script_no_rval(scope: &Scope<'_>, script: HandleScript) -> Result<(), ExnThrown> {
    let ok = unsafe { wrappers2::JS_ExecuteScript1(scope.cx_mut(), script) };
    ExnThrown::check(ok)
}

/// Compile a function from UTF-16 source.
///
/// # Safety
///
/// `env_chain`, `options`, `name`, `argnames`, and `src_buf` must be valid.
pub unsafe fn compile_function_utf16_raw<'s>(
    scope: &'s Scope<'_>,
    env_chain: *const EnvironmentChain,
    options: *const ReadOnlyCompileOptions,
    name: *const std::os::raw::c_char,
    nargs: std::os::raw::c_uint,
    argnames: *const *const std::os::raw::c_char,
    src_buf: *mut SourceText<u16>,
) -> Result<Handle<'s, *mut JSFunction>, ExnThrown> {
    let fun = wrappers2::CompileFunction(
        scope.cx_mut(),
        env_chain,
        options,
        name,
        nargs,
        argnames,
        src_buf,
    );
    NonNull::new(fun)
        .map(|p| scope.root_function(p))
        .ok_or(ExnThrown)
}

/// Compile a function from UTF-8 source.
///
/// # Safety
///
/// `env_chain`, `options`, `name`, `argnames`, and `src_buf` must be valid.
pub unsafe fn compile_function_raw<'s>(
    scope: &'s Scope<'_>,
    env_chain: *const EnvironmentChain,
    options: *const ReadOnlyCompileOptions,
    name: *const std::os::raw::c_char,
    nargs: std::os::raw::c_uint,
    argnames: *const *const std::os::raw::c_char,
    src_buf: *mut SourceText<Utf8Unit>,
) -> Result<Handle<'s, *mut JSFunction>, ExnThrown> {
    let fun = wrappers2::CompileFunction1(
        scope.cx_mut(),
        env_chain,
        options,
        name,
        nargs,
        argnames,
        src_buf,
    );
    NonNull::new(fun)
        .map(|p| scope.root_function(p))
        .ok_or(ExnThrown)
}

/// Decompile a script to source text.
pub fn decompile_script<'s>(
    scope: &'s Scope<'_>,
    script: HandleScript,
) -> Result<Handle<'s, *mut mozjs::jsapi::JSString>, ExnThrown> {
    let s = unsafe { wrappers2::JS_DecompileScript(scope.cx_mut(), script) };
    NonNull::new(s)
        .map(|p| scope.root_string(p))
        .ok_or(ExnThrown)
}

/// Decompile a function to source text.
pub fn decompile_function<'s>(
    scope: &'s Scope<'_>,
    fun: HandleFunction,
) -> Result<Handle<'s, *mut mozjs::jsapi::JSString>, ExnThrown> {
    let s = unsafe { wrappers2::JS_DecompileFunction(scope.cx_mut(), fun) };
    NonNull::new(s)
        .map(|p| scope.root_string(p))
        .ok_or(ExnThrown)
}

/// Check whether a UTF-8 string is a complete JavaScript compilation unit.
///
/// Returns `true` if the string could be submitted for compilation (even if
/// it would produce errors), `false` if it is incomplete (e.g. missing closing
/// braces).
pub fn is_compilable_unit(scope: &Scope<'_>, source: &str) -> bool {
    // Copy the global pointer before taking the mutable borrow on cx.
    let global = scope.global();
    unsafe {
        wrappers2::JS_Utf8BufferIsCompilableUnit(
            scope.cx_mut(),
            global.handle(),
            source.as_ptr() as *const std::os::raw::c_char,
            source.len(),
        )
    }
}
