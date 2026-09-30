// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The wasm build of StarlingMonkey, `starling.wasm`: a component exporting
//! `wasi:cli/run`, `wasi:http/handler`, `wizer-initialize`, and the
//! `starling:componentize/init` interface `starling-componentize` calls. The
//! componentizer links a world's wit-dylib bindings against its core module,
//! whose `wit_dylib_*` exports `wit_bridge::interpreter` provides.

#![cfg(target_arch = "wasm32")]

/// With the `libc-alloc` feature, Rust allocations go through wasi-libc's `malloc`, which
/// SpiderMonkey's `js_malloc` also ends in, so one allocator owns the heap. Alignments above
/// `max_align_t` (16 on wasm32) go through `posix_memalign`.
#[cfg(feature = "libc-alloc")]
mod libc_alloc {
    use std::alloc::{GlobalAlloc, Layout};

    struct LibcAlloc;

    const MAX_ALIGN: usize = 16;

    // SAFETY: every method forwards to the C allocator with a layout it accepts, and
    // `dealloc`/`realloc` only receive pointers `alloc` returned.
    unsafe impl GlobalAlloc for LibcAlloc {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if layout.align() <= MAX_ALIGN {
                return libc::malloc(layout.size()).cast();
            }
            let mut out = std::ptr::null_mut();
            if libc::posix_memalign(&mut out, layout.align(), layout.size()) != 0 {
                return std::ptr::null_mut();
            }
            out.cast()
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if layout.align() <= MAX_ALIGN {
                return libc::calloc(1, layout.size()).cast();
            }
            let out = self.alloc(layout);
            if !out.is_null() {
                std::ptr::write_bytes(out, 0, layout.size());
            }
            out
        }

        unsafe fn dealloc(&self, ptr: *mut u8, _layout: Layout) {
            libc::free(ptr.cast());
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if layout.align() <= MAX_ALIGN {
                return libc::realloc(ptr.cast(), new_size).cast();
            }
            let new_layout = Layout::from_size_align_unchecked(new_size, layout.align());
            let out = self.alloc(new_layout);
            if !out.is_null() {
                std::ptr::copy_nonoverlapping(ptr, out, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
            out
        }
    }

    #[global_allocator]
    static GLOBAL: LibcAlloc = LibcAlloc;
}

/// Runs the static constructors at most once.
///
/// `--wrap=__wasm_call_ctors` (see `build.rs`) redirects every reference to
/// `__wasm_call_ctors` here, so every path that runs the constructors shares
/// one guard. `wit_bindgen::rt::run_ctors_once` guards only its own static, and
/// at least one other path runs them as well: without the wrap, the first
/// `wasi:cli/run` call aborts in SpiderMonkey's `InitializeUptime` with "Must
/// not be called more than once".
#[unsafe(no_mangle)]
pub extern "C" fn __wrap___wasm_call_ctors() {
    static RAN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if RAN.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    unsafe extern "C" {
        fn __real___wasm_call_ctors();
    }
    // SAFETY: `__real___wasm_call_ctors` is the linker's constructor entry point, which takes no
    // arguments. The guard above runs it at most once.
    unsafe { __real___wasm_call_ctors() };
}

/// The wasi-libc process setup the `cdylib` link leaves out.
///
/// The `cdylib` links without the crt, whose `_initialize` would otherwise run
/// the static constructors at instantiation time and then again from the
/// wit-bindgen `run_ctors_once` shim on the first export call. That
/// `_initialize` also calls `__wasi_init_tp`, which fills in the main thread's
/// `pthread_self` record (stack bounds and the thread-local base). This
/// constructor supplies that call instead. It runs first, from
/// `__wasm_call_ctors`, ahead of SpiderMonkey's own static constructors, which
/// have the default priority of 65535.
///
/// Only compiled when the sysroot exports `__wasi_init_tp`. See `build.rs`.
#[cfg(has_wasi_init_tp)]
mod static_init {
    unsafe extern "C" {
        fn __wasi_init_tp();
    }

    extern "C" fn init_tp() {
        // SAFETY: `__wasi_init_tp` is wasi-libc's main-thread setup, which the
        // crt calls exactly here, before any constructor or export runs.
        unsafe { __wasi_init_tp() }
    }

    #[used]
    #[unsafe(link_section = ".init_array.00000")]
    static INIT_TP: extern "C" fn() = init_tp;
}

/// The wasm component exports both `wasi:cli/run` and `wasi:http/handler`, so one build serves
/// HTTP and runs as a command. There is no dispatch between them: the host picks which export to
/// call. `wasmtime run` calls the former, `wasmtime serve` the latter.
///
/// The two differ only in where their configuration comes from. The CLI export is handed argv;
/// the HTTP export is not (`wasmtime serve` passes none), so it reads `STARLINGMONKEY_CONFIG`.
mod wasm_entry {
    use core_runtime::invocation::{InvocationState, OwnedInvocation};
    use libstarling::config::RuntimeConfig;
    use wit_bridge::run::{report_run_outcome, run_main_export, MainModule};

