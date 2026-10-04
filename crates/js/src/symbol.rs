// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Symbol creation and well-known symbol access.
//!
//! ES2015 Symbols are unique, immutable property keys. This module provides
//! access to creating new symbols, looking up the global symbol registry, and
//! retrieving well-known symbols like `Symbol.iterator`.

use std::ffi::CStr;
use std::ptr::NonNull;

use crate::gc::scope::Scope;
use mozjs::gc::{Handle, HandleId, HandleString, HandleSymbol, HandleValue};
use mozjs::jsapi::{JSString, PropertyKey, Symbol, SymbolCode};
use mozjs::rust::wrappers2;

use super::error::ExnThrown;

/// Create a new unique symbol with the given description string.
pub fn new_symbol<'s>(
    scope: &'s Scope<'_>,
    description: HandleString,
) -> Result<Handle<'s, *mut Symbol>, ExnThrown> {
    let sym = unsafe { wrappers2::NewSymbol(scope.cx_mut(), description) };
    NonNull::new(sym)
        .map(|p| scope.root_symbol(p))
        .ok_or(ExnThrown)
}

/// Create a new private name symbol with the given description string, the kind of symbol the
/// engine creates for a `#name` in a class body. Reflection doesn't list the properties it keys,
/// `Object.assign`, spread and structured cloning don't copy them, and `Object.freeze` doesn't
/// change them. On a scripted `Proxy`, they are defined on the proxy itself without running a
/// trap.
///
/// The symbol must never be passed to JavaScript as a value.
pub fn new_private_name_symbol<'s>(
    scope: &'s Scope<'_>,
    description: HandleString,
) -> Result<Handle<'s, *mut Symbol>, ExnThrown> {
    let sym = unsafe { wrappers2::NewPrivateNameSymbol(scope.cx_mut(), description) };
    NonNull::new(sym)
        .map(|p| scope.root_symbol(p))
        .ok_or(ExnThrown)
}

/// Look up a symbol in the global symbol registry (`Symbol.for(key)`).
///
/// If a symbol with the given key already exists, it is returned; otherwise a
/// new one is created and registered.
pub fn get_symbol_for<'s>(
    scope: &'s Scope<'_>,
    key: HandleString,
) -> Result<Handle<'s, *mut Symbol>, ExnThrown> {
    let sym = unsafe { wrappers2::GetSymbolFor(scope.cx_mut(), key) };
    NonNull::new(sym)
        .map(|p| scope.root_symbol(p))
        .ok_or(ExnThrown)
}

/// Get the description string of a symbol, if it has one.
pub fn get_description(symbol: HandleSymbol) -> Option<NonNull<JSString>> {
    NonNull::new(unsafe { wrappers2::GetSymbolDescription(symbol) })
}

/// Get the `SymbolCode` of a symbol (identifies well-known symbols).
pub fn get_code(symbol: HandleSymbol) -> SymbolCode {
    unsafe { wrappers2::GetSymbolCode(symbol) }
}

/// Get a well-known symbol by its `SymbolCode`.
///
/// Well-known symbols include `Symbol.iterator`, `Symbol.toPrimitive`,
/// `Symbol.hasInstance`, etc.
///
/// Well-known symbols are always present in a valid runtime, so this
/// roots the result in the scope.
pub fn get_well_known<'s>(scope: &'s Scope<'_>, which: SymbolCode) -> Handle<'s, *mut Symbol> {
    let ptr = unsafe { wrappers2::GetWellKnownSymbol(scope.cx(), which) };
    // SAFETY: Well-known symbols are always present in a valid runtime.
    let nn = unsafe { NonNull::new_unchecked(ptr) };
    scope.root_symbol(nn)
}

/// Get the `PropertyKey` for a well-known symbol.
///
/// This is useful for property lookups using symbol keys.
pub fn get_well_known_key(scope: &Scope<'_>, which: SymbolCode) -> PropertyKey {
    unsafe { wrappers2::GetWellKnownSymbolKey(scope.cx(), which) }
}

/// A symbol that is created once per global, on first use. The address of the `static` that holds
/// a `CachedSymbol` identifies the symbol, so each unregistered symbol has exactly one `static`.
#[derive(Debug)]
pub struct CachedSymbol {
    description: &'static CStr,
    kind: CachedSymbolKind,
}

/// How a [`CachedSymbol`] is created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CachedSymbolKind {
    /// As `Symbol(description)` creates it.
    Unique,
    /// As `Symbol.for(description)` returns it.
    Registered,
    /// As [`new_private_name_symbol`] creates it.
    PrivateName,
}

impl CachedSymbol {
    /// A new symbol with `description`, as `Symbol(description)` creates.
    pub const fn new(description: &'static CStr) -> Self {
        CachedSymbol {
            description,
            kind: CachedSymbolKind::Unique,
        }
    }

    /// The symbol of the global symbol registry for `description`, as `Symbol.for(description)`
    /// returns.
    pub const fn registered(description: &'static CStr) -> Self {
        CachedSymbol {
            description,
            kind: CachedSymbolKind::Registered,
        }
    }

    /// A new private name symbol with `description`, as [`new_private_name_symbol`] creates. It is
    /// only a property key: [`value`](Self::value) panics for it.
    pub const fn private_name(description: &'static CStr) -> Self {
        CachedSymbol {
            description,
            kind: CachedSymbolKind::PrivateName,
        }
    }

    /// The symbol as a property key.
    pub fn id<'s>(&'static self, scope: &'s Scope<'_>) -> Result<HandleId<'s>, ExnThrown> {
        crate::class::get_or_init_symbol_id(scope, std::ptr::from_ref(self).addr(), |scope| {
            let description = self.description.to_str().expect("descriptions are UTF-8");
            let description = crate::string::Str::from_str(scope, description)?;
            let symbol = match self.kind {
                CachedSymbolKind::Unique => new_symbol(scope, description.handle())?,
                CachedSymbolKind::Registered => get_symbol_for(scope, description.handle())?,
                CachedSymbolKind::PrivateName => {
                    new_private_name_symbol(scope, description.handle())?
                }
            };
            Ok(mozjs::jsid::SymbolId(symbol.get()))
        })
    }

    /// The symbol as a value. Panics for a [private name](Self::private_name) symbol.
    pub fn value<'s>(&'static self, scope: &'s Scope<'_>) -> Result<HandleValue<'s>, ExnThrown> {
        assert!(
            self.kind != CachedSymbolKind::PrivateName,
            "private name symbols are never passed to JavaScript"
        );
        let id = self.id(scope)?;
        crate::id::id_to_value(scope, id.get())
    }
}
