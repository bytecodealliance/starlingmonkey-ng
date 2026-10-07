// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The parts of `node:crypto` that need no asymmetric keys: hashes, HMACs, secret `KeyObject`s,
//! the key derivation functions PBKDF2, scrypt and HKDF, and random values. See
//! <https://nodejs.org/api/crypto.html>.
//!
//! The digests are MD5, RIPEMD-160, SHA-1, the SHA-2 family including SHA-512/224 and SHA-512/256,
//! and SHA3-224, SHA3-256, SHA3-384 and SHA3-512.
//! `Hash` and `Hmac` objects have `update`, `digest` and, for `Hash`, `copy`, but aren't streams.
//! The key derivation functions derive keys on the JS thread, and their callback forms call back
//! in a later turn of the event loop.

use digest::{DynDigest, Mac};
use js::conversion::{FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{ArrayBufferView, Object};

use crate::buffer::{decode_bytes, encode_string, new_buffer, Encoding};
use crate::errors::{out_of_range_received, validate_function, ErrorKind, NodeError, Thrown};

/// The canonical name of the digest `name`, or `None` for a digest that isn't supported. Names
/// are case-insensitive, and may have an `RSA-` prefix or a hyphen after `SHA`.
fn digest_name(name: &str) -> Option<&'static str> {
    let name = name.to_ascii_lowercase();
    let name = name.strip_prefix("rsa-").unwrap_or(&name);
    Some(match name {
        "md5" => "md5",
        "sha1" | "sha-1" => "sha1",
        "sha224" | "sha-224" => "sha224",
        "sha256" | "sha-256" => "sha256",
        "sha384" | "sha-384" => "sha384",
        "sha512" | "sha-512" => "sha512",
        "sha512-224" | "sha-512/224" | "sha512/224" => "sha512-224",
        "sha512-256" | "sha-512/256" | "sha512/256" => "sha512-256",
        "sha3-224" => "sha3-224",
        "sha3-256" => "sha3-256",
        "sha3-384" => "sha3-384",
        "sha3-512" => "sha3-512",
        "ripemd160" | "ripemd" | "rmd160" | "ripemd-160" => "ripemd160",
        _ => return None,
    })
}

/// Evaluate `$body` with the type `$D` bound to the digest of the canonical digest name `$name`,
/// as [`digest_name`] returns it.
macro_rules! with_digest {
    ($name:expr, |$D:ident| $body:expr) => {
        match $name {
            "md5" => {
                type $D = md5::Md5;
                $body
            }
            "sha1" => {
                type $D = sha1::Sha1;
                $body
            }
            "sha224" => {
                type $D = sha2::Sha224;
                $body
            }
            "sha256" => {
                type $D = sha2::Sha256;
                $body
            }
            "sha384" => {
                type $D = sha2::Sha384;
                $body
            }
            "sha512" => {
                type $D = sha2::Sha512;
                $body
            }
            "sha512-224" => {
                type $D = sha2::Sha512_224;
                $body
            }
            "sha512-256" => {
                type $D = sha2::Sha512_256;
                $body
            }
            "sha3-224" => {
                type $D = sha3::Sha3_224;
                $body
            }
            "sha3-256" => {
                type $D = sha3::Sha3_256;
                $body
            }
            "sha3-384" => {
                type $D = sha3::Sha3_384;
                $body
            }
            "sha3-512" => {
                type $D = sha3::Sha3_512;
                $body
            }
            _ => {
                type $D = ripemd::Ripemd160;
                $body
            }
        }
    };
}

/// A hasher of one of the supported digests, which can be cloned without its type.
trait SendDigest: DynDigest + Send {
    fn clone_boxed(&self) -> Box<dyn SendDigest>;
}

impl<D: DynDigest + Clone + Send + 'static> SendDigest for D {
    fn clone_boxed(&self) -> Box<dyn SendDigest> {
        Box::new(self.clone())
    }
}

/// A new hasher for the digest `name`, or `None` for a digest that isn't supported.
fn hasher(name: &str) -> Option<Box<dyn SendDigest>> {
    let name = digest_name(name)?;
    Some(with_digest!(name, |D| Box::new(D::default())))
}

/// An HMAC of one of the supported digests, which `update` and `finalize` reach without its type.
trait DynMac: Send {
    fn update(&mut self, data: &[u8]);
    fn finalize(self: Box<Self>) -> Vec<u8>;
}

impl<M: Mac + Send + 'static> DynMac for M {
    fn update(&mut self, data: &[u8]) {
        Mac::update(self, data);
    }

    fn finalize(self: Box<Self>) -> Vec<u8> {
        Mac::finalize(*self).into_bytes().to_vec()
    }
}

/// A new HMAC of the digest `name` keyed with `key`, or `None` for a digest that isn't supported.
fn hmac(name: &str, key: &[u8]) -> Option<Box<dyn DynMac>> {
    let name = digest_name(name)?;
    Some(with_digest!(name, |D| Box::new(
        <hmac::Hmac<D> as hmac::KeyInit>::new_from_slice(key)
            .expect("HMAC takes keys of any length")
    )))
}

/// The supported digest names, as `getHashes` lists them, in byte order as Node.js sorts them.
const HASHES: &[&str] = &[
    "RSA-MD5",
    "RSA-RIPEMD160",
    "RSA-SHA1",
    "RSA-SHA224",
    "RSA-SHA256",
    "RSA-SHA3-224",
    "RSA-SHA3-256",
    "RSA-SHA3-384",
    "RSA-SHA3-512",
    "RSA-SHA384",
    "RSA-SHA512",
    "RSA-SHA512/224",
    "RSA-SHA512/256",
    "md5",
    "ripemd",
    "ripemd160",
    "rmd160",
    "sha1",
    "sha224",
    "sha256",
    "sha3-224",
    "sha3-256",
    "sha3-384",
    "sha3-512",
    "sha384",
    "sha512",
    "sha512-224",
    "sha512-256",
];

/// The error `createHash` throws for a digest that isn't supported: a plain `Error`.
fn unsupported_digest(scope: &Scope<'_>, message: &str) -> Thrown {
    let message = std::ffi::CString::new(message).unwrap_or_default();
    js::error::throw_plain_error(scope, &message);
    Thrown::Pending
}

/// `ERR_CRYPTO_INVALID_DIGEST`, the error `createHmac` throws for a digest that isn't supported.
fn invalid_digest(name: &str) -> NodeError {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_CRYPTO_INVALID_DIGEST",
        format!("Invalid digest: {name}"),
    )
}

/// The `outputLength` of the `options` of a `Hash`, checked as Node.js's `Hash` constructor
/// checks it: `None` if `options` isn't an object or the option is `undefined`, and otherwise a
/// uint32 or `ERR_INVALID_ARG_TYPE`/`ERR_OUT_OF_RANGE`.
fn output_length_option(
    scope: &Scope<'_>,
    options: HandleValue<'_>,
) -> Result<Option<f64>, Thrown> {
    let Ok(options) = Object::from_value(scope, *options) else {
        return Ok(None);
    };
    if options.is_callable() {
        return Ok(None);
    }
    let length = options.get_property(scope, c"outputLength")?;
    if length.is_undefined() {
        return Ok(None);
    }
    let name = "options.outputLength";
    Ok(Some(crate::errors::validate_integer(
        scope,
        length,
        name,
        0.0,
        4294967295.0,
    )?))
}

/// Check that the `outputLength` option `length` is the output length of `hasher`, none of whose
/// digests has extendable output. A different length throws OpenSSL's
/// `ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH` error, an `Error` with `library`, `reason` and `code`.
fn check_output_length(
    scope: &Scope<'_>,
    hasher: &dyn SendDigest,
    length: Option<f64>,
) -> Result<(), Thrown> {
    match length {
        Some(length) if length != hasher.output_size() as f64 => {
            let message = "error:030000B2:digital envelope routines::not XOF or invalid length";
            let error = js::error::new_error(scope, js::error::JSExnType::JSEXN_ERR, message)?;
            let object = Object::from_value(scope, *error).map_err(|e| e.throw(scope))?;
            object.set_property(scope, c"library", "digital envelope routines")?;
            object.set_property(scope, c"reason", "not XOF or invalid length")?;
            object.set_property(scope, c"code", "ERR_OSSL_EVP_NOT_XOF_OR_INVALID_LENGTH")?;
            Err(js::exception::throw_value(scope, error).into())
        }
        _ => Ok(()),
    }
}

