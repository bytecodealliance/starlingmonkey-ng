// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The parts of `node:crypto` that need no keys: hashes, HMACs and random values. See
//! <https://nodejs.org/api/crypto.html>.
//!
//! The digests are MD5, RIPEMD-160, SHA-1, the SHA-2 family including SHA-512/224 and SHA-512/256,
//! and SHA3-224, SHA3-256, SHA3-384 and SHA3-512.
//! `Hash` and `Hmac` objects have `update`, `digest` and, for `Hash`, `copy`, but aren't streams.

use digest::DynDigest;
use js::conversion::{FromJSVal, ToJSVal};
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{ArrayBufferView, Function, Object};

use crate::buffer::{decode_bytes, encode_string, new_buffer, Encoding};
use crate::errors::{validate_function, ErrorKind, NodeError, Thrown};

/// Register the classes of `node:crypto` on `global`.
pub(crate) fn register_classes(scope: &Scope<'_>, global: Object<'_>) -> Result<(), ExnThrown> {
    NativeHash::add_to_global(scope, global);
    NativeHmac::add_to_global(scope, global);
    hash_class::register(scope)?;
    hmac_class::register(scope)?;
    Ok(())
}

/// A new hasher for the digest `name`, and the digest's block size in bytes, or `None` for a
/// digest that isn't supported. Names are case-insensitive, and may have an `RSA-` prefix or a
/// hyphen after `SHA`.
fn hasher(name: &str) -> Option<(Box<dyn DynDigest>, usize)> {
    let name = name.to_ascii_lowercase();
    let name = name.strip_prefix("rsa-").unwrap_or(&name);
    let name = match name {
        "sha-1" => "sha1",
        "sha-224" => "sha224",
        "sha-256" => "sha256",
        "sha-384" => "sha384",
        "sha-512" => "sha512",
        "sha-512/224" | "sha512/224" => "sha512-224",
        "sha-512/256" | "sha512/256" => "sha512-256",
        "ripemd" | "rmd160" | "ripemd-160" => "ripemd160",
        name => name,
    };
    Some(match name {
        "md5" => (Box::new(md5::Md5::default()), 64),
        "sha1" => (Box::new(sha1::Sha1::default()), 64),
        "sha224" => (Box::new(sha2::Sha224::default()), 64),
        "sha256" => (Box::new(sha2::Sha256::default()), 64),
        "sha384" => (Box::new(sha2::Sha384::default()), 128),
        "sha512" => (Box::new(sha2::Sha512::default()), 128),
        "sha512-224" => (Box::new(sha2::Sha512_224::default()), 128),
        "sha512-256" => (Box::new(sha2::Sha512_256::default()), 128),
        "sha3-224" => (Box::new(sha3::Sha3_224::default()), 144),
        "sha3-256" => (Box::new(sha3::Sha3_256::default()), 136),
        "sha3-384" => (Box::new(sha3::Sha3_384::default()), 104),
        "sha3-512" => (Box::new(sha3::Sha3_512::default()), 72),
        "ripemd160" => (Box::new(ripemd::Ripemd160::default()), 64),
        _ => return None,
    })
}

/// The supported digest names, as `getHashes` lists them.
const HASHES: &[&str] = &[
    "RSA-MD5",
    "RSA-RIPEMD160",
    "RSA-SHA1",
    "RSA-SHA224",
    "RSA-SHA256",
    "RSA-SHA384",
    "RSA-SHA512",
    "RSA-SHA512/224",
    "RSA-SHA512/256",
    "RSA-SHA3-224",
    "RSA-SHA3-256",
    "RSA-SHA3-384",
    "RSA-SHA3-512",
    "md5",
    "ripemd",
    "ripemd160",
    "rmd160",
    "sha1",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "sha512-224",
    "sha512-256",
    "sha3-224",
    "sha3-256",
    "sha3-384",
    "sha3-512",
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

/// `ERR_CRYPTO_HASH_FINALIZED`.
fn finalized() -> NodeError {
    NodeError::new(
        ErrorKind::Error,
        "ERR_CRYPTO_HASH_FINALIZED",
        "Digest already called",
    )
}

/// The bytes of `data`, a string in `encoding` or a `Buffer`, `TypedArray` or `DataView`. A
/// string in an unknown encoding is UTF-8, and one in hex must have an even length.
fn data_bytes(
    scope: &Scope<'_>,
    data: HandleValue<'_>,
    encoding: Option<HandleValue<'_>>,
    name: &str,
) -> Result<Vec<u8>, Thrown> {
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
        return Ok(encode_string(scope, string, encoding)?);
    }
    let view = Object::from_value(scope, *data)
        .ok()
        .and_then(ArrayBufferView::from_object)
        .ok_or_else(|| {
            NodeError::invalid_arg_type(
                scope,
                name,
                "of type string or an instance of Buffer, TypedArray, or DataView",
                data,
            )
        })?;
    Ok(view.copy_bytes(scope)?)
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

/// The state of a `Hash`, which it holds in its `_starlingNative` property.
#[core_runtime::jsclass(hidden)]
pub struct NativeHash {
    /// The hasher, or `None` once `digest` was called.
    #[no_trace]
    hasher: Option<Box<dyn DynDigest>>,
    /// The digest's name, for `copy`.
    #[no_trace]
    algorithm: String,
}

#[core_runtime::jsmethods]
impl NativeHash {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(illegal_constructor())
    }
}

