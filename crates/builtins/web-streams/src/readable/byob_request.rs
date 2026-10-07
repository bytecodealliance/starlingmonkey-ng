// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://streams.spec.whatwg.org/>
use super::byte_stream_controller::ReadableByteStreamControllerImpl;
use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::EnforceRange;
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::OptionHeapExt;

/// <https://streams.spec.whatwg.org/#rs-byob-request-class>
#[webidl_interface]
pub struct ReadableStreamBYOBRequest {
    /// <https://streams.spec.whatwg.org/#ReadableStreamBYOBRequest-controller>
    /// The parent ReadableByteStreamController instance, or null once the BYOB
    /// request has been invalidated.
    pub(crate) controller: Option<Heap<ReadableByteStreamControllerImpl>>,
    /// <https://streams.spec.whatwg.org/#ReadableStreamBYOBRequest-view>
    /// A typed array representing the destination region to which the controller can write generated
    /// data, or null after the BYOB request has been invalidated.
    pub(crate) view: Option<Heap<js::typedarray::ArrayBufferView>>,
}

#[webidl_methods]
impl ReadableStreamBYOBRequest {
    /// <https://streams.spec.whatwg.org/#dom-ReadableStreamBYOBRequest-constructor>
    fn new() -> Self {
        ReadableStreamBYOBRequestImpl::default()
    }

    /// <https://streams.spec.whatwg.org/#rs-byob-request-view>
    #[getter]
    pub fn view<'r>(&self, scope: &'r Scope<'_>) -> Option<js::ArrayBufferView<'r>> {
        // WebIDL: Uint8Array
        // Step 1: Return `this`.`[[view]]`.
        //         The slot holds a `Uint8Array`, or `None` after invalidation, which the WebIDL
        //         nullable surfaces as null.
        self.data().view.get(scope)
    }

    /// <https://streams.spec.whatwg.org/#rs-byob-request-respond>
    #[method]
    pub fn respond(
        &self,
        scope: &Scope<'_>,
        bytes_written: EnforceRange<u64>,
    ) -> Result<(), ExnThrown> {
        let bytes_written = bytes_written.0;
        // Step 1: If `this`.`[[controller]]` is undefined, throw a ``TypeError`` exception.
        let controller = match self.data().controller.get(scope) {
            Some(c) => c,
            None => {
                return Err(crate::algorithms::throw_invalid_state(
                    scope,
                    "ReadableStreamBYOBRequest",
                    c"respond() called on an invalidated BYOB request",
                ));
            }
        };
        // Step 2: If ! `IsDetachedBuffer`(`this`.`[[view]]`.[[ArrayBuffer]]) is true, throw a
        //         ``TypeError`` exception.
        let view = self
            .data()
            .view
            .get(scope)
            .expect("BYOB request view is set while its controller is");
        if view.viewed_buffer(scope)?.is_detached() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBRequest",
                c"respond() called with a detached view buffer",
            ));
        }
        // Step 3: Assert: `this`.`[[view]]`.[[ByteLength]] > 0.
        debug_assert!(view.byte_length() > 0);
        // Step 4: Assert: `this`.`[[view]]`.[[ViewedArrayBuffer]].[[ByteLength]] > 0.
        debug_assert!(view.viewed_buffer(scope)?.byte_length() > 0);
        // Step 5: Perform ? `ByteStreamControllerRespond`(`this`.`[[controller]]`,
        //         _bytesWritten_).
        super::algorithms::readable_byte_stream_controller_respond(
            scope,
            &controller,
            bytes_written,
        )
    }

    /// <https://streams.spec.whatwg.org/#rs-byob-request-respond-with-new-view>
    #[method]
    fn respond_with_new_view(
        &self,
        scope: &Scope<'_>,
        view: js::ArrayBufferView<'_>,
    ) -> Result<(), ExnThrown> {
        // Step 1: If `this`.`[[controller]]` is undefined, throw a ``TypeError`` exception.
        let controller = match self.data().controller.get(scope) {
            Some(c) => c,
            None => {
                return Err(crate::algorithms::throw_invalid_state(
                    scope,
                    "ReadableStreamBYOBRequest",
                    c"respondWithNewView() called on an invalidated BYOB request",
                ));
            }
        };
        // Step 2: If ! `IsDetachedBuffer`(_view_.[[ViewedArrayBuffer]]) is true, throw a
        //         ``TypeError`` exception.
        if view.viewed_buffer(scope)?.is_detached() {
            return Err(crate::algorithms::throw_invalid_state(
                scope,
                "ReadableStreamBYOBRequest",
                c"respondWithNewView() called with a detached view buffer",
            ));
        }
        // Step 3: Return ?
        //         `ByteStreamControllerRespondWithNewView`(`this`.`[[controller]]`,
        //         _view_).
        super::algorithms::readable_byte_stream_controller_respond_with_new_view(
            scope,
            &controller,
            view,
        )
    }
}
