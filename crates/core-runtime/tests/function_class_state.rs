// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The Rust state of `#[jsfunction_class(state = ...)]` instances.
//!
//! `Reader`, `Writer` and `Duplex` mirror the shape of Node.js's stream classes: `Duplex` extends
//! `Reader` and has `Writer`'s methods copied onto its prototype. A `Duplex` instance holds one
//! state, which holds both parts, and `Writer`'s methods reach the writable part through
//! `WriterState`, which accepts both classes.
// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::test_util::eval_with_setup;
use core_runtime::{jsclass, jsfunction_class, jsmethods};
use js::class::InstanceState;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::Object;

/// The readable part of a state.
#[jsclass(hidden)]
struct ReaderPart {
    reads: u32,
}

#[jsmethods]
impl ReaderPart {}

/// The state of a `Writer`.
#[jsclass(hidden)]
struct WriterPart {
    writes: u32,
}

#[jsmethods]
impl WriterPart {}

/// The state of a `Duplex`: its readable part, as the parent, and its writable part.
#[jsclass(hidden, extends = ReaderPart)]
struct DuplexPart {
    parent: ReaderPartImpl,
    writes: u32,
}

#[jsmethods]
impl DuplexPart {}

/// The writable part of a `Writer` or a `Duplex`.
enum WriterState<'s> {
    Writer(WriterPart<'s>),
    Duplex(DuplexPart<'s>),
}

impl<'s> InstanceState<'s> for WriterState<'s> {
    type Class = WriterPartImpl;

    fn of_instance(instance: Object<'s>) -> Option<Self> {
        if let Some(writer) = WriterPart::of_instance(instance) {
            return Some(WriterState::Writer(writer));
        }
        DuplexPart::of_instance(instance).map(WriterState::Duplex)
    }
}

impl WriterState<'_> {
    /// Count a write, and return how many there were.
    fn write(&self) -> u32 {
        match self {
            WriterState::Writer(writer) => {
                let mut data = writer.data_mut();
                data.writes += 1;
                data.writes
            }
            WriterState::Duplex(duplex) => {
                let mut data = duplex.data_mut();
                data.writes += 1;
                data.writes
            }
        }
    }
}

#[jsfunction_class(name = "Reader", state = "ReaderPart")]
pub mod reader {
    use super::*;

    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        state_of(scope, this)?.data_mut().reads = 0;
        Ok(())
    }

    /// Count a read, and return how many there were.
    pub fn read(scope: &Scope<'_>, this: Object<'_>) -> Result<u32, ExnThrown> {
        let state = state_of(scope, this)?;
        let mut data = state.data_mut();
        data.reads += 1;
        Ok(data.reads)
    }
}

#[jsfunction_class(name = "Writer", state = "WriterState")]
pub mod writer {
    use super::*;

    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        state_of(scope, this)?;
        Ok(())
    }

    /// Count a write, and return how many there were.
    pub fn write(scope: &Scope<'_>, this: Object<'_>) -> Result<u32, ExnThrown> {
        Ok(state_of(scope, this)?.write())
    }
}

#[jsfunction_class(name = "Duplex", extends = "reader", state = "DuplexPart")]
pub mod duplex {
    use super::*;

    #[constructor]
    pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
        reader::init(scope, this)?;
        writer::init(scope, this)
    }

    /// Copy `Writer`'s methods onto the prototype.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        let write = writer::prototype_object(scope)?.get_property(scope, c"write")?;
        proto.set_property(scope, c"write", write)
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        ReaderPart::add_to_global(scope, global);
        WriterPart::add_to_global(scope, global);
        DuplexPart::add_to_global(scope, global);
        let install = || -> Result<(), ExnThrown> {
            global.set_property(scope, c"Reader", reader::register(scope)?)?;
            global.set_property(scope, c"Writer", writer::register(scope)?)?;
            global.set_property(scope, c"Duplex", duplex::register(scope)?)
        };
        install().expect("defining the classes succeeds");
    });
}

#[test]
fn new_instances_hold_their_state() {
    let result = eval_with_setup(
        setup,
        "const r = new Reader(); r.read(); \
         class Sub extends Writer { constructor() { super(); } } \
         const w = new Sub(); w.write(); \
         [r.read(), w.write(), w instanceof Writer, Object.getOwnPropertySymbols(w).length].join()",
    );
    assert_eq!(result, "2,2,true,0");
}

