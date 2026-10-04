// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `util.parseArgs`: <https://nodejs.org/api/util.html#utilparseargsconfig>.

use std::ffi::CString;

use js::conversion::{FromJSVal, ToJSVal};
use js::error::{ExnThrown, ThrowException};
use js::gc::scope::Scope;
use js::prelude::{HandleObject, HandleValue};
use js::Object;

use crate::errors::{validate_object, ErrorKind, NodeError, Thrown};
use crate::stream::{rust_string, string, truthy, Props};

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

fn property<'s>(
    scope: &'s Scope<'_>,
    object: Object<'_>,
    name: &str,
) -> Result<HandleValue<'s>, ExnThrown> {
    let name = CString::new(name).map_err(|_| ExnThrown)?;
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
    let length = array.get_property(scope, c"length")?;
    Ok(f64::from_jsval(scope, length, ()).map_err(|_| ExnThrown)? as u32)
}

fn parse_error(code: &'static str, message: String) -> NodeError {
    NodeError::new(ErrorKind::TypeError, code, message)
}

/// `util.parseArgs(config)`: the `values` and `positionals` of `config.args`, which defaults to
/// `process.argv` without its first two elements, and the `tokens` if `config.tokens` is truthy.
pub(crate) fn parse_args<'s>(
    scope: &'s Scope<'_>,
    config: Option<HandleValue<'_>>,
) -> Result<Object<'s>, Thrown> {
    let config = match config {
        Some(config) if !config.is_undefined() => Some(validate_object(scope, config, "config")?),
        _ => None,
    };
    let get = |name: &str| -> Result<Option<HandleValue<'s>>, ExnThrown> {
        match config {
            Some(config) => {
                let value = property(scope, config, name)?;
                Ok((!value.is_undefined()).then_some(value))
            }
            None => Ok(None),
        }
    };
    let args = match get("args")? {
        Some(args) => strings(scope, validate_object(scope, args, "args")?, 0)?,
        None => match scope
            .global()
            .obj(scope, c"process")?
            .and_then(|process| process.obj(scope, c"argv").transpose())
        {
            Some(argv) => strings(scope, argv?, 2)?,
            None => Vec::new(),
        },
    };
    let strict = get("strict")?.is_none_or(|value| truthy(scope, value));
    let allow_positionals = get("allowPositionals")?.map_or(!strict, |value| truthy(scope, value));
    let allow_negative = get("allowNegative")?.is_some_and(|value| truthy(scope, value));
    let return_tokens = get("tokens")?.is_some_and(|value| truthy(scope, value));

    let mut options = Vec::new();
    if let Some(given) = get("options")? {
        let given = validate_object(scope, given, "options")?;
        for key in given.own_keys(scope)? {
            let name = String::from_jsval(scope, key, ()).map_err(|e| e.throw(scope))?;
            let option = validate_object(
                scope,
                property(scope, given, &name)?,
                &format!("options.{name}"),
            )?;
            let kind = rust_string(scope, option.val(scope, c"type")?);
            let string = match kind.as_deref() {
                Some("string") => true,
                Some("boolean") => false,
                _ => {
                    return Err(NodeError::invalid_arg_type(
                        scope,
                        &format!("options.{name}.type"),
                        "one of: 'string', 'boolean'",
                        option.val(scope, c"type")?,
                    )
                    .into());
                }
            };
            let short = rust_string(scope, option.val(scope, c"short")?);
            if short
                .as_ref()
                .is_some_and(|short| short.chars().count() != 1)
            {
                return Err(NodeError::invalid_arg_value(
                    scope,
                    &format!("options.{name}.short"),
                    option.val(scope, c"short")?,
                    "must be a single character",
                )
                .into());
            }
            let multiple = truthy(scope, option.val(scope, c"multiple")?);
            let default = option.val(scope, c"default")?;
            if !default.is_undefined() {
                let element_valid = |value: HandleValue<'_>| {
                    if string {
                        value.is_string()
                    } else {
                        value.is_boolean()
                    }
                };
                let valid = if multiple {
                    match Object::from_value(scope, *default) {
                        Ok(list) if js::Array::is_array(scope, list.handle())? => {
                            let length = array_length(scope, list)?;
                            let mut all = true;
                            for index in 0..length {
                                all &= element_valid(list.get_element(scope, index)?);
                            }
                            all
                        }
                        _ => false,
                    }
                } else {
                    element_valid(default)
                };
                if !valid {
                    let kind = match (string, multiple) {
                        (true, false) => "of type string",
                        (false, false) => "of type boolean",
                        (true, true) => "an instance of Array",
                        (false, true) => "an instance of Array",
                    };
                    return Err(NodeError::invalid_arg_type(
                        scope,
                        &format!("options.{name}.default"),
                        kind,
                        default,
                    )
                    .into());
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
                let negated = allow_negative
                    && name.starts_with("no-")
                    && find(&name[3..]).is_none_or(|option| !option.string)
                    && value.is_none();
                let name = if negated { &name[3..] } else { name.as_str() };
                let option = find(name);
                if strict {
                    let Some(option) = option else {
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
                    let short_and_long = match &option.short {
                        Some(short) => format!("-{short}, --{name}"),
                        None => format!("--{name}"),
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
                    if option.string
                        && *inline_value == Some(false)
                        && value
                            .as_deref()
                            .is_some_and(|value| value.len() > 1 && value.starts_with('-'))
                    {
                        let example = match &option.short {
                            Some(short) if raw_name.len() == 2 => {
                                format!("'--{name}=-XYZ' or '-{short}-XYZ'")
                            }
                            _ => format!("'--{name}=-XYZ'"),
                        };
                        return Err(parse_error(
                            "ERR_PARSE_ARGS_INVALID_OPTION_VALUE",
                            format!(
                                "Option '{short_and_long}' argument is ambiguous.\nDid you forget to specify the option argument for '{raw_name}'?\nTo specify an option argument starting with a dash use {example}."
                            ),
                        )
                        .into());
                    }
                }
                let stored = match value {
                    Some(value) if option.is_none_or(|option| option.string) => {
                        string(scope, value)?
                    }
                    _ => scope.root_value(js::value::from_bool(!negated)),
                };
                if option.is_some_and(|option| option.multiple) {
                    let existing = property(scope, values, name)?;
                    let list = match Object::from_value(scope, *existing) {
                        Ok(list) => list,
                        Err(_) => {
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
                positionals.set_element(scope, positional_count, string(scope, value)?)?;
                positional_count += 1;
            }
            Token::Terminator { .. } => {}
        }
    }
    for option in &options {
        if let Some(default) = option.default {
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
                    entry.set_property(scope, c"kind", string(scope, "option")?)?;
                    let negated = allow_negative
                        && name.starts_with("no-")
                        && find(&name[3..]).is_none_or(|option| !option.string)
                        && value.is_none();
                    let name = if negated { &name[3..] } else { name.as_str() };
                    entry.set_property(scope, c"name", string(scope, name)?)?;
                    entry.set_property(scope, c"rawName", string(scope, raw_name)?)?;
                    entry.set_property(scope, c"index", *index as f64)?;
                    match value {
                        Some(value) => {
                            entry.set_property(scope, c"value", string(scope, value)?)?
                        }
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
                    entry.set_property(scope, c"kind", string(scope, "positional")?)?;
                    entry.set_property(scope, c"index", *index as f64)?;
                    entry.set_property(scope, c"value", string(scope, value)?)?;
                }
                Token::Terminator { index } => {
                    entry.set_property(scope, c"kind", string(scope, "option-terminator")?)?;
                    entry.set_property(scope, c"index", *index as f64)?;
                }
            }
            list.set_element(scope, position as u32, entry)?;
        }
        result.set_property(scope, c"tokens", *list)?;
    }
    Ok(result)
}