/// `ERR_CRYPTO_HASH_FINALIZED`.
fn finalized() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_CRYPTO_HASH_FINALIZED",
        "Digest already called",
    )
}

/// The bytes of a string or of a view, as [`data_bytes`] gives them.
enum DataBytes<'s> {
    Encoded(Vec<u8>),
    View(ArrayBufferView<'s>),
}

impl DataBytes<'_> {
    /// Call `f` with the bytes.
    fn with<R>(&self, f: impl FnOnce(&[u8]) -> R + Send) -> R {
        match self {
            DataBytes::Encoded(bytes) => f(bytes),
            DataBytes::View(view) => view.with_bytes(f),
        }
    }
}

/// The bytes of `data`, a string in `encoding` or a `Buffer`, `TypedArray` or `DataView`. A
/// string in an unknown encoding is UTF-8, and one in hex must have an even length.
fn data_bytes<'s>(
    scope: &'s Scope<'_>,
    data: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    name: &str,
) -> Result<DataBytes<'s>, Thrown> {
    if data.is_string() {
        let encoding_value = encoding.filter(|encoding| encoding.is_string());
        let encoding = match encoding_value {
            Some(encoding) => {
                Encoding::parse(&String::from_jsval(scope, encoding, ())?).unwrap_or_default()
            }
            None => Encoding::Utf8,
        };
        let string = js::JSString::from_value(scope, data)?;
        if encoding == Encoding::Hex && string.len() % 2 != 0 {
            let reason = format!("is invalid for data of length {}", string.len());
            let encoding_value = encoding_value.expect("hex is named");
            return Err(
                NodeError::invalid_arg_value(scope, "encoding", encoding_value, &reason).into(),
            );
        }
        return Ok(DataBytes::Encoded(encode_string(scope, string, encoding)?));
    }
    match Object::from_value(scope, *data)
        .ok()
        .and_then(ArrayBufferView::from_object)
    {
        Some(view) => Ok(DataBytes::View(view)),
        None => Err(NodeError::invalid_arg_type(
            scope,
            name,
            "of type string or an instance of Buffer, TypedArray, or DataView",
            data,
        )
        .into()),
    }
}

/// `bytes` as a `Buffer`, or as a string in the encoding `encoding` names. An unknown encoding
/// gives a `Buffer`.
fn output<'s>(
    scope: &'s Scope<'_>,
    bytes: &[u8],
    encoding: Option<HandleValue<'_>>,
) -> Result<HandleValue<'s>, Thrown> {
    match encoding.filter(|encoding| encoding.is_string()) {
        Some(encoding) => {
            let name = String::from_jsval(scope, encoding, ())?;
            match Encoding::parse(&name) {
                Some(encoding) => {
                    Ok(decode_bytes(scope, bytes, encoding)?.to_jsval_throwing(scope)?)
                }
                None => Ok(new_buffer(scope, bytes)?.to_jsval_throwing(scope)?),
            }
        }
        None => Ok(new_buffer(scope, bytes)?.to_jsval_throwing(scope)?),
    }
}

/// The state of a `Hash`.
#[core_runtime::jsclass(hidden)]
pub struct NativeHash {
    /// The hasher, or `None` once `digest` was called.
    #[no_trace]
    hasher: Option<Box<dyn SendDigest>>,
    /// The digest's name, for `copy`.
    #[no_trace]
    algorithm: String,
}

#[core_runtime::jsmethods]
impl NativeHash {}

/// The state of an `Hmac`.
#[core_runtime::jsclass(hidden)]
pub struct NativeHmac {
    /// The HMAC, or `None` once `digest` was called.
    #[no_trace]
    mac: Option<Box<dyn DynMac>>,
}

#[core_runtime::jsmethods]
impl NativeHmac {}

/// Make `this` a `Hash` of the digest `algorithm`, with `hasher` as its state and `options` as
/// its own `_options`.
fn init_hash(
    scope: &Scope<'_>,
    this: Object<'_>,
    hasher: Box<dyn SendDigest>,
    algorithm: String,
    options: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    *hash_class::state_of(scope, this)?.data_mut() = NativeHashImpl {
        hasher: Some(hasher),
        algorithm,
    };
    this.set_property(scope, c"_options", options)
}

/// A new `Hash` of the digest `algorithm`, with `hasher` as its state and `options` as its own
/// `_options`.
fn new_hash<'s>(
    scope: &'s Scope<'_>,
    hasher: Box<dyn SendDigest>,
    algorithm: String,
    options: HandleValue<'_>,
) -> Result<Object<'s>, ExnThrown> {
    let object = hash_class::new_instance(scope)?;
    init_hash(scope, object, hasher, algorithm, options)?;
    Ok(object)
}

/// Make `this` an `Hmac` of the digest `algorithm` keyed with `key`, with `options` as its own
/// `_options`.
fn init_hmac(
    scope: &Scope<'_>,
    this: Object<'_>,
    algorithm: HandleValue<'_>,
    key: HandleValue<'_>,
    options: Option<HandleValue<'_>>,
) -> Result<(), Thrown> {
    let name = crate::errors::validate_string(scope, algorithm, "hmac")?;
    let encoding = string_option(scope, options, c"encoding", "options.encoding")?;
    let key = secret_key_bytes(scope, key, encoding.as_deref())?;
    let mac = hmac(&name, &key).ok_or_else(|| invalid_digest(&name))?;
    *hmac_class::state_of(scope, this)?.data_mut() = NativeHmacImpl { mac: Some(mac) };
    let options = options.unwrap_or(HandleValue::undefined());
    Ok(this.set_property(scope, c"_options", options)?)
}

#[core_runtime::jsfunction_class(
    name = "Hash",
    self_construct,
    state = "NativeHash",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod hash_class {
    use super::*;

    /// <https://nodejs.org/api/crypto.html#class-hash>: a hash of the digest `algorithm`.
    /// Emits DEP0179 the first time.
    #[constructor(length = 2)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        algorithm: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::process::deprecate_once(scope, "crypto.Hash constructor is deprecated.", "DEP0179")?;
        let name = crate::errors::validate_string(scope, algorithm, "algorithm")?;
        let options = options.unwrap_or(HandleValue::undefined());
        let output_length = output_length_option(scope, options)?;
        let Some(hasher) = hasher(&name) else {
            return Err(unsupported_digest(scope, "Digest method not supported"));
        };
        check_output_length(scope, &*hasher, output_length)?;
        Ok(init_hash(scope, this, hasher, name, options)?)
    }

    /// <https://nodejs.org/api/crypto.html#hashupdatedata-inputencoding>
    #[method(length = 2)]
    pub fn update<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        data: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().hasher.is_none() {
            return Err(finalized().into());
        }
        let input = data_bytes(scope, data, encoding, "data")?;
        let mut data = state.data_mut();
        let hasher = data.hasher.as_mut().ok_or_else(finalized)?;
        input.with(|bytes| hasher.update(bytes));
        Ok(this)
    }

    /// <https://nodejs.org/api/crypto.html#hashdigestencoding>
    #[method(length = 1)]
    pub fn digest<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let hasher = state_of(scope, this)?
            .data_mut()
            .hasher
            .take()
            .ok_or_else(finalized)?;
        output(scope, &hasher.finalize(), encoding)
    }

    /// <https://nodejs.org/api/crypto.html#hashcopyoptions>
    #[method(length = 1)]
    pub fn copy<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let (hasher, algorithm) = {
            let state = state_of(scope, this)?;
            let data = state.data();
            let hasher = data.hasher.as_ref().ok_or_else(finalized)?.clone_boxed();
            (hasher, data.algorithm.clone())
        };
        let options = options.unwrap_or(HandleValue::undefined());
        check_output_length(scope, &*hasher, output_length_option(scope, options)?)?;
        Ok(new_hash(scope, hasher, algorithm, options)?)
    }
}

#[core_runtime::jsfunction_class(
    name = "Hmac",
    self_construct,
    state = "NativeHmac",
    invalid_this = "crate::errors::invalid_this"
)]
pub mod hmac_class {
    use super::*;

