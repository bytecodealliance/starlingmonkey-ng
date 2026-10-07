// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The errors Node.js throws for its web classes, such as `URL`, `EventTarget` and
//! `ReadableStream`, which Node.js implements on its own validators.
//!
//! [`binding_error`] is the [`js::error::BindingErrorHook`] installed in `--node-compat` mode. It
//! creates Node's error for the errors that the binding code of the web builtins throws, which are
//! the `TypeError`s of WebIDL's argument conversions and brand checks, and for the
//! [`Operation`](BindingError::Operation) errors of the builtins' own steps. The tables below give
//! Node's error for each interface and member, as the pinned Node.js release throws it. Members
//! without an entry, and argument errors that an entry doesn't describe, keep the web error.

use js::conversion::FromJSVal;
use js::error::{BindingError, BindingSite, ExnThrown, JSExnType, MemberKind, ThrowException};
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use super::{ErrorKind, NodeError};
use crate::stream::adapters::is_readable_stream;

/// How Node.js reports a `this` that isn't an instance of an interface.
#[derive(Clone, Copy)]
enum This {
    /// `ERR_INVALID_THIS` naming the type.
    Invalid(&'static str),
    /// `ERR_INVALID_THIS` with a fixed message.
    InvalidMessage(&'static str),
    /// A `TypeError` without a code, with a fixed message.
    Plain(&'static str),
    /// V8's `TypeError` for reading the private field with this name from another object.
    PrivateMember(&'static str),
    /// The web error.
    Web,
}

/// How Node.js reports a call of a constructor that only the runtime calls.
#[derive(Clone, Copy)]
enum Constructor {
    /// `ERR_ILLEGAL_CONSTRUCTOR`.
    Illegal,
    /// A `TypeError` with a code and a fixed message.
    Coded(&'static str, &'static str),
}

struct Interface {
    /// The interface's name in the runtime, which can differ from Node's name for it.
    name: &'static str,
    this: This,
    constructor: Constructor,
    /// Whether Node.js takes the class from undici, whose class names start with `_`.
    undici: bool,
}

/// How Node.js reports a call without a required argument.
#[derive(Clone, Copy)]
enum Missing {
    /// `ERR_MISSING_ARGS` naming the arguments.
    Args(&'static [&'static str]),
    /// The argument is validated as `undefined`, with the member's [`Arg`] for it.
    Undefined,
    /// undici's `TypeError` without a code, for a member with this many required arguments.
    Undici(u32),
    /// The `ERR_MISSING_ARGS` of Node's WebIDL layer, for a member with this many required
    /// arguments.
    WebIdl(u32),
    /// `ERR_MISSING_ARGS` with a fixed message.
    Message(&'static str),
    /// The web error.
    Web,
}

/// A function creating Node's error for an argument value that fails to convert, or `None` to
/// keep the web error.
type Validate = fn(&Scope<'_>, HandleValue<'_>) -> Result<Option<NodeError>, ExnThrown>;

/// How Node.js reports an argument that fails to convert.
#[derive(Clone, Copy)]
enum Arg {
    /// `ERR_INVALID_ARG_TYPE` naming the argument and the expected type, such as "of type
    /// function" or "an instance of Event".
    Type(&'static str, &'static str),
    /// `ERR_INVALID_ARG_TYPE` naming the argument, which must be "of type object", for a value
    /// that isn't an object. An object that fails to convert keeps the web error.
    Object(&'static str),
    /// A `TypeError` with a code and a fixed message.
    Coded(&'static str, &'static str),
    /// A `TypeError` without a code, with a fixed message.
    Plain(&'static str),
    /// The `ERR_INVALID_ARG_VALUE` of Node's WebIDL layer for a value of the named enumeration.
    Enum(&'static str),
    /// Node's error as a function creates it.
    Custom(Validate),
    /// The web error.
    Web,
}

struct Member {
    interface: &'static str,
    member: &'static str,
    kind: MemberKind,
    /// The `this` error, where it differs from the interface's.
    this: Option<This>,
    missing: Missing,
    args: &'static [Arg],
}

const fn member(
    interface: &'static str,
    member: &'static str,
    kind: MemberKind,
    missing: Missing,
    args: &'static [Arg],
) -> Member {
    Member {
        interface,
        member,
        kind,
        this: None,
        missing,
        args,
    }
}

const fn method(
    interface: &'static str,
    name: &'static str,
    missing: Missing,
    args: &'static [Arg],
) -> Member {
    member(interface, name, MemberKind::Method, missing, args)
}

const fn constructor(interface: &'static str, missing: Missing, args: &'static [Arg]) -> Member {
    member(
        interface,
        "constructor",
        MemberKind::Constructor,
        missing,
        args,
    )
}

const fn this(interface: &'static str, name: &'static str, kind: MemberKind, this: This) -> Member {
    Member {
        interface,
        member: name,
        kind,
        this: Some(this),
        missing: Missing::Web,
        args: &[],
    }
}

const fn interface(name: &'static str, this: This) -> Interface {
    Interface {
        name,
        this,
        constructor: Constructor::Illegal,
        undici: false,
    }
}

/// An interface whose class Node.js takes from undici.
const fn undici(name: &'static str) -> Interface {
    Interface {
        name,
        this: This::InvalidMessage("Illegal invocation"),
        constructor: Constructor::Illegal,
        undici: true,
    }
}

use Arg::*;
use MemberKind::{Getter, Method, StaticMethod};
use Missing::{Args, Undefined, Undici, WebIdl};

const OBJECT: &str = "of type object";
const FUNCTION: &str = "of type function";
const SEQUENCE: Arg = Coded(
    "ERR_INVALID_ARG_TYPE",
    "Value cannot be converted to sequence.",
);
const URL_RECEIVER: This = This::Plain("Receiver must be an instance of class URL");

static INTERFACES: &[Interface] = &[
    interface("AbortController", This::PrivateMember("signal")),
    interface("AbortSignal", This::Invalid("AbortSignal")),
    interface("Blob", This::Invalid("Blob")),
    interface("BroadcastChannel", This::Invalid("BroadcastChannel")),
    interface("ByteLengthQueuingStrategy", This::PrivateMember("state")),
    interface("CompressionStream", This::PrivateMember("transform")),
    interface("CountQueuingStrategy", This::PrivateMember("state")),
    interface("Crypto", This::Invalid("Crypto")),
    interface("CustomEvent", This::Invalid("CustomEvent")),
    interface(
        "DOMException",
        This::InvalidMessage("Value of \"this\" must be of DOMException"),
    ),
    interface("DecompressionStream", This::PrivateMember("transform")),
    interface("Event", This::Invalid("Event")),
    interface("EventTarget", This::Invalid("EventTarget")),
    interface("File", This::Invalid("File")),
    undici("Headers"),
    interface("MessageChannel", This::Web),
    undici("MessageEvent"),
    Interface {
        name: "MessagePort",
        this: This::Plain("Illegal invocation"),
        constructor: Constructor::Coded(
            "ERR_CONSTRUCT_CALL_INVALID",
            "Constructor cannot be called",
        ),
        undici: false,
    },
    interface("Navigator", This::Web),
    interface("Performance", This::Invalid("Performance")),
    interface("PerformanceEntry", This::Invalid("PerformanceEntry")),
    interface("PerformanceMark", This::Invalid("PerformanceMark")),
    interface("PerformanceMeasure", This::Invalid("PerformanceMeasure")),
    interface("PerformanceObserver", This::PrivateMember("entryTypes")),
    interface(
        "PerformanceObserverEntryList",
        This::Invalid("PerformanceObserverEntryList"),
    ),
    interface(
        "ReadableByteStreamController",
        This::Invalid("ReadableByteStreamController"),
    ),
    interface("ReadableStream", This::Invalid("ReadableStream")),
    interface(
        "ReadableStreamBYOBReader",
        This::Invalid("ReadableStreamBYOBReader"),
    ),
    interface(
        "ReadableStreamBYOBRequest",
        This::Invalid("ReadableStreamBYOBRequest"),
    ),
    interface("ReadableStreamDefaultController", This::Web),
    interface(
        "ReadableStreamDefaultReader",
        This::Invalid("ReadableStreamDefaultReader"),
    ),
    undici("Request"),
    undici("Response"),
    interface("TextDecoder", This::Invalid("TextDecoder")),
    interface("TextDecoderStream", This::PrivateMember("transform")),
    interface("TextEncoder", This::Invalid("TextEncoder")),
    interface("TextEncoderStream", This::PrivateMember("transform")),
    interface("TransformStream", This::Invalid("TransformStream")),
    interface(
        "TransformStreamDefaultController",
        This::Invalid("TransformStreamDefaultController"),
    ),
    interface("URL", This::PrivateMember("context")),
    interface("URLPattern", This::Plain("Illegal invocation")),
    interface("URLSearchParams", This::Invalid("URLSearchParams")),
    interface(
        "URLSearchParams Iterator",
        This::Invalid("URLSearchParamsIterator"),
    ),
    interface("WritableStream", This::Invalid("WritableStream")),
    interface(
        "WritableStreamDefaultController",
        This::Invalid("WritableStreamDefaultController"),
    ),
    interface(
        "WritableStreamDefaultWriter",
        This::Invalid("WritableStreamDefaultWriter"),
    ),
    undici("WebSocket"),
];

static MEMBERS: &[Member] = &[
    // `URL` and `URLSearchParams`
    constructor("URL", Args(&["url"]), &[]),
    member("URL", "canParse", StaticMethod, Args(&["url"]), &[]),
    member("URL", "parse", StaticMethod, Args(&["url"]), &[]),
    member(
        "URL",
        "createObjectURL",
        StaticMethod,
        Undefined,
        &[Type("obj", "an instance of Blob")],
    ),
    member("URL", "revokeObjectURL", StaticMethod, Args(&["url"]), &[]),
    this("URL", "href", Getter, URL_RECEIVER),
    this("URL", "search", Getter, URL_RECEIVER),
    this("URL", "toString", Method, URL_RECEIVER),
    this("URL", "toJSON", Method, URL_RECEIVER),
    this(
        "URL",
        "searchParams",
        Getter,
        This::PrivateMember("searchParams"),
    ),
    method("URLSearchParams", "append", Args(&["name", "value"]), &[]),
    method("URLSearchParams", "set", Args(&["name", "value"]), &[]),
    method("URLSearchParams", "delete", Args(&["name"]), &[]),
    method("URLSearchParams", "get", Args(&["name"]), &[]),
    method("URLSearchParams", "getAll", Args(&["name"]), &[]),
    method("URLSearchParams", "has", Args(&["name"]), &[]),
    method(
        "URLSearchParams",
        "forEach",
        Undefined,
        &[Type("callback", FUNCTION)],
    ),
    // `AbortSignal`
    member(
        "AbortSignal",
        "timeout",
        StaticMethod,
        Undefined,
        &[Custom(validate_delay)],
    ),
    member(
        "AbortSignal",
        "any",
        StaticMethod,
        Undefined,
        &[Custom(validate_signals)],
    ),
    // `EventTarget`, `Event` and `CustomEvent`
    // The third argument of `addEventListener` fails to convert only for its `signal` member,
    // whose value is the error's value.
    method(
        "EventTarget",
        "addEventListener",
        Args(&["type", "listener"]),
        &[
            Web,
            Type("listener", "an instance of EventListener"),
            Type("options.signal", "an instance of AbortSignal"),
        ],
    ),
    method(
        "EventTarget",
        "removeEventListener",
        Args(&["type", "listener"]),
        &[Web, Type("listener", "an instance of EventListener")],
    ),
    method(
        "EventTarget",
        "dispatchEvent",
        Args(&["event"]),
        &[Type("event", "an instance of Event")],
    ),
    constructor("Event", Args(&["type"]), &[Web, Object("options")]),
    method("Event", "initEvent", Args(&["type"]), &[]),
    constructor("CustomEvent", Args(&["type"]), &[Web, Object("options")]),
    // `TextEncoder` and `TextDecoder`
    method(
        "TextEncoder",
        "encodeInto",
        Undefined,
        &[
            Type("src", "of type string"),
            Type("dest", "an instance of Uint8Array"),
        ],
    ),
    constructor("TextDecoder", Missing::Web, &[Web, Object("options")]),
    method(
        "TextDecoder",
        "decode",
        Missing::Web,
        &[
            Coded(
                "ERR_INVALID_ARG_TYPE",
                "The \"list\" argument must be an instance of SharedArrayBuffer, ArrayBuffer or \
                 ArrayBufferView.",
            ),
            Object("options"),
        ],
    ),
    constructor("TextDecoderStream", Missing::Web, &[Web, Object("options")]),
    this(
        "TextDecoderStream",
        "encoding",
        Getter,
        This::PrivateMember("handle"),
    ),
    this(
        "TextDecoderStream",
        "fatal",
        Getter,
        This::PrivateMember("handle"),
    ),
    this(
        "TextDecoderStream",
        "ignoreBOM",
        Getter,
        This::PrivateMember("handle"),
    ),
    this(
        "TextEncoderStream",
        "encoding",
        Getter,
        This::PrivateMember("handle"),
    ),
    // `Blob` and `File`
    constructor(
        "Blob",
        Missing::Web,
        &[SEQUENCE, Custom(validate_blob_options)],
    ),
    constructor(
        "File",
        Args(&["fileBits", "fileName"]),
        &[SEQUENCE, Web, Custom(validate_blob_options)],
    ),
    // `performance` and the `Performance*` classes
    method(
        "Performance",
        "mark",
        Args(&["name"]),
        &[Web, Object("options")],
    ),
    method("Performance", "measure", Args(&["name"]), &[]),
    method("Performance", "getEntriesByName", Args(&["name"]), &[]),
    method("Performance", "getEntriesByType", Args(&["type"]), &[]),
    constructor(
        "PerformanceMark",
        Args(&["name"]),
        &[Web, Object("options")],
    ),
    constructor(
        "PerformanceObserver",
        Undefined,
        &[Type("callback", FUNCTION)],
    ),
    method(
        "PerformanceObserver",
        "observe",
        Missing::Web,
        &[Custom(validate_observer_options)],
    ),
    this(
        "PerformanceObserver",
        "takeRecords",
        Method,
        This::PrivateMember("buffer"),
    ),
    method(
        "PerformanceObserverEntryList",
        "getEntriesByName",
        Args(&["name"]),
        &[],
    ),
    method(
        "PerformanceObserverEntryList",
        "getEntriesByType",
        Args(&["type"]),
        &[],
    ),
    // Messaging and `structuredClone`
    constructor("BroadcastChannel", Args(&["name"]), &[]),
    method("BroadcastChannel", "postMessage", Args(&["message"]), &[]),
    method(
        "MessagePort",
        "postMessage",
        Missing::Message("Not enough arguments to MessagePort.postMessage"),
        &[Web, Custom(validate_transfer_list)],
    ),
    constructor(
        "URLSearchParams",
        Missing::Web,
        &[Coded(
            "ERR_INVALID_TUPLE",
            "Each query pair must be an iterable [name, value] tuple",
        )],
    ),
    member(
        "",
        "structuredClone",
        MemberKind::Function,
        Missing::Message("The \"The value argument must be specified\" argument must be specified"),
        &[Web, Custom(validate_structured_clone_options)],
    ),
    // Compression streams
    constructor("CompressionStream", Undefined, &[Enum("CompressionFormat")]),
    constructor(
        "DecompressionStream",
        Undefined,
        &[Enum("CompressionFormat")],
    ),
    // The chunks written to the compression streams, as their transform algorithms receive them.
    method(
        "CompressionStream",
        "",
        Missing::Web,
        &[Custom(validate_chunk)],
    ),
    method(
        "DecompressionStream",
        "",
        Missing::Web,
        &[Custom(validate_chunk)],
    ),
    // Streams
    constructor(
        "ReadableStream",
        Missing::Web,
        &[Custom(validate_source), Object("strategy")],
    ),
    method(
        "ReadableStream",
        "getReader",
        Missing::Web,
        &[Custom(validate_reader_options)],
    ),
    method(
        "ReadableStream",
        "pipeTo",
        Undefined,
        &[
            Type("transform.writable", "an instance of WritableStream"),
            Object("options"),
        ],
    ),
    method(
        "ReadableStream",
        "pipeThrough",
        Undefined,
        &[Custom(validate_transform), Object("options")],
    ),
    constructor(
        "ReadableStreamDefaultReader",
        Undefined,
        &[Type("stream", "an instance of ReadableStream")],
    ),
    constructor(
        "ReadableStreamBYOBReader",
        Undefined,
        &[Type("stream", "an instance of ReadableStream")],
    ),
    constructor(
        "WritableStream",
        Missing::Web,
        &[Object("sink"), Object("strategy")],
    ),
    constructor(
        "WritableStreamDefaultWriter",
        Undefined,
        &[Type("stream", "an instance of WritableStream")],
    ),
    constructor(
        "TransformStream",
        Missing::Web,
        &[
            Object("transformer"),
            Object("writableStrategy"),
            Object("readableStrategy"),
        ],
    ),
    constructor(
        "ByteLengthQueuingStrategy",
        Undefined,
        &[Custom(validate_strategy_init)],
    ),
    this(
        "ByteLengthQueuingStrategy",
        "size",
        Getter,
        This::PrivateMember("byteSizeFunction"),
    ),
    constructor(
        "CountQueuingStrategy",
        Undefined,
        &[Custom(validate_strategy_init)],
    ),
    this(
        "CountQueuingStrategy",
        "size",
        Getter,
        This::PrivateMember("countSizeFunction"),
    ),
    // undici's classes
    constructor(
        "Headers",
        Missing::Web,
        &[Plain(
            "Headers constructor: Argument 1 could not be converted to one of: \
             sequence<sequence<ByteString>>, record<ByteString, ByteString>.",
        )],
    ),
    method("Headers", "append", Undici(2), &[]),
    method("Headers", "set", Undici(2), &[]),
    method("Headers", "delete", Undici(1), &[]),
    method("Headers", "get", Undici(1), &[]),
    method("Headers", "has", Undici(1), &[]),
    method("Headers", "forEach", Undici(1), &[]),
    constructor("Request", Undici(1), &[]),
    member("Response", "json", StaticMethod, Undici(1), &[]),
    member("Response", "redirect", StaticMethod, Undici(1), &[]),
    constructor("MessageEvent", Undici(1), &[]),
    constructor("WebSocket", Undici(1), &[]),
    method("WebSocket", "send", Undici(1), &[]),
    // `crypto`
    method("Crypto", "getRandomValues", WebIdl(1), &[]),
    // Global functions
    member("", "atob", MemberKind::Function, Args(&["input"]), &[]),
    member("", "btoa", MemberKind::Function, Args(&["input"]), &[]),
    member(
        "",
        "queueMicrotask",
        MemberKind::Function,
        Undefined,
        &[Type("callback", FUNCTION)],
    ),
];

fn find_interface(name: &str) -> Option<&'static Interface> {
    INTERFACES.iter().find(|interface| interface.name == name)
}

fn find_member(site: &BindingSite) -> Option<&'static Member> {
    MEMBERS.iter().find(|member| {
        member.interface == site.interface
            && member.member == site.member
            && member.kind == site.kind
    })
}

/// The [`js::error::BindingErrorHook`] of `--node-compat` mode: Node's error for `error` at
/// `site`, or `None` to keep the web error.
pub(crate) fn binding_error<'s>(
    scope: &'s Scope<'_>,
    site: &BindingSite,
    error: &BindingError<'_>,
) -> Option<Result<HandleValue<'s>, ExnThrown>> {
    let created = match *error {
        BindingError::InvalidThis { .. } => this_error(site)?,
        BindingError::IllegalConstructor => match find_interface(site.interface)?.constructor {
            Constructor::Illegal => Created::Node(NodeError::illegal_constructor()),
            Constructor::Coded(code, message) => {
                Created::Node(NodeError::new(ErrorKind::TypeError, code, message))
            }
        },
        BindingError::CallWithoutNew { .. } => {
            let interface = find_interface(site.interface)?;
            match interface.name {
                "MessageChannel" | "URLPattern" => Created::Node(NodeError::new(
                    ErrorKind::TypeError,
                    "ERR_CONSTRUCT_CALL_REQUIRED",
                    "Cannot call constructor without `new`",
                )),
                name => {
                    let prefix = if interface.undici { "_" } else { "" };
                    Created::Plain(format!(
                        "Class constructor {prefix}{name} cannot be invoked without 'new'"
                    ))
                }
            }
        }
        BindingError::MissingArgument { index, given } => {
            missing_argument(scope, site, index, given)?
        }
        BindingError::InvalidArgument { index, value, .. } => {
            invalid_argument(scope, site, index, value)?
        }
        BindingError::Operation { .. } => return operation_error(scope, site, error),
    };
    Some(match created {
        Created::Node(error) => error.to_value(scope),
        Created::Plain(message) => js::error::new_error(scope, JSExnType::JSEXN_TYPEERR, &message),
        Created::Failed(thrown) => Err(thrown),
    })
}

/// The error [`binding_error`] creates.
enum Created {
    Node(NodeError),
    /// A `TypeError` without a code.
    Plain(String),
    /// Creating the error threw.
    Failed(ExnThrown),
}

impl From<Result<NodeError, ExnThrown>> for Created {
    fn from(result: Result<NodeError, ExnThrown>) -> Self {
        match result {
            Ok(error) => Created::Node(error),
            Err(thrown) => Created::Failed(thrown),
        }
    }
}

fn this_error(site: &BindingSite) -> Option<Created> {
    let this = match find_member(site).and_then(|member| member.this) {
        Some(this) => this,
        // Node.js defines the event handler attributes on `EventTarget`, except for undici's
        // classes.
        None if site.member.starts_with("on")
            && matches!(site.kind, MemberKind::Getter | MemberKind::Setter)
            && !find_interface(site.interface)?.undici =>
        {
            This::Invalid("EventTarget")
        }
        None => find_interface(site.interface)?.this,
    };
    match this {
        This::Invalid(name) => Some(Created::Node(NodeError::invalid_this(name))),
        This::InvalidMessage(message) => Some(Created::Node(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_THIS",
            message,
        ))),
        This::Plain(message) => Some(Created::Plain(message.into())),
        This::PrivateMember(field) => Some(Created::Plain(format!(
            "Cannot read private member #{field} from an object whose class did not declare it"
        ))),
        This::Web => None,
    }
}

fn missing_argument(
    scope: &Scope<'_>,
    site: &BindingSite,
    index: u32,
    given: u32,
) -> Option<Created> {
    let member = find_member(site)?;
    let plural = |count: u32| if count == 1 { "" } else { "s" };
    match member.missing {
        Missing::Args(names) => Some(Created::Node(NodeError::missing_args(names))),
        Missing::Undefined => invalid_argument(scope, site, index, HandleValue::undefined()),
        Missing::Undici(required) => {
            let header = if site.kind == MemberKind::Constructor {
                format!("{} constructor", site.interface)
            } else {
                format!("{}.{}", site.interface, site.member)
            };
            let only = if given > 0 { " only" } else { "" };
            Some(Created::Plain(format!(
                "{header}: {required} argument{} required, but{only} {given} found.",
                plural(required)
            )))
        }
        Missing::WebIdl(required) => Some(Created::Node(NodeError::new(
            ErrorKind::TypeError,
            "ERR_MISSING_ARGS",
            format!(
                "Failed to execute '{}' on '{}': {required} argument{} required, but only \
                 {given} present.",
                site.member,
                site.interface,
                plural(required)
            ),
        ))),
        Missing::Message(message) => Some(Created::Node(NodeError::new(
            ErrorKind::TypeError,
            "ERR_MISSING_ARGS",
            message,
        ))),
        Missing::Web => None,
    }
}

fn invalid_argument(
    scope: &Scope<'_>,
    site: &BindingSite,
    index: u32,
    value: HandleValue<'_>,
) -> Option<Created> {
    let member = find_member(site)?;
    match *member.args.get(index as usize)? {
        Type(name, expected) => Some(Created::Node(NodeError::invalid_arg_type(
            scope, name, expected, value,
        ))),
        Object(name) if !value.is_object() => Some(Created::Node(NodeError::invalid_arg_type(
            scope, name, OBJECT, value,
        ))),
        Coded(code, message) => Some(Created::Node(NodeError::new(
            ErrorKind::TypeError,
            code,
            message,
        ))),
        Object(_) => None,
        Plain(message) => Some(Created::Plain(message.into())),
        Enum(name) => Some(enum_error(scope, site, index, value, name).into()),
        Custom(validate) => match validate(scope, value) {
            Ok(error) => error.map(Created::Node),
            Err(thrown) => Some(Created::Failed(thrown)),
        },
        Web => None,
    }
}

/// The `ERR_INVALID_ARG_VALUE` of Node's WebIDL layer for argument `index`, `value`, which isn't
/// a value of the enumeration `name`.
fn enum_error(
    scope: &Scope<'_>,
    site: &BindingSite,
    index: u32,
    value: HandleValue<'_>,
    name: &str,
) -> Result<NodeError, ExnThrown> {
    let operation = if site.kind == MemberKind::Constructor {
        format!("construct '{}'", site.interface)
    } else {
        format!("execute '{}' on '{}'", site.member, site.interface)
    };
    let text = super::js_string(scope, value)?;
    Ok(NodeError::new(
        ErrorKind::TypeError,
        "ERR_INVALID_ARG_VALUE",
        format!(
            "Failed to {operation}: {} argument '{text}' is not a valid enum value of type {name}.",
            ordinal(index + 1)
        ),
    ))
}

/// `number` as an English ordinal, such as "1st" or "12th".
fn ordinal(number: u32) -> String {
    let suffix = match (number % 10, number % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{number}{suffix}")
}

/// Node's error for the [`Operation`](BindingError::Operation) error `error` at `site`, or `None`
/// for a code without a message here.
///
/// `ERR_INVALID_STATE` errors have the type of `error`, and the detail [`INVALID_STATE`] gives for
/// its message. `ERR_INVALID_ARG_VALUE` errors name the argument or member its first argument
/// names and show its value. The errors that Node's C++ code throws lack the `toString` of the
/// other errors with an `ERR_` code: those of the `URL` constructor and of
/// `TextDecoder.prototype.decode`. `ERR_INVALID_URL` has the URL as its `input` property, and the
/// base URL, if given, as its `base` property.
fn operation_error<'s>(
    scope: &'s Scope<'_>,
    site: &BindingSite,
    error: &BindingError<'_>,
) -> Option<Result<HandleValue<'s>, ExnThrown>> {
    let BindingError::Operation {
        kind,
        message: web_message,
        code,
        args,
        value,
    } = *error
    else {
        return None;
    };
    let arg = |index: usize| args.get(index).copied().unwrap_or_default();
    let error_kind = if kind == JSExnType::JSEXN_RANGEERR {
        ErrorKind::RangeError
    } else {
        ErrorKind::TypeError
    };
    let (kind, message, native) = match code {
        "ERR_INVALID_STATE" => {
            let web_message = web_message.to_string_lossy();
            let detail = INVALID_STATE
                .iter()
                .find(|(interface, message, _)| {
                    (interface.is_empty() || *interface == site.interface)
                        && *message == web_message
                })
                .map_or(&*web_message, |(_, _, detail)| detail);
            (error_kind, format!("Invalid state: {detail}"), false)
        }
        "ERR_INVALID_ARG_VALUE" => {
            let mut error = NodeError::invalid_arg_value(scope, arg(0), value?, "is invalid");
            error.kind = error_kind;
            return Some(error.to_value(scope));
        }
        "ERR_ENCODING_NOT_SUPPORTED" => (
            ErrorKind::RangeError,
            format!("The \"{}\" encoding is not supported", arg(0)),
            false,
        ),
        "ERR_ENCODING_INVALID_ENCODED_DATA" => (
            ErrorKind::TypeError,
            format!("The encoded data was not valid for encoding {}", arg(0)),
            true,
        ),
        "ERR_INVALID_TUPLE" => (
            ErrorKind::TypeError,
            format!("{} must be an iterable {} tuple", arg(0), arg(1)),
            false,
        ),
        "ERR_INVALID_URL" => (
            ErrorKind::TypeError,
            "Invalid URL".to_string(),
            site.kind == MemberKind::Constructor,
        ),
        "ERR_PERFORMANCE_INVALID_TIMESTAMP" => (
            ErrorKind::TypeError,
            format!("{} is not a valid timestamp", arg(0)),
            false,
        ),
        "ERR_MISSING_ARGS" => {
            return Some(NodeError::missing_args(args).to_value(scope));
        }
        _ => return None,
    };
    let error = NodeError::new(kind, code, message);
    let create = || {
        let value = if native {
            error.to_native_value(scope)?
        } else {
            error.to_value(scope)?
        };
        if code == "ERR_INVALID_URL" {
            let object = Object::from_value(scope, value).map_err(|e| e.throw(scope))?;
            object.set_property(scope, c"input", arg(0))?;
            if let Some(base) = args.get(1) {
                object.set_property(scope, c"base", *base)?;
            }
        }
        Ok(value)
    };
    Some(create())
}

/// Node's `validateUint32(delay, 'delay')` for `AbortSignal.timeout`.
fn validate_delay(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    match super::validate_integer(scope, value, "delay", 0.0, 4294967295.0) {
        Ok(_) => Ok(None),
        Err(super::Thrown::Node(error)) => Ok(Some(error)),
        Err(super::Thrown::Pending) => Err(ExnThrown),
    }
}

/// The details of Node's `ERR_INVALID_STATE` messages, "Invalid state: {detail}", by the
/// interface that throws the error, or `""` for any, and the web error's message. A web message
/// without an entry is the detail.
static INVALID_STATE: &[(&str, &str, &str)] = &[
    (
        "",
        "This stream has already been locked for exclusive reading by another reader",
        "ReadableStream is locked",
    ),
    (
        "",
        "Cannot cancel a stream that already has a reader",
        "ReadableStream is locked",
    ),
    (
        "",
        "cannot pipe from a locked ReadableStream",
        "The ReadableStream is locked",
    ),
    (
        "",
        "cannot pipe to a locked WritableStream",
        "The WritableStream is locked",
    ),
    (
        "",
        "Cannot read from a reader that does not have an owner stream",
        "The reader is not attached to a stream",
    ),
    (
        "",
        "Cannot cancel a reader that does not have an owner stream",
        "The reader is not attached to a stream",
    ),
    (
        "",
        "Cannot read from a reader that is not attached to a stream",
        "The reader is not attached to a stream",
    ),
    (
        "",
        "Cannot cancel a reader that has no stream",
        "The reader is not attached to a stream",
    ),
    (
        "",
        "Reader was released and can no longer be used to monitor the stream's closedness",
        "Reader released",
    ),
    ("", "Reader was released", "Reader released"),
    (
        "",
        "read() view has a byte length of 0",
        "View or Viewed ArrayBuffer is zero-length or detached",
    ),
    (
        "",
        "read() view's buffer has a byte length of 0",
        "View or Viewed ArrayBuffer is zero-length or detached",
    ),
    (
        "",
        "read() view's buffer is detached",
        "View or Viewed ArrayBuffer is zero-length or detached",
    ),
    (
        "",
        "respond() called on an invalidated BYOB request",
        "This BYOB request has been invalidated",
    ),
    (
        "",
        "respondWithNewView() called on an invalidated BYOB request",
        "This BYOB request has been invalidated",
    ),
    (
        "",
        "respond() called with a detached view buffer",
        "Viewed ArrayBuffer is detached",
    ),
    (
        "",
        "respondWithNewView() called with a detached view buffer",
        "Viewed ArrayBuffer is detached",
    ),
    (
        "",
        "view byte length must be 0 for a closed stream",
        "View is not zero-length",
    ),
    (
        "",
        "view byte length must not be 0 for a readable stream",
        "View is zero-length",
    ),
    (
        "",
        "close() called on a controller that already requested close",
        "Controller is already closed",
    ),
    (
        "",
        "close() called on a controller whose stream is not readable",
        "ReadableStream is already closed",
    ),
    (
        "",
        "enqueue() called on a controller that requested close",
        "Controller is already closed",
    ),
    (
        "",
        "enqueue() called on a controller whose stream is not readable",
        "ReadableStream is already closed",
    ),
    (
        "",
        "enqueue() view has a byte length of 0",
        "chunk ArrayBuffer is zero-length or detached",
    ),
    (
        "",
        "enqueue() view's buffer has a byte length of 0",
        "chunk ArrayBuffer is zero-length or detached",
    ),
    (
        "",
        "enqueue() view's buffer is detached",
        "chunk ArrayBuffer is zero-length or detached",
    ),
    (
        "",
        "The stream is not in a state that permits close",
        "Controller is already closed",
    ),
    (
        "",
        "The stream is not in a state that permits enqueue",
        "Controller is already closed",
    ),
    (
        "",
        "the destination writable stream closed before all data could be piped to it",
        "Destination WritableStream is closed",
    ),
    (
        "",
        "Insufficient bytes to fill the pending pull-into",
        "Partial read",
    ),
    (
        "",
        "This stream has already been locked for exclusive writing by another writer",
        "WritableStream is locked",
    ),
    (
        "",
        "Cannot abort a stream that already has a writer",
        "WritableStream is locked",
    ),
    (
        "",
        "Cannot close a stream that already has a writer",
        "WritableStream is locked",
    ),
    (
        "WritableStream",
        "Cannot close an already-closing stream",
        "Failure closing WritableStream",
    ),
    (
        "WritableStreamDefaultWriter",
        "Cannot close an already-closing stream",
        "Failure to close WritableStream",
    ),
    (
        "",
        "The stream (in closed or errored state) is not in the writable state and cannot be closed",
        "WritableStream is closed",
    ),
    (
        "",
        "The stream is closing or closed and cannot be written to",
        "WritableStream is closed",
    ),
    (
        "",
        "Cannot read desiredSize of a writer that does not have an owner stream",
        "Writer is not bound to a WritableStream",
    ),
    (
        "",
        "Cannot abort a stream using a released writer",
        "Writer is not bound to a WritableStream",
    ),
    (
        "",
        "Cannot close a stream using a released writer",
        "Writer is not bound to a WritableStream",
    ),
    (
        "",
        "Cannot write to a stream using a released writer",
        "Writer is not bound to a WritableStream",
    ),
    (
        "",
        "Writer was released and can no longer be used to monitor the stream's closedness",
        "Writer has been released",
    ),
    (
        "",
        "Readable side is not in a state that permits enqueue",
        "Unable to enqueue",
    ),
    (
        "",
        "The stream has been terminated",
        "TransformStream has been terminated",
    ),
];

/// The error of Node's `ReadableStream` constructor for its `source` argument: for a value that
/// isn't an object, or for a `type` member other than `"bytes"`.
fn validate_source(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let Ok(source) = Object::from_value(scope, *value) else {
        return Ok(Some(NodeError::invalid_arg_type(
            scope, "source", OBJECT, value,
        )));
    };
    let source_type = source.get_property(scope, c"type")?;
    if source_type.is_undefined()
        || source_type.is_string() && super::js_string(scope, source_type)? == "bytes"
    {
        return Ok(None);
    }
    Ok(Some(NodeError::invalid_arg_value(
        scope,
        "source.type",
        source_type,
        "is invalid",
    )))
}

/// The error of Node's `AbortSignal.any` for its `signals` argument: for a value that isn't
/// iterable, or for an iterable with an element that isn't an `AbortSignal`.
fn validate_signals(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let invalid = |message: String| {
        Ok(Some(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_ARG_TYPE",
            message,
        )))
    };
    if !is_iterable(scope, value)? {
        return invalid("signals cannot be converted to sequence.".into());
    }
    let elements = Vec::<HandleValue<'_>>::from_jsval(scope, scope.root_value(value.get()), ())
        .map_err(|error| error.throw(scope))?;
    for (index, element) in elements.into_iter().enumerate() {
        let is_signal = Object::from_value(scope, *element)
            .is_ok_and(|element| element.cast::<web_globals::signals::AbortSignal>().is_ok());
        if !is_signal {
            return invalid(format!("signals[{index}] is not of type AbortSignal."));
        }
    }
    Ok(None)
}