    struct StarlingCli;

    impl wasip3::exports::cli::run::Guest for StarlingCli {
        /// Call the main module's `run` export in an instance whose top level ran before a
        /// snapshot: a componentized command, or a script wizened with `wizer-initialize`.
        /// In any other instance, run the script the arguments name.
        async fn run() -> Result<(), ()> {
            let snapshot = if wit_bridge::init::is_initialized() {
                Some((wit_bridge::init::runtime(), MainModule::Bootstrap))
            } else {
                libstarling::serve_wasm::installed_runtime()
                    .map(|(runtime, raw_cx)| (runtime, MainModule::Entry(raw_cx)))
            };
            let Some((runtime, main)) = snapshot else {
                let config = match RuntimeConfig::from_args(std::env::args()) {
                    Ok(config) => config,
                    Err(e) => {
                        let _ = e.print();
                        return Err(());
                    }
                };
                return libstarling::run(config).await.map_err(|e| {
                    eprintln!("{e}");
                });
            };
            // The snapshot's top level left no work behind, since both kinds of snapshot refuse
            // any, so `run` starts on a fresh loop, after the resume fixups have repaired the
            // clocks.
            libstarling::serve_wasm::fix_up_after_resume();
            let invocation = OwnedInvocation::new(runtime, InvocationState::new());
            report_run_outcome(run_main_export(&invocation, main).await)
        }
    }

    struct StarlingHttp;

    impl wasip3::exports::http::handler::Guest for StarlingHttp {
        async fn handle(
            request: wasip3::http::types::Request,
        ) -> Result<wasip3::http::types::Response, wasip3::http::types::ErrorCode> {
            libstarling::serve_wasm::handle(request).await
        }
    }

    wasip3::cli::command::export!(StarlingCli);
    wasip3::http::service::export!(StarlingHttp);

    /// Pre-initialization entry point for `wasmtime wizer`, which runs it and snapshots the
    /// initialized instance.
    ///
    /// Wizer calls a component-level function export, and a component exports only what its world
    /// declares. A bare `#[export_name]` does not survive componentization, so this declares the
    /// smallest possible world containing just that function. Its fragment merges with the two
    /// `export!`s above into one component exporting all three.
    ///
    /// The export has to survive the snapshot (`--keep-init-func=true`): dropping it, which is
    /// wizer's default, leaves the component referring to a core export that is no longer there,
    /// and the result fails to load. See `scripts/test-wizer.sh`.
    mod wizer {
        wit_bindgen::generate!({
            inline: r#"
                package local:wizer;
                world wizer {
                    export wizer-initialize: async func();
                }
            "#,
            world: "wizer",
        });

        struct Init;

        impl Guest for Init {
            async fn wizer_initialize() {
                // A failure here would otherwise be baked into the snapshot as a runtime that
                // silently starts from scratch on every request.
                if let Err(e) = libstarling::serve_wasm::pre_initialize().await {
                    panic!("pre-initialization failed: {e}");
                }
                // Last, so that the clock reading it records is past every timestamp the
                // snapshot holds.
                libstarling::serve_wasm::prepare_for_snapshot();
            }
        }