    /// <https://nodejs.org/api/crypto.html#class-hmac>: an HMAC of the digest `algorithm` keyed
    /// with `key`, a secret `KeyObject` or the bytes of a string in `options.encoding`, a
    /// `Buffer`, a `TypedArray`, a `DataView` or an `ArrayBuffer`. Emits DEP0181 the first time.
    #[constructor(length = 3)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        algorithm: HandleValue<'_>,
        key: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        crate::process::deprecate_once(scope, "crypto.Hmac constructor is deprecated.", "DEP0181")?;
        init_hmac(scope, this, algorithm, key, options)
    }

    /// <https://nodejs.org/api/crypto.html#hmacupdatedata-inputencoding>
    #[method(length = 2)]
    pub fn update<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        data: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = state_of(scope, this)?;
        if state.data().mac.is_none() {
            return Err(finalized().into());
        }
        let input = data_bytes(scope, data, encoding, "data")?;
        let mut data = state.data_mut();
        let mac = data.mac.as_mut().ok_or_else(finalized)?;
        input.with(|bytes| mac.update(bytes));
        Ok(this)
    }

    /// <https://nodejs.org/api/crypto.html#hmacdigestencoding>
    #[method(length = 1)]
    pub fn digest<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let mac = state_of(scope, this)?
            .data_mut()
            .mac
            .take()
            .ok_or_else(finalized)?;
        output(scope, &mac.finalize(), encoding)
    }
}

/// `options[name]` as a string, or `None` if `options` is `undefined` or `null` or the option is
/// `undefined` or `null`, as Node.js's `getStringOption` reads it. A value that isn't a string is
/// `ERR_INVALID_ARG_TYPE` naming `label`.
fn string_option(
    scope: &Scope<'_>,
    options: Option<HandleValue<'_>>,
    name: &'static std::ffi::CStr,
    label: &str,
) -> Result<Option<String>, Thrown> {
    let Some(options) = options.and_then(|options| Object::from_value(scope, *options).ok()) else {
        return Ok(None);
    };
    let value = options.get_property(scope, name)?;
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    Ok(Some(crate::errors::validate_string(scope, value, label)?))
}

/// The bytes of `value`, a string in `encoding`, an `ArrayBuffer`, `SharedArrayBuffer` or
/// `ArrayBufferView`, or `None` for other values. A string is UTF-8 if `encoding` is `None`, empty
/// or `buffer`, and an unknown encoding is `ERR_UNKNOWN_ENCODING`, as `Buffer.from` throws it.
fn byte_source(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    encoding: Option<&str>,
) -> Result<Option<Vec<u8>>, Thrown> {
    if value.is_string() {
        let encoding = match encoding {
            None | Some("" | "buffer") => Encoding::Utf8,
            Some(name) => match Encoding::parse(name) {
                Some(encoding) => encoding,
                None => {
                    let name = name.to_jsval_throwing(scope)?;
                    return Err(crate::buffer::unknown_encoding(scope, name));
                }
            },
        };
        let string = js::JSString::from_value(scope, value)?;
        return Ok(Some(encode_string(scope, string, encoding)?));
    }
    match Object::from_value(scope, *value) {
        Ok(object) => Ok(js::typedarray::copy_buffer_source_bytes(scope, object)?),
        Err(_) => Ok(None),
    }
}

/// The bytes of `key` as Node.js's `prepareSecretKey` takes them for an HMAC: the material of a
/// secret `KeyObject`, or the bytes of a string in `encoding` or of a buffer.
fn secret_key_bytes(
    scope: &Scope<'_>,
    key: HandleValue<'_>,
    encoding: Option<&str>,
) -> Result<Vec<u8>, Thrown> {
    if let Some(key) = key_object(scope, key) {
        let data = key.data();
        if data.kind != KeyKind::Secret {
            return Err(NodeError::new(
                ErrorKind::TypeError,
                "ERR_CRYPTO_INVALID_KEY_OBJECT_TYPE",
                format!(
                    "Invalid key object type {}, expected secret.",
                    data.kind.name()
                ),
            )
            .into());
        }
        return Ok(js::typedarray::copy_slice_or_report(scope, &data.bytes)?);
    }
    byte_source(scope, key, encoding)?.ok_or_else(|| {
        let expected = "of type string or an instance of ArrayBuffer, Buffer, TypedArray, \
                        DataView, KeyObject, or CryptoKey";
        NodeError::invalid_arg_type(scope, "key", expected, key).into()
    })
}

/// The type of a `KeyObject`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum KeyKind {
    #[default]
    Secret,
    Public,
    Private,
}

impl KeyKind {
    /// The kind `name` names, as `KeyObject`'s `type` shows it.
    fn parse(name: &str) -> Option<Self> {
        match name {
            "secret" => Some(KeyKind::Secret),
            "public" => Some(KeyKind::Public),
            "private" => Some(KeyKind::Private),
            _ => None,
        }
    }

    /// The kind's name, as `KeyObject`'s `type` shows it.
    fn name(self) -> &'static str {
        match self {
            KeyKind::Secret => "secret",
            KeyKind::Public => "public",
            KeyKind::Private => "private",
        }
    }
}

/// A `KeyObject`: its type and its material. See <https://nodejs.org/api/crypto.html#class-keyobject>.
#[core_runtime::jsclass(
    name = "KeyObject",
    hidden,
    invalid_this = "crate::errors::invalid_this"
)]
pub struct KeyObject {
    #[no_trace]
    kind: KeyKind,
    #[no_trace]
    bytes: Vec<u8>,
}

/// Node.js's `KeyObject` constructor checks: `ERR_INVALID_ARG_VALUE` for a `kind` other than
/// `secret`, `public` and `private`, and `ERR_INVALID_ARG_TYPE` for any `handle`, as only
/// Node.js's internal key handles are accepted.
fn check_key_object_args(
    scope: &Scope<'_>,
    kind: HandleValue<'_>,
    handle: HandleValue<'_>,
) -> Thrown {
    let parsed = crate::support::rust_string(scope, kind).and_then(|kind| KeyKind::parse(&kind));
    if parsed.is_none() {
        return NodeError::invalid_arg_value(scope, "type", kind, "is invalid").into();
    }
    NodeError::invalid_arg_type(scope, "handle", "of type object", handle).into()
}

#[core_runtime::jsmethods(node_args)]
impl KeyObject {
    /// <https://nodejs.org/api/crypto.html#class-keyobject>: always throws, as only
    /// `createSecretKey` creates keys.
    #[constructor]
    pub fn create(
        scope: &Scope<'_>,
        kind: HandleValue<'_>,
        handle: HandleValue<'_>,
    ) -> Result<Self, Thrown> {
        Err(check_key_object_args(scope, kind, handle))
    }

    /// <https://nodejs.org/api/crypto.html#static-method-keyobjectfromkey>: always throws
    /// `ERR_INVALID_ARG_TYPE`, as `CryptoKey` isn't implemented.
    #[static_method(length = 1)]
    pub fn from(scope: &Scope<'_>, key: Option<HandleValue<'_>>) -> Result<(), Thrown> {
        let key = key.unwrap_or(HandleValue::undefined());
        Err(NodeError::invalid_arg_type(scope, "key", "an instance of CryptoKey", key).into())
    }

    /// <https://nodejs.org/api/crypto.html#keyobjecttype>
    #[getter(name = "type")]
    pub fn kind(&self) -> &'static str {
        self.data().kind.name()
    }

    /// <https://nodejs.org/api/crypto.html#keyobjectequalsotherkeyobject>: whether both keys
    /// have the same type and material.
    #[method(length = 1)]
    pub fn equals(
        &self,
        scope: &Scope<'_>,
        other: Option<HandleValue<'_>>,
    ) -> Result<bool, Thrown> {
        let other = other.unwrap_or(HandleValue::undefined());
        let Some(theirs) = key_object(scope, other) else {
            let expected = "an instance of KeyObject";
            return Err(
                NodeError::invalid_arg_type(scope, "otherKeyObject", expected, other).into(),
            );
        };
        let (mine, theirs) = (self.data(), theirs.data());
        Ok(mine.kind == theirs.kind && mine.bytes == theirs.bytes)
    }
}

/// A secret `KeyObject`. See <https://nodejs.org/api/crypto.html#class-keyobject>.
#[core_runtime::jsclass(
    name = "SecretKeyObject",
    hidden,
    extends = KeyObject,
    invalid_this = "crate::errors::invalid_this"
)]
pub struct SecretKeyObject {
    parent: KeyObjectImpl,
}

#[core_runtime::jsmethods(node_args)]
impl SecretKeyObject {
    /// Always throws, as only `createSecretKey` creates keys.
    #[constructor]
    pub fn create(scope: &Scope<'_>, handle: Option<HandleValue<'_>>) -> Result<Self, Thrown> {
        let secret = js::class::get_or_init_atom(scope, c"secret")?;
        let handle = handle.unwrap_or(HandleValue::undefined());
        Err(check_key_object_args(scope, secret, handle))
    }