/// The state of an `Hmac`, which it holds in its `_starlingNative` property.
#[core_runtime::jsclass(hidden)]
pub struct NativeHmac {
    /// The inner hasher, or `None` once `digest` was called.
    #[no_trace]
    inner: Option<Box<dyn DynDigest>>,
    /// The outer hasher, already updated with the key XORed with the outer pad, or `None` once
    /// `digest` was called.
    #[no_trace]
    outer: Option<Box<dyn DynDigest>>,
}

#[core_runtime::jsmethods]
impl NativeHmac {
    #[constructor]
    fn new() -> Result<Self, NodeError> {
        Err(illegal_constructor())
    }
}

/// `ERR_ILLEGAL_CONSTRUCTOR`.
fn illegal_constructor() -> NodeError {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_ILLEGAL_CONSTRUCTOR",
        "Illegal constructor",
    )
}

/// The `_starlingNative` property of `this`, or `ERR_INVALID_THIS` naming `class` if it isn't an
/// object.
fn native_of<'s>(
    scope: &'s Scope<'_>,
    this: Object<'_>,
    class: &str,
) -> Result<Object<'s>, Thrown> {
    let native = this.get_property(scope, c"_starlingNative")?;
    Object::from_value(scope, *native).map_err(|_| invalid_this(class).into())
}

/// `ERR_INVALID_THIS` naming `class`.
fn invalid_this(class: &str) -> NodeError {
    NodeError::new(
        ErrorKind::TypeError,
        "ERR_INVALID_THIS",
        format!("Value of \"this\" must be of type {class}"),
    )
}

/// The state of the `Hash` `this`.
fn hash_state<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<NativeHash<'s>, Thrown> {
    native_of(scope, this, "Hash")?
        .cast::<NativeHash>()
        .map_err(|_| invalid_this("Hash").into())
}

/// The state of the `Hmac` `this`.
fn hmac_state_of<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<NativeHmac<'s>, Thrown> {
    native_of(scope, this, "Hmac")?
        .cast::<NativeHmac>()
        .map_err(|_| invalid_this("Hmac").into())
}

/// Make `this` a `Hash` of the digest `algorithm`, with `hasher` as its state.
fn init_hash(
    scope: &Scope<'_>,
    this: Object<'_>,
    hasher: Box<dyn DynDigest>,
    algorithm: String,
) -> Result<(), ExnThrown> {
    let native = js::class::create_instance_with::<NativeHashImpl>(scope, |_| NativeHashImpl {
        hasher: Some(hasher),
        algorithm,
    })?;
    this.define_property(scope, c"_starlingNative", native, 0)
}

/// A new `Hash` of the digest `algorithm`, with `hasher` as its state.
fn new_hash<'s>(
    scope: &'s Scope<'_>,
    hasher: Box<dyn DynDigest>,
    algorithm: String,
) -> Result<HandleValue<'s>, ExnThrown> {
    let prototype = hash_class::class_constructor(scope)?.get_property(scope, c"prototype")?;
    let object = Object::new_plain(scope)?;
    object.set_prototype(
        scope,
        Object::from_value(scope, prototype)
            .map_err(|e| e.throw(scope))?
            .handle(),
    )?;
    init_hash(scope, object, hasher, algorithm)?;
    object.to_jsval_throwing(scope)
}

#[core_runtime::jsfunction_class(name = "Hash", self_construct)]
pub mod hash_class {
    use super::*;