        export!(Init);
    }
}

/// The `starling:componentize/init` export, which `starling/wit/init.wit`
/// declares.
///
/// `starling-componentize` links this runtime together with the user's wit-dylib
/// bindings and calls `init` once, under Wizer, to bootstrap the SpiderMonkey
/// runtime and evaluate the user's modules. The heap is then wizened, and
/// every subsequent component call runs against that runtime, through the
/// `wit_bridge::interpreter` glue this crate pulls in so its `wit_dylib_*`
/// exports land in the export table.
///
/// The generated `run_ctors_once` shim runs the static constructors before the
/// body, so SpiderMonkey is safe to use here.
mod init_export {
    /// A `Request` for `value` if it is a wrapper of a `wasi:http/types`
    /// `request`, whose handle the `Request` takes. See
    /// [`web_fetch::request::register_request_input_adapter`].
    fn request_input<'s>(
        scope: &'s js::gc::scope::Scope<'_>,
        value: js::prelude::HandleValue<'_>,
    ) -> Result<Option<web_fetch::request::Request<'s>>, js::error::ExnThrown> {
        let handle = wit_bridge::interpreter::take_imported_handle(
            scope,
            value,
            libstarling::serve_wasm::WASI_HTTP_TYPES,
            "request",
        )?;
        handle
            .map(|handle| {
                // SAFETY: `take_imported_handle` took `handle`, a live `request`
                // handle, out of its wrapper, which no longer owns it.
                unsafe { libstarling::serve_wasm::request_from_handle(scope, handle) }
            })
            .transpose()
    }

    mod bindings {
        wit_bindgen::generate!({
            world: "runtime",
            path: "wit/init.wit",
            generate_all,
        });

        use super::Init;

        export!(Init);
    }

    use core_runtime::invocation::OwnedInvocation;
    use libstarling::serve_wasm::RawHttpHandler;

    struct Init;

    impl bindings::exports::starling::componentize::init::Guest for Init {
        /// Bootstrap the runtime and evaluate the application's modules, as
        /// `init` in `wit/init.wit` describes.
        async fn init(
            main: String,
            main_name: String,
            main_path: Option<String>,
            cli: bool,
            raw_http_handler: Option<String>,
            init_location: Option<String>,
        ) -> Result<bool, String> {
            // Registered before `initialize_runtime` calls `Runtime::init`, which
            // fires every global initializer on the default global.
            libstarling::register_builtins();
            libstarling::runtime::register_global_initializer(wit_bridge::add_to_global);
            // A `Response` lowers to an owned `wasi:http/types` `response`, so an
            // application's own `wasi:http/handler` can return one.
            wit_bridge::resources::register_own_adapter(
                libstarling::serve_wasm::WASI_HTTP_TYPES,
                "response",
                libstarling::serve_wasm::response_handle,
                libstarling::serve_wasm::response_accepts,
            );
            // `fetch` takes a `wasi:http/types` `request`, such as the one an
            // application's own `wasi:http/handler` receives.
            web_fetch::request::register_request_input_adapter(request_input);

            let config = core_runtime::config::RuntimeConfig {
                pre_initialize: true,
                init_location,
                ..Default::default()
            };
            libstarling::apply_pre_init_config(&config)?;
            // The host's imports are not linked while the snapshot is taken.
            wit_bridge::imports::set_imports_unavailable(true);
            wit_bridge::init::set_before_export(libstarling::serve_wasm::fix_up_after_resume);
            let initialized = wit_bridge::initialize_runtime(
                &config,
                &main,
                &main_name,
                main_path.as_deref(),
                wit_bridge::interpreter::synthesize,
            )?;
            let invocation =
                OwnedInvocation::new(wit_bridge::init::runtime(), initialized.invocation);
            let raw_cx = wit_bridge::init::raw_cx();

            // A top-level `await` settles only once the loop runs the timers and
            // I/O it waits on, and the application's exports are in their
            // temporal dead zone until it does. We drive the loop only while the
            // top level is unfinished, so work it merely queued, such as a timer,
            // is refused below rather than run under Wizer.
            if !wit_bridge::init::with_scope(|scope| initialized.evaluation.is_finished(scope)) {
                // SAFETY: `initialize_runtime` left the default global's realm
                // entered for the process, and keeps its runtime alive for the
                // process.
                unsafe {
                    libstarling::serve_wasm::drive_startup(
                        raw_cx,
                        invocation.state().event_loop(),
                        &initialized.evaluation,
                    )
                    .await;
                }
            }
            wit_bridge::init::with_scope(|scope| {
                initialized
                    .evaluation
                    .rejection(scope, "main module threw during evaluation")?;
                if initialized.evaluation.is_finished(scope) {
                    Ok(())
                } else {
                    Err(
                        "the application's top-level `await` never settled: the event loop \
                         ran out of work while it was still pending"
                            .to_string(),
                    )
                }
            })?;

            let raw_export = wit_bridge::init::with_main_scope(|scope, ns| {
                wit_bridge::interpreter::install_exports(
                    scope,
                    ns,
                    cli,
                    raw_http_handler.as_deref(),
                )
            })?;

            wit_bridge::init::with_scope(|scope| {
                libstarling::serve_wasm::finish_for_snapshot(scope, invocation.state().event_loop())
            })?;
            drop(invocation);

            let http_handler = raw_export
                .map(|raw| {
                    // SAFETY: `initialize_runtime` left the default global's realm
                    // of `raw_cx` entered for the process.
                    unsafe {
                        libstarling::serve_wasm::select_http_handler(
                            raw_cx,
                            RawHttpHandler {
                                paths: &raw.paths,
                                provided: raw.provided,
                            },
                        )
                    }
                })
                .transpose()?;

            // Hand the runtime to the serve path, so `wasi:http/handler`
            // dispatches against this global instead of standing up a second
            // runtime.
            libstarling::serve_wasm::adopt_runtime(wit_bridge::init::runtime(), raw_cx, &config)?;

            // The instance is wizened as soon as this returns, so everything
            // above is state a resumed instance inherits. Last, since this
            // records the clock reading the resumed instance's own clocks are
            // advanced past.
            wit_bridge::imports::set_imports_unavailable(false);
            libstarling::serve_wasm::prepare_for_snapshot();
            Ok(matches!(
                http_handler,
                Some(libstarling::serve_wasm::HttpHandler::Raw)
            ))
        }
    }
}
