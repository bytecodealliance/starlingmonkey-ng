// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `fs.Stats` and `fs.Dirent`.

use js::conversion::ToJSVal;
use js::error::{ExnThrown, ThrowException};
use js::function::CallbackArgs;
use js::gc::scope::Scope;
use js::native::Value;
use js::object::{PropertyInit, PropertyTemplate, TemplateKey};
use js::prelude::HandleValue;
use js::{Function, Object};
use platform::fs::{FileKind, Stat};

use super::{text_value, Entry};
use crate::buffer::Encoding;
use crate::errors::Thrown;

/// The numeric fields of `Stats`, in the order of the `Stats` constructor's parameters.
pub(crate) static STATS_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"dev"),
    TemplateKey::Name(c"mode"),
    TemplateKey::Name(c"nlink"),
    TemplateKey::Name(c"uid"),
    TemplateKey::Name(c"gid"),
    TemplateKey::Name(c"rdev"),
    TemplateKey::Name(c"blksize"),
    TemplateKey::Name(c"ino"),
    TemplateKey::Name(c"size"),
    TemplateKey::Name(c"blocks"),
    TemplateKey::Name(c"atimeMs"),
    TemplateKey::Name(c"mtimeMs"),
    TemplateKey::Name(c"ctimeMs"),
    TemplateKey::Name(c"birthtimeMs"),
]);

/// The date accessors of `Stats`, each with the field holding its milliseconds.
const DATES: [(&std::ffi::CStr, &std::ffi::CStr); 4] = [
    (c"atime", c"atimeMs"),
    (c"mtime", c"mtimeMs"),
    (c"ctime", c"ctimeMs"),
    (c"birthtime", c"birthtimeMs"),
];

/// Whether the `S_IFMT` bits of `this.mode` are `bits`.
fn has_type(scope: &Scope<'_>, this: Object<'_>, bits: u32) -> Result<bool, ExnThrown> {
    let mode = this.get_property(scope, c"mode")?;
    Ok(mode.is_number() && (mode.to_number() as u32) & 0o170000 == bits)
}

/// The getter of a `Stats` date accessor, whose payload is the accessor's index in [`DATES`]:
/// define the accessor's name on `this` as a data property holding a `Date` of its milliseconds
/// rounded as `Math.round` rounds, and return the `Date`.
fn date_getter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (name, field) = DATES[payload.to_number() as usize];
    let this =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    let millis = this.get_property(scope, field)?;
    let millis = if millis.is_number() {
        let millis = millis.to_number();
        let floor = millis.floor();
        scope.root_value(js::value::from_f64(if millis - floor >= 0.5 {
            floor + 1.0
        } else {
            floor
        }))
    } else {
        millis
    };
    let date_class = js::class::get_class_object(scope, js::class_spec::JSProtoKey::JSProto_Date)?;
    let date_class = Object::from_handle(date_class).ok_or(ExnThrown)?;
    let date = Function::construct(scope, date_class, &[millis])?;
    this.define_property(scope, name, date, js::class_spec::JSPROP_ENUMERATE as u32)?;
    date.to_jsval_raw_throwing(scope)
}

/// The setter of a `Stats` date accessor, whose payload is the accessor's index in [`DATES`]:
/// define the accessor's name on `this` as a data property holding the value.
fn date_setter(
    scope: &Scope<'_>,
    args: CallbackArgs<'_>,
    payload: HandleValue,
) -> Result<Value, ExnThrown> {
    let (name, _) = DATES[payload.to_number() as usize];
    let this =
        Object::from_value(scope, scope.root_value(args.this())).map_err(|e| e.throw(scope))?;
    this.define_property(
        scope,
        name,
        args.get(0),
        js::class_spec::JSPROP_ENUMERATE as u32,
    )?;
    Ok(js::value::undefined())
}

#[core_runtime::jsfunction_class(name = "Stats", templates = "STATS_PROPERTIES")]
pub mod stats_class {
    use super::*;

