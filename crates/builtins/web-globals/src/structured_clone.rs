// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `structuredClone()` global function.
//!
//! Implements the [structured cloning API] from the HTML specification,
//! which deep-copies a JavaScript value using the structured clone algorithm.
//! Supports an optional `transfer` list for transferring ownership of
//! `ArrayBuffer`s and `MessagePort`s.
//!
//! [structured cloning API]: https://html.spec.whatwg.org/multipage/structured-data.html#dom-structuredclone

/// <https://html.spec.whatwg.org/multipage/structured-data.html#structuredserializeoptions>
#[core_runtime::webidl_dictionary]
pub struct StructuredSerializeOptions<'a> {
    #[webidl(default = Vec::new())]
    pub transfer: Vec<js::Object<'a>>,
}

/// <https://html.spec.whatwg.org/multipage/structured-data.html#dom-structuredclone>
#[core_runtime::jsglobals]
pub mod structured_clone_globals {
    use super::StructuredSerializeOptions;
    use js::error::ExnThrown;
    use js::gc::scope::Scope;
    use js::prelude::HandleValue;

    /// <https://html.spec.whatwg.org/multipage/structured-data.html#dom-structuredclone>
    pub fn structured_clone<'r>(
        scope: &'r Scope<'_>,
        value: HandleValue<'_>,
        options: Option<StructuredSerializeOptions<'_>>,
    ) -> Result<HandleValue<'r>, ExnThrown> {
        let transfer = options.map(|o| o.transfer).unwrap_or_default();
        // Step 1: Let _serialized_ be ? StructuredSerializeWithTransfer(_value_,
        //         _options_["``transfer``"]).
        // Step 2: Let _deserializeRecord_ be ? StructuredDeserializeWithTransfer(_serialized_,
        //         `this`'s `relevant realm`).
        // Step 3: Return _deserializeRecord_.[[Deserialized]].
        crate::messaging::message_port::structured_clone(scope, value, &transfer)
    }
}
