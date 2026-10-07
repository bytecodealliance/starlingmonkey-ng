// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Structured clone read and write.
//!
//! Structured cloning serializes and deserializes JavaScript values in a
//! format that preserves object graphs, typed arrays, and other complex types.
//! This is the mechanism behind `postMessage`, `structuredClone`, and
//! `IndexedDB` serialization.

use crate::gc::scope::Scope;
use mozjs::jsapi::{
    CloneDataPolicy, JSStructuredCloneCallbacks, JSStructuredCloneData, StructuredCloneScope,
};
use mozjs::jsval::UndefinedValue;
use mozjs::rust::wrappers2;
use mozjs::rust::HandleValue;

use super::error::ExnThrown;
use crate::Object;

/// Perform a structured clone of a value (serialize + deserialize in one step).
///
/// # Safety
///
/// `callbacks` must be a valid pointer (or null). `closure` is passed through.
pub unsafe fn clone<'r>(
    scope: &'r Scope<'_>,
    value: HandleValue,
    callbacks: *const JSStructuredCloneCallbacks,
    closure: *mut std::os::raw::c_void,
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut rval = scope.root_value_mut(UndefinedValue());
    let ok =
        wrappers2::JS_StructuredClone(scope.cx_mut(), value, rval.reborrow(), callbacks, closure);
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// Write a value into structured clone data.
///
/// # Safety
///
/// `data`, `policy`, `callbacks` must be valid pointers (or null for callbacks).
/// `closure` is passed through to callbacks.
pub unsafe fn write(
    scope: &Scope<'_>,
    value: HandleValue,
    data: *mut JSStructuredCloneData,
    clone_scope: StructuredCloneScope,
    policy: *const CloneDataPolicy,
    callbacks: *const JSStructuredCloneCallbacks,
    closure: *mut std::os::raw::c_void,
    transferable: HandleValue,
) -> Result<(), ExnThrown> {
    let ok = wrappers2::JS_WriteStructuredClone(
        scope.cx_mut(),
        value,
        data,
        clone_scope,
        policy,
        callbacks,
        closure,
        transferable,
    );
    ExnThrown::check(ok)
}

/// Read a value from structured clone data.
///
/// # Safety
///
/// `data` and `policy` must be valid pointers. `callbacks` may be null.
/// `closure` is passed through to callbacks.
pub unsafe fn read<'r>(
    scope: &'r Scope<'_>,
    data: *const JSStructuredCloneData,
    version: u32,
    clone_scope: StructuredCloneScope,
    policy: *const CloneDataPolicy,
    callbacks: *const JSStructuredCloneCallbacks,
    closure: *mut std::os::raw::c_void,
) -> Result<HandleValue<'r>, ExnThrown> {
    let mut rval = scope.root_value_mut(UndefinedValue());
    let ok = wrappers2::JS_ReadStructuredClone(
        scope.cx_mut(),
        data,
        version,
        clone_scope,
        rval.reborrow(),
        policy,
        callbacks,
        closure,
    );
    ExnThrown::check(ok)?;
    Ok(rval.handle())
}

/// The tag of the transferables that a [`HostTransfer`] handles, above `JS_SCTAG_USER_MIN`
/// (0xFFFF8000), where the tags of the embedding start.
const HOST_TRANSFER_TAG: u32 = 0xFFFF_8001;

/// The transferables that the engine doesn't handle itself, such as `MessagePort`s, for
/// [`Serialized::write`] and [`Serialized::read`].
pub trait HostTransfer {
    /// Whether `object`, an object in a transfer list, is a transferable this handles.
    fn can_transfer(&mut self, scope: &Scope<'_>, object: Object<'_>) -> bool;

    /// Transfer `object`, for which [`HostTransfer::can_transfer`] returned true. Returns the
    /// number that [`HostTransfer::receive`] gets for it.
    fn transfer(&mut self, scope: &Scope<'_>, object: Object<'_>) -> Result<u64, ExnThrown>;

    /// The object that the transferable `transfer` returned `index` for becomes.
    fn receive<'s>(&mut self, scope: &'s Scope<'_>, index: u64) -> Result<Object<'s>, ExnThrown>;
}

/// Why [`Serialized::write`] or [`Serialized::read`] failed.
pub enum CloneError {
    /// An exception is pending, such as one a getter threw.
    Thrown(ExnThrown),
    /// The value can't be serialized or deserialized, a `DataCloneError` with this message.
    DataClone(String),
}

/// The state the callbacks of [`HOST_CALLBACKS`] receive as their closure.
struct HostClosure<'a> {
    host: &'a mut dyn HostTransfer,
    /// The message of the first error the engine reported.
    error: Option<String>,
}

