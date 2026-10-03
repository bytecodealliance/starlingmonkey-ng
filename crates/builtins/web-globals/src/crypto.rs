// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `crypto` global's random number generation: `getRandomValues` and `randomUUID`.
//!
//! <https://w3c.github.io/webcrypto/#crypto-interface>. `subtle` isn't implemented.

use core_runtime::{webidl_interface, webidl_methods};
use js::gc::scope::Scope;
use js::typedarray::ViewKind;
use js::Object;

use crate::dom_exception::DOMExceptionError;

#[webidl_interface]
pub struct Crypto {}

/// A version 4 UUID from 16 random bytes, in the lowercase hyphenated form.
pub fn random_uuid() -> String {
    platform::random::uuid_v4()
}

#[webidl_methods]
impl Crypto {
    /// <https://w3c.github.io/webcrypto/#Crypto-method-getRandomValues>
    #[method]
    fn get_random_values(
        &self,
        array: js::ArrayBufferView<'s>,
    ) -> Result<js::ArrayBufferView<'s>, DOMExceptionError> {
        // Step 1: If array is not an Int8Array, Uint8Array, Uint8ClampedArray, Int16Array,
        // Uint16Array, Int32Array, Uint32Array, BigInt64Array, or BigUint64Array, then throw a
        // TypeMismatchError and terminate the algorithm.
        let integer = matches!(
            array.view_kind(),
            ViewKind::Int8
                | ViewKind::Uint8
                | ViewKind::Uint8Clamped
                | ViewKind::Int16
                | ViewKind::Uint16
                | ViewKind::Int32
                | ViewKind::Uint32
                | ViewKind::BigInt64
                | ViewKind::BigUint64
        );
        if !integer {
            return Err(DOMExceptionError::new(
                "TypeMismatchError",
                "The array must be an integer typed array",
            ));
        }
        // Step 2: Let byteLength be array's byte length.
        // Step 3: If byteLength is greater than 65536, throw a QuotaExceededError and terminate
        // the algorithm.
        if array.byte_length() > 65536 {
            return Err(DOMExceptionError::new(
                "QuotaExceededError",
                "The array's byte length exceeds 65536",
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

    /// <https://w3c.github.io/webcrypto/#Crypto-method-randomUUID>
    #[method(name = "randomUUID")]
    fn random_uuid(&self) -> String {
        random_uuid()
    }
}

/// Register the `Crypto` class on `global` and install the `crypto` instance on it.
pub fn add_to_global<'s>(scope: &'s Scope<'_>, global: Object<'s>) {
    Crypto::add_to_global(scope, global);
    let crypto = js::class::create_instance_with::<CryptoImpl>(scope, |_| CryptoImpl {})
        .expect("failed to allocate the crypto object");
    global
        .set_property(scope, c"crypto", crypto)
        .expect("failed to define globalThis.crypto");
}
