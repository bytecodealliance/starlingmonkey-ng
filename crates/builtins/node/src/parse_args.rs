// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `util.parseArgs`: <https://nodejs.org/api/util.html#utilparseargsconfig>.

use std::ffi::CString;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::prelude::{HandleObject, HandleValue};
use js::Object;

use crate::errors::{
    validate_boolean, validate_plain_object, validate_string, ErrorKind, NodeError, Thrown,
};
use crate::support::{rust_string, Props};

/// An option of the `options` config.
struct OptionConfig<'s> {
    name: String,
    string: bool,
    short: Option<String>,
    multiple: bool,
    default: Option<HandleValue<'s>>,
}

/// A token of the parsed arguments.
enum Token {
    Option {
        index: usize,
        name: String,
        raw_name: String,
        value: Option<String>,
        inline_value: Option<bool>,
    },
    Positional {
        index: usize,
        value: String,
    },
    Terminator {
        index: usize,
    },
}

/// Node.js's `objectGetOwn(object, name)`: `object[name]` if `object` has an own property
/// `name`, and `undefined` otherwise.
fn property<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let name = CString::new(name).map_err(|_| ExnThrown)?;
    if !object.has_own_property_by_name(scope, &name)? {
        return Ok(HandleValue::undefined());
    }
    object.get_property_by_name(scope, &name)
}

fn set<'v>(
    scope: &'v Scope<'_>,
    object: Object<'_>,
    name: &str,
    value: impl ToJSVal<'v>,
) -> Result<(), ExnThrown> {
    let name = CString::new(name).map_err(|_| ExnThrown)?;
    object.set_property_by_name(scope, &name, value)
}

fn strings(scope: &Scope<'_>, array: Object<'_>, from: u32) -> Result<Vec<String>, ExnThrown> {
    let length = array_length(scope, array)?;
    (from..length)
        .map(|index| {
            String::from_jsval(scope, array.get_element(scope, index)?, ())
                .map_err(|e| e.throw(scope))
        })
        .collect()
}

fn array_length(scope: &Scope<'_>, array: Object<'_>) -> Result<u32, ExnThrown> {
    match array.cast::<js::Array>() {
        Ok(array) => array.length(scope),
        Err(_) => {
            let length = array.get_property(scope, c"length")?;
            Ok(f64::from_jsval(scope, length, ()).map_err(|_| ExnThrown)? as u32)
        }
    }
}

fn parse_error(code: &'static str, message: String) -> NodeError {
    NodeError::new(ErrorKind::TypeError, code, message)
}