    /// <https://nodejs.org/api/crypto.html#keyobjectsymmetrickeysize>
    #[getter]
    pub fn symmetric_key_size(&self) -> f64 {
        self.data().parent.bytes.len() as f64
    }

    /// <https://nodejs.org/api/crypto.html#keyobjectexportoptions>: the key's bytes as a
    /// `Buffer`, or as a JSON Web Key if `options.format` is `jwk`.
    #[method]
    pub fn export<'r>(
        &self,
        scope: &'r Scope<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'r>, Thrown> {
        let mut jwk = false;
        if let Some(options) = options.filter(|options| !options.is_undefined()) {
            let options = crate::errors::validate_plain_object(scope, options, "options", false)?;
            let format = options.get_property(scope, c"format")?;
            match crate::support::rust_string(scope, format).as_deref() {
                _ if format.is_undefined() => {}
                Some("buffer") => {}
                Some("jwk") => jwk = true,
                _ => {
                    let reason = "must be one of: undefined, 'buffer', 'jwk'";
                    return Err(NodeError::invalid_arg_value(
                        scope,
                        "options.format",
                        format,
                        reason,
                    )
                    .into());
                }
            }
        }
        let bytes = js::typedarray::copy_slice_or_report(scope, &self.data().parent.bytes)?;
        if jwk {
            let jwk = Object::new_plain(scope)?;
            jwk.set_property(scope, c"kty", "oct")?;
            let k = decode_bytes(scope, &bytes, Encoding::Base64Url)?;
            jwk.set_property(scope, c"k", k)?;
            return Ok(jwk.to_jsval_throwing(scope)?);
        }
        Ok(crate::buffer::buffer_from_vec(scope, bytes)?.to_jsval_throwing(scope)?)
    }
}

/// `value` as a `KeyObject`, if it is one.
pub(crate) fn key_object<'s>(
    scope: &'s Scope<'_>,
    value: HandleValue<'_>,
) -> Option<KeyObject<'s>> {
    Object::from_value(scope, *value)
        .ok()
        .and_then(|object| object.cast::<KeyObject>().ok())
}

/// A new secret `KeyObject` of `bytes`.
fn new_secret_key<'s>(scope: &'s Scope<'_>, bytes: Vec<u8>) -> Result<Object<'s>, ExnThrown> {
    let key =
        js::class::create_instance_with::<SecretKeyObjectImpl>(scope, |_| SecretKeyObjectImpl {
            parent: KeyObjectImpl {
                kind: KeyKind::Secret,
                bytes,
            },
        })?;
    Ok(key.as_object())
}

/// Register the classes of `node:crypto`, and define `Symbol.toStringTag` on
/// `KeyObject.prototype`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    NativeHash::add_to_global(scope, global);
    NativeHmac::add_to_global(scope, global);
    KeyObject::add_to_global(scope, global);
    SecretKeyObject::add_to_global(scope, global);
    hash_class::register(scope)?;
    hmac_class::register(scope)?;
    let proto = js::class::get_prototype_object_for::<KeyObjectImpl>(scope)
        .expect("KeyObject was registered above");
    js::class::define_well_known_symbol_property(
        scope,
        proto,
        js::native::SymbolCode::toStringTag,
        "KeyObject",
    )
}

/// Node.js's `assertSize`: `value`, a number of elements of `element_size` bytes, as a byte count
/// from 0 to 2^31 - 1, not yet rounded down.
fn validate_size(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    element_size: usize,
) -> Result<f64, Thrown> {
    if !value.is_number() {
        return Err(NodeError::invalid_arg_type(scope, name, "of type number", value).into());
    }
    let number = value.to_number() * element_size as f64;
    if !(0.0..=2147483647.0).contains(&number) {
        return Err(NodeError::out_of_range(
            name,
            ">= 0 && <= 2147483647",
            &out_of_range_received(number),
        )
        .into());
    }
    Ok(number)
}

/// Call `callback` with `null` and `value` in a later turn of the event loop, as `setImmediate`
/// does.
fn defer(scope: &Scope<'_>, callback: Object<'_>, value: HandleValue<'_>) -> Result<(), ExnThrown> {
    let null = scope.root_value(js::value::null());
    let callback = callback.to_jsval_throwing(scope)?;
    crate::timers::create_immediate(scope, callback, &[null, value])?;
    Ok(())
}

/// The failure of a key derivation.
enum DeriveError {
    /// An error Node.js throws before it starts the derivation, even for the callback forms.
    Invalid(NodeError),
    /// OpenSSL's failure to derive the key, which Node.js reports as a plain `Error` with the
    /// message "Deriving bits failed", thrown by the synchronous forms and passed to the callback
    /// by the others.
    Failed,
    /// A failed allocation of the derivation's memory, which is reported as `Failed` is, with
    /// OpenSSL's message for a failed allocation.
    OutOfMemory,
}

impl DeriveError {
    /// The message of the plain `Error` of `Failed` and `OutOfMemory`.
    fn message(&self) -> &'static std::ffi::CStr {
        match self {
            DeriveError::OutOfMemory => c"error:078C0100:common libcrypto routines::malloc failure",
            _ => c"Deriving bits failed",
        }
    }
}

impl From<NodeError> for DeriveError {
    fn from(error: NodeError) -> Self {
        DeriveError::Invalid(error)
    }
}

/// A `Vec` with room for `length` bytes, or `DeriveError::OutOfMemory` if allocating it fails.
fn derived_key_buffer(length: usize) -> Result<Vec<u8>, DeriveError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| DeriveError::OutOfMemory)?;
    Ok(output)
}

/// PBKDF2 as RFC 8018 defines it, with HMAC of the digest `name` as the pseudorandom function.
fn pbkdf2_bytes(
    name: &str,
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    length: usize,
) -> Result<Vec<u8>, DeriveError> {
    let digest = digest_name(name).ok_or_else(|| invalid_digest(name))?;
    if length == 0 {
        return Err(DeriveError::Failed);
    }
    let mut output = derived_key_buffer(length)?;
    output.resize(length, 0);
    with_digest!(digest, |D| pbkdf2::pbkdf2_hmac::<D>(
        password,
        salt,
        iterations,
        &mut output
    ));
    Ok(output)
}

/// HKDF as RFC 5869 defines it, with HMAC of the digest `name`. A length above 255 times the
/// digest's size is `ERR_CRYPTO_INVALID_KEYLEN`.
fn hkdf_bytes(
    name: &str,
    key: &[u8],
    salt: &[u8],
    info: &[u8],
    length: usize,
) -> Result<Vec<u8>, DeriveError> {
    let digest = digest_name(name).ok_or_else(|| invalid_digest(name))?;
    let size = with_digest!(digest, |D| <D as digest::Digest>::output_size());
    if length > 255 * size {
        return Err(NodeError::new(
            ErrorKind::RangeError,
            "ERR_CRYPTO_INVALID_KEYLEN",
            "Invalid key length",
        )
        .into());
    }
    if length == 0 {
        return Err(DeriveError::Failed);
    }
    let mut output = derived_key_buffer(length)?;
    output.resize(length, 0);
    with_digest!(digest, |D| hkdf::Hkdf::<D>::new(Some(salt), key)
        .expand(info, &mut output)
        .expect("the length is at most 255 times the digest's size"));
    Ok(output)
}

/// The parameters of scrypt, as Node.js's `scrypt` options give them.
#[derive(Clone, Copy)]
struct ScryptParams {
    cost: u64,
    block_size: u64,
    parallelization: u64,
    max_memory: f64,
}

impl Default for ScryptParams {
    fn default() -> Self {
        ScryptParams {
            cost: 16384,
            block_size: 8,
            parallelization: 1,
            max_memory: (32 << 20) as f64,
        }
    }
}

/// `ERR_CRYPTO_INVALID_SCRYPT_PARAMS`, with OpenSSL's reason if it has one.
fn invalid_scrypt_params(memory: bool) -> NodeError {
    let message = if memory {
        "Invalid scrypt params: error:030000AC:digital envelope routines::memory limit exceeded"
    } else {
        "Invalid scrypt params"
    };
    NodeError::new(
        ErrorKind::RangeError,
        "ERR_CRYPTO_INVALID_SCRYPT_PARAMS",
        message,
    )
}

