// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `crypto` global's random number generation: `getRandomValues` and `randomUUID`.
//!
//! <https://w3c.github.io/webcrypto/#crypto-interface>. `subtle` isn't implemented.

use core_runtime::{webidl_interface, webidl_methods};
use js::gc::scope::Scope;
use js::Object;

use crate::dom_exception::DOMExceptionError;

#[webidl_interface]
pub struct Crypto {}

/// A version 4 UUID from 16 random bytes, in the lowercase hyphenated form.
pub fn random_uuid() -> String {
    platform::random::uuid_v4()
}

/// The message of the `TypeMismatchError` that [`get_random_values`] throws, Node.js's message.
pub const TYPE_MISMATCH_MESSAGE: &str = "The data argument must be an integer-type TypedArray";

/// <https://w3c.github.io/webcrypto/#Crypto-method-getRandomValues>, with Node.js's error
/// messages.
pub fn get_random_values(
    array: js::ArrayBufferView<'_>,
) -> Result<js::ArrayBufferView<'_>, DOMExceptionError> {
    // Step 1: If array is not an Int8Array, Uint8Array, Uint8ClampedArray, Int16Array,
    // Uint16Array, Int32Array, Uint32Array, BigInt64Array, or BigUint64Array, then throw a
    // TypeMismatchError and terminate the algorithm.
    if !array.view_kind().is_integer_array() {
        return Err(DOMExceptionError::new(
            "TypeMismatchError",
            TYPE_MISMATCH_MESSAGE,
        ));
    }
    // Step 2: Let byteLength be array's byte length.
    // Step 3: If byteLength is greater than 65536, throw a QuotaExceededError and terminate
    // the algorithm.
    if array.byte_length() > 65536 {
        return Err(DOMExceptionError::new(
            "QuotaExceededError",
            "The requested length exceeds 65,536 bytes",
        ));
    }
    // Step 4: Let byteList be a byte sequence containing the result of generating byteLength
    // random bytes.
    // Step 5: Write byteList to array, starting at array's byte offset.
    // SAFETY: filling the bytes runs no JS, so nothing can GC or detach the buffer.
    platform::random::fill(unsafe { array.bytes_mut() });
    // Step 6: Return array.
    Ok(array)
}

#[webidl_methods]
impl Crypto {
    /// <https://w3c.github.io/webcrypto/#Crypto-method-getRandomValues>
    #[method]
    fn get_random_values(
        &self,
        array: js::ArrayBufferView<'s>,
    ) -> Result<js::ArrayBufferView<'s>, DOMExceptionError> {
        get_random_values(array)
    }

    /// <https://w3c.github.io/webcrypto/#Crypto-method-randomUUID>
    #[method(name = "randomUUID")]
    fn random_uuid(&self) -> String {
        random_uuid()
    }
}

/// The realm object marker of the `crypto` instance [`add_to_global`] installs.
struct CryptoInstance;

/// The `crypto` instance [`add_to_global`] installed on the current global, even if the global's
/// `crypto` property was reassigned since.
pub fn crypto_object<'s>(scope: &'s Scope<'_>) -> Option<Object<'s>> {
    js::class::realm_object::<CryptoInstance>(scope)
}

/// Register the `Crypto` class on `global` and install the `crypto` instance on it.
pub fn add_to_global<'s>(scope: &'s Scope<'_>, global: Object<'s>) {
    Crypto::add_to_global(scope, global);
    let crypto = js::class::create_instance_with::<CryptoImpl>(scope, |_| CryptoImpl {})
        .expect("failed to allocate the crypto object");
    js::class::register_realm_object::<CryptoInstance>(scope, *crypto);
    global
        .set_property(scope, c"crypto", crypto)
        .expect("failed to define globalThis.crypto");
}
