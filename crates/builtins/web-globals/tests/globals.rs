// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for the globals that have no test file of their own: `location`,
//! `DOMException` and the errors builtins throw, and `performance`.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

mod worker_location {
    use core_runtime::{runtime, test_util::eval_with_setup};
    use web_globals::worker_location::{add_to_global, clear_init_location, set_init_location};

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                clear_init_location();
                runtime::register_global_initializer(add_to_global);
            },
            code,
        )
    }

    fn eval_with_url(location_url: &str, code: &str) -> String {
        eval_with_setup(
            || {
                set_init_location(url::Url::parse(location_url).unwrap());
                runtime::register_global_initializer(add_to_global);
            },
            code,
        )
    }

    #[test]
    fn accessors_serialize_the_url() {
        assert_eq!(
            eval_with_url(
                "https://example.com:8443/foo/bar?x=1#frag",
                "[location.href, location.origin, location.protocol, location.host,
                  location.hostname, location.port, location.pathname, location.search,
                  location.hash, location.toString() === location.href,
                  Object.prototype.toString.call(location)].join(' ')"
            ),
            "https://example.com:8443/foo/bar?x=1#frag https://example.com:8443 https: \
             example.com:8443 example.com 8443 /foo/bar ?x=1 #frag true [object Location]"
        );
    }

    #[test]
    fn absent_port_search_and_hash_are_empty_strings() {
        assert_eq!(
            eval_with_url(
                "https://example.com/",
                "[location.port, location.search, location.hash].join(',')"
            ),
            ",,"
        );
    }

    #[test]
    fn constructor_throws() {
        // WorkerLocation has no IDL constructor; `new WorkerLocation()` must throw.
        assert_eq!(
            eval(
                "try { new WorkerLocation(); 'no-throw' } \
                 catch(e) { (e instanceof TypeError) + ',' + e.message }"
            ),
            "true,Illegal constructor"
        );
    }

    #[test]
    fn unset_location_throws_typeerror() {
        // With no init-location configured, every accessor throws.
        assert_eq!(
            eval(
                "try { location.href; 'no-throw' } \
                 catch(e) { (e instanceof TypeError) + ',' + e.message.includes('--init-location') }"
            ),
            "true,true"
        );
        assert_eq!(
            eval(
                "try { location.toString(); 'no-throw' } \
                 catch(e) { e instanceof TypeError }"
            ),
            "true"
        );
    }
}

mod dom_exception {
    use core_runtime::{
        config::RuntimeConfig,
        runtime::{self, Runtime},
    };
    use web_globals::dom_exception::DOMException;

    #[test]
    fn rust_new() {
        runtime::register_global_initializer(DOMException::add_to_global);
        let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = rt.default_global();
        let e = DOMException::new(&scope, String::new(), "Error".into()).unwrap();
        assert_eq!(&*e.name(), "Error");
        assert_eq!(&*e.message(), "");
        assert_eq!(e.code(), 0);
    }
}

/// Tests for typed error dispatch via `ThrowException` trait.
///
/// These tests use `#[jsglobals]` to register functions that return
/// `Result<T, TypeError>`, `Result<T, RangeError>`, `Result<T, SyntaxError>`,
/// `Result<T, DOMExceptionError>`, and `Result<T, String>`.
mod throw_exception {
    use core_runtime::{jsglobals, test_util::eval_with_setup};
    use js::error::{RangeError, SyntaxError, TypeError};
    use web_globals::dom_exception::DOMExceptionError;

    #[jsglobals]
    mod test_error_globals {
        use super::*;

        /// Throws a TypeError.
        pub fn throw_type_error() -> Result<(), TypeError> {
            Err(TypeError("test type error".into()))
        }

        /// Throws a RangeError.
        pub fn throw_range_error() -> Result<(), RangeError> {
            Err(RangeError("test range error".into()))
        }

        /// Throws a SyntaxError.
        pub fn throw_syntax_error() -> Result<(), SyntaxError> {
            Err(SyntaxError("test syntax error".into()))
        }

        /// Throws a TypeError from a String error.
        pub fn throw_string_error() -> Result<(), String> {
            Err("test string error".into())
        }

