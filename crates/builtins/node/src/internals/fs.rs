// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The functions of `internal/fs/utils`.

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::prelude::HandleValue;
use js::{Function, Object};

use crate::errors::{number_to_string, out_of_range_received, NodeError, Thrown};

/// The argument at `index`, or `undefined`.
fn arg<'a>(args: &'a CallbackArgs<'_>, index: u32) -> HandleValue<'a> {
    if index < args.len() {
        args.get(index)
    } else {
        HandleValue::undefined()
    }
}

/// The value of the `O_*` constant `name` on this host, or 0 if it lacks it.
fn open_flag(name: &str) -> i32 {
    platform::fs::open_flags()
        .into_iter()
        .find(|(constant, _)| *constant == name)
        .map_or(0, |(_, value)| value)
}

/// `stringToFlags(flags, name = 'flags')`: the `O_*` bits of the flags string `flags`, such as
/// `'r+'`. A number is returned as is, and `null` and `undefined` are `O_RDONLY`.
pub(super) fn string_to_flags(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let flags = arg(&args, 0);
    if flags.is_number() {
        return Ok(flags.get());
    }
    if flags.is_null() || flags.is_undefined() {
        return Ok(js::value::from_i32(open_flag("O_RDONLY")));
    }
    let [read, write, read_write, append, create, exclusive, sync, truncate] = [
        "O_RDONLY", "O_WRONLY", "O_RDWR", "O_APPEND", "O_CREAT", "O_EXCL", "O_SYNC", "O_TRUNC",
    ]
    .map(open_flag);
    let text = flags
        .is_string()
        .then(|| String::from_jsval(scope, flags, ()).ok())
        .flatten();
    let bits = match text.as_deref() {
        Some("r") => Some(read),
        Some("rs" | "sr") => Some(read | sync),
        Some("r+") => Some(read_write),
        Some("rs+" | "sr+") => Some(read_write | sync),
        Some("w") => Some(truncate | create | write),
        Some("wx" | "xw") => Some(truncate | create | write | exclusive),
        Some("w+") => Some(truncate | create | read_write),
        Some("wx+" | "xw+") => Some(truncate | create | read_write | exclusive),
        Some("a") => Some(append | create | write),
        Some("ax" | "xa") => Some(append | create | write | exclusive),
        Some("as" | "sa") => Some(append | create | write | sync),
        Some("a+") => Some(append | create | read_write),
        Some("ax+" | "xa+") => Some(append | create | read_write | exclusive),
        Some("as+" | "sa+") => Some(append | create | read_write | sync),
        _ => None,
    };
    match bits {
        Some(bits) => Ok(js::value::from_i32(bits)),
        None => {
            let name = if args.len() > 1 && args.get(1).is_string() {
                String::from_jsval(scope, args.get(1), ()).unwrap_or_default()
            } else {
                "flags".to_string()
            };
            Err(NodeError::invalid_arg_value(scope, &name, flags, "is invalid").throw(scope))
        }
    }
}

/// `ERR_OUT_OF_RANGE` for the argument `name`, which must be in `range`, and is `value`.
fn out_of_range(scope: &Scope<'_>, name: &str, range: &str, value: HandleValue<'_>) -> ExnThrown {
    let received = if value.is_number() {
        out_of_range_received(value.to_number())
    } else {
        crate::errors::describe_value(scope, value)
    };
    NodeError::out_of_range(name, range, &received).throw(scope)
}

/// `validateOffsetLengthRead(offset, length, bufferLength)`: that `offset` and `length` are
/// non-negative and `offset + length` is at most `bufferLength`.
pub(super) fn validate_offset_length_read(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let (offset, length) = (arg(&args, 0), arg(&args, 1));
    let buffer_length = arg(&args, 2).to_number();
    if offset.to_number() < 0.0 {
        return Err(out_of_range(scope, "offset", ">= 0", offset));
    }
    if length.to_number() < 0.0 {
        return Err(out_of_range(scope, "length", ">= 0", length));
    }
    if offset.to_number() + length.to_number() > buffer_length {
        let range = format!(
            "<= {}",
            number_to_string(buffer_length - offset.to_number())
        );
        return Err(out_of_range(scope, "length", &range, length));
    }
    Ok(js::value::undefined())
}

