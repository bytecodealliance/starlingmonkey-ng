// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The native functions defined on the debugger global. The adapter's `host.d.ts` declares them.

#[core_runtime::jsglobals]
pub mod host_globals {
    use js::gc::scope::Scope;

    /// Queue an outgoing DAP message. The connection assigns its `seq`.
    pub fn send(json: String) {
        if let Some(client) = crate::session::current_client() {
            client.send(json);
        }
    }

    /// Record that the client sent `configurationDone`.
    pub fn configured() {
        if let Some(client) = crate::session::current_client() {
            client.mark_configured();
        }
    }

    /// Record that the client detached with a `disconnect` request.
    pub fn detached() {
        if let Some(client) = crate::session::current_client() {
            client.mark_detached();
        }
    }

    /// Block, passing incoming messages to `handleMessage`, until `resume` is called or the
    /// client disconnects. Returns `false` without blocking when `mayPause` is `false`.
    pub fn pause(scope: &Scope<'_>) -> bool {
        crate::session::pause(scope)
    }

    /// End the current pause once the current `handleMessage` call returns.
    pub fn resume() {
        if let Some(client) = crate::session::current_client() {
            client.request_resume();
        }
    }

    /// Whether `pause` would block.
    pub fn may_pause() -> bool {
        crate::session::may_pause()
    }

    /// Write `message` and a newline to stderr.
    pub fn print(message: String) {
        eprintln!("{message}");
    }

    /// Send `exited` and `terminated`, flush the transport and exit the process.
    pub fn terminate() {
        crate::session::terminate()
    }
}