/// Check `params` as OpenSSL's `EVP_PBE_scrypt_ex` does: `N` must be a power of two of at least
/// 2, `r` and `p` must not be 0, `p * r` must be below 2^30, `N` below 2^(16 * r), the `p` blocks
/// of `128 * r` bytes at most `i32::MAX` bytes, and those blocks with the `32 * r * (N + 2)` words
/// of the mixing vectors at most `maxmem` bytes, or the address space if that is smaller.
fn check_scrypt_params(params: &ScryptParams) -> Result<(), NodeError> {
    let ScryptParams {
        cost: n,
        block_size: r,
        parallelization: p,
        max_memory,
    } = *params;
    if r == 0 || p == 0 || n < 2 || n & (n - 1) != 0 {
        return Err(invalid_scrypt_params(false));
    }
    const PR_MAX: u64 = (1 << 30) - 1;
    if p > PR_MAX / r || (16 * r <= 63 && n >= 1 << (16 * r)) {
        return Err(invalid_scrypt_params(true));
    }
    let blocks = p * 128 * r;
    if blocks > i32::MAX as u64 || n + 2 > u64::MAX / 128 / r {
        return Err(invalid_scrypt_params(true));
    }
    let vectors = 32 * r * (n + 2) * 4;
    let max_memory = (max_memory as u64).min(usize::MAX as u64);
    match blocks.checked_add(vectors) {
        Some(total) if total <= max_memory => Ok(()),
        _ => Err(invalid_scrypt_params(true)),
    }
}

/// The Salsa20/8 core of RFC 7914 applied to `block` in place.
fn salsa20_8(block: &mut [u32; 16]) {
    use salsa20::cipher::StreamCipherCore;
    let mut output =
        salsa20::cipher::Block::<salsa20::SalsaCore<salsa20::cipher::consts::U4>>::default();
    salsa20::SalsaCore::<salsa20::cipher::consts::U4>::from_raw_state(*block)
        .write_keystream_block(&mut output);
    for (word, bytes) in block.iter_mut().zip(output.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*bytes);
    }
}

/// scryptBlockMix of RFC 7914 on the `2 * r` blocks of 16 words in `input`, written to `output`.
fn block_mix(input: &[u32], output: &mut [u32], r: usize) {
    let mut x: [u32; 16] = input[(2 * r - 1) * 16..].try_into().expect("16 words");
    for i in 0..2 * r {
        for (word, other) in x.iter_mut().zip(&input[i * 16..(i + 1) * 16]) {
            *word ^= other;
        }
        salsa20_8(&mut x);
        let target = if i % 2 == 0 { i / 2 } else { r + i / 2 };
        output[target * 16..(target + 1) * 16].copy_from_slice(&x);
    }
}

/// scryptROMix of RFC 7914 on `block`, `128 * r` bytes, with the cost `n`. Fails if allocating
/// the `32 * r * (n + 2)` words it uses fails.
fn ro_mix(block: &mut [u8], r: usize, n: usize) -> Result<(), DeriveError> {
    let words = 32 * r;
    let zeroed = |length: Option<usize>| {
        length
            .and_then(crate::support::try_filled_vec::<u32>)
            .ok_or(DeriveError::OutOfMemory)
    };
    let mut x = zeroed(Some(words))?;
    for (word, chunk) in x.iter_mut().zip(block.as_chunks::<4>().0) {
        *word = u32::from_le_bytes(*chunk);
    }
    let mut v = zeroed(words.checked_mul(n))?;
    let mut y = zeroed(Some(words))?;
    for i in 0..n {
        v[i * words..(i + 1) * words].copy_from_slice(&x);
        block_mix(&x, &mut y, r);
        std::mem::swap(&mut x, &mut y);
    }
    for _ in 0..n {
        let last = (2 * r - 1) * 16;
        let j = (u64::from(x[last]) | (u64::from(x[last + 1]) << 32)) as usize & (n - 1);
        for (word, other) in x.iter_mut().zip(&v[j * words..(j + 1) * words]) {
            *word ^= other;
        }
        block_mix(&x, &mut y, r);
        std::mem::swap(&mut x, &mut y);
    }
    for (chunk, word) in block.as_chunks_mut::<4>().0.iter_mut().zip(x) {
        *chunk = word.to_le_bytes();
    }
    Ok(())
}

/// scrypt as RFC 7914 defines it, with parameters [`check_scrypt_params`] accepted.
fn scrypt_bytes(
    password: &[u8],
    salt: &[u8],
    params: &ScryptParams,
    length: usize,
) -> Result<Vec<u8>, DeriveError> {
    check_scrypt_params(params)?;
    if length == 0 {
        return Ok(Vec::new());
    }
    let r = params.block_size as usize;
    let p = params.parallelization as usize;
    let mut blocks = pbkdf2_bytes("sha256", password, salt, 1, p * 128 * r)?;
    for block in blocks.chunks_exact_mut(128 * r) {
        ro_mix(block, r, params.cost as usize)?;
    }
    pbkdf2_bytes("sha256", password, &blocks, 1, length)
}

/// The bytes of the argument `value`, named `name`, as Node.js's `getArrayBufferOrView` takes
/// them: a string in UTF-8 or a buffer.
fn array_buffer_or_view(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
) -> Result<Vec<u8>, Thrown> {
    byte_source(scope, value, None)?.ok_or_else(|| {
        let expected =
            "of type string or an instance of ArrayBuffer, Buffer, TypedArray, or DataView";
        NodeError::invalid_arg_type(scope, name, expected, value).into()
    })
}

/// Node.js's `validateInt32` of the argument `value`, named `name`, with the minimum `min`.
fn validate_int32(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    min: f64,
) -> Result<f64, Thrown> {
    crate::errors::validate_integer(scope, value, name, min, 2147483647.0)
}

/// Node.js's `validateUint32` of the argument `value`, named `name`.
fn validate_uint32(scope: &Scope<'_>, value: HandleValue<'_>, name: &str) -> Result<u64, Thrown> {
    Ok(crate::errors::validate_integer(scope, value, name, 0.0, 4294967295.0)? as u64)
}

/// The scrypt parameters `options` gives, checked as Node.js's `scrypt` checks them.
fn scrypt_options(scope: &Scope<'_>, options: HandleValue<'_>) -> Result<ScryptParams, Thrown> {
    let mut params = ScryptParams::default();
    let Ok(options) = Object::from_value(scope, *options) else {
        return Ok(params);
    };
    let pairs: [(&std::ffi::CStr, &str, &std::ffi::CStr, &str); 3] = [
        (c"N", "N", c"cost", "cost"),
        (c"r", "r", c"blockSize", "blockSize"),
        (c"p", "p", c"parallelization", "parallelization"),
    ];
    for (index, (short, short_name, long, long_name)) in pairs.into_iter().enumerate() {
        let mut value = None;
        let given = options.get_property(scope, short)?;
        if !given.is_undefined() {
            value = Some(validate_uint32(scope, given, short_name)?);
        }
        let given_long = options.get_property(scope, long)?;
        if !given_long.is_undefined() {
            if value.is_some() {
                return Err(NodeError::new(
                    ErrorKind::TypeError,
                    "ERR_INCOMPATIBLE_OPTION_PAIR",
                    format!(
                        "Option \"{short_name}\" cannot be used in combination with option \
                         \"{long_name}\""
                    ),
                )
                .into());
            }
            value = Some(validate_uint32(scope, given_long, long_name)?);
        }
        if let Some(value) = value.filter(|value| *value != 0) {
            match index {
                0 => params.cost = value,
                1 => params.block_size = value,
                _ => params.parallelization = value,
            }
        }
    }
    let max_memory = options.get_property(scope, c"maxmem")?;
    if !max_memory.is_undefined() {
        let max_memory = crate::errors::validate_integer(
            scope,
            max_memory,
            "maxmem",
            0.0,
            crate::errors::MAX_SAFE_INTEGER,
        )?;
        if max_memory != 0.0 {
            params.max_memory = max_memory;
        }
    }
    Ok(params)
}

/// Return the result of a synchronous key derivation as a `Buffer`, or as an `ArrayBuffer` if
/// `array_buffer`.
fn derived_sync<'s>(
    scope: &'s Scope<'_>,
    result: Result<Vec<u8>, DeriveError>,
    array_buffer: bool,
) -> Result<HandleValue<'s>, Thrown> {
    match result {
        Ok(bytes) if array_buffer => {
            Ok(js::ArrayBuffer::with_data(scope, &bytes)?.to_jsval_throwing(scope)?)
        }
        Ok(bytes) => Ok(new_buffer(scope, &bytes)?.to_jsval_throwing(scope)?),
        Err(DeriveError::Invalid(error)) => Err(error.into()),
        Err(error) => {
            js::error::throw_plain_error(scope, error.message());
            Err(Thrown::Pending)
        }
    }
}

