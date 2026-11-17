// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/webcrypto/#crypto-interface>

use std::fmt::Write as _;

use core_runtime::{webidl_interface, webidl_methods};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::{ArrayBufferView, Object};
use web_globals::dom_exception::DOMExceptionError;

use crate::subtle_crypto::{SubtleCrypto, SubtleCryptoImpl};

/// <https://w3c.github.io/webcrypto/#crypto-interface>
#[webidl_interface]
pub struct Crypto {
    /// <https://w3c.github.io/webcrypto/#Crypto-attribute-subtle>
    subtle: Heap<SubtleCryptoImpl>,
}

/// The most bytes `getRandomValues` fills in one call.
const MAX_RANDOM_BYTES: usize = 65536;

#[webidl_methods]
impl Crypto<'_> {
    /// <https://w3c.github.io/webcrypto/#Crypto-attribute-subtle>
    #[getter]
    fn subtle<'r>(&self, scope: &'r Scope<'_>) -> SubtleCrypto<'r> {
        // The spec gives no steps: the attribute "provides an instance of the `SubtleCrypto`
        // interface". That instance is created once, with `crypto`, and returned every time.
        self.data().subtle.get(scope)
    }

    /// <https://w3c.github.io/webcrypto/#Crypto-method-getRandomValues>
    #[method]
    fn get_random_values<'r>(
        &self,
        array: ArrayBufferView<'r>,
    ) -> Result<ArrayBufferView<'r>, DOMExceptionError> {
        // Step 1: If _array_ is not an `Int8Array`, `Uint8Array`, `Uint8ClampedArray`,
        //         `Int16Array`, `Uint16Array`, `Int32Array`, `Uint32Array`, `BigInt64Array`, or
        //         `BigUint64Array`, then `throw` a `TypeMismatchError` and terminate the
        //         algorithm.
        use js::typedarray::ViewKind::*;
        if !matches!(
            array.view_kind(),
            Int8 | Uint8 | Uint8Clamped | Int16 | Uint16 | Int32 | Uint32 | BigInt64 | BigUint64
        ) {
            return Err(DOMExceptionError::new(
                "TypeMismatchError",
                "The data argument must be an integer-type TypedArray",
            ));
        }
        // Step 2: Let _byteLength_ be the byte length of _array_.
        // Step 3: If _byteLength_ is greater than 65536, `throw` a `QuotaExceededError` and
        //         terminate the algorithm.
        //         (`throw_dom_exception` turns this name into the `QuotaExceededError` subclass.
        if array.byte_length() > MAX_RANDOM_BYTES {
            return Err(DOMExceptionError::new(
                "QuotaExceededError",
                "The ArrayBufferView's byte length exceeds the number of bytes of entropy available via this API (65536)",
            ));
        }
        // Step 4: Let _bytes_ be a byte sequence of length _byteLength_.
        // Step 5: Fill _bytes_ with cryptographically secure random bytes.
        // Step 6: Write _bytes_ into _array_.
        // (Filling _array_'s own bytes in place is the same without the intermediate copy.)
        let mut result = Ok(());
        array.with_bytes_mut(|bytes| result = getrandom::fill(bytes));
        result.map_err(|e| DOMExceptionError::new("OperationError", e.to_string()))?;
        // Step 7: Return _array_.
        Ok(array)
    }

    /// <https://w3c.github.io/webcrypto/#Crypto-method-randomUUID>
    #[method(name = "randomUUID")]
    fn random_uuid(&self) -> Result<String, DOMExceptionError> {
        // Step 1: Let _bytes_ be a byte sequence of length 16.
        let mut bytes = [0u8; 16];
        // Step 2: Fill _bytes_ with cryptographically secure random bytes.
        getrandom::fill(&mut bytes)
            .map_err(|e| DOMExceptionError::new("OperationError", e.to_string()))?;
        // Step 3: Set the 4 most significant bits of _bytes_[6], which represent the UUID
        //         version, to 0100.
        // Step 4: Set the 2 most significant bits of _bytes_[8], which represent the UUID
        //         variant, to 10.
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        // Step 5: Return the string concatenation of « the hexadecimal representation of
        //         _bytes_[0] through _bytes_[3], "-", that of _bytes_[4] and _bytes_[5], "-",
        //         that of _bytes_[6] and _bytes_[7], "-", that of _bytes_[8] and _bytes_[9], "-",
        //         and that of _bytes_[10] through _bytes_[15] ».
        //         (A "-" goes in before bytes 4, 6, 8 and 10; each byte is two lowercase digits.)
        let mut out = String::with_capacity(36);
        for (i, b) in bytes.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                out.push('-');
            }
            write!(out, "{b:02x}").expect("writing to a String doesn't fail");
        }
        Ok(out)
    }
}

/// Register `Crypto` and `SubtleCrypto` on `global` and install the `crypto` singleton.
pub fn add_to_global<'s>(scope: &'s Scope<'_>, global: Object<'s>) {
    Crypto::add_to_global(scope, global);

    let subtle =
        js::class::create_instance_with::<SubtleCryptoImpl>(scope, |_| SubtleCryptoImpl {})
            .expect("failed to allocate SubtleCrypto singleton");
    let crypto = js::class::create_instance_with::<CryptoImpl>(scope, |_| CryptoImpl {
        subtle: Heap::from(subtle),
    })
    .expect("failed to allocate Crypto singleton");

    global
        .set_property(scope, c"crypto", crypto)
        .expect("failed to define globalThis.crypto");
}
