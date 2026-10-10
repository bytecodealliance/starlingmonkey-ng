// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`CloseEvent`](https://websockets.spec.whatwg.org/#the-closeevent-interface) interface.

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::conversion::Clamp;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use web_globals::events::event::{EventImpl, EventInit};
use web_globals::events::Event;

/// <https://websockets.spec.whatwg.org/#closeevent>
#[webidl_interface(extends = Event)]
pub struct CloseEvent {
    parent: EventImpl,
    /// <https://websockets.spec.whatwg.org/#dom-closeevent-wasclean>
    #[no_trace]
    was_clean: bool,
    /// <https://websockets.spec.whatwg.org/#dom-closeevent-code>
    #[no_trace]
    code: u16,
    /// <https://websockets.spec.whatwg.org/#dom-closeevent-reason>
    #[no_trace]
    reason: String,
}

#[webidl_methods]
impl CloseEvent {
    /// <https://websockets.spec.whatwg.org/#dom-closeevent-closeevent>
    #[constructor]
    fn new(event_type: String, event_init_dict: Option<&CloseEventInit>) -> Self {
        let default = CloseEventInit::default();
        let init = event_init_dict.unwrap_or(&default);
        CloseEventImpl {
            parent: EventImpl::new_untrusted(event_type, Some(init.parent)),
            was_clean: init.was_clean,
            code: init.code.0,
            reason: init.reason.clone(),
        }
    }

    /// <https://websockets.spec.whatwg.org/#dom-closeevent-wasclean>
    #[getter]
    fn was_clean(&self) -> bool {
        self.data().was_clean
    }

    /// <https://websockets.spec.whatwg.org/#dom-closeevent-code>
    #[getter]
    fn code(&self) -> u16 {
        self.data().code
    }

    /// <https://websockets.spec.whatwg.org/#dom-closeevent-reason>
    #[getter]
    fn reason(&self) -> String {
        self.data().reason.clone()
    }

    /// A trusted `close` event with `was_clean`, `code` and `reason`.
    pub fn new_trusted<'r>(
        scope: &'r Scope<'_>,
        was_clean: bool,
        code: u16,
        reason: String,
    ) -> Result<CloseEvent<'r>, ExnThrown> {
        js::class::create_instance_with::<CloseEventImpl>(scope, |_| CloseEventImpl {
            parent: EventImpl::new_trusted("close".to_string(), None),
            was_clean,
            code,
            reason,
        })
    }
}

/// <https://websockets.spec.whatwg.org/#dictdef-closeeventinit>
#[derive(Default)]
#[webidl_dictionary(extends = EventInit)]
pub struct CloseEventInit {
    pub parent: EventInit,
    #[webidl(default = false, name = "wasClean")]
    pub was_clean: bool,
    #[webidl(default = Clamp(0))]
    pub code: Clamp<u16>,
    #[webidl(default = String::new())]
    pub reason: String,
}