    /// <https://nodejs.org/api/crypto.html#class-hash>: a hash of the digest `algorithm`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        algorithm: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let name = crate::errors::validate_string(scope, algorithm, "algorithm")?;
        let Some((hasher, _)) = hasher(&name) else {
            return Err(unsupported_digest(scope, "Digest method not supported"));
        };
        Ok(init_hash(scope, this, hasher, name)?)
    }

    /// <https://nodejs.org/api/crypto.html#hashupdatedata-inputencoding>
    pub fn update<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        data: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = hash_state(scope, this)?;
        if state.data().hasher.is_none() {
            return Err(finalized().into());
        }
        let bytes = data_bytes(scope, data, encoding, "data")?;
        let mut data = state.data_mut();
        data.hasher.as_mut().ok_or_else(finalized)?.update(&bytes);
        Ok(this)
    }

    /// <https://nodejs.org/api/crypto.html#hashdigestencoding>
    pub fn digest<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let hasher = hash_state(scope, this)?
            .data_mut()
            .hasher
            .take()
            .ok_or_else(finalized)?;
        output(scope, &hasher.finalize(), encoding)
    }

    /// <https://nodejs.org/api/crypto.html#hashcopyoptions>
    pub fn copy<'s>(scope: &'s Scope<'_>, this: Object<'_>) -> Result<HandleValue<'s>, Thrown> {
        let state = {
            let state = hash_state(scope, this)?;
            let data = state.data();
            let hasher = data.hasher.as_ref().ok_or_else(finalized)?.box_clone();
            (hasher, data.algorithm.clone())
        };
        Ok(new_hash(scope, state.0, state.1)?)
    }
}

#[core_runtime::jsfunction_class(name = "Hmac", self_construct)]
pub mod hmac_class {
    use super::*;

    /// <https://nodejs.org/api/crypto.html#class-hmac>: an HMAC of the digest `algorithm` keyed
    /// with `key`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        algorithm: HandleValue<'_>,
        key: HandleValue<'_>,
    ) -> Result<(), Thrown> {
        let (inner, outer) = hmac_state(scope, algorithm, key)?;
        let native =
            js::class::create_instance_with::<NativeHmacImpl>(scope, |_| NativeHmacImpl {
                inner: Some(inner),
                outer: Some(outer),
            })?;
        this.define_property(scope, c"_starlingNative", native, 0)?;
        Ok(())
    }

    /// <https://nodejs.org/api/crypto.html#hmacupdatedata-inputencoding>
    pub fn update<'s>(
        scope: &Scope<'_>,
        this: Object<'s>,
        data: HandleValue<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<Object<'s>, Thrown> {
        let state = hmac_state_of(scope, this)?;
        if state.data().inner.is_none() {
            return Err(finalized().into());
        }
        let bytes = data_bytes(scope, data, encoding, "data")?;
        let mut data = state.data_mut();
        data.inner.as_mut().ok_or_else(finalized)?.update(&bytes);
        Ok(this)
    }

    /// <https://nodejs.org/api/crypto.html#hmacdigestencoding>
    pub fn digest<'s>(
        scope: &'s Scope<'_>,
        this: Object<'_>,
        encoding: Option<HandleValue<'_>>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let (inner, mut outer) = {
            let state = hmac_state_of(scope, this)?;
            let mut data = state.data_mut();
            let inner = data.inner.take().ok_or_else(finalized)?;
            (inner, data.outer.take().ok_or_else(finalized)?)
        };
        outer.update(&inner.finalize());
        output(scope, &outer.finalize(), encoding)
    }
}

/// The inner and the outer hasher of an HMAC.
type HmacState = (Box<dyn DynDigest>, Box<dyn DynDigest>);

