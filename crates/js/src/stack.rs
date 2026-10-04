// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Stack capture and saved frame inspection.
//!
//! This module wraps SpiderMonkey's stack capture API and the `SavedFrame`
//! introspection functions for walking captured stack traces.
//!
//! # Usage
//!
//! Use [`capture_current_stack`] to capture the current JS call stack into a
//! `SavedFrame` object. Then use [`build_stack_string`] to convert it to a
//! human-readable string, or use the individual `get_saved_frame_*` functions
//! to inspect each frame.
//!
//! Most functions accept an optional `*mut JSPrincipals` for security-filtered
//! access. Pass `std::ptr::null_mut()` for unprivileged access.

use crate::gc::scope::{RootScope, Scope};
use mozjs::gc::HandleObject;
use mozjs::jsapi::{
    JSContext as RawJSContext, JSObject, JSPrincipals, JSString, SavedFrameSelfHosted,
    StackCapture, StackFormat,
};
use mozjs::rust::wrappers2;
use mozjs::rust::{MutableHandleObject, MutableHandleString};
use std::cell::Cell;
use std::rc::Rc;

use super::error::ExnThrown;

crate::instance_local! {
    /// The context [`current_origin`] captures stacks on, and null while origins are not being
    /// recorded.
    static ORIGIN_CX: Cell<*mut RawJSContext> = const { Cell::new(std::ptr::null_mut()) };
}

/// Record where asynchronous work is created from now on, capturing stacks on `cx`, or stop
/// recording for `None`. While recording, [`current_origin`] returns the current JS call stack.
///
/// # Safety
///
/// `cx` must stay a valid context until recording is stopped.
pub unsafe fn record_origins(cx: Option<*mut RawJSContext>) {
    ORIGIN_CX.with(|cell| cell.set(cx.unwrap_or(std::ptr::null_mut())));
}

/// The current JS call stack, formatted as `Error.prototype.stack` formats one, for describing
/// where asynchronous work being created comes from.
///
/// Returns `None` while [`record_origins`] is not recording, when no realm is entered or no
/// script is running, and when capturing the stack fails. A failed capture's exception is
/// cleared.
pub fn current_origin() -> Option<Rc<str>> {
    // The most frames an origin holds, counted from the call that created the work.
    const MAX_FRAMES: u32 = 16;

    let cx = ORIGIN_CX.with(Cell::get);
    // SAFETY: `record_origins`' caller keeps a non-null `cx` valid while recording.
    if cx.is_null() || unsafe { mozjs::jsapi::GetCurrentRealmOrNull(cx) }.is_null() {
        return None;
    }
    // SAFETY: `cx` is valid, as above, and has a realm entered.
    let scope = unsafe { RootScope::from_current_realm(cx) };
    let frame = match capture_saved_frame(&scope, MAX_FRAMES) {
        Ok(frame) => frame?,
        Err(ExnThrown) => {
            crate::exception::clear(&scope);
            return None;
        }
    };
    mozjs::rooted!(in(cx) let mut formatted = std::ptr::null_mut::<JSString>());
    // SAFETY: null principals request unfiltered frames, and `frame` is a rooted `SavedFrame`.
    let built = unsafe {
        build_stack_string(
            &scope,
            std::ptr::null_mut(),
            frame.handle(),
            formatted.handle_mut(),
            0,
            StackFormat::Default,
        )
    };
    if built.is_err() {
        crate::exception::clear(&scope);
        return None;
    }
    let formatted = std::ptr::NonNull::new(formatted.get())?;
    let text = crate::conversion::jsstr_to_string(&scope, formatted);
    let text = text.trim_end();
    (!text.is_empty()).then(|| Rc::from(text))
}

/// Capture the current JavaScript call stack.
///
/// # Safety
///
/// `capture` must be a valid pointer to a `StackCapture` (an opaque
/// SpiderMonkey type that cannot be constructed from safe Rust).
// TODO: make `capture` NonNull
pub unsafe fn capture_current_stack(
    scope: &Scope<'_>,
    stackp: MutableHandleObject,
    capture: *mut StackCapture,
    start_after: HandleObject,
) -> Result<(), ExnThrown> {
    let ok = wrappers2::CaptureCurrentStack(scope.cx_mut(), stackp, capture, start_after);
    ExnThrown::check(ok)
}

/// Capture up to `max_frames` frames of the current JavaScript call stack as a `SavedFrame`
/// object. Returns `None` if no script is running.
pub fn capture_saved_frame<'s>(
    scope: &'s Scope<'_>,
    max_frames: u32,
) -> Result<Option<crate::Object<'s>>, ExnThrown> {
    let mut capture = std::mem::MaybeUninit::<StackCapture>::uninit();
    // SAFETY: the glue function initializes `capture`, which holds no pointers into itself and
    // so can be moved afterwards.
    let mut capture = unsafe {
        mozjs::jsapi::JS_StackCapture_MaxFrames(max_frames, capture.as_mut_ptr());
        capture.assume_init()
    };
    // SAFETY: the pointer is only used to root `frame` in this frame.
    let raw = unsafe { scope.cx_mut().raw_cx() };
    mozjs::rooted!(in(raw) let mut frame = std::ptr::null_mut::<JSObject>());
    // SAFETY: `capture` was initialized above.
    unsafe {
        capture_current_stack(
            scope,
            frame.handle_mut(),
            &mut capture,
            HandleObject::null(),
        )?
    };
    // SAFETY: `frame` is null or a live `SavedFrame`, which `from_raw` roots in `scope`.
    Ok(unsafe { crate::Object::from_raw(scope, frame.get()) })
}

