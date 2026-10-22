// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://html.spec.whatwg.org/multipage/system-state.html#dom-navigator-useragent>

use core_runtime::webidl_interface;
use core_runtime::webidl_methods;
use js::gc::scope::Scope;
use js::Object;

#[webidl_interface(hidden)]
pub struct Navigator {}

#[webidl_methods]
impl Navigator {
    /// <https://html.spec.whatwg.org/multipage/system-state.html#dom-navigator-useragent>
    #[getter]
    fn user_agent(&self) -> String {
        String::from("StarlingMonkey")
    }
}

/// Register the `Navigator` class on `global` and install the singleton
/// `navigator` instance as a property on it.
pub fn add_to_global<'s>(scope: &'s Scope<'_>, global: Object<'s>) {
    Navigator::add_to_global(scope, global);

    let navigator = js::class::create_instance_with::<NavigatorImpl>(scope, |_| NavigatorImpl {})
        .expect("failed to allocate Navigator singleton");

    global
        .set_property(scope, c"navigator", navigator)
        .expect("failed to define globalThis.navigator");
}