/// The state of an HMAC of the digest `algorithm` keyed with `key`, as RFC 2104 defines it: the
/// inner hasher updated with the key XORed with the inner pad, and the outer hasher updated with
/// the key XORed with the outer pad.
fn hmac_state(
    scope: &Scope<'_>,
    algorithm: HandleValue<'_>,
    key: HandleValue<'_>,
) -> Result<HmacState, Thrown> {
    let name = crate::errors::validate_string(scope, algorithm, "hmac")?;
    let mut key = data_bytes(scope, key, None, "key")?;
    let (mut inner, block) = hasher(&name).ok_or_else(|| invalid_digest(&name))?;
    if key.len() > block {
        let (mut long, _) = hasher(&name).ok_or_else(|| invalid_digest(&name))?;
        long.update(&key);
        key = long.finalize().to_vec();
    }
    key.resize(block, 0);
    let inner_key: Vec<u8> = key.iter().map(|byte| byte ^ 0x36).collect();
    inner.update(&inner_key);
    let mut outer = inner.box_clone();
    outer.reset();
    let outer_key: Vec<u8> = key.iter().map(|byte| byte ^ 0x5c).collect();
    outer.update(&outer_key);
    Ok((inner, outer))
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
        return Err(
            NodeError::out_of_range(name, ">= 0 && <= 2147483647", &number.to_string()).into(),
        );
    }
    Ok(number)
}

/// The body of the microtask that calls an async random function's callback, whose payload is
/// The body of the microtask that calls `randomInt`'s callback, whose payload is
/// Call `callback` with `null` and `value` in a later turn of the event loop, as `setImmediate`
/// does.
fn defer(scope: &Scope<'_>, callback: Object<'_>, value: HandleValue<'_>) -> Result<(), ExnThrown> {
    let set_immediate = scope.global().get_property(scope, c"setImmediate")?;
    let null = scope.root_value(js::value::null());
    let callback = callback.to_jsval_throwing(scope)?;
    Function::call(
        scope,
        js::value::undefined(),
        set_immediate,
        &[callback, null, value],
    )?;
    Ok(())
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

    /// <https://nodejs.org/api/crypto.html#cryptocreatehashalgorithm-options>
    pub fn create_hash<'s>(
        scope: &'s Scope<'_>,
        algorithm: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let name = crate::errors::validate_string(scope, algorithm, "algorithm")?;
        let Some((hasher, _)) = hasher(&name) else {
            return Err(unsupported_digest(scope, "Digest method not supported"));
        };
        Ok(new_hash(scope, hasher, name)?)
    }

    /// <https://nodejs.org/api/crypto.html#cryptocreatehmacalgorithm-key-options>
    pub fn create_hmac<'s>(
        scope: &'s Scope<'_>,
        algorithm: HandleValue<'s>,
        key: HandleValue<'s>,
    ) -> Result<HandleValue<'s>, Thrown> {
        let ctor = hmac_class::class_constructor(scope)?;
        Ok(Function::construct(scope, ctor, &[algorithm, key])?.to_jsval_throwing(scope)?)
    }

    /// <https://nodejs.org/api/crypto.html#cryptohashalgorithm-data-outputencoding>: the digest of
    /// `data` in one call, as hex unless `options`, or its `outputEncoding`, names another encoding
    /// or `buffer`.
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
                "an instance of Buffer, TypedArray, or DataView or of type string",
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
        let Some((mut hasher, _)) = hasher(&name) else {
            return Err(unsupported_digest(
                scope,
                &format!("Digest method {name} is not supported"),
            ));
        };
        hasher.update(&data_bytes(scope, data, None, "data")?);
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
        let mut bytes = vec![0u8; size];
        platform::random::fill(&mut bytes);
        let buffer = new_buffer(scope, &bytes)?.to_jsval_throwing(scope)?;
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
        let object = Object::from_value(scope, *buffer).ok();
        let view = object.and_then(ArrayBufferView::from_object);
        let array_buffer = object.and_then(|object| object.cast::<js::ArrayBuffer>().ok());
        let (length, element_size) = match (view, array_buffer) {
            (Some(view), _) => (view.byte_length(), view.view_kind().element_size()),
            (None, Some(array_buffer)) => (array_buffer.byte_length(), 1),
            (None, None) => {
                return Err(NodeError::invalid_arg_type(
                    scope,
                    "buf",
                    "an instance of ArrayBuffer or ArrayBufferView",
                    buffer,
                )
                .into())
            }
        };
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
                    return Err(
                        NodeError::out_of_range("offset", &range, &bytes.to_string()).into(),
                    );
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
                        &(offset as f64 + size).to_string(),
                    )
                    .into());
                }
                size as usize
            }
            None => length - offset,
        };
        if let Some(view) = view {
            view.with_bytes_mut(|bytes| platform::random::fill(&mut bytes[offset..offset + size]));
        } else if let Some(array_buffer) = array_buffer {
            array_buffer
                .with_bytes_mut(|bytes| platform::random::fill(&mut bytes[offset..offset + size]));
        }
        Ok(buffer)
    }

    /// <https://nodejs.org/api/crypto.html#cryptorandomfillbuffer-offset-size-callback>
    pub fn random_fill(
        scope: &Scope<'_>,
        buffer: HandleValue<'_>,
        args: RestArgs<HandleValue<'_>>,
    ) -> Result<(), Thrown> {
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
    #[function(name = "randomUUID")]
    pub fn random_uuid() -> String {
        platform::random::uuid_v4()
    }

    /// <https://nodejs.org/api/crypto.html#cryptorandomintmin-max-callback>: `min` may be left
    /// out. The callback gets `undefined` and the integer.
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
            match number.filter(|n| n.fract() == 0.0 && n.abs() <= 9007199254740991.0) {
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
                &max.to_string(),
            )
            .into());
        }
        let range = max - min;
        if range > 281474976710655.0 {
            let name = if min_left_out { "max" } else { "max - min" };
            return Err(
                NodeError::out_of_range(name, "<= 281474976710655", &range.to_string()).into(),
            );
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
    /// whatever the result.
    pub fn timing_safe_equal(
        scope: &Scope<'_>,
        a: HandleValue<'_>,
        b: HandleValue<'_>,
    ) -> Result<bool, Thrown> {
        let bytes = |value: HandleValue<'_>, name: &str| -> Result<Vec<u8>, Thrown> {
            let bytes = match Object::from_value(scope, *value) {
                Ok(object) => js::typedarray::copy_buffer_source_bytes(scope, object)?,
                Err(_) => None,
            };
            bytes.ok_or_else(|| {
                let expected = "an instance of ArrayBuffer, Buffer, TypedArray, or DataView";
                NodeError::invalid_arg_type(scope, name, expected, value).into()
            })
        };
        let a = bytes(a, "buf1")?;
        let b = bytes(b, "buf2")?;
        if a.len() != b.len() {
            return Err(NodeError::new(
                ErrorKind::RangeError,
                "ERR_CRYPTO_TIMING_SAFE_EQUAL_LENGTH",
                "Input buffers must have the same byte length",
            )
            .into());
        }
        Ok(a.iter()
            .zip(&b)
            .fold(0u8, |difference, (x, y)| difference | (x ^ y))
            == 0)
    }

    /// <https://nodejs.org/api/crypto.html#cryptogetrandomvaluestypedarray>: the global
    /// `crypto.getRandomValues`.
    pub fn get_random_values<'s>(
        scope: &'s Scope<'_>,
        array: HandleValue<'_>,
    ) -> Result<HandleValue<'s>, ExnThrown> {
        let captured = webcrypto(scope)?;
        let crypto = captured.get_property(scope, c"crypto")?;
        let crypto = Object::from_value(scope, crypto).map_err(|e| e.throw(scope))?;
        let get_random_values = captured.get_property(scope, c"getRandomValues")?;
        let result = Function::call(scope, crypto, get_random_values, &[array])?;
        Ok(scope.root_value(result.get()))
    }
}

