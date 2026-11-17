// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Web Cryptography API
//! <https://w3c.github.io/webcrypto/>
//!
//! Provides the `crypto` global (`Crypto`) and `SubtleCrypto`. Supported so far:
//! `getRandomValues`, `randomUUID`, and `subtle.digest`.

mod algorithm;
mod crypto;
mod subtle_crypto;

use js::gc::scope::Scope;
use js::Object;

pub fn add_to_global(scope: &Scope<'_>, global: Object<'_>) {
    subtle_crypto::SubtleCrypto::add_to_global(scope, global);
    crypto::add_to_global(scope, global);
}
