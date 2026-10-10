// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! DOM Event interfaces.
//!
//! Implements [`Event`], [`EventTarget`], and [`CustomEvent`] from the
//! [WHATWG DOM Living Standard](https://dom.spec.whatwg.org/).

pub mod algorithms;
pub mod custom_event;
pub mod event;
pub mod event_target;
pub mod message_event;

pub use event::Event;
pub use event_target::EventTarget;

use js::class::{get_prototype_object_for, set_global_private_and_proto};
use js::conversion::FromJSVal;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Callable, Object};

/// Replace the listener of the event handler of `target` for `event_type`: remove the listener of
/// `old`, the current handler, and add `value` as a listener if it is callable. Returns the new
/// handler, which is `value` if it is callable and `None` otherwise.
pub fn replace_event_handler<'s>(
    scope: &'s Scope<'_>,
    target: &EventTarget<'_>,
    event_type: &str,
    old: Option<Callable<'_>>,
    value: HandleValue<'_>,
) -> Option<Callable<'s>> {
    if let Some(old) = old {
        algorithms::remove_an_event_listener(&mut target.data_mut(), event_type, &old, false);
    }
    let handler = Callable::from_jsval(scope, value, ()).ok();
    if let Some(handler) = handler {
        algorithms::add_an_event_listener(
            target,
            event_type.to_string(),
            *handler,
            false,
            None,
            false,
            None,
        );
    }
    handler
}

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    event::Event::add_to_global(scope, global);
    custom_event::CustomEvent::add_to_global(scope, global);
    message_event::MessageEvent::add_to_global(scope, global);
    event_target::EventTarget::add_to_global(scope, global);

    // For now, the main global simply *is* an `EventTarget`.
    // We might introduce a dedicated class extending `EventTarget` later.
    let proto = get_prototype_object_for::<event_target::EventTargetImpl>(scope).unwrap();
    unsafe {
        set_global_private_and_proto(scope, global, event_target::EventTargetImpl::new(), proto)
    };
}