/// The error of Node's `PerformanceObserver.prototype.observe` for its `options` argument: for a
/// value that isn't an object, or for an `entryTypes` member that isn't an array.
fn validate_observer_options(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let Ok(options) = Object::from_value(scope, *value) else {
        return Ok(Some(NodeError::invalid_arg_type(
            scope, "options", OBJECT, value,
        )));
    };
    let entry_types = options.get_property(scope, c"entryTypes")?;
    if !entry_types.is_undefined() && !js::Array::is_array_value(scope, entry_types)? {
        return Ok(Some(NodeError::invalid_arg_type(
            scope,
            "options.entryTypes",
            "string[]",
            entry_types,
        )));
    }
    Ok(None)
}

/// The error of Node's `structuredClone` for its `options` argument: for a value that isn't an
/// object, a `transfer` member that isn't iterable, or an array `transfer` member with an element
/// that isn't an object.
fn validate_structured_clone_options(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let invalid = |problem: &str| {
        Ok(Some(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_ARG_TYPE",
            format!("Failed to execute 'structuredClone': {problem}"),
        )))
    };
    let Ok(options) = Object::from_value(scope, *value) else {
        return invalid("Options cannot be converted to a dictionary");
    };
    let transfer = options.get_property(scope, c"transfer")?;
    if transfer.is_undefined() {
        return Ok(None);
    }
    if !is_iterable(scope, transfer)? {
        return invalid("transfer in Options cannot be converted to sequence.");
    }
    let transfer = Object::from_value(scope, *transfer).map(|object| object.cast::<js::Array>());
    let Ok(Ok(transfer)) = transfer else {
        return Ok(None);
    };
    for (index, element) in transfer.to_vec(scope)?.into_iter().enumerate() {
        if !element.is_object() {
            return invalid(&format!("transfer in Options[{index}] is not an object."));
        }
    }
    Ok(None)
}

