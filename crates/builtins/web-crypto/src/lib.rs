// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Web Cryptography API
//! <https://w3c.github.io/webcrypto/>
//!
//! Provides the `crypto` global (`Crypto`), `SubtleCrypto` and `CryptoKey`. Supported so far:
//! `getRandomValues`, `randomUUID`, `subtle.digest`, and `subtle.importKey`, `subtle.sign` and
//! `subtle.verify` for HMAC keys (in the "raw" and "jwk" formats) and Ed25519 keys (in every
//! format).

mod algorithm;
mod crypto;
mod crypto_key;
mod ed25519;
mod hmac;
mod jwk;
mod subtle_crypto;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    subtle_crypto::SubtleCrypto::add_to_global(scope, global);
    crypto_key::CryptoKey::add_to_global(scope, global);
    crypto::add_to_global(scope, global);
}