/// Register `node:crypto`, whose `webcrypto` and `crypto` are the global `crypto`.
pub(crate) fn register_module(scope: &Scope<'_>, name: &'static str) -> bool {
    let overrides = || -> Result<Vec<(&'static str, HandleValue<'_>)>, ExnThrown> {
        let crypto = webcrypto(scope)?.get_property(scope, c"crypto")?;
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
    crate::stub::register_implemented_with::<module::js_module>(scope, name, &overrides)
}

/// The key of the captured `crypto` global in the global's shared objects.
static WEBCRYPTO_KEY: u8 = 0;

/// The `crypto` global and its `getRandomValues` as they were when first asked for, so that
/// reassigning the global doesn't change `node:crypto`.
fn webcrypto<'s>(scope: &'s Scope<'_>) -> Result<Object<'s>, ExnThrown> {
    js::class::get_or_init_shared_object(scope, &WEBCRYPTO_KEY as *const u8 as usize, |scope| {
        let captured = Object::new_plain(scope)?;
        let crypto = scope.global().get_property(scope, c"crypto")?;
        captured.set_property(scope, c"crypto", crypto)?;
        if let Ok(object) = Object::from_value(scope, crypto) {
            let get_random_values = object.get_property(scope, c"getRandomValues")?;
            captured.set_property(scope, c"getRandomValues", get_random_values)?;
        }
        Ok(captured)
    })
}