/// The error of Node's compression streams for a chunk that isn't a `Buffer`, `TypedArray` or
/// `DataView`: `ERR_STREAM_NULL_VALUES` for `null`, and `ERR_INVALID_ARG_TYPE` otherwise.
fn validate_chunk(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    if value.is_null() {
        return Ok(Some(NodeError::new(
            ErrorKind::TypeError,
            "ERR_STREAM_NULL_VALUES",
            "May not write null values to stream",
        )));
    }
    Ok(Some(NodeError::invalid_arg_type(
        scope,
        "chunk",
        "of type string or an instance of Buffer, TypedArray, or DataView",
        value,
    )))
}

/// The error of Node's `MessagePort.prototype.postMessage` for its `transferList` argument: for a
/// value that isn't an object, or for an object that isn't iterable and whose `transfer` member
/// isn't iterable.
fn validate_transfer_list(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let invalid = |name: &str| {
        Ok(Some(NodeError::new(
            ErrorKind::TypeError,
            "ERR_INVALID_ARG_TYPE",
            format!("Optional {name} argument must be an iterable"),
        )))
    };
    let Ok(options) = Object::from_value(scope, *value) else {
        return invalid("transferList");
    };
    if is_iterable(scope, value)? {
        return Ok(None);
    }
    let transfer = options.get_property(scope, c"transfer")?;
    if !transfer.is_undefined() && !is_iterable(scope, transfer)? {
        return invalid("options.transfer");
    }
    Ok(None)
}

