use mozjs::jsapi::{JSContext, JSObject};
use mozjs::rust::HandleObject;

unsafe extern "C" {
    fn console_install(cx: *mut JSContext, global: *mut JSObject) -> bool;
}

/// Install all C++ builtins on the given global object.
///
/// # Safety
/// `cx` and `global` must be valid SpiderMonkey context/global pointers
/// with an active realm.
pub unsafe fn install(cx: *mut JSContext, global: HandleObject) -> bool {
    unsafe { console_install(cx, global.get()) }
}
