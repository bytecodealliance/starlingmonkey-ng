// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! <https://w3c.github.io/webcrypto/#cryptokey-interface>

use core_runtime::{webidl_interface, webidl_methods};
use js::conversion::ToJSVal;
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::Object;

use crate::algorithm::HashAlgorithm;
use crate::ed25519::Ed25519Key;
use crate::jwk::JsonWebKey;

// <https://w3c.github.io/webcrypto/#dfn-KeyType>
js::webidl_enum! {
    pub enum KeyType {
        Public => "public",
        Private => "private",
        Secret => "secret",
    }
}

// <https://w3c.github.io/webcrypto/#dfn-KeyUsage>
js::webidl_enum! {
    pub enum KeyUsage {
        Encrypt => "encrypt",
        Decrypt => "decrypt",
        Sign => "sign",
        Verify => "verify",
        DeriveKey => "deriveKey",
        DeriveBits => "deriveBits",
        WrapKey => "wrapKey",
        UnwrapKey => "unwrapKey",
    }
}

// <https://w3c.github.io/webcrypto/#dfn-KeyFormat>
js::webidl_enum! {
    pub enum KeyFormat {
        Raw => "raw",
        Spki => "spki",
        Pkcs8 => "pkcs8",
        Jwk => "jwk",
    }
}

impl KeyUsage {
    /// The recognized key usage values, in the order the spec lists them.
    const RECOGNIZED: [Self; 8] = [
        Self::Encrypt,
        Self::Decrypt,
        Self::Sign,
        Self::Verify,
        Self::DeriveKey,
        Self::DeriveBits,
        Self::WrapKey,
        Self::UnwrapKey,
    ];

    /// The [normalized value] of the usages list _usages_: the [usage intersection] of
    /// _usages_ and a sequence of all [recognized key usage values]. That is each recognized
    /// value that appears in _usages_, once, "in the order listed in the list of recognized
    /// key usage values".
    ///
    /// [normalized value]: https://w3c.github.io/webcrypto/#concept-normalized-usages
    /// [usage intersection]: https://w3c.github.io/webcrypto/#concept-usage-intersection
    /// [recognized key usage values]: https://w3c.github.io/webcrypto/#dfn-RecognizedKeyUsage
    pub fn normalize(usages: &[Self]) -> Vec<Self> {
        Self::RECOGNIZED
            .into_iter()
            .filter(|usage| usages.contains(usage))
            .collect()
    }
}

// `webidl_interface` needs a `Default` for the state of a class without a constructor.
#[allow(clippy::derivable_impls)] // `webidl_enum!` can't derive it.
impl Default for KeyType {
    fn default() -> Self {
        Self::Secret
    }
}

/// The `[[algorithm]]` and `[[handle]]` internal slots of a key: the algorithm's parameters
/// and the underlying key material.
pub enum KeyMaterial {
    /// An HMAC key: its `HmacKeyAlgorithm`'s `hash` and `length`, and the key.
    Hmac {
        hash: HashAlgorithm,
        /// The length of the key in bits.
        length: u64,
        /// The first `length` bits of the imported data. If `length` isn't a multiple of 8,
        /// the unused bits of the last byte are zero.
        key: Vec<u8>,
    },
    /// An Ed25519 key, whose `[[algorithm]]` is a `KeyAlgorithm` with just a `name`.
    Ed25519(Ed25519Key),
}

impl KeyMaterial {
    /// The `name` attribute of the key's `[[algorithm]]`.
    pub fn algorithm_name(&self) -> &'static str {
        match self {
            Self::Hmac { .. } => "HMAC",
            Self::Ed25519(_) => "Ed25519",
        }
    }

    /// The `[[algorithm]]` converted to an ECMAScript object: a `KeyAlgorithm`, or the
    /// dictionary that inherits from it for the algorithm.
    fn algorithm_object<'s>(&self, scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
        let object = Object::new_plain(scope)?;
        object.set_property(scope, c"name", self.algorithm_name())?;
        match self {
            // An `HmacKeyAlgorithm`, whose `hash` is a `KeyAlgorithm`.
            Self::Hmac { hash, length, .. } => {
                let hash_object = Object::new_plain(scope)?;
                hash_object.set_property(scope, c"name", hash.name())?;
                object.set_property(scope, c"hash", hash_object)?;
                object.set_property(scope, c"length", *length as f64)?;
            }
            // A `KeyAlgorithm`, which has only `name`.
            Self::Ed25519(_) => {}
        }
        Ok(object)
    }
}

/// The _keyData_ of `importKey()`, after its steps 4.1-4.2: a `JsonWebKey` for the "jwk"
/// format, and a copy of the bytes for the others.
pub enum KeyData {
    Bytes(Vec<u8>),
    Jwk(Box<JsonWebKey>),
}

/// What an import key operation makes: the `[[type]]`, `[[algorithm]]` and `[[handle]]` of the
/// key. `importKey()` sets `[[extractable]]` and `[[usages]]` itself.
pub struct ImportedKey {
    pub key_type: KeyType,
    pub material: KeyMaterial,
}