/// Node.js's `validateArray` for the element `value` of `name`, followed by `validate` on each
/// element, with errors naming the elements `name[index]`.
fn validate_elements(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
    name: &str,
    validate: impl Fn(HandleValue<'_>, &str) -> Result<(), Thrown>,
) -> Result<(), Thrown> {
    let array = Object::from_value(scope, *value)
        .ok()
        .filter(|array| js::Array::is_array(scope, array.handle()).unwrap_or(false))
        .ok_or_else(|| NodeError::invalid_arg_type(scope, name, "an instance of Array", value))?;
    for index in 0..array_length(scope, array)? {
        validate(
            array.get_element(scope, index)?,
            &format!("{name}[{index}]"),
        )?;
    }
    Ok(())
}

/// `util.parseArgs(config)`: the `values` and `positionals` of `config.args`, which defaults to
/// `process.argv` without its first two elements, and the `tokens` if `config.tokens` is true.
/// The members of `config` are read as own properties, `null` ones as missing, and a `config`
/// that isn't an object has none. A `null` `config` throws a `TypeError`.
pub(crate) fn parse_args<'s>(
    scope: &'s Scope<'_>,
    config: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let config = match config {
        Some(config) if config.is_null() => {
            let message = "Cannot convert undefined or null to object";
            return Err(js::error::TypeError(message.into()).throw(scope).into());
        }
        Some(config) => Object::from_value(scope, *config).ok(),
        None => None,
    };
    let get = |name: &str| -> Result<Option<HandleValue<'s>>, ExnThrown> {
        match config {
            Some(config) => {
                let value = property(scope, config, name)?;
                Ok((!value.is_null_or_undefined()).then_some(value))
            }
            None => Ok(None),
        }
    };
    let args = match get("args")? {
        Some(args) => {
            let array = Object::from_value(scope, *args)
                .ok()
                .filter(|array| js::Array::is_array(scope, array.handle()).unwrap_or(false))
                .ok_or_else(|| {
                    NodeError::invalid_arg_type(scope, "args", "an instance of Array", args)
                })?;
            strings(scope, array, 0)?
        }
        None => match Object::from_value(scope, *crate::process::process_value(scope)?)
            .ok()
            .map(|process| process.obj(scope, c"argv"))
            .transpose()?
            .flatten()
        {
            Some(argv) => strings(scope, argv, 2)?,
            None => Vec::new(),
        },
    };
    let boolean = |name: &str, default: bool| -> Result<bool, Thrown> {
        match get(name)? {
            Some(value) => validate_boolean(scope, value, name),
            None => Ok(default),
        }
    };
    let strict_value = get("strict")?;
    let strict = strict_value.is_none_or(|value| !value.is_boolean() || value.to_boolean());
    let allow_positionals_value = get("allowPositionals")?;
    if let Some(value) = strict_value {
        validate_boolean(scope, value, "strict")?;
    }
    if let Some(value) = allow_positionals_value {
        validate_boolean(scope, value, "allowPositionals")?;
    }
    let allow_positionals = allow_positionals_value.map_or(!strict, |value| value.to_boolean());
    let return_tokens = boolean("tokens", false)?;
    let allow_negative = boolean("allowNegative", false)?;

    let mut options = Vec::new();
    if let Some(given) = get("options")? {
        let given = validate_plain_object(scope, given, "options", false)?;
        for key in given.own_keys(scope)? {
            let name = String::from_jsval(scope, key, ()).map_err(|e| e.throw(scope))?;
            let option = validate_plain_object(
                scope,
                property(scope, given, &name)?,
                &format!("options.{name}"),
                false,
            )?;
            let kind = rust_string(scope, property(scope, option, "type")?);
            let string = match kind.as_deref() {
                Some("string") => true,
                Some("boolean") => false,
                _ => {
                    return Err(NodeError::invalid_arg_type(
                        scope,
                        &format!("options.{name}.type"),
                        "('string|boolean')",
                        property(scope, option, "type")?,
                    )
                    .into());
                }
            };
            let mut short = None;
            if option.has_own_property(scope, c"short")? {
                let label = format!("options.{name}.short");
                let value = option.get_property(scope, c"short")?;
                let text = validate_string(scope, value, &label)?;
                if text.encode_utf16().count() != 1 {
                    let reason = "must be a single character";
                    return Err(NodeError::invalid_arg_value(scope, &label, value, reason).into());
                }
                short = Some(text);
            }
            let multiple = property(scope, option, "multiple")?;
            if option.has_own_property(scope, c"multiple")? {
                validate_boolean(scope, multiple, &format!("options.{name}.multiple"))?;
            }
            let multiple = js::conversion::to_boolean(multiple);
            let default = property(scope, option, "default")?;
            if !default.is_undefined() {
                let label = format!("options.{name}.default");
                let validate = |value: HandleValue<'_>, label: &str| -> Result<(), Thrown> {
                    if string {
                        validate_string(scope, value, label)?;
                    } else {
                        validate_boolean(scope, value, label)?;
                    }
                    Ok(())
                };
                if multiple {
                    validate_elements(scope, default, &label, validate)?;
                } else {
                    validate(default, &label)?;
                }
            }
            options.push(OptionConfig {
                name,
                string,
                short,
                multiple,
                default: (!default.is_undefined()).then_some(default),
            });
        }
    }
    let find = |name: &str| options.iter().find(|option| option.name == name);
    // Whether the option `name` with `value` sets `NAME` to `false`, as `--no-NAME` does.
    let negates = |name: &str, value: &Option<String>| {
        allow_negative && name.starts_with("no-") && value.is_none()
    };
    let long_of_short = |short: &str| {
        options
            .iter()
            .find(|option| option.short.as_deref() == Some(short))
            .map_or_else(|| short.to_string(), |option| option.name.clone())
    };
    let is_string = |name: &str| find(name).is_some_and(|option| option.string);

    let mut tokens = Vec::new();
    let mut remaining: std::collections::VecDeque<String> = args.into();
    let mut index = 0;
    while let Some(arg) = remaining.pop_front() {
        let start = index;
        index += 1;
        let chars: Vec<char> = arg.chars().collect();
        if arg == "--" {
            tokens.push(Token::Terminator { index: start });
            for value in remaining.drain(..) {
                tokens.push(Token::Positional { index, value });
                index += 1;
            }
            break;
        }
        if chars.len() == 2 && chars[0] == '-' && chars[1] != '-' {
            let short = chars[1].to_string();
            let name = long_of_short(&short);
            let value = if is_string(&name) {
                remaining.pop_front()
            } else {
                None
            };
            if value.is_some() {
                index += 1;
            }
            let inline_value = value.as_ref().map(|_| false);
            tokens.push(Token::Option {
                index: start,
                name,
                raw_name: arg,
                value,
                inline_value,
            });
            continue;
        }
        if chars.len() > 2 && chars[0] == '-' && chars[1] != '-' {
            if is_string(&long_of_short(&chars[1].to_string())) {
                let short = chars[1].to_string();
                tokens.push(Token::Option {
                    index: start,
                    name: long_of_short(&short),
                    raw_name: format!("-{short}"),
                    value: Some(chars[2..].iter().collect()),
                    inline_value: Some(true),
                });
                continue;
            }
            // A group of short options: each character is its own option, and a string option
            // takes the rest of the group as its value, or the next argument at the end.
            for (position, letter) in chars[1..].iter().enumerate() {
                let short = letter.to_string();
                let name = long_of_short(&short);
                let last = position == chars.len() - 2;
                if is_string(&name) && !last {
                    tokens.push(Token::Option {
                        index: start,
                        name,
                        raw_name: format!("-{short}"),
                        value: Some(chars[position + 2..].iter().collect()),
                        inline_value: Some(true),
                    });
                    break;
                }
                let value = if is_string(&name) && last {
                    remaining.pop_front()
                } else {
                    None
                };
                if value.is_some() {
                    index += 1;
                }
                let inline_value = value.as_ref().map(|_| false);
                tokens.push(Token::Option {
                    index: start,
                    name,
                    raw_name: format!("-{short}"),
                    value,
                    inline_value,
                });
            }
            continue;
        }
        if let Some(long) = arg.strip_prefix("--") {
            if let Some((name, value)) = long.split_once('=') {
                tokens.push(Token::Option {
                    index: start,
                    name: name.to_string(),
                    raw_name: format!("--{name}"),
                    value: Some(value.to_string()),
                    inline_value: Some(true),
                });
            } else {
                let value = if is_string(long) {
                    remaining.pop_front()
                } else {
                    None
                };
                if value.is_some() {
                    index += 1;
                }
                let inline_value = value.as_ref().map(|_| false);
                tokens.push(Token::Option {
                    index: start,
                    name: long.to_string(),
                    raw_name: arg.clone(),
                    value,
                    inline_value,
                });
            }
            continue;
        }
        tokens.push(Token::Positional {
            index: start,
            value: arg,
        });
    }

    let values = Object::new_plain(scope)?;
    values.set_prototype(scope, HandleObject::null())?;
    let positionals = js::Array::new(scope, 0)?;
    let mut positional_count = 0;
    for token in &tokens {
        match token {
            Token::Option {
                name,
                raw_name,
                value,
                inline_value,
                ..
            } => {
                if strict {
                    // A missing option `no-NAME` is the negation of the boolean option `NAME`.
                    let checked = match find(name) {
                        Some(option) => Some(option),
                        None if allow_negative && name.starts_with("no-") => {
                            find(&name[3..]).filter(|option| !option.string)
                        }
                        None => None,
                    };
                    let Some(option) = checked else {
                        let hint = if allow_positionals {
                            format!(
                                ". To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- \"{raw_name}\""
                            )
                        } else {
                            String::new()
                        };
                        return Err(parse_error(
                            "ERR_PARSE_ARGS_UNKNOWN_OPTION",
                            format!("Unknown option '{raw_name}'{hint}"),
                        )
                        .into());
                    };
                    let long = &option.name;
                    let short_and_long = match &option.short {
                        Some(short) => format!("-{short}, --{long}"),
                        None => format!("--{long}"),
                    };
                    if option.string && value.is_none() {
                        return Err(parse_error(
                            "ERR_PARSE_ARGS_INVALID_OPTION_VALUE",
                            format!("Option '{short_and_long} <value>' argument missing"),
                        )
                        .into());
                    }
                    if !option.string && value.is_some() {
                        return Err(parse_error(
                            "ERR_PARSE_ARGS_INVALID_OPTION_VALUE",
                            format!("Option '{short_and_long}' does not take an argument"),
                        )
                        .into());
                    }
                    if *inline_value != Some(true)
                        && value.as_deref().is_some_and(|value| {
                            value.starts_with('-') && value.encode_utf16().count() > 1
                        })
                    {
                        let example = if raw_name.starts_with("--") {
                            format!("'{raw_name}=-XYZ'")
                        } else {
                            format!("'--{name}=-XYZ' or '{raw_name}-XYZ'")
                        };
                        return Err(parse_error(
                            "ERR_PARSE_ARGS_INVALID_OPTION_VALUE",
                            format!(
                                "Option '{raw_name}' argument is ambiguous.\nDid you forget to specify the option argument for '{raw_name}'?\nTo specify an option argument starting with a dash use {example}."
                            ),
                        )
                        .into());
                    }
                }
                if name == "__proto__" {
                    continue;
                }
                let negated = negates(name, value);
                let name = if negated { &name[3..] } else { name.as_str() };
                let stored = match value {
                    Some(value) => js::conversion::ToJSVal::to_jsval_throwing(value, scope)?,
                    None => scope.root_value(js::value::from_bool(!negated)),
                };
                if find(name).is_some_and(|option| option.multiple) {
                    let existing = property(scope, values, name)?;
                    let list = match Object::from_value(scope, *existing) {
                        Ok(list) if js::conversion::to_boolean(existing) => list,
                        _ => {
                            let list = *js::Array::new(scope, 0)?;
                            set(scope, values, name, list)?;
                            list
                        }
                    };
                    let length = array_length(scope, list)?;
                    list.set_element(scope, length, stored)?;
                } else {
                    set(scope, values, name, stored)?;
                }
            }
            Token::Positional { value, .. } => {
                if !allow_positionals {
                    return Err(parse_error(
                        "ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL",
                        format!("Unexpected argument '{value}'. This command does not take positional arguments"),
                    )
                    .into());
                }
                positionals.set_element(
                    scope,
                    positional_count,
                    js::conversion::ToJSVal::to_jsval_throwing(value, scope)?,
                )?;
                positional_count += 1;
            }
            Token::Terminator { .. } => {}
        }
    }
    for option in &options {
        if let Some(default) = option.default.filter(|_| option.name != "__proto__") {
            if property(scope, values, &option.name)?.is_undefined() {
                set(scope, values, &option.name, default)?;
            }
        }
    }

    let result = Object::new_plain(scope)?;
    result.set_property(scope, c"values", values)?;
    result.set_property(scope, c"positionals", *positionals)?;
    if return_tokens {
        let list = js::Array::new(scope, 0)?;
        for (position, token) in tokens.iter().enumerate() {
            let entry = Object::new_plain(scope)?;
            match token {
                Token::Option {
                    index,
                    name,
                    raw_name,
                    value,
                    inline_value,
                } => {
                    entry.set_property(
                        scope,
                        c"kind",
                        js::class::get_or_init_atom(scope, c"option")?,
                    )?;
                    let negated = name != "__proto__" && negates(name, value);
                    let name = if negated { &name[3..] } else { name.as_str() };
                    entry.set_property(
                        scope,
                        c"name",
                        js::conversion::ToJSVal::to_jsval_throwing(name, scope)?,
                    )?;
                    entry.set_property(
                        scope,
                        c"rawName",
                        js::conversion::ToJSVal::to_jsval_throwing(raw_name, scope)?,
                    )?;
                    entry.set_property(scope, c"index", *index as f64)?;
                    match value {
                        Some(value) => entry.set_property(
                            scope,
                            c"value",
                            js::conversion::ToJSVal::to_jsval_throwing(value, scope)?,
                        )?,
                        None => entry.set_property(scope, c"value", js::value::undefined())?,
                    }
                    match inline_value {
                        Some(inline) => entry.set_property(scope, c"inlineValue", *inline)?,
                        None => {
                            entry.set_property(scope, c"inlineValue", js::value::undefined())?
                        }
                    }
                }
                Token::Positional { index, value } => {
                    entry.set_property(
                        scope,
                        c"kind",
                        js::class::get_or_init_atom(scope, c"positional")?,
                    )?;
                    entry.set_property(scope, c"index", *index as f64)?;
                    entry.set_property(
                        scope,
                        c"value",
                        js::conversion::ToJSVal::to_jsval_throwing(value, scope)?,
                    )?;
                }
                Token::Terminator { index } => {
                    entry.set_property(
                        scope,
                        c"kind",
                        js::class::get_or_init_atom(scope, c"option-terminator")?,
                    )?;
                    entry.set_property(scope, c"index", *index as f64)?;
                }
            }
            list.set_element(scope, position as u32, entry)?;
        }
        result.set_property(scope, c"tokens", *list)?;
    }
    Ok(result)
}