        /// Throws a DOMExceptionError.
        pub fn throw_dom_exception_error() -> Result<(), DOMExceptionError> {
            Err(DOMExceptionError::new(
                "NotFoundError",
                "test dom exception",
            ))
        }
    }

    fn eval(code: &str) -> String {
        eval_with_setup(
            || {
                core_runtime::runtime::register_global_initializer(
                    web_globals::dom_exception::DOMException::add_to_global,
                );
                core_runtime::runtime::register_global_initializer(
                    test_error_globals::add_to_global,
                );
            },
            code,
        )
    }

    /// Capturing the stack for a new `DOMException` is best-effort: a throwing
    /// `Error.prototype.stack` accessor leaves the construction intact and no
    /// exception pending.
    #[test]
    fn construction_survives_a_throwing_stack_accessor() {
        assert_eq!(
            eval(
                "Object.defineProperty(Error.prototype, 'stack', { get() { throw 1; }, configurable: true });
                 const e = new DOMException('m', 'NotFoundError');
                 e.name + ',' + e.message"
            ),
            "NotFoundError,m"
        );
    }

    #[test]
    fn type_error_thrown() {
        assert_eq!(
            eval(
                "try { throwTypeError(); 'no' } catch(e) { (e instanceof TypeError) + ',' + e.message }"
            ),
            "true,test type error"
        );
    }

    #[test]
    fn range_error_thrown() {
        assert_eq!(
            eval(
                "try { throwRangeError(); 'no' } catch(e) { (e instanceof RangeError) + ',' + e.message }"
            ),
            "true,test range error"
        );
    }

    #[test]
    fn syntax_error_thrown() {
        assert_eq!(
            eval(
                "try { throwSyntaxError(); 'no' } catch(e) { (e instanceof SyntaxError) + ',' + e.message }"
            ),
            "true,test syntax error"
        );
    }

    #[test]
    fn string_error_throws_type_error() {
        // String errors throw TypeError.
        assert_eq!(
            eval(
                "try { throwStringError(); 'no' } catch(e) { (e instanceof TypeError) + ',' + e.message }"
            ),
            "true,test string error"
        );
    }

    #[test]
    fn dom_exception_error_thrown() {
        assert_eq!(
            eval(
                "try { throwDomExceptionError(); 'no' } catch(e) { \
                    (e instanceof DOMException) + ',' + e.name + ',' + e.message }"
            ),
            "true,NotFoundError,test dom exception"
        );
    }
}

mod performance {
    use core_runtime::{runtime, test_util::eval_with_setup};

    /// Whether 20 readings each of `performance.now()` and `new Event('x').timeStamp` are all
    /// multiples of 100 microseconds, as `"{now},{timeStamp}"`, with the web security
    /// restrictions set to `restricted`.
    fn readings_are_coarsened(restricted: bool) -> String {
        eval_with_setup(
            || match restricted {
                true => runtime::register_global_initializer(|scope, global| {
                    core_runtime::config::set_web_security_restrictions(true);
                    web_globals::add_to_global(scope, global);
                }),
                false => runtime::register_global_initializer(|scope, global| {
                    core_runtime::config::set_web_security_restrictions(false);
                    web_globals::add_to_global(scope, global);
                }),
            },
            "(() => {
                const coarsened = (read) => {
                    const times = [];
                    for (let i = 0; i < 20; i++) {
                        times.push(read());
                    }
                    return times.every((t) => Math.abs(t * 10 - Math.round(t * 10)) < 1e-6);
                };
                return [
                    coarsened(() => performance.now()),
                    coarsened(() => new Event('x').timeStamp),
                ].join(',');
            })()",
        )
    }

    #[test]
    fn clocks_are_coarsened_under_web_security_restrictions() {
        assert_eq!(readings_are_coarsened(true), "true,true");
    }

    /// Only `performance.now()` is checked: the coarse clock behind `Event.timeStamp` can lag the
    /// time origin, and a reading that lags it is zero, which is a multiple of 100 microseconds.
    #[test]
    fn performance_now_is_precise_without_web_security_restrictions() {
        assert!(readings_are_coarsened(false).starts_with("false,"));
    }
}