/// Pass the result of a key derivation to `callback` in a later turn of the event loop: `null`
/// and a `Buffer`, or an `ArrayBuffer` if `array_buffer`, or the error of a failed derivation.
/// An [`DeriveError::Invalid`] error is thrown instead.
fn derived_async(
    scope: &Scope<'_>,
    result: Result<Vec<u8>, DeriveError>,
    callback: Object<'_>,
    array_buffer: bool,
) -> Result<(), Thrown> {
    let value = match result {
        Ok(bytes) if array_buffer => {
            js::ArrayBuffer::with_data(scope, &bytes)?.to_jsval_throwing(scope)?
        }
        Ok(bytes) => new_buffer(scope, &bytes)?.to_jsval_throwing(scope)?,
        Err(DeriveError::Invalid(error)) => return Err(error.into()),
        Err(error) => {
            js::error::throw_plain_error(scope, error.message());
            let error = js::exception::take_pending_or_undefined(scope);
            let callback = callback.to_jsval_throwing(scope)?;
            crate::timers::create_immediate(scope, callback, &[error])?;
            return Ok(());
        }
    };
    Ok(defer(scope, callback, value)?)
}

/// The arguments of `pbkdf2` and `pbkdf2Sync`, checked as Node.js's `check` in
/// `lib/internal/crypto/pbkdf2.js` checks them, and a function that derives the key.
fn pbkdf2_args(
    scope: &Scope<'_>,
    password: HandleValue<'_>,
    salt: HandleValue<'_>,
    iterations: HandleValue<'_>,
    keylen: HandleValue<'_>,
    digest: HandleValue<'_>,
) -> Result<impl FnOnce() -> Result<Vec<u8>, DeriveError>, Thrown> {
    let digest = crate::errors::validate_string(scope, digest, "digest")?;
    let password = array_buffer_or_view(scope, password, "password")?;
    let salt = array_buffer_or_view(scope, salt, "salt")?;
    let iterations = validate_int32(scope, iterations, "iterations", 1.0)?;
    let keylen = validate_int32(scope, keylen, "keylen", 0.0)?;
    Ok(move || {
        pbkdf2_bytes(
            &digest,
            &password,
            &salt,
            iterations as u32,
            keylen as usize,
        )
    })
}

/// The arguments of `scrypt` and `scryptSync`, checked as Node.js's `check` in
/// `lib/internal/crypto/scrypt.js` checks them, and a function that derives the key.
fn scrypt_args(
    scope: &Scope<'_>,
    password: HandleValue<'_>,
    salt: HandleValue<'_>,
    keylen: HandleValue<'_>,
    options: HandleValue<'_>,
) -> Result<impl FnOnce() -> Result<Vec<u8>, DeriveError>, Thrown> {
    let password = array_buffer_or_view(scope, password, "password")?;
    let salt = array_buffer_or_view(scope, salt, "salt")?;
    let keylen = validate_int32(scope, keylen, "keylen", 0.0)?;
    let params = scrypt_options(scope, options)?;
    Ok(move || scrypt_bytes(&password, &salt, &params, keylen as usize))
}

/// The arguments of `hkdf` and `hkdfSync`, checked as Node.js's `validateParameters` in
/// `lib/internal/crypto/hkdf.js` checks them, and a function that derives the key.
fn hkdf_args(
    scope: &Scope<'_>,
    digest: HandleValue<'_>,
    key: HandleValue<'_>,
    salt: HandleValue<'_>,
    info: HandleValue<'_>,
    length: HandleValue<'_>,
) -> Result<impl FnOnce() -> Result<Vec<u8>, DeriveError>, Thrown> {
    let digest = crate::errors::validate_string(scope, digest, "digest")?;
    let key = match key_object(scope, key) {
        Some(key) => js::typedarray::copy_slice_or_report(scope, &key.data().bytes)?,
        None => byte_source(scope, key, None)?.ok_or_else(|| {
            let expected = "of type string or an instance of SecretKeyObject, ArrayBuffer, \
                            TypedArray, DataView, or Buffer";
            NodeError::invalid_arg_type(scope, "ikm", expected, key)
        })?,
    };
    let byte_source_arg = |value: HandleValue<'_>, name: &str| -> Result<Vec<u8>, Thrown> {
        byte_source(scope, value, None)?.ok_or_else(|| {
            let expected =
                "of type string or an instance of ArrayBuffer, TypedArray, DataView, or Buffer";
            NodeError::invalid_arg_type(scope, name, expected, value).into()
        })
    };
    let salt = byte_source_arg(salt, "salt")?;
    let info = byte_source_arg(info, "info")?;
    let length = crate::errors::validate_integer(scope, length, "length", 0.0, 2147483647.0)?;
    if info.len() > 1024 {
        return Err(NodeError::out_of_range(
            "info",
            "must not contain more than 1024 bytes",
            &out_of_range_received(info.len() as f64),
        )
        .into());
    }
    Ok(move || hkdf_bytes(&digest, &key, &salt, &info, length as usize))
}

/// A view of the bytes `randomFill` and `randomFillSync` fill in `buffer`, and the size of its
/// elements: `buffer` itself if it is an `ArrayBufferView`, and a `Uint8Array` over it if it is an
/// `ArrayBuffer` or a `SharedArrayBuffer`. Other values are `ERR_INVALID_ARG_TYPE`.
fn fill_target<'s>(
    scope: &'s Scope<'_>,
    buffer: HandleValue<'_>,
) -> Result<(ArrayBufferView<'s>, usize), Thrown> {
    if let Ok(object) = Object::from_value(scope, *buffer) {
        if let Some(view) = ArrayBufferView::from_object(object) {
            return Ok((view, view.view_kind().element_size()));
        }
        let whole = if let Ok(array_buffer) = object.cast::<js::ArrayBuffer>() {
            Some(js::Uint8Array::with_buffer_to_end(scope, array_buffer, 0)?)
        } else if let Ok(shared) = object.cast::<js::SharedArrayBuffer>() {
            Some(js::Uint8Array::with_buffer_to_end(scope, shared, 0)?)
        } else {
            None
        };
        if let Some(whole) = whole {
            return Ok((whole.as_array_buffer_view(), 1));
        }
    }
    let expected = "an instance of ArrayBuffer or ArrayBufferView";
    Err(NodeError::invalid_arg_type(scope, "buf", expected, buffer).into())
}

/// A random integer from `0` up to, but excluding, `range`, without bias.
fn random_below(range: u64) -> u64 {
    let limit = u64::MAX - u64::MAX % range;
    loop {
        let mut bytes = [0u8; 8];
        platform::random::fill(&mut bytes);
        let value = u64::from_le_bytes(bytes);
        if value < limit {
            return value % range;
        }
    }
}

#[core_runtime::jsmodule(node_args, name = "node:crypto", default_namespace)]
pub mod module {
    use super::*;

    pub use super::hash_class as Hash;
    pub use super::hmac_class as Hmac;
    pub use super::KeyObject;

