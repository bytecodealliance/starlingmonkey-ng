// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A [`urlpattern::RegExp`] backed by SpiderMonkey's regular expression engine.
//!
//! The URL Pattern specification defines matching in terms of ECMAScript regular expressions, so
//! patterns are compiled to `RegExp` objects rather than to Rust `regex`es, which differ in syntax
//! and in the captures of non-participating groups.
//!
//! The `urlpattern` crate's trait methods take no JS scope, so [`with_scope`] makes the scope of
//! the running native call available to them for as long as the call lasts.

use std::cell::Cell;
use std::ptr::NonNull;

use js::conversion::{ConversionBehavior, FromJSVal};
use js::gc::handle::RootedHeap;
use js::gc::scope::Scope;
use js::regexp::{
    RegExpFlag_HasIndices, RegExpFlag_IgnoreCase, RegExpFlag_UnicodeSets, RegExpFlags,
};
use js::{Array, Object};
use urlpattern::parser::RegexSyntax;

js::instance_local! {
    /// The scope [`with_scope`] is running under, with its lifetime erased.
    ///
    /// Scoped to the runtime, as the rest of the engine's state is, and empty outside a
    /// `with_scope` call, so nothing of it is in a Wizer snapshot or crosses an export call.
    static CURRENT_SCOPE: Cell<Option<NonNull<()>>> = const { Cell::new(None) };
}

/// Run `f` with `scope` as the scope [`SpiderMonkeyRegExp`]'s methods use.
pub fn with_scope<R>(scope: &Scope<'_>, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<NonNull<()>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_SCOPE.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(CURRENT_SCOPE.with(|c| c.replace(Some(NonNull::from(scope).cast()))));
    f()
}

/// Run `f` with the scope [`with_scope`] installed, or return `None` if there is none.
fn using_scope<R>(f: impl FnOnce(&Scope<'_>) -> R) -> Option<R> {
    let ptr = CURRENT_SCOPE.with(Cell::get)?;
    // SAFETY: `with_scope` stores a pointer to a scope that outlives the call it wraps, and clears
    // it on the way out. Nothing keeps the reference beyond `f`.
    Some(f(unsafe { ptr.cast::<Scope<'_>>().as_ref() }))
}

/// A compiled ECMAScript `RegExp` object, kept alive by a rooted heap reference.
pub struct SpiderMonkeyRegExp {
    object: RootedHeap<js::regexp::RegExp>,
    source: String,
}

impl urlpattern::regexp::RegExp for SpiderMonkeyRegExp {
    fn syntax() -> RegexSyntax {
        RegexSyntax::EcmaScript
    }

    fn parse(pattern: &str, flags: &str, _force_eval: bool) -> Result<Self, ()> {
        let mut bits = RegExpFlag_HasIndices;
        for flag in flags.chars() {
            bits |= match flag {
                'i' => RegExpFlag_IgnoreCase,
                // The specification compiles component patterns with the `v` flag.
                'u' => RegExpFlag_UnicodeSets,
                _ => return Err(()),
            };
        }
        let flags = RegExpFlags { flags_: bits };
        let chars: Vec<u16> = pattern.encode_utf16().collect();
        using_scope(|scope| {
            let compiled = js::RegExp::from_utf16(scope, &chars, flags);
            match compiled {
                Ok(object) => Ok(SpiderMonkeyRegExp {
                    object: RootedHeap::new(object),
                    source: pattern.to_owned(),
                }),
                Err(_) => {
                    // An invalid pattern is reported to the caller as an error, not as a pending
                    // exception.
                    js::exception::clear(scope);
                    Err(())
                }
            }
        })
        .unwrap_or(Err(()))
    }

    fn matches<'a>(&self, text: &'a str) -> Option<Vec<Option<&'a str>>> {
        using_scope(|scope| self.exec(scope, text))?
    }

    fn pattern_string(&self) -> &str {
        &self.source
    }
}

impl SpiderMonkeyRegExp {
    fn exec<'a>(&self, scope: &Scope<'_>, text: &'a str) -> Option<Vec<Option<&'a str>>> {
        let result = self.exec_inner(scope, text);
        if result.is_err() {
            js::exception::clear(scope);
        }
        result.ok().flatten()
    }

    fn exec_inner<'a>(
        &self,
        scope: &Scope<'_>,
        text: &'a str,
    ) -> Result<Option<Vec<Option<&'a str>>>, js::error::ExnThrown> {
        let regexp = self.object.get(scope);
        let chars: Vec<u16> = text.encode_utf16().collect();
        let mut index = 0;
        // SAFETY: `chars` is a valid UTF-16 string that outlives the call.
        let value = unsafe { regexp.execute_no_statics(scope, &chars, &mut index, false)? };
        if !value.get().is_object() {
            return Ok(None);
        }
        let Ok(result) = Object::from_jsval(scope, value, ()) else {
            return Ok(None);
        };

        // The match's `indices` give each group's UTF-16 range, from which the captures are sliced
        // out of `text`.
        let indices = result.get_property(scope, c"indices")?;
        let Ok(indices) = Array::from_jsval(scope, indices, ()) else {
            return Ok(None);
        };
        let len = indices.length(scope)?;

        // UTF-16 offset -> UTF-8 offset, for the offsets that start or end a group.
        let to_byte_offset = |utf16: u32| -> Option<usize> {
            let mut units = 0u32;
            for (byte, c) in text.char_indices() {
                if units == utf16 {
                    return Some(byte);
                }
                units += c.len_utf16() as u32;
            }
            (units == utf16).then_some(text.len())
        };

        let mut captures = Vec::with_capacity(len.saturating_sub(1) as usize);
        for i in 1..len {
            let range = indices.get_element(scope, i)?;
            if range.get().is_undefined() {
                captures.push(None);
                continue;
            }
            let Ok(range) = Object::from_jsval(scope, range, ()) else {
                return Ok(None);
            };
            let start = range.get_element(scope, 0)?;
            let start = u32::from_jsval(scope, start, ConversionBehavior::Default);
            let end = range.get_element(scope, 1)?;
            let end = u32::from_jsval(scope, end, ConversionBehavior::Default);
            let (Ok(start), Ok(end)) = (start, end) else {
                return Ok(None);
            };
            let (Some(start), Some(end)) = (to_byte_offset(start), to_byte_offset(end)) else {
                return Ok(None);
            };
            captures.push(Some(&text[start..end]));
        }
        Ok(Some(captures))
    }
}