/// Whether `value` is an object with a `Symbol.iterator` method.
fn is_iterable(scope: &Scope<'_>, value: HandleValue<'_>) -> Result<bool, ExnThrown> {
    js::conversion::is_iterable_value(scope, value).map_err(|error| error.throw(scope))
}

/// The error of Node's `Blob` and `File` constructors for their `options` argument: an
/// `ERR_INVALID_ARG_VALUE` for its `endings`, or an `ERR_INVALID_ARG_TYPE` if it isn't an object.
fn validate_blob_options(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let Ok(options) = Object::from_value(scope, *value) else {
        return Ok(Some(NodeError::invalid_arg_type(
            scope,
            "options",
            "a dictionary",
            value,
        )));
    };
    let endings = options.get_property(scope, c"endings")?;
    Ok(Some(NodeError::invalid_arg_value(
        scope,
        "options.endings",
        endings,
        "is invalid",
    )))
}

/// The error of Node's `ReadableStream.prototype.getReader` for its `options` argument.
fn validate_reader_options(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let Ok(options) = Object::from_value(scope, *value) else {
        return Ok(Some(NodeError::invalid_arg_type(
            scope, "options", OBJECT, value,
        )));
    };
    let mode = options.get_property(scope, c"mode")?;
    Ok(Some(NodeError::invalid_arg_value(
        scope,
        "options.mode",
        mode,
        "is invalid",
    )))
}