    /// <https://nodejs.org/api/crypto.html#cryptocreatesecretkeykey-encoding>
    #[function(length = 2)]
    pub fn create_secret_key<'s>(
        scope: &'s Scope<'_>,
        key: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let encoding = match encoding.filter(|encoding| encoding.is_string()) {
            Some(encoding) => Some(String::from_jsval(scope, encoding, ())?),
            None => None,
        };
        let bytes = byte_source(scope, key, encoding.as_deref())?.ok_or_else(|| {
            let expected = "an instance of ArrayBuffer, Buffer, TypedArray, or DataView";
            NodeError::invalid_arg_type(scope, "key", expected, key)
        })?;
        Ok(new_secret_key(scope, bytes)?)
    }

    /// <https://nodejs.org/api/crypto.html#cryptopbkdf2password-salt-iterations-keylen-digest-callback>:
    /// derives the key synchronously, and calls `callback` in a later turn of the event loop.
    #[function(length = 6)]
    pub fn pbkdf2(scope: &Scope<'_>, args: RestArgs<HandleValue<'_>>) -> Result<(), Thrown> {
        let undefined = HandleValue::undefined();
        let arg = |index: usize| args.get(index).copied().unwrap_or(undefined);
        let (mut digest, mut callback) = (arg(4), arg(5));
        if js::function::is_callable(digest) {
            callback = digest;
            digest = undefined;
        }
        let derive = pbkdf2_args(scope, arg(0), arg(1), arg(2), arg(3), digest)?;
        let callback = validate_function(scope, callback, "callback")?;
        derived_async(scope, derive(), callback, false)
    }

    /// <https://nodejs.org/api/crypto.html#cryptopbkdf2syncpassword-salt-iterations-keylen-digest>
    #[function(length = 5)]
    pub fn pbkdf2_sync<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        let arg = |index: usize| args.get(index).copied().unwrap_or(undefined);
        let derive = pbkdf2_args(scope, arg(0), arg(1), arg(2), arg(3), arg(4))?;
        derived_sync(scope, derive(), false)
    }

    /// <https://nodejs.org/api/crypto.html#cryptoscryptpassword-salt-keylen-options-callback>:
    /// derives the key synchronously, and calls `callback` in a later turn of the event loop.
    #[function(length = 4)]
    pub fn scrypt(scope: &Scope<'_>, args: RestArgs<HandleValue<'_>>) -> Result<(), Thrown> {
        let undefined = HandleValue::undefined();
        let arg = |index: usize| args.get(index).copied().unwrap_or(undefined);
        let (options, callback) = if args.len() <= 4 {
            (undefined, arg(3))
        } else {
            (arg(3), arg(4))
        };
        let derive = scrypt_args(scope, arg(0), arg(1), arg(2), options)?;
        let callback = validate_function(scope, callback, "callback")?;
        derived_async(scope, derive(), callback, false)
    }

    /// <https://nodejs.org/api/crypto.html#cryptoscryptsyncpassword-salt-keylen-options>
    #[function(length = 3)]
    pub fn scrypt_sync<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        let arg = |index: usize| args.get(index).copied().unwrap_or(undefined);
        let derive = scrypt_args(scope, arg(0), arg(1), arg(2), arg(3))?;
        derived_sync(scope, derive(), false)
    }

    /// <https://nodejs.org/api/crypto.html#cryptohkdfdigest-ikm-salt-info-keylen-callback>:
    /// derives the key synchronously, and calls `callback` with an `ArrayBuffer` in a later turn
    /// of the event loop.
    #[function(length = 6)]
    pub fn hkdf(scope: &Scope<'_>, args: RestArgs<HandleValue<'_>>) -> Result<(), Thrown> {
        let undefined = HandleValue::undefined();
        let arg = |index: usize| args.get(index).copied().unwrap_or(undefined);
        let derive = hkdf_args(scope, arg(0), arg(1), arg(2), arg(3), arg(4))?;
        let callback = validate_function(scope, arg(5), "callback")?;
        derived_async(scope, derive(), callback, true)
    }

    /// <https://nodejs.org/api/crypto.html#cryptohkdfsyncdigest-ikm-salt-info-keylen>: the key as
    /// an `ArrayBuffer`.
    #[function(length = 5)]
    pub fn hkdf_sync<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let undefined = HandleValue::undefined();
        let arg = |index: usize| args.get(index).copied().unwrap_or(undefined);
        let derive = hkdf_args(scope, arg(0), arg(1), arg(2), arg(3), arg(4))?;
        derived_sync(scope, derive(), true)
    }

    /// <https://nodejs.org/api/crypto.html#cryptocreatehashalgorithm-options>
    #[function(length = 2)]
    pub fn create_hash<'s>(
        scope: &'s Scope<'_>,
        algorithm: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let name = crate::errors::validate_string(scope, algorithm, "algorithm")?;
        let options = options.unwrap_or(HandleValue::undefined());
        let output_length = output_length_option(scope, options)?;
        let Some(hasher) = hasher(&name) else {
            return Err(unsupported_digest(scope, "Digest method not supported"));
        };
        check_output_length(scope, &*hasher, output_length)?;
        Ok(new_hash(scope, hasher, name, options)?)
    }

    /// <https://nodejs.org/api/crypto.html#cryptocreatehmacalgorithm-key-options>
    #[function(length = 3)]
    pub fn create_hmac<'s>(
        scope: &'s Scope<'_>,
        algorithm: HandleValue<'s>,
        key: HandleValue<'s>,
        options: Option<HandleValue<'s>>,
    ) -> Result<Object<'s>, Thrown> {
        let hmac = hmac_class::new_instance(scope)?;
        init_hmac(scope, hmac, algorithm, key, options)?;
        Ok(hmac)
    }

    /// <https://nodejs.org/api/crypto.html#cryptohashalgorithm-data-outputencoding>: the digest of
    /// `data` in one call, as hex unless `options`, or its `outputEncoding`, names another encoding
    /// or `buffer`.
    #[function(length = 3)]
    pub fn hash<'s>(
        scope: &'s Scope<'_>,
        algorithm: HandleValue<'_>,
        data: HandleValue<'_>,
        options: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let name = crate::errors::validate_string(scope, algorithm, "algorithm")?;
        if !data.is_string()
            && Object::from_value(scope, *data)
                .ok()
                .and_then(ArrayBufferView::from_object)
                .is_none()
        {
            return Err(NodeError::invalid_arg_type(
                scope,
                "input",
                "of type string or an instance of Buffer, TypedArray, or DataView",
                data,
            )
            .into());
        }
        let encoding = match options.filter(|options| !options.is_undefined()) {
            Some(options) if options.is_string() => Some(options),
            Some(options) => {
                let options = crate::errors::validate_object(scope, options, "options")?;
                Some(options.get_property(scope, c"outputEncoding")?)
                    .filter(|encoding| !encoding.is_undefined() && !encoding.is_null())
            }
            None => None,
        };
        // `None` stands for a `Buffer`.
        let encoding = match encoding {
            None => Some(Encoding::Hex),
            Some(encoding) => {
                let name = crate::errors::validate_string(scope, encoding, "outputEncoding")?;
                match Encoding::parse(&name) {
                    Some(encoding) => Some(encoding),
                    None if name.eq_ignore_ascii_case("buffer") => None,
                    None => {
                        let reason = "is invalid";
                        return Err(NodeError::invalid_arg_value(
                            scope,
                            "outputEncoding",
                            encoding,
                            reason,
                        )
                        .into());
                    }
                }
            }
        };
        let Some(mut hasher) = hasher(&name) else {
            return Err(unsupported_digest(
                scope,
                &format!("Digest method {name} is not supported"),
            ));
        };
        data_bytes(scope, data, None, "data")?.with(|bytes| hasher.update(bytes));
        let digest = hasher.finalize();
        match encoding {
            Some(encoding) => Ok(decode_bytes(scope, &digest, encoding)?.to_jsval_throwing(scope)?),
            None => Ok(new_buffer(scope, &digest)?.to_jsval_throwing(scope)?),
        }
    }

    /// <https://nodejs.org/api/crypto.html#cryptogethashes>
    pub fn get_hashes<'s>(scope: &'s Scope<'_>) -> Result<js::Array<'s>, ExnThrown> {
        let names: Vec<HandleValue<'_>> = HASHES
            .iter()
            .map(|name| js::JSString::from_str(scope, name)?.to_jsval_throwing(scope))
            .collect::<Result<_, _>>()?;
        js::Array::with_contents(scope, &names)
    }

    /// <https://nodejs.org/api/crypto.html#cryptorandombytessize-callback>
    #[function(length = 2)]
    pub fn random_bytes<'s>(
        scope: &'s Scope<'_>,
        size: HandleValue<'_>,
        callback: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let size = validate_size(scope, size, "size", 1)? as usize;
        let callback = match callback.filter(|callback| !callback.is_undefined()) {
            Some(callback) => Some(validate_function(scope, callback, "callback")?),
            None => None,
        };
        let buffer = crate::buffer::new_buffer_with(scope, size, platform::random::fill)?;
        let buffer = buffer.to_jsval_throwing(scope)?;
        if let Some(callback) = callback {
            defer(scope, callback, buffer)?;
            return Ok(scope.root_value(js::value::undefined()));
        }
        Ok(buffer)
    }

    /// <https://nodejs.org/api/crypto.html#cryptorandomfillsyncbuffer-offset-size>: `offset` and
    /// `size` count elements of a typed array, and bytes otherwise.
    pub fn random_fill_sync<'s>(
        scope: &'s Scope<'_>,
        buffer: HandleValue<'s>,
        offset: Option<HandleValue<'_>>,
        size: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let (view, element_size) = fill_target(scope, buffer)?;
        let length = view.byte_length();
        let offset = match offset.filter(|offset| !offset.is_undefined()) {
            Some(offset) => {
                if !offset.is_number() {
                    let error =
                        NodeError::invalid_arg_type(scope, "offset", "of type number", offset);
                    return Err(error.into());
                }
                let bytes = offset.to_number() * element_size as f64;
                if !(0.0..=length.min(2147483647) as f64).contains(&bytes) {
                    let range = format!(">= 0 && <= {}", length.min(2147483647));
                    return Err(NodeError::out_of_range(
                        "offset",
                        &range,
                        &out_of_range_received(bytes),
                    )
                    .into());
                }
                bytes as usize
            }
            None => 0,
        };
        let size = match size.filter(|size| !size.is_undefined()) {
            Some(size) => {
                let size = validate_size(scope, size, "size", element_size)?;
                if offset as f64 + size > length as f64 {
                    return Err(NodeError::out_of_range(
                        "size + offset",
                        &format!("<= {length}"),
                        &out_of_range_received(offset as f64 + size),
                    )
                    .into());
                }
                size as usize
            }
            None => length - offset,
        };
        view.with_bytes_mut(|bytes| platform::random::fill(&mut bytes[offset..offset + size]));
        Ok(buffer)
    }

    /// <https://nodejs.org/api/crypto.html#cryptorandomfillbuffer-offset-size-callback>
    #[function(length = 4)]
    pub fn random_fill(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
        fill_target(scope, buffer)?;
        let Some((callback, sizes)) = args.split_last() else {
            return Err(NodeError::invalid_arg_type(
                scope,
                "callback",
                "of type function",
                scope.root_value(js::value::undefined()),
            )
            .into());
        };
        let callback = validate_function(scope, *callback, "callback")?;
        let buffer = scope.root_value(buffer.get());
        let filled =
            random_fill_sync(scope, buffer, sizes.first().copied(), sizes.get(1).copied())?;
        defer(scope, callback, filled)?;
        Ok(())
    }

    /// <https://nodejs.org/api/crypto.html#cryptorandomuuidoptions>
    #[function(name = "randomUUID", length = 1)]
    pub fn random_uuid() -> String {
        web_globals::crypto::random_uuid()
    }

    /// <https://nodejs.org/api/crypto.html#cryptorandomintmin-max-callback>: `min` may be left
    /// out. The callback gets `undefined` and the integer.
    #[function(length = 3)]
    pub fn random_int<'s>(
        scope: &'s Scope<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let undefined = scope.root_value(js::value::undefined());
        let arg = |index: usize| args.get(index).copied().unwrap_or(undefined);
        let (mut min, mut max, mut callback) = (arg(0), arg(1), arg(2));
        let is_function = |value: HandleValue<'_>| {
            Object::from_value(scope, *value).is_ok_and(|object| object.is_callable())
        };
        let min_left_out = max.is_undefined() || is_function(max);
        if min_left_out {
            callback = max;
            max = min;
            min = scope.root_value(js::value::from_f64(0.0));
        }
        let callback = if callback.is_undefined() {
            None
        } else {
            Some(validate_function(scope, callback, "callback")?)
        };
        let safe_integer = |value: HandleValue<'_>, name: &str| -> Result<f64, Thrown> {
            let number = value.is_number().then(|| value.to_number());
            match number.filter(|n| n.fract() == 0.0 && n.abs() <= crate::errors::MAX_SAFE_INTEGER)
            {
                Some(number) => Ok(number),
                None => {
                    Err(NodeError::invalid_arg_type(scope, name, "a safe integer", value).into())
                }
            }
        };
        let min = safe_integer(min, "min")?;
        let max = safe_integer(max, "max")?;
        if max <= min {
            return Err(NodeError::out_of_range(
                "max",
                &format!("greater than the value of \"min\" ({min})"),
                &out_of_range_received(max),
            )
            .into());
        }
        let range = max - min;
        if range > 281474976710655.0 {
            let name = if min_left_out { "max" } else { "max - min" };
            return Err(NodeError::out_of_range(
                name,
                "<= 281474976710655",
                &out_of_range_received(range),
            )
            .into());
        }
        let value = min + random_below(range as u64) as f64;
        let value = scope.root_value(js::value::from_f64(value));
        if let Some(callback) = callback {
            let callback = callback.to_jsval_throwing(scope)?;
            js::jobs::queue_tick(scope, callback, &[undefined, value])?;
            return Ok(undefined);
        }
        Ok(value)
    }

    /// <https://nodejs.org/api/crypto.html#cryptotimingsafeequala-b>: compares every byte,
    /// whatever the result. An argument of the wrong type gets the message of Node.js's C++
    /// check, which doesn't describe the value.
    #[function(length = 0)]
    pub fn timing_safe_equal(
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let source = |value: HandleValue<'_>, name: &str| -> Result<Object<'_>, Thrown> {
            Object::from_value(scope, *value)
                .ok()
                .filter(|object| js::typedarray::is_buffer_source(*object))
                .ok_or_else(|| {
                    let message = format!(
                        "The \"{name}\" argument must be an instance of ArrayBuffer, Buffer, \
                         TypedArray, or DataView."
                    );
                    NodeError::new(ErrorKind::TypeError, "ERR_INVALID_ARG_TYPE", message).into()
                })
        };
        let a = source(a, "buf1")?;
        let b = source(b, "buf2")?;
        let equal = js::typedarray::with_buffer_source_bytes_pair(a, b, |a, b| {
            (a.len() == b.len()).then(|| {
                a.iter()
                    .zip(b)
                    .fold(0u8, |difference, (x, y)| difference | (x ^ y))
                    == 0
            })
        });
        equal.flatten().ok_or_else(|| {
            NodeError::new(
                ErrorKind::RangeError,
                "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH",
                "Input buffers must have the same byte length",
            )
            .into()
        })
    }

    /// <https://nodejs.org/api/crypto.html#cryptogetrandomvaluestypedarray>: the global
    /// `crypto.getRandomValues`.
    pub fn get_random_values<'s>(
        scope: &'s Scope<'_>,
        array: HandleValue<'s>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let view = Object::from_value(scope, *array)
            .ok()
            .and_then(|object| object.cast::<js::ArrayBufferView>().ok());
        let Some(view) = view else {
            return Err(web_globals::dom_exception::throw_dom_exception(
                scope,
                "TypeMismatchError",
                web_globals::crypto::TYPE_MISMATCH_MESSAGE,
            ));
        };
        web_globals::crypto::get_random_values(view)
            .map_err(|error| js::error::ThrowException::throw(error, scope))?;
        Ok(array)
    }
}