    /// <https://nodejs.org/api/fs.html#class-fsstats>: the fields, in [`STATS_PROPERTIES`] order.
    #[constructor(length = 14)]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        fields: RestArgs<HandleValue<'_>>,
    ) -> Result<(), ExnThrown> {
        // SAFETY: `Assign` has no requirements.
        let mut properties =
            unsafe { this.property_writer(scope, &STATS_PROPERTIES, PropertyInit::Assign) }?;
        for (key, value) in STATS_PROPERTIES.keys().iter().zip(fields.iter()) {
            properties.write(scope, *key, *value)?;
        }
        properties.finish(scope)
    }

    pub fn is_file(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        has_type(scope, this, 0o100000)
    }

    pub fn is_directory(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        has_type(scope, this, 0o040000)
    }

    pub fn is_symbolic_link(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        has_type(scope, this, 0o120000)
    }

    pub fn is_block_device(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        has_type(scope, this, 0o060000)
    }

    pub fn is_character_device(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        has_type(scope, this, 0o020000)
    }

    #[method(name = "isFIFO")]
    pub fn is_fifo(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        has_type(scope, this, 0o010000)
    }

    pub fn is_socket(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        has_type(scope, this, 0o140000)
    }

    /// Define the date accessors, which cache the `Date` as a data property on first use.
    #[setup]
    pub fn setup(scope: &Scope<'_>, _ctor: Object<'_>, proto: Object<'_>) -> Result<(), ExnThrown> {
        for (index, (name, _)) in DATES.iter().enumerate() {
            let getter = Function::new_callback(
                scope,
                name,
                0,
                date_getter,
                js::value::from_f64(index as f64),
            )?;
            let setter = Function::new_callback(
                scope,
                name,
                1,
                date_setter,
                js::value::from_f64(index as f64),
            )?;
            let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
            proto.define_accessor(scope, name, Some(*getter), Some(*setter), attrs)?;
        }
        Ok(())
    }
}

/// A new `Stats` object for `stat`.
pub(super) fn stats_value<'s>(
    scope: &'s Scope<'_>,
    stat: &Stat,
) -> Result<HandleValue<'s>, ExnThrown> {
    let stats = stats_class::new_instance(scope)?;
    // SAFETY: `stats` was just created, and no JavaScript runs before the writer finishes.
    let mut properties =
        unsafe { stats.property_writer(scope, &STATS_PROPERTIES, PropertyInit::Define) }?;
    let values = [
        stat.dev as f64,
        f64::from(stat.mode),
        stat.nlink as f64,
        f64::from(stat.uid),
        f64::from(stat.gid),
        stat.rdev as f64,
        stat.blksize as f64,
        stat.ino as f64,
        stat.size as f64,
        stat.blocks as f64,
        stat.atime_ms,
        stat.mtime_ms,
        stat.ctime_ms,
        stat.birthtime_ms,
    ];
    for (key, value) in STATS_PROPERTIES.keys().iter().zip(values) {
        properties.write(scope, *key, value)?;
    }
    properties.finish(scope)?;
    stats.to_jsval_throwing(scope)
}

/// Node's `Symbol('type')`, the key of the property of a `Dirent` that holds its type, a
/// `UV_DIRENT_*` constant.
static K_TYPE: js::symbol::CachedSymbol = js::symbol::CachedSymbol::new(c"type");

/// The own properties of a `Dirent`, in the order its constructor creates them.
pub(crate) static DIRENT_PROPERTIES: PropertyTemplate = PropertyTemplate::new(&[
    TemplateKey::Name(c"name"),
    TemplateKey::Name(c"parentPath"),
    TemplateKey::Symbol(&K_TYPE),
]);

/// Whether the type of the `Dirent` `this` is `kind`.
fn is_kind(scope: &Scope<'_>, this: Object<'_>, kind: FileKind) -> Result<bool, ExnThrown> {
    let value = this.get_property_by_id(scope, K_TYPE.id(scope)?)?;
    Ok(value.is_number() && value.to_number() == f64::from(dirent_type(kind)))
}

/// The `UV_DIRENT_*` constant for `kind`.
fn dirent_type(kind: FileKind) -> u8 {
    match kind {
        FileKind::Unknown => 0,
        FileKind::File => 1,
        FileKind::Directory => 2,
        FileKind::Symlink => 3,
        FileKind::Fifo => 4,
        FileKind::Socket => 5,
        FileKind::CharDevice => 6,
        FileKind::BlockDevice => 7,
    }
}