/// The error of Node's `ReadableStream.prototype.pipeThrough` for its `transform` argument, which
/// names the `readable` or `writable` member that isn't a stream.
fn validate_transform(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    let Ok(transform) = Object::from_value(scope, *value) else {
        return Ok(Some(NodeError::invalid_arg_type(
            scope,
            "transform.readable",
            "an instance of ReadableStream",
            HandleValue::undefined(),
        )));
    };
    let readable = transform.get_property(scope, c"readable")?;
    if !is_readable_stream(scope, readable) {
        return Ok(Some(NodeError::invalid_arg_type(
            scope,
            "transform.readable",
            "an instance of ReadableStream",
            readable,
        )));
    }
    let writable = transform.get_property(scope, c"writable")?;
    Ok(Some(NodeError::invalid_arg_type(
        scope,
        "transform.writable",
        "an instance of WritableStream",
        writable,
    )))
}

/// The error of Node's queuing strategy constructors for their `init` argument: an
/// `ERR_INVALID_ARG_TYPE` for a value that isn't an object or is an array or a function, and an
/// `ERR_MISSING_OPTION` for an object without `highWaterMark`.
fn validate_strategy_init(
    scope: &Scope<'_>,
    value: HandleValue<'_>,
) -> Result<Option<NodeError>, ExnThrown> {
    match super::validate_plain_object(scope, value, "init", false) {
        Ok(_) => Ok(Some(NodeError::new(
            ErrorKind::TypeError,
            "ERR_MISSING_OPTION",
            "init.highWaterMark is required",
        ))),
        Err(super::Thrown::Node(error)) => Ok(Some(error)),
        Err(super::Thrown::Pending) => Err(ExnThrown),
    }
}