/// Register `node:crypto`, whose `webcrypto` and `crypto` are the global `crypto`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let overrides = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let crypto = match web_globals::crypto::crypto_object(scope) {
            Some(crypto) => crypto.to_jsval_throwing(scope)?,
            None => HandleValue::undefined(),
        };
        let mut overrides = vec![("webcrypto", crypto)];
        if let Ok(object) = Object::from_value(scope, crypto) {
            let subtle = object.get_property(scope, c"subtle")?;
            if !subtle.is_undefined() {
                overrides.push(("subtle", subtle));
            }
        }
        Ok(overrides)
    };
    let Ok(overrides) = overrides() else {
        return false;
    };
    if !crate::stub::register_implemented_with::<module::js_module>(scope, name, &overrides) {
        return false;
    }
    define_random_bytes_aliases(scope, name).is_ok()
}

/// Define the deprecated aliases of `randomBytes`, `pseudoRandomBytes`, `prng` and `rng`, as
/// non-enumerable properties of the module's `default` export, as Node.js does. They aren't
/// named exports.
fn define_random_bytes_aliases(scope: &Scope<'_>, name: &str) -> Result<(), ExnThrown> {
    let Some(namespace) = core_runtime::module::builtin_namespace(scope, name)? else {
        return Ok(());
    };
    let Ok(crypto) = Object::from_value(scope, *namespace.get_property(scope, c"default")?) else {
        return Ok(());
    };
    let random_bytes = crypto.get_property(scope, c"randomBytes")?;
    for alias in [c"pseudoRandomBytes", c"prng", c"rng"] {
        crypto.define_property(scope, alias, random_bytes, 0)?;
    }
    Ok(())
}