/// `validateOffsetLengthWrite(offset, length, byteLength)`: that `offset` is at most
/// `byteLength`, and `length` is an int32 from 0 to `byteLength - offset`.
pub(super) fn validate_offset_length_write(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let (offset, length) = (arg(&args, 0), arg(&args, 1));
    let byte_length = arg(&args, 2).to_number();
    if offset.to_number() > byte_length {
        let range = format!("<= {}", number_to_string(byte_length));
        return Err(out_of_range(scope, "offset", &range, offset));
    }
    if length.to_number() > byte_length - offset.to_number() {
        let range = format!("<= {}", number_to_string(byte_length - offset.to_number()));
        return Err(out_of_range(scope, "length", &range, length));
    }
    if length.to_number() < 0.0 {
        return Err(out_of_range(scope, "length", ">= 0", length));
    }
    crate::errors::validate_integer(scope, length, "length", 0.0, 2147483647.0)
        .map_err(|error| error.throw(scope))?;
    Ok(js::value::undefined())
}

/// The path of the entry `name` in the directory `path`, either of which is a string or a
/// `Buffer`, as `internal/fs/utils`' `join` joins them.
fn join(scope: &Scope<'_>, path: HandleValue<'_>, name: HandleValue<'_>) -> Result<String, Thrown> {
    let text = |value: HandleValue<'_>| -> Result<Option<String>, Thrown> {
        if value.is_string() {
            return Ok(Some(String::from_jsval(scope, value, ())?));
        }
        let view = Object::from_value(scope, value)
            .ok()
            .and_then(js::ArrayBufferView::from_object);
        Ok(view.map(|view| view.with_bytes(|bytes| String::from_utf8_lossy(bytes).into_owned())))
    };
    let invalid = || {
        NodeError::invalid_arg_type(
            scope,
            "path",
            "of type string or an instance of Buffer",
            path,
        )
    };
    let directory = text(path)?.ok_or_else(invalid)?;
    let name = text(name)?.ok_or_else(invalid)?;
    Ok(std::path::Path::new(&directory)
        .join(name)
        .to_string_lossy()
        .into_owned())
}

/// A new `Dirent` for the entry `name` of type `kind` in the directory `path`.
fn dirent<'s>(
    scope: &'s Scope<'_>,
    path: HandleValue<'s>,
    name: HandleValue<'s>,
    kind: HandleValue<'_>,
) -> Result<HandleValue<'s>, Thrown> {
    let file = join(scope, path, name)?;
    crate::fs::new_dirent(scope, name, path, kind.to_number(), &file)
}

/// Call `callback` with `result`: the error, or `null` and the value.
fn call_back(
    scope: &Scope<'_>,
    callback: HandleValue<'_>,
    result: Result<HandleValue<'_>, Thrown>,
) -> Result<(), ExnThrown> {
    let args = match result {
        Ok(value) => vec![scope.root_value(js::value::null()), value],
        Err(error) => {
            let thrown = error.throw(scope);
            vec![js::exception::take_pending(scope).ok_or(thrown)?]
        }
    };
    Function::call(scope, HandleValue::undefined(), callback, &args)?;
    Ok(())
}

/// `getDirent(path, name, type, callback)`: a new `Dirent` for the entry `name` of `type` in the
/// directory `path`, with its type read with `lstat` if it is `UV_DIRENT_UNKNOWN`. With a
/// callback, the `Dirent` or the error is passed to it.
pub(super) fn get_dirent(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let result = dirent(scope, arg(&args, 0), arg(&args, 1), arg(&args, 2));
    let callback = arg(&args, 3);
    if crate::stream::is_function(scope, callback) {
        call_back(scope, callback, result)?;
        return Ok(js::value::undefined());
    }
    Ok(result.map_err(|error| error.throw(scope))?.get())
}

/// `getDirents(path, [names, types], callback)`: replace each name in `names` with a new `Dirent`
/// for the entry of that name and type in the directory `path`, as `getDirent` creates them.
/// With a callback, `names` or the first error is passed to it, and otherwise `names` is
/// returned.
pub(super) fn get_dirents(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    _: HandleValue,
) -> Result<Value, ExnThrown> {
    let path = arg(&args, 0);
    let result = (|| -> Result<HandleValue<'_>, Thrown> {
        let entries = Object::from_value(scope, arg(&args, 1))?;
        let names = Object::from_value(scope, entries.get_element(scope, 0)?)?;
        let types = Object::from_value(scope, entries.get_element(scope, 1)?)?;
        let length = names.get_property(scope, c"length")?.to_number() as u32;
        for index in 0..length {
            let name = names.get_element(scope, index)?;
            let kind = types.get_element(scope, index)?;
            let dirent = dirent(scope, path, name, kind)?;
            names.set_element(scope, index, dirent)?;
        }
        Ok(names.to_jsval_throwing(scope)?)
    })();
    let callback = arg(&args, 2);
    if crate::stream::is_function(scope, callback) {
        call_back(scope, callback, result)?;
        return Ok(js::value::undefined());
    }
    Ok(result.map_err(|error| error.throw(scope))?.get())
}
