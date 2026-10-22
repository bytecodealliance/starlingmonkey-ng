// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The [`webidl_enum!`](crate::webidl_enum) macro: a WebIDL enumeration's Rust
//! enum and its entire string table from one `Variant => "string"` list.

/// Define a WebIDL enumeration: the Rust enum plus `as_str`, `Display`,
/// `FromStr`, and the JS value conversions (`FromJSVal`/`ToJSVal`), all
/// generated from a single `Variant => "string"` list.
///
/// ```ignore
/// js::webidl_enum! {
///     /// WebIDL enum `RequestRedirect`
///     pub enum RequestRedirect {
///         Follow => "follow",
///         Error => "error",
///         Manual => "manual",
///     }
/// }
/// ```
///
/// `FromJSVal` converts the value to a string and rejects anything outside the
/// table with a `TypeError`-producing conversion failure, per WebIDL's
/// es-to-enumeration steps.
#[macro_export]
macro_rules! webidl_enum {
    (
        $(#[$meta:meta])*
        $vis:vis enum $Name:ident {
            $(
                $(#[$vmeta:meta])*
                $Variant:ident => $string:literal
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        $vis enum $Name {
            $(
                $(#[$vmeta])*
                #[doc = concat!("`\"", $string, "\"`")]
                $Variant,
            )+
        }

        impl $Name {
            /// The enumeration value's string form.
            $vis fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$Variant => $string,)+
                }
            }
        }

        impl ::std::fmt::Display for $Name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl ::std::str::FromStr for $Name {
            type Err = ();

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($string => Ok(Self::$Variant),)+
                    _ => Err(()),
                }
            }
        }

        impl<'s, 'v> $crate::conversion::FromJSVal<'s, 'v> for $Name {
            type Config = ();

            fn from_jsval(
                scope: &'s $crate::gc::scope::Scope<'s>,
                val: $crate::prelude::HandleValue<'v>,
                _: (),
            ) -> Result<Self, $crate::conversion::ConversionError> {
                let s =
                    <String as $crate::conversion::FromJSVal>::from_jsval(scope, val, ())?;
                s.parse().map_err(|()| {
                    const MSG: &::std::ffi::CStr = unsafe {
                        ::std::ffi::CStr::from_bytes_with_nul_unchecked(
                            concat!("invalid value for ", stringify!($Name), "\0").as_bytes(),
                        )
                    };
                    $crate::conversion::ConversionError::Failure(::std::borrow::Cow::Borrowed(
                        MSG,
                    ))
                })
            }
        }

        impl<'s> $crate::conversion::ToJSVal<'s> for $Name {
            fn to_jsval_raw(
                &self,
                scope: &'s $crate::gc::scope::Scope<'s>,
            ) -> Result<$crate::value::Value, $crate::conversion::ConversionError>
            {
                $crate::conversion::ToJSVal::to_jsval_raw(self.as_str(), scope)
            }
        }
    };
}
