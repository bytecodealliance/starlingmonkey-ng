use mozjs::jsapi::{JSContext, JSObject};
use mozjs::rust::HandleObject;

unsafe extern "C" {
    fn console_install(cx: *mut JSContext, global: *mut JSObject) -> bool;
    fn starling_set_console_sink(sink: Option<ConsoleSink>);
}

/// Receives each console message in place of stdout and stderr. `level` follows the console's
/// `LogType`: 0 `log`, 1 `info`, 2 `debug`, 3 `warn`, 4 `error`. `message` is the formatted
/// message without a level prefix. It is NUL-terminated and valid only for the duration of the
/// call.
pub type ConsoleSink = unsafe extern "C" fn(level: u8, message: *const std::ffi::c_char);

/// Route console messages to `sink`, or back to stdout and stderr with `None`. The sink is
/// process-wide.
pub fn set_console_sink(sink: Option<ConsoleSink>) {
    // SAFETY: the C++ side only stores the pointer.
    unsafe { starling_set_console_sink(sink) }
}

/// Install all C++ builtins on the given global object.
///
/// # Safety
/// `cx` and `global` must be valid SpiderMonkey context/global pointers
/// with an active realm.
pub unsafe fn install(cx: *mut JSContext, global: HandleObject) -> bool {
    unsafe { console_install(cx, global.get()) }
}

#[cfg(test)]
mod tests {
    mod console_integration {
        use core_runtime::test_util::throws_with_setup;

        fn eval(code: &str) -> bool {
            libstarling::register_builtins();
            let rt =
                libstarling::runtime::Runtime::init(&libstarling::config::RuntimeConfig::default())
                    .expect("runtime init");
            let scope = rt.default_global();
            let rval = js::compile::evaluate_with_filename(&scope, code, "test.js", 1)
                .expect("eval failed");
            rval.is_undefined() || rval.to_boolean()
        }

        fn eval_throws(code: &str) -> bool {
            throws_with_setup(libstarling::register_builtins, code)
        }

        #[test]
        fn console_log_exists() {
            assert!(eval("typeof console === 'object'"));
        }

        #[test]
        fn console_methods_exist() {
            assert!(eval("typeof console.log === 'function'"));
            assert!(eval("typeof console.warn === 'function'"));
            assert!(eval("typeof console.error === 'function'"));
            assert!(eval("typeof console.info === 'function'"));
            assert!(eval("typeof console.debug === 'function'"));
        }

        #[test]
        fn console_log_returns_undefined() {
            assert!(eval("console.log('test') === undefined"));
        }

        #[test]
        fn console_log_no_args() {
            // Should not throw with zero arguments.
            assert!(!eval_throws("console.log()"));
        }

        #[test]
        fn console_log_multiple_args() {
            assert!(!eval_throws("console.log('a', 'b', 'c')"));
        }

        #[test]
        fn console_log_non_string_args() {
            assert!(!eval_throws(
                "console.log(42, true, null, undefined, {x: 1})"
            ));
        }

        thread_local! {
            static SUNK: std::cell::RefCell<Vec<(u8, String)>> =
                const { std::cell::RefCell::new(Vec::new()) };
        }

        unsafe extern "C" fn record(level: u8, message: *const std::ffi::c_char) {
            // SAFETY: the console passes a NUL-terminated string valid for this call.
            let message = unsafe { std::ffi::CStr::from_ptr(message) }
                .to_string_lossy()
                .into_owned();
            SUNK.with(|sunk| sunk.borrow_mut().push((level, message)));
        }

        #[test]
        fn console_sink_receives_messages_instead_of_the_streams() {
            crate::set_console_sink(Some(record));
            let evaluated = eval("console.log('a', 1); console.error('b'); true");
            crate::set_console_sink(None);
            assert!(evaluated);
            SUNK.with(|sunk| {
                assert_eq!(
                    *sunk.borrow(),
                    [(0, "a 1".to_string()), (4, "b".to_string())]
                )
            });
        }
    }
}
