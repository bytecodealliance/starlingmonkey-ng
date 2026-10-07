// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! [`MessageChannel`](https://html.spec.whatwg.org/multipage/web-messaging.html#message-channels)
//! interface.

use core_runtime::{webidl_interface, webidl_methods};
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;

use super::message_port::{MessagePort, MessagePortImpl};

/// <https://html.spec.whatwg.org/multipage/web-messaging.html#messagechannel>
#[webidl_interface]
pub struct MessageChannel {
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#port-1>
    port1: Heap<MessagePortImpl>,
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#port-2>
    port2: Heap<MessagePortImpl>,
}

#[webidl_methods]
impl MessageChannel {
    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-messagechannel>
    #[constructor]
    fn new(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Set `this`'s `port 1` to a `new` ``MessagePort`` in `this`'s `relevant
        //         realm`.
        let port1 = MessagePort::new(scope)?;
        self.data_mut().port1.set(port1);
        // Step 2: Set `this`'s `port 2` to a `new` ``MessagePort`` in `this`'s `relevant
        //         realm`.
        let port2 = MessagePort::new(scope)?;
        self.data_mut().port2.set(port2);
        // Step 3: `Entangle` `this`'s `port 1` and `this`'s `port 2`.
        port1.entangle(scope, &port2);
        Ok(())
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-messagechannel-port1>
    #[getter]
    fn port1<'r>(&self, scope: &'r Scope<'_>) -> MessagePort<'r> {
        // The `port1` getter steps are to return `this`'s `port 1`.
        self.data().port1.get(scope)
    }

    /// <https://html.spec.whatwg.org/multipage/web-messaging.html#dom-messagechannel-port2>
    #[getter]
    fn port2<'r>(&self, scope: &'r Scope<'_>) -> MessagePort<'r> {
        // The `port2` getter steps are to return `this`'s `port 2`.
        self.data().port2.get(scope)
    }
}