/// The source URL of the innermost frame of the current JS stack, or `None` if no script is
/// running.
pub fn current_frame_source(scope: &Scope<'_>) -> Result<Option<String>, ExnThrown> {
    let Some(frame) = capture_saved_frame(scope, 1)? else {
        return Ok(None);
    };
    // SAFETY: the pointer is only used to root `source` in this frame.
    let raw = unsafe { scope.cx_mut().raw_cx() };
    mozjs::rooted!(in(raw) let mut source = std::ptr::null_mut::<mozjs::jsapi::JSString>());
    // SAFETY: `frame` is a rooted `SavedFrame`, and `source` a rooted location.
    unsafe {
        get_saved_frame_source(
            scope,
            std::ptr::null_mut(),
            frame.handle(),
            source.handle_mut(),
            mozjs::jsapi::SavedFrameSelfHosted::Exclude,
        )
    };
    match std::ptr::NonNull::new(source.get()) {
        Some(source) => Ok(Some(crate::conversion::jsstr_to_string(scope, source))),
        None => Ok(None),
    }
}

/// Build a string representation of a stack trace.
///
/// `principals` controls security-filtered access — pass `std::ptr::null_mut()`
/// for unprivileged access.
///
/// # Safety
///
/// `principals`, if non-null, must be a valid pointer to `JSPrincipals`.
// TODO: change `principals` to `Option<NonNull<JSPrincipals>>` to enforce this at the type level. Here and everywhere else! DO NOT REMOVE THIS TODO WITHOUT ADDRESSING
pub unsafe fn build_stack_string(
    scope: &Scope<'_>,
    principals: *mut JSPrincipals,
    stack: HandleObject,
    stringp: MutableHandleString,
    indent: usize,
    stack_format: StackFormat,
) -> Result<(), ExnThrown> {
    let ok = wrappers2::BuildStackString(
        scope.cx_mut(),
        principals,
        stack,
        stringp,
        indent,
        stack_format,
    );
    ExnThrown::check(ok)
}

/// Get the source URL from a `SavedFrame`.
///
/// # Safety
///
/// `principals`, if non-null, must be a valid pointer. `saved_frame` must
/// be a valid `SavedFrame` object.
pub unsafe fn get_saved_frame_source(
    scope: &Scope<'_>,
    principals: *mut JSPrincipals,
    saved_frame: HandleObject,
    sourcep: MutableHandleString,
    self_hosted: SavedFrameSelfHosted,
) -> mozjs::jsapi::SavedFrameResult {
    wrappers2::GetSavedFrameSource(
        scope.cx_mut(),
        principals,
        saved_frame,
        sourcep,
        self_hosted,
    )
}

/// Get the line number from a `SavedFrame`.
///
/// # Safety
///
/// `principals`, if non-null, must be a valid pointer. `saved_frame` must
/// be a valid `SavedFrame` object.
pub unsafe fn get_saved_frame_line(
    scope: &Scope<'_>,
    principals: *mut JSPrincipals,
    saved_frame: HandleObject,
    linep: &mut u32,
    self_hosted: SavedFrameSelfHosted,
) -> mozjs::jsapi::SavedFrameResult {
    wrappers2::GetSavedFrameLine(scope.cx_mut(), principals, saved_frame, linep, self_hosted)
}

/// Get the function display name from a `SavedFrame`.
///
/// # Safety
///
/// `principals`, if non-null, must be a valid pointer. `saved_frame` must
/// be a valid `SavedFrame` object.
pub unsafe fn get_saved_frame_function_display_name(
    scope: &Scope<'_>,
    principals: *mut JSPrincipals,
    saved_frame: HandleObject,
    namep: MutableHandleString,
    self_hosted: SavedFrameSelfHosted,
) -> mozjs::jsapi::SavedFrameResult {
    wrappers2::GetSavedFrameFunctionDisplayName(
        scope.cx_mut(),
        principals,
        saved_frame,
        namep,
        self_hosted,
    )
}

/// Get the parent `SavedFrame` (caller).
///
/// # Safety
///
/// `principals`, if non-null, must be a valid pointer. `saved_frame` must
/// be a valid `SavedFrame` object.
pub unsafe fn get_saved_frame_parent(
    scope: &Scope<'_>,
    principals: *mut JSPrincipals,
    saved_frame: HandleObject,
    parentp: MutableHandleObject,
    self_hosted: SavedFrameSelfHosted,
) -> mozjs::jsapi::SavedFrameResult {
    wrappers2::GetSavedFrameParent(
        scope.cx_mut(),
        principals,
        saved_frame,
        parentp,
        self_hosted,
    )
}

/// Convert a `SavedFrame` to a plain object.
///
/// The returned pointer must be immediately rooted.
///
/// # Safety
///
/// `saved_frame` must be a valid `SavedFrame` object.
pub unsafe fn convert_to_plain_object(
    scope: &Scope<'_>,
    saved_frame: HandleObject,
    self_hosted: SavedFrameSelfHosted,
) -> *mut JSObject {
    wrappers2::ConvertSavedFrameToPlainObject(scope.cx_mut(), saved_frame, self_hosted)
}

/// Set the stack format (SpiderMonkey or V8-style).
pub fn set_stack_format(scope: &Scope<'_>, format: StackFormat) {
    unsafe { wrappers2::SetStackFormat(scope.cx(), format) }
}

/// Get the current stack format.
pub fn get_stack_format(scope: &Scope<'_>) -> StackFormat {
    unsafe { wrappers2::GetStackFormat(scope.cx()) }
}
