// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Integration tests for WebIDL [`Promise<T>`] arguments.
//!
//! `Promise<T>` is the one WebIDL type whose argument conversion accepts every
//! value: the value is wrapped in a promise resolved with it, so a method
//! declared to take `Promise<T>` accepts a promise, a bare `T`, or anything
//! else. `T` is checked only once the wrapper settles, and a value that fails
//! the check rejects the promise the method holds rather than throwing at the
//! call.
//!
//! A parameter spelled `Promise<'_>` is the wrapping alone, for the `T`s every
//! value converts to (`any`, `undefined`); `PromiseOf<'_, T>` is the wrapping
//! plus the check.
//!
//! [`Promise<T>`]: https://webidl.spec.whatwg.org/#idl-promise

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::{clear_global_initializers, register_global_initializer, Runtime};
use core_runtime::{jsclass, jsmethods};
use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::{Promise, PromiseOf};

/// The interface type a checked `Promise<Payload>` argument must resolve to.
#[jsclass]
struct Payload {
    tag: String,
}

#[jsmethods]
impl Payload {
    #[constructor]
    fn construct(tag: String) -> Self {
        PayloadImpl { tag }
    }

    #[getter]
    fn tag(&self) -> String {
        self.data().tag.clone()
    }
}

/// The same two spellings as dictionary members. WebIDL converts a dictionary's members once, when
/// the dictionary itself is converted, so a member declared `Promise<T>` holds one promise — which
/// is what lets an attribute initialized from such a member return the same object on every read.
#[core_runtime::webidl_dictionary]
struct PromiseMembers<'a> {
    unchecked: Option<Promise<'a>>,
    checked: Option<PromiseOf<'a, Payload<'a>>>,
}

#[jsclass]
struct PromiseArgs {}

#[jsmethods]
impl PromiseArgs {
    #[constructor]
    fn construct() -> Self {
        Self {}
    }

    /// WebIDL `Promise<any> anyPromise(Promise<any> p)` — wrapped, never
    /// checked. Handing the promise straight back lets JS observe what the
    /// conversion produced.
    #[method]
    fn any_promise<'r>(
        &self,
        _scope: &'r Scope<'_>,
        p: Promise<'r>,
    ) -> Result<Promise<'r>, ExnThrown> {
        Ok(p)
    }

    /// WebIDL `Promise<any> payloadPromise(Promise<Payload> p)` — wrapped, then
    /// checked against `Payload` once it settles.
    #[method]
    fn payload_promise<'r>(
        &self,
        _scope: &'r Scope<'_>,
        p: PromiseOf<'r, Payload>,
    ) -> Result<Promise<'r>, ExnThrown> {
        Ok(p.promise())
    }

    /// WebIDL `undefined expectPayloadPromise(Promise<Payload> p)`. The
    /// argument conversion is the same; only the failure mode differs, because
    /// this operation doesn't return a promise to reject.
    #[method]
    fn expect_payload_promise(&self, _p: PromiseOf<'_, Payload<'_>>) {}

    /// The unchecked member of a dictionary, handed back for JS to observe.
    #[method]
    fn unchecked_member<'r>(
        &self,
        scope: &'r Scope<'_>,
        members: PromiseMembers<'r>,
    ) -> Result<Promise<'r>, ExnThrown> {
        match members.unchecked {
            Some(p) => Ok(p),
            None => Promise::new_resolved_with_value(scope, js::prelude::HandleValue::undefined()),
        }
    }

    /// The checked member, whose `Payload` check runs the same way as for an argument.
    #[method]
    fn checked_member<'r>(
        &self,
        scope: &'r Scope<'_>,
        members: PromiseMembers<'r>,
    ) -> Result<Promise<'r>, ExnThrown> {
        match members.checked {
            Some(p) => Ok(p.promise()),
            None => Promise::new_resolved_with_value(scope, js::prelude::HandleValue::undefined()),
        }
    }

    /// An optional `Promise<Payload>`: a missing or `undefined` argument is
    /// `None`, everything else goes through the conversion.
    #[method]
    fn optional_payload_promise<'r>(
        &self,
        scope: &'r Scope<'_>,
        p: Option<PromiseOf<'r, Payload>>,
    ) -> Result<Promise<'r>, ExnThrown> {
        match p {
            Some(p) => Ok(p.promise()),
            None => Promise::new_resolved_with_value(scope, js::prelude::HandleValue::undefined()),
        }
    }
}

/// Evaluate `code`, drain the microtask queue, then evaluate `probe` and return
/// its value as a string. The two phases are what make the delayed check
/// observable: everything `code` sees happens before any promise reaction runs.
fn eval_then_drain(code: &str, probe: &str) -> String {
    clear_global_initializers();
    register_global_initializer(Payload::add_to_global);
    register_global_initializer(PromiseArgs::add_to_global);
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    if js::compile::evaluate_with_filename(&scope, code, "test.js", 1).is_err() {
        panic!(
            "JS evaluation threw an exception: {:?}",
            ExnThrown::capture(&scope)
        );
    }
    js::jobs::run_jobs(&scope);
    let value = js::compile::evaluate_with_filename(&scope, probe, "probe.js", 1)
        .expect("probe evaluation threw");
    String::from_jsval(&scope, value, ()).expect("probe value is not a string")
}

