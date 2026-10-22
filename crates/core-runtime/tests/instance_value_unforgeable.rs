// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Instances carry `[LegacyUnforgeable]` own accessors whichever path creates them: the
//! constructor, a method returning `Self` (the `InstanceValue` codegen path), or a JS or Rust
//! subclass.

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::jsclass;
use core_runtime::jsmethods;
use core_runtime::test_util::eval_with_setup;

#[jsclass]
struct Widget {
    id: i32,
}

#[jsmethods]
impl Widget {
    #[constructor]
    fn construct() -> Self {
        Self { id: 7 }
    }

    /// `[LegacyUnforgeable]`: an own accessor on each instance, not the prototype.
    #[getter(unforgeable)]
    fn kind(&self) -> i32 {
        42
    }

    /// Returns a fresh instance (the `InstanceValue` path).
    #[method]
    fn dup(&self) -> Self {
        Self { id: self.id }
    }
}

/// A subclass with an unforgeable accessor of its own, which also inherits `kind`.
#[jsclass(extends = Widget)]
struct Gadget {
    parent: WidgetImpl,
}

#[jsmethods]
impl Gadget {
    #[constructor]
    fn construct() -> Self {
        Self {
            parent: WidgetImpl::construct(),
        }
    }

    #[getter(unforgeable)]
    fn level(&self) -> i32 {
        3
    }
}

fn setup() {
    core_runtime::runtime::register_global_initializer(|scope, global| {
        Widget::add_to_global(scope, global);
        Gadget::add_to_global(scope, global);
    });
}

#[test]
fn dup_instance_has_unforgeable_own_accessor() {
    assert_eq!(
        eval_with_setup(
            setup,
            "const d = new Widget().dup(); \
             [typeof Object.getOwnPropertyDescriptor(d, 'kind'), d.kind].join()"
        ),
        "object,42"
    );
}

#[test]
fn instances_share_one_getter_function() {
    assert_eq!(
        eval_with_setup(
            setup,
            "const get = o => Object.getOwnPropertyDescriptor(o, 'kind').get; \
             const a = new Widget(); \
             get(a) === get(new Widget()) && get(a) === get(a.dup()) && get(a).name"
        ),
        "get kind"
    );
}

/// The accessor is an own property only, not one of the prototype.
#[test]
fn unforgeable_accessor_is_an_enumerable_non_configurable_own_property() {
    assert_eq!(
        eval_with_setup(
            setup,
            "const d = Object.getOwnPropertyDescriptor(new Widget(), 'kind'); \
             [d.enumerable, d.configurable, d.set, \
              Object.getOwnPropertyDescriptor(Widget.prototype, 'kind')].join()"
        ),
        "true,false,,"
    );
}

#[test]
fn js_subclass_instances_get_the_unforgeable_accessor() {
    assert_eq!(
        eval_with_setup(
            setup,
            "class Sub extends Widget { extra() { return 1; } } \
             const s = new Sub(); \
             [Object.getOwnPropertyNames(s).join(), s.kind, s.extra(), \
              Object.getPrototypeOf(s) === Sub.prototype].join()"
        ),
        "kind,42,1,true"
    );
}

#[test]
fn rust_subclass_instances_get_inherited_and_own_unforgeable_accessors() {
    assert_eq!(
        eval_with_setup(
            setup,
            "const g = new Gadget(); \
             [Object.getOwnPropertyNames(g).sort().join(), g.kind, g.level, \
              Object.getOwnPropertyNames(new Widget()).join()].join('|')"
        ),
        "kind,level|42|3|kind"
    );
}
