// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Hooks a builtin can register into the lifecycle of a script run: the end of its event loop, the
//! end of the run, and errors nothing caught. [`crate::ScriptEvaluation::run_to_completion`] and
//! [`crate::setup`] call the `before_exit` hook, and [`crate::run`] and [`crate::setup`] call the
//! `exit` hook. Embedders that don't use them call [`exit`] themselves, or not at all. The
//! `uncaught` hook is called in every run, including runs that serve requests.

use std::cell::Cell;

use js::gc::scope::Scope;
use js::prelude::HandleValue;

/// The lifecycle hooks.
#[derive(Clone, Copy)]
pub struct Hooks {
    /// Called when a run's event loop has no work left. It may queue more work, which the run
    /// then runs before calling it again.
    pub before_exit: fn(&Scope<'_>),
    /// Called once a run ends without an error. It may end the process itself.
    pub exit: fn(&Scope<'_>),
    /// Called with an exception a task or microtask threw, or with the reason and the
    /// [`Rejection`] of a promise rejected with no handler attached. Returns whether it handled the
    /// error, which is otherwise reported. It may end the process itself.
    pub uncaught: fn(&Scope<'_>, HandleValue<'_>, Option<Rejection<'_>>) -> bool,
    /// Called with a promise that got a handler after the `uncaught` hook was called for its
    /// rejection, and the promise's rejection ID.
    pub rejection_handled: fn(&Scope<'_>, js::Promise<'_>, u64),
}

/// A promise rejected with no handler attached, which the `uncaught` hook is called for.
#[derive(Clone, Copy)]
pub struct Rejection<'a> {
    pub promise: HandleValue<'a>,
    /// The number of promises rejected with no handler attached up to and including this one.
    pub id: u64,
}

js::instance_local! {
    static HOOKS: Cell<Option<Hooks>> = const { Cell::new(None) };
}

/// Register `hooks`, replacing hooks registered before.
pub fn set_hooks(hooks: Hooks) {
    HOOKS.with(|cell| cell.set(Some(hooks)));
}

/// Whether hooks are registered.
pub(crate) fn has_hooks() -> bool {
    HOOKS.with(Cell::get).is_some()
}

/// Call the `before_exit` hook, and run the microtasks it queued. Returns whether there is one.
pub(crate) fn before_exit(scope: &Scope<'_>) -> bool {
    match HOOKS.with(Cell::get) {
        Some(hooks) => {
            (hooks.before_exit)(scope);
            crate::event_loop::run_microtasks(scope);
            true
        }
        None => false,
    }
}

/// Call the `exit` hook, if there is one. Runs call it once they end without an error, with an
/// event loop active.
pub fn exit(scope: &Scope<'_>) {
    if let Some(hooks) = HOOKS.with(Cell::get) {
        (hooks.exit)(scope);
    }
}

/// Call the `uncaught` hook with `error`, and the `rejection` it is the reason of. Returns whether
/// it handled the error.
pub(crate) fn uncaught(
    scope: &Scope<'_>,
    error: HandleValue<'_>,
    rejection: Option<Rejection<'_>>,
) -> bool {
    HOOKS
        .with(Cell::get)
        .is_some_and(|hooks| (hooks.uncaught)(scope, error, rejection))
}

/// Call the `rejection_handled` hook with `promise` and its rejection `id`, if there is one.
pub(crate) fn rejection_handled(scope: &Scope<'_>, promise: js::Promise<'_>, id: u64) {
    if let Some(hooks) = HOOKS.with(Cell::get) {
        (hooks.rejection_handled)(scope, promise, id);
    }
}