/// A key to create as a `CryptoKey`: the contents of its internal slots, as Rust data.
///
/// The import key steps produce this in parallel. The `CryptoKey` object is only created when
/// the task that resolves the promise with it runs.
pub struct NewKey {
    pub key_type: KeyType,
    pub extractable: bool,
    /// Normalized, as by [`KeyUsage::normalize`].
    pub usages: Vec<KeyUsage>,
    pub material: KeyMaterial,
}

/// <https://w3c.github.io/webcrypto/#cryptokey-interface>
#[webidl_interface]
pub struct CryptoKey {
    /// <https://w3c.github.io/webcrypto/#dfn-CryptoKey-slot-type>
    #[no_trace]
    key_type: KeyType,
    /// <https://w3c.github.io/webcrypto/#dfn-CryptoKey-slot-extractable>
    extractable: bool,
    /// <https://w3c.github.io/webcrypto/#dfn-CryptoKey-slot-usages>
    #[no_trace]
    usages: Vec<KeyUsage>,
    /// <https://w3c.github.io/webcrypto/#dfn-CryptoKey-slot-algorithm> and
    /// <https://w3c.github.io/webcrypto/#dfn-CryptoKey-slot-handle>. Always `Some` once the key
    /// is created.
    #[no_trace]
    material: Option<KeyMaterial>,
    /// <https://w3c.github.io/webcrypto/#dfn-CryptoKey-slot-algorithm_cached>
    algorithm_cached: Heap<Value>,
    /// <https://w3c.github.io/webcrypto/#dfn-CryptoKey-slot-usages_cached>
    usages_cached: Heap<Value>,
}

#[webidl_methods]
impl CryptoKey<'_> {
    /// <https://w3c.github.io/webcrypto/#dom-cryptokey-type>
    #[getter(name = "type")]
    fn key_type(&self) -> KeyType {
        self.data().key_type
    }

    /// <https://w3c.github.io/webcrypto/#dom-cryptokey-extractable>
    #[getter]
    fn extractable(&self) -> bool {
        self.data().extractable
    }

    /// <https://w3c.github.io/webcrypto/#dom-cryptokey-algorithm>
    #[getter]
    fn algorithm<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        // Returns the `cached ECMAScript object` associated with the `[[algorithm]]` internal
        // slot:
        // Step 1: If the `[[algorithm_cached]]` internal slot of _object_ is undefined: Set the
        //         `[[algorithm_cached]]` internal slot of _object_ to the result of performing type
        //         conversion to an ECMAScript object as defined in [WebIDL] to the contents of
        //         the `[[algorithm]]` internal slot of _object_.
        //         (Done when the key is created. See `create_crypto_key`.)
        // Step 2: Return the contents of the `[[algorithm_cached]]` internal slot of _object_.
        self.data().algorithm_cached.get(scope)
    }

    /// <https://w3c.github.io/webcrypto/#dom-cryptokey-usages>
    #[getter]
    fn usages<'r>(&self, scope: &'r Scope<'_>) -> HandleValue<'r> {
        // Returns the `cached ECMAScript object` associated with the `[[usages]]` internal
        // slot, which indicates which cryptographic operations are permissible to be used
        // with this key:
        // Step 1: If the `[[usages_cached]]` internal slot of _object_ is undefined: Set the
        //         `[[usages_cached]]` internal slot of _object_ to the result of performing type
        //         conversion to an ECMAScript object as defined in [WebIDL] to the contents of
        //         the `[[usages]]` internal slot of _object_.
        //         (Done when the key is created. See `create_crypto_key`.)
        // Step 2: Return the contents of the `[[usages_cached]]` internal slot of _object_.
        self.data().usages_cached.get(scope)
    }
}

impl CryptoKeyImpl {
    /// Whether the `[[usages]]` internal slot contains _usage_.
    pub fn has_usage(&self, usage: KeyUsage) -> bool {
        self.usages.contains(&usage)
    }

    /// The `[[algorithm]]` and `[[handle]]` internal slots.
    pub fn material(&self) -> &KeyMaterial {
        self.material.as_ref().expect("set when the key is created")
    }
}

/// Create the `CryptoKey` object for _key_.
///
/// The [cached ECMAScript object]s of `[[algorithm]]` and `[[usages]]` are created here rather
/// than on first access, which isn't observable: each is a new object either way, and the same
/// one is returned on every access.
///
/// [cached ECMAScript object]: https://w3c.github.io/webcrypto/#concept-cached-object
pub fn create_crypto_key<'s>(
    scope: &'s Scope<'_>,
    key: NewKey,
) -> Result<CryptoKey<'s>, ExnThrown> {
    let algorithm = key.material.algorithm_object(scope)?;
    let usages = key.usages.to_jsval_throwing(scope)?;
    js::class::try_create_instance_with::<CryptoKeyImpl>(scope, |_| {
        Ok(CryptoKeyImpl {
            key_type: key.key_type,
            extractable: key.extractable,
            usages: key.usages,
            material: Some(key.material),
            algorithm_cached: Heap::from(algorithm.as_value()),
            usages_cached: Heap::from(usages.get()),
        })
    })
}