#[test]
fn promise_arguments_wrap_any_value_and_check_the_type_later() {
    let log = eval_then_drain(
        r#"
        globalThis.log = [];
        const record = (name) => [
            (v) => log.push(`${name}:fulfilled:${v && v.tag ? v.tag : v}`),
            // The rejection a failed check produces has to name the type the value should have
            // been — the conversion's own "not an object" says nothing about which promise or what
            // it owed.
            (e) => log.push(`${name}:rejected:${e.constructor.name}:${e.message}`),
        ];
        const t = new PromiseArgs();

        // An unchecked `Promise<any>` accepts a bare value...
        t.anyPromise(1).then(...record("any-value"));
        // ...a promise, whose value it adopts...
        t.anyPromise(Promise.resolve(2)).then(...record("any-promise"));
        // ...a rejected promise, whose rejection it adopts...
        t.anyPromise(Promise.reject(new RangeError("no"))).then(...record("any-rejected"));
        // ...and a thenable, which it resolves like any other.
        t.anyPromise({ then: (resolve) => resolve(3) }).then(...record("any-thenable"));

        // A checked `Promise<Payload>` accepts a bare `Payload`...
        t.payloadPromise(new Payload("bare")).then(...record("payload-value"));
        // ...and a promise for one.
        t.payloadPromise(Promise.resolve(new Payload("wrapped")))
            .then(...record("payload-promise"));
        // A value of the wrong type is accepted at the call and rejects later.
        t.payloadPromise(4).then(...record("payload-wrong"));
        t.payloadPromise(Promise.resolve(4)).then(...record("payload-wrong-promise"));

        // Dictionary members convert the same way arguments do: a bare value is wrapped, and a
        // checked member's wrong type rejects rather than throwing at the call.
        t.uncheckedMember({ unchecked: 5 }).then(...record("member-unchecked"));
        t.checkedMember({ checked: new Payload("member") }).then(...record("member-checked"));
        t.checkedMember({ checked: 6 }).then(...record("member-wrong"));
        t.checkedMember({}).then(...record("member-absent"));

        // The optional form: absent is `None`, present goes through the conversion.
        t.optionalPayloadPromise().then(...record("optional-absent"));
        t.optionalPayloadPromise(undefined).then(...record("optional-undefined"));
        t.optionalPayloadPromise(new Payload("present")).then(...record("optional-present"));

        // Nothing has been checked yet: the calls returned pending promises, and
        // the wrong-typed one did not throw.
        globalThis.settledDuringCall = log.length;
        "#,
        "log.sort().join(' | ') + ` || settledDuringCall=${settledDuringCall}`",
    );

    assert_eq!(
        log,
        [
            "any-promise:fulfilled:2",
            "any-rejected:rejected:RangeError:no",
            "any-thenable:fulfilled:3",
            "any-value:fulfilled:1",
            "member-absent:fulfilled:undefined",
            "member-checked:fulfilled:member",
            "member-unchecked:fulfilled:5",
            "member-wrong:rejected:TypeError:promise resolved with a value that is not a Payload",
            "optional-absent:fulfilled:undefined",
            "optional-present:fulfilled:present",
            "optional-undefined:fulfilled:undefined",
            "payload-promise:fulfilled:wrapped",
            "payload-value:fulfilled:bare",
            // The declared value type by the name script knows it by — `Payload`, not the Rust
            // spelling `Payload<'r>` the parameter carries.
            "payload-wrong-promise:rejected:TypeError:promise resolved with a value that is not a \
             Payload",
            "payload-wrong:rejected:TypeError:promise resolved with a value that is not a Payload",
        ]
        .join(" | ")
            + " || settledDuringCall=0"
    );
}

/// A missing required argument fails at the call rather than being wrapped:
/// WebIDL counts arguments before it converts any of them. Which *kind* of
/// error is the ordinary rule for any operation — a promise-returning one
/// rejects, everything else throws.
#[test]
fn a_missing_promise_argument_is_an_error_at_the_call() {
    let log = eval_then_drain(
        r#"
        globalThis.log = [];
        const t = new PromiseArgs();
        const call = (name, f) => {
            try {
                const result = f();
                log.push(`${name}:returned`);
                if (result) result.catch((e) => log.push(`${name}:rejected:${e.constructor.name}`));
            } catch (e) {
                log.push(`${name}:threw:${e.constructor.name}`);
            }
        };
        call("promise-returning", () => t.payloadPromise());
        call("undefined-returning", () => t.expectPayloadPromise());
        "#,
        "log.join(' | ')",
    );
    assert_eq!(
        log,
        "promise-returning:returned | undefined-returning:threw:TypeError \
         | promise-returning:rejected:TypeError"
    );
}

/// The check is a fulfillment reaction on the wrapper, so it runs a microtask
/// after the call — not synchronously, and not eagerly at conversion time.
#[test]
fn the_type_check_runs_in_a_later_microtask() {
    let log = eval_then_drain(
        r#"
        globalThis.log = [];
        const t = new PromiseArgs();
        // The wrapper for a non-promise argument is already fulfilled, so its
        // check is queued at call time — ahead of this `then`, whose handler is
        // queued a moment later. The rejection the check produces is therefore
        // observed *after* it.
        t.payloadPromise(5).catch(() => log.push("checked"));
        Promise.resolve().then(() => log.push("microtask"));
        "#,
        "log.join(' | ')",
    );
    assert_eq!(log, "microtask | checked");
}