/// Make `this` a `Dirent` named `name` of the type `type` in the directory `path`, creating its
/// properties as `init` creates them.
///
/// # Safety
///
/// With [`PropertyInit::Define`], `this` is a `Dirent` this module just created, which nothing
/// else references.
unsafe fn init_dirent(
    scope: &Scope<'_>,
    this: Object<'_>,
    name: HandleValue<'_>,
    r#type: HandleValue<'_>,
    path: HandleValue<'_>,
    init: PropertyInit,
) -> Result<(), ExnThrown> {
    // SAFETY: the caller guarantees the requirements of `Define`, and no JavaScript runs before
    // the writer finishes.
    let mut properties = unsafe { this.property_writer(scope, &DIRENT_PROPERTIES, init) }?;
    properties.write(scope, c"name", name)?;
    properties.write(scope, c"parentPath", path)?;
    properties.write(scope, &K_TYPE, r#type)?;
    properties.finish(scope)
}

#[core_runtime::jsfunction_class(name = "Dirent", class_syntax, templates = "DIRENT_PROPERTIES")]
pub mod dirent_class {
    use super::*;

    /// <https://nodejs.org/api/fs.html#class-fsdirent>: an entry named `name` of the type `type`,
    /// a `UV_DIRENT_*` constant, in the directory `path`.
    #[constructor]
    pub fn init(
        scope: &Scope<'_>,
        this: Object<'_>,
        name: HandleValue<'_>,
        r#type: HandleValue<'_>,
        path: HandleValue<'_>,
    ) -> Result<(), ExnThrown> {
        // SAFETY: `Assign` has no requirements.
        unsafe { init_dirent(scope, this, name, r#type, path, PropertyInit::Assign) }
    }

    pub fn is_directory(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        is_kind(scope, this, FileKind::Directory)
    }

    pub fn is_file(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        is_kind(scope, this, FileKind::File)
    }

    pub fn is_block_device(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        is_kind(scope, this, FileKind::BlockDevice)
    }

    pub fn is_character_device(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        is_kind(scope, this, FileKind::CharDevice)
    }

    pub fn is_symbolic_link(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        is_kind(scope, this, FileKind::Symlink)
    }

    #[method(name = "isFIFO")]
    pub fn is_fifo(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        is_kind(scope, this, FileKind::Fifo)
    }

    pub fn is_socket(scope: &Scope<'_>, this: Object<'_>) -> Result<bool, ExnThrown> {
        is_kind(scope, this, FileKind::Socket)
    }
}

/// A new `Dirent` named `name` in the directory `parent`, of the type `kind`, a `UV_DIRENT_*`
/// constant. With `UV_DIRENT_UNKNOWN`, the type is that of the file `path`, as `lstat` reports it.
pub(crate) fn new_dirent<'s>(
    scope: &'s Scope<'_>,
    name: HandleValue<'s>,
    parent: HandleValue<'s>,
    kind: f64,
    path: &str,
) -> Result<HandleValue<'s>, Thrown> {
    let kind = if kind == f64::from(dirent_type(FileKind::Unknown)) {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|error| super::UvError::new(error, "lstat").path(path).throw(scope))?;
        f64::from(dirent_type(FileKind::of(metadata.file_type())))
    } else {
        kind
    };
    let dirent = dirent_class::new_instance(scope)?;
    let kind = scope.root_value(js::value::from_f64(kind));
    // SAFETY: `dirent` was just created, and nothing else references it.
    unsafe { init_dirent(scope, dirent, name, kind, parent, PropertyInit::Define) }?;
    Ok(dirent.to_jsval_throwing(scope)?)
}

/// A new `Dirent` for `entry`, with the name in `encoding`.
pub(super) fn dirent_value<'s>(
    scope: &'s Scope<'_>,
    entry: &Entry,
    encoding: Option<Encoding>,
) -> Result<HandleValue<'s>, ExnThrown> {
    let dirent = dirent_class::new_instance(scope)?;
    let name = text_value(scope, &entry.name, encoding)?;
    let kind = scope.root_value(js::value::from_f64(f64::from(dirent_type(entry.kind))));
    let parent = js::JSString::from_str(scope, &entry.parent)?.to_jsval_throwing(scope)?;
    // SAFETY: `dirent` was just created, and nothing else references it.
    unsafe { init_dirent(scope, dirent, name, kind, parent, PropertyInit::Define) }?;
    dirent.to_jsval_throwing(scope)
}