/// The data of a serialized value, held until [`Serialized::read`] deserializes it. Dropping it
/// frees the contents of the `ArrayBuffer`s transferred into it.
pub struct Serialized {
    buffer: mozjs::rust::JSAutoStructuredCloneBufferWrapper,
    /// Whether the data transferred objects.
    transfers: bool,
}

impl Serialized {
    /// Serialize `value`, transferring the objects of `transfer`. `host` transfers the
    /// transferables that the engine doesn't.
    ///
    /// <https://html.spec.whatwg.org/multipage/structured-data.html#structuredserializewithtransfer>
    pub fn write(
        scope: &Scope<'_>,
        value: HandleValue,
        transfer: &[Object<'_>],
        host: &mut dyn HostTransfer,
    ) -> Result<Self, CloneError> {
        let transfer_list = if transfer.is_empty() {
            scope.root_value(UndefinedValue())
        } else {
            let array = crate::Array::with_contents(scope, transfer).map_err(CloneError::Thrown)?;
            scope.root_value(array.as_value())
        };
        let clone_scope = StructuredCloneScope::SameProcess;
        // SAFETY: `HOST_CALLBACKS` is a static, so it outlives the buffer.
        let buffer = unsafe {
            mozjs::rust::JSAutoStructuredCloneBufferWrapper::new(clone_scope, &HOST_CALLBACKS)
        };
        let mut closure = HostClosure { host, error: None };
        let policy = clone_policy();
        // SAFETY: `closure` outlives the call, which the callbacks receive it in.
        let ok = unsafe {
            wrappers2::JS_WriteStructuredClone(
                scope.cx_mut(),
                value,
                &raw mut (*buffer.as_raw_ptr()).data_,
                clone_scope,
                &policy,
                &HOST_CALLBACKS,
                (&raw mut closure).cast(),
                transfer_list,
            )
        };
        if ok {
            return Ok(Serialized {
                buffer,
                transfers: !transfer.is_empty(),
            });
        }
        Err(clone_error(scope, closure.error))
    }

    /// Deserialize the data into the current realm. `host` receives the transferables its
    /// [`HostTransfer::transfer`] transferred.
    ///
    /// <https://html.spec.whatwg.org/multipage/structured-data.html#structureddeserializewithtransfer>
    pub fn read<'r>(
        self,
        scope: &'r Scope<'_>,
        host: &mut dyn HostTransfer,
    ) -> Result<HandleValue<'r>, CloneError> {
        self.read_data(scope, host)
    }

    /// Deserialize data that transferred no objects into the current realm, which can be done any
    /// number of times. Fails with a `DataCloneError` for data that transferred objects.
    ///
    /// <https://html.spec.whatwg.org/multipage/structured-data.html#structureddeserialize>
    pub fn read_copy<'r>(&self, scope: &'r Scope<'_>) -> Result<HandleValue<'r>, CloneError> {
        if self.transfers {
            return Err(CloneError::DataClone(
                "Data that transferred objects can only be read once".into(),
            ));
        }
        self.read_data(scope, &mut NoHostTransfer)
    }

    fn read_data<'r>(
        &self,
        scope: &'r Scope<'_>,
        host: &mut dyn HostTransfer,
    ) -> Result<HandleValue<'r>, CloneError> {
        let mut closure = HostClosure { host, error: None };
        let mut rval = scope.root_value_mut(UndefinedValue());
        let policy = clone_policy();
        // SAFETY: the buffer holds the data its write produced, and `closure` outlives the call,
        // which the callbacks receive it in.
        let ok = unsafe {
            let buffer = self.buffer.as_raw_ptr();
            wrappers2::JS_ReadStructuredClone(
                scope.cx_mut(),
                &raw const (*buffer).data_,
                (*buffer).version_,
                StructuredCloneScope::SameProcess,
                rval.reborrow(),
                &policy,
                &HOST_CALLBACKS,
                (&raw mut closure).cast(),
            )
        };
        if ok {
            return Ok(rval.handle());
        }
        Err(clone_error(scope, closure.error))
    }
}

/// The policy of every write and read, which clones a `SharedArrayBuffer` as a new
/// `SharedArrayBuffer` sharing the original's memory. Reading one requires both flags.
fn clone_policy() -> CloneDataPolicy {
    CloneDataPolicy {
        allowIntraClusterClonableSharedObjects_: true,
        allowSharedMemoryObjects_: true,
    }
}

/// A [`HostTransfer`] that handles no transferables.
pub struct NoHostTransfer;

impl HostTransfer for NoHostTransfer {
    fn can_transfer(&mut self, _scope: &Scope<'_>, _object: Object<'_>) -> bool {
        false
    }

    fn transfer(&mut self, _scope: &Scope<'_>, _object: Object<'_>) -> Result<u64, ExnThrown> {
        unreachable!("`can_transfer` accepts nothing")
    }

    fn receive<'s>(&mut self, _scope: &'s Scope<'_>, _index: u64) -> Result<Object<'s>, ExnThrown> {
        Err(ExnThrown)
    }
}