#[test]
fn es5_subclasses_hold_their_state_in_a_handle() {
    let result = eval_with_setup(
        setup,
        "function Sub() { Reader.call(this); } \
         Object.setPrototypeOf(Sub.prototype, Reader.prototype); \
         const s = new Sub(); s.read(); \
         [s.read(), Reflect.ownKeys(s).length, Object.getOwnPropertySymbols(s).length].join()",
    );
    assert_eq!(result, "2,0,0");
}

#[test]
fn duplex_reaches_both_parts() {
    let result = eval_with_setup(
        setup,
        "const d = new Duplex(); d.read(); d.write(); d.write(); \
         function Old() { Duplex.call(this); } \
         Object.setPrototypeOf(Old.prototype, Duplex.prototype); \
         const o = new Old(); o.write(); \
         [d.read(), d.write(), d instanceof Reader, o.read(), o.write()].join()",
    );
    assert_eq!(result, "2,3,true,1,2");
}

#[test]
fn objects_without_state_are_rejected() {
    let result = eval_with_setup(
        setup,
        "let message; \
         try { Writer.prototype.write.call({}); } catch (e) { message = e.message; } \
         message",
    );
    assert_eq!(result, "Value of \"this\" must be of type Writer");
}

#[test]
fn proxies_reach_the_state_of_their_target() {
    let result = eval_with_setup(
        setup,
        "const r = new Reader(); r.read(); \
         function Sub() { Reader.call(this); } \
         Object.setPrototypeOf(Sub.prototype, Reader.prototype); \
         const s = new Sub(); \
         [new Proxy(r, {}).read(), new Proxy(new Proxy(s, {}), {}).read()].join()",
    );
    assert_eq!(result, "2,1");
}

#[test]
fn proxies_hold_handles_on_their_target() {
    let result = eval_with_setup(
        setup,
        "const target = {}; \
         const proxy = new Proxy(new Proxy(target, {}), {}); \
         Reader.call(proxy); Reader.prototype.read.call(proxy); \
         const { proxy: revoked, revoke } = Proxy.revocable({}, {}); \
         revoke(); \
         let message; \
         try { Reader.call(revoked); } catch (e) { message = e.constructor.name; } \
         [Reader.prototype.read.call(target), Reflect.ownKeys(target).length, message].join()",
    );
    assert_eq!(result, "2,0,TypeError");
}

#[test]
fn state_lookup_runs_no_javascript() {
    let result = eval_with_setup(
        setup,
        "let calls = 0; \
         const handler = {}; \
         for (const trap of Reflect.ownKeys(Reflect)) { \
           handler[trap] = (...args) => { \
             if (trap !== 'get' || typeof args[1] === 'symbol') calls++; \
             return Reflect[trap](...args); \
           }; \
         } \
         const r = new Proxy(new Reader(), handler); r.read(); \
         const p = new Proxy({}, handler); Reader.call(p); Reader.prototype.read.call(p); \
         const inherits = Object.create(new Proxy({}, handler)); \
         let message; \
         try { Reader.prototype.read.call(inherits); } catch (e) { message = e.message; } \
         [calls, message].join()",
    );
    assert_eq!(result, "0,Value of \"this\" must be of type Reader");
}

#[test]
fn objects_inherit_the_state_of_their_prototype() {
    let result = eval_with_setup(
        setup,
        "const r = new Reader(); r.read(); \
         function Sub() { Reader.call(this); } \
         Sub.prototype = new Reader(); \
         const s = new Sub(); s.read(); \
         [Object.create(r).read(), s.read(), Sub.prototype.read()].join()",
    );
    assert_eq!(result, "2,2,1");
}

#[test]
fn handles_are_hidden_from_javascript() {
    let result = eval_with_setup(
        setup,
        "function Sub() { Reader.call(this); } \
         Object.setPrototypeOf(Sub.prototype, Reader.prototype); \
         const s = new Sub(); s.read(); \
         const copies = [Object.assign(Object.create(Sub.prototype), s), { __proto__: Sub.prototype, ...s }]; \
         const messages = copies.map((copy) => { try { copy.read(); } catch (e) { return e.message; } }); \
         Object.freeze(s); \
         [s.read(), JSON.stringify(s), Object.isFrozen(s), ...messages].join()",
    );
    assert_eq!(
        result,
        "2,{},true,Value of \"this\" must be of type Reader,Value of \"this\" must be of type Reader"
    );
}
