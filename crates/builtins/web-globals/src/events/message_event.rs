// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`MessageEvent`](https://html.spec.whatwg.org/multipage/comms.html#the-messageevent-interface)
//! interface.

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::{Heap, OptionHeapExt};
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use super::event::{Event, EventImpl, EventInit};
use crate::messaging::message_port::{MessagePort, MessagePortImpl};

/// <https://html.spec.whatwg.org/multipage/comms.html#messageevent>
#[webidl_interface(extends = Event)]
pub struct MessageEvent {
    parent: EventImpl,
    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-data>
    data: Heap<Value>,
    /// <https://html.spec.whatwg.org/multipage/comms.html#concept-messageevent-origin>
    #[no_trace]
    origin: String,
    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-lasteventid>
    #[no_trace]
    last_event_id: String,
    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-source>
    source: Option<Heap<MessagePortImpl>>,
    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-ports>: a frozen array
    /// of `MessagePort`s, set by the constructor.
    ports: Option<Heap<js::object::Object>>,
}

#[webidl_methods]
impl MessageEvent {
    /// <https://html.spec.whatwg.org/multipage/comms.html#messageevent>
    #[constructor]
    fn new(
        &self,
        scope: &Scope<'_>,
        event_type: String,
        event_init_dict: Option<MessageEventInit<'_>>,
    ) -> Result<(), ExnThrown> {
        let init = event_init_dict.unwrap_or_default();
        let ports = frozen_ports(scope, &init.ports)?;
        let mut data = self.data_mut();
        data.parent = EventImpl::new_untrusted(event_type, Some(init.parent));
        data.data
            .set(init.data.map_or(js::value::null(), |data| data.get()));
        data.origin = init.origin;
        data.last_event_id = init.last_event_id;
        if let Some(Some(source)) = init.source {
            data.source.set(source);
        }
        data.ports.set(ports);
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-data>
    #[getter(name = "data")]
    fn get_data<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        self.data().data.get(scope)
    }

    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-origin>
    #[getter]
    fn origin(&self) -> String {
        self.data().origin.clone()
    }

    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-lasteventid>
    #[getter]
    fn last_event_id(&self) -> String {
        self.data().last_event_id.clone()
    }

    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-source>
    #[getter]
    fn source<'r>(&self, scope: &'r Scope<'_>) -> Option<MessagePort<'r>> {
        self.data().source.get(scope)
    }

    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-ports>
    #[getter]
    fn ports<'r>(&self, scope: &'r Scope<'_>) -> Object<'r> {
        self.data()
            .ports
            .get(scope)
            .expect("set by the constructor")
    }

    /// <https://html.spec.whatwg.org/multipage/comms.html#dom-messageevent-initmessageevent>
    #[method(length = 1)]
    #[allow(clippy::too_many_arguments)]
    fn init_message_event(
        &self,
        scope: &Scope<'_>,
        event_type: String,
        bubbles: Option<bool>,
        cancelable: Option<bool>,
        data: Option<HandleValue<'_>>,
        origin: Option<String>,
        last_event_id: Option<String>,
        source: Option<Option<MessagePort<'_>>>,
        ports: Option<Vec<MessagePort<'_>>>,
    ) -> Result<(), ExnThrown> {
        if self.is_dispatching() {
            return Ok(());
        }
        let ports = frozen_ports(scope, &ports.unwrap_or_default())?;
        let mut this = self.data_mut();
        this.parent.initialize(
            event_type,
            bubbles.unwrap_or(false),
            cancelable.unwrap_or(false),
        );
        this.data
            .set(data.map_or(js::value::null(), |data| data.get()));
        this.origin = origin.unwrap_or_default();
        this.last_event_id = last_event_id.unwrap_or_default();
        match source.flatten() {
            Some(source) => this.source.set(source),
            None => this.source = None,
        }
        this.ports.set(ports);
        Ok(())
    }

    /// Initialize the event's origin to `origin`.
    pub fn set_origin(&self, origin: &str) {
        self.data_mut().origin = origin.to_string();
    }

    /// A trusted `message` or `messageerror` event of type `event_type`, with `data`, and `ports`
    /// as its frozen array of ports.
    pub fn new_trusted<'r>(
        scope: &'r Scope<'_>,
        event_type: &str,
        data: HandleValue<'_>,
        ports: &[MessagePort<'_>],
    ) -> Result<MessageEvent<'r>, ExnThrown> {
        let ports = frozen_ports(scope, ports)?;
        js::class::create_instance_with::<MessageEventImpl>(scope, |_| MessageEventImpl {
            parent: EventImpl::new_trusted(event_type.to_string(), None),
            data: Heap::from(data.get()),
            ports: Some(Heap::from(ports)),
            ..Default::default()
        })
    }
}

/// A new frozen array of `ports`.
fn frozen_ports<'r>(
    scope: &'r Scope<'_>,
    ports: &[MessagePort<'r>],
) -> Result<Object<'r>, ExnThrown> {
    let array: Object<'r> = *js::Array::with_contents(scope, ports)?;
    array.freeze(scope)?;
    Ok(array)
}

/// <https://html.spec.whatwg.org/multipage/comms.html#messageeventinit>
#[derive(Default)]
#[webidl_dictionary(extends = EventInit)]
pub struct MessageEventInit<'a> {
    pub parent: EventInit,
    pub data: Option<HandleValue<'a>>,
    #[webidl(default = String::new())]
    pub origin: String,
    #[webidl(default = String::new(), name = "lastEventId")]
    pub last_event_id: String,
    pub source: Option<Option<MessagePort<'a>>>,
    #[webidl(default = Vec::new())]
    pub ports: Vec<MessagePort<'a>>,
}