/// The error of a failed write or read: the pending exception, or the error the engine reported.
fn clone_error(scope: &Scope<'_>, reported: Option<String>) -> CloneError {
    if crate::exception::is_pending(scope) {
        return CloneError::Thrown(ExnThrown);
    }
    CloneError::DataClone(reported.unwrap_or_else(|| "The value could not be cloned".into()))
}

/// The callbacks of [`Serialized`], which report errors to their [`HostClosure`] and hand the
/// transferables the engine doesn't handle to its [`HostTransfer`].
static HOST_CALLBACKS: JSStructuredCloneCallbacks = JSStructuredCloneCallbacks {
    read: None,
    write: None,
    reportError: Some(report_error),
    readTransfer: Some(read_transfer),
    writeTransfer: Some(write_transfer),
    freeTransfer: Some(free_transfer),
    canTransfer: Some(can_transfer),
    sabCloned: None,
};

/// # Safety
///
/// `closure` is a `HostClosure`, and `message` is null or a C string.
unsafe extern "C" fn report_error(
    _cx: *mut mozjs::jsapi::JSContext,
    _error_id: u32,
    closure: *mut std::os::raw::c_void,
    message: *const std::os::raw::c_char,
) {
    let closure = &mut *closure.cast::<HostClosure<'_>>();
    if closure.error.is_none() && !message.is_null() {
        let message = std::ffi::CStr::from_ptr(message).to_string_lossy();
        closure.error = Some(message.into_owned());
    }
}

/// # Safety
///
/// `cx` has a realm entered, `obj` is a rooted object, and `closure` is a `HostClosure`.
unsafe extern "C" fn can_transfer(
    cx: *mut mozjs::jsapi::JSContext,
    obj: mozjs::jsapi::JS::HandleObject,
    _same_process_scope_required: *mut bool,
    closure: *mut std::os::raw::c_void,
) -> bool {
    let closure = &mut *closure.cast::<HostClosure<'_>>();
    let scope = crate::gc::scope::RootScope::from_current_realm(cx);
    match Object::from_raw(&scope, obj.get()) {
        Some(object) => closure.host.can_transfer(&scope, object),
        None => false,
    }
}

/// # Safety
///
/// As for [`can_transfer`], and the out-parameters are valid.
unsafe extern "C" fn write_transfer(
    cx: *mut mozjs::jsapi::JSContext,
    obj: mozjs::jsapi::JS::HandleObject,
    closure: *mut std::os::raw::c_void,
    tag: *mut u32,
    ownership: *mut mozjs::jsapi::JS::TransferableOwnership,
    content: *mut *mut std::os::raw::c_void,
    extra_data: *mut u64,
) -> bool {
    let closure = &mut *closure.cast::<HostClosure<'_>>();
    let scope = crate::gc::scope::RootScope::from_current_realm(cx);
    let Some(object) = Object::from_raw(&scope, obj.get()) else {
        return false;
    };
    let Ok(index) = closure.host.transfer(&scope, object) else {
        return false;
    };
    *tag = HOST_TRANSFER_TAG;
    *ownership = mozjs::jsapi::JS::TransferableOwnership::SCTAG_TMO_CUSTOM;
    *content = std::ptr::null_mut();
    *extra_data = index;
    true
}

/// The engine calls this for each transferable of a [`HostTransfer`] in data that is dropped
/// without being read. The transferables hold nothing to free: a `HostTransfer` keeps the state
/// of what it transferred.
unsafe extern "C" fn free_transfer(
    _tag: u32,
    _ownership: mozjs::jsapi::JS::TransferableOwnership,
    _content: *mut std::os::raw::c_void,
    _extra_data: u64,
    _closure: *mut std::os::raw::c_void,
) {
}

/// # Safety
///
/// `cx` has a realm entered, `closure` is a `HostClosure`, and `return_object` is valid.
#[allow(clippy::too_many_arguments)]
unsafe extern "C" fn read_transfer(
    cx: *mut mozjs::jsapi::JSContext,
    _reader: *mut mozjs::jsapi::JSStructuredCloneReader,
    _policy: *const CloneDataPolicy,
    tag: u32,
    _content: *mut std::os::raw::c_void,
    extra_data: u64,
    closure: *mut std::os::raw::c_void,
    return_object: mozjs::jsapi::JS::MutableHandleObject,
) -> bool {
    if tag != HOST_TRANSFER_TAG {
        return false;
    }
    let closure = &mut *closure.cast::<HostClosure<'_>>();
    let scope = crate::gc::scope::RootScope::from_current_realm(cx);
    match closure.host.receive(&scope, extra_data) {
        Ok(object) => {
            return_object.set(object.as_raw());
            true
        }
        Err(ExnThrown) => false,
    }
}
