// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Proc macros for defining JavaScript classes backed by Rust structs in StarlingMonkey.
//!
//! Provides `#[jsclass]` and `#[jsmethods]` attribute macros that
//! generate the boilerplate needed to expose Rust types as SpiderMonkey JS classes.

use heck::{ToLowerCamelCase, ToUpperCamelCase};
use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::parse::{Parse, ParseStream};
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, FnArg, Ident, ImplItem, ImplItemFn, ItemEnum,
    ItemImpl, ItemStruct, LitInt, LitStr, Pat, ReturnType, Token, Type, Visibility,
};

// ============================================================================
// Attribute option parsing (shared across all macros)
// ============================================================================

/// Parsed key-value options from attribute arguments.
/// Used by `#[jsclass]`, `#[jsmethods]`, `#[jsmodule]`, and `#[method]`.
#[derive(Default)]
struct AttrOpts {
    /// Optional name override for the JS class or method. By default, a
    /// camel-case version of the Rust struct or method name is used.
    name: Option<String>,
    /// Optional length property for the class. Used by `#[method(length = N)]`,
    /// `#[static_method(length = N)]` and `#[constructor(length = N)]` to set the
    /// `length` property on generated methods and constructors.
    length: Option<usize>,
    /// Optional parent class for inheritance. `#[jsclass(extends = Parent)]`
    /// or `#[webidl_interface(extends = Parent)]` generates a JS class that
    /// inherits from `Parent` (which must also be defined with `#[jsclass]`
    /// or `#[webidl_interface]`), and requires a `parent: ParentImpl` field
    /// holding the parent's data. `#[webidl_dictionary(extends = Parent)]`
    /// requires a `parent: Parent`.
    extends: Option<Ident>,
    /// Inherit the prototype from a built-in JS class by `JSProtoKey`.
    ///
    /// `#[jsclass(js_proto = "Error")]` uses `Error.prototype` as the
    /// class prototype's `__proto__`. Mutually exclusive with `extends`.
    js_proto: Option<String>,
    /// Define `Symbol.toStringTag` on the prototype.
    ///
    /// `#[jsclass(to_string_tag = "DOMException")]` sets the well-known
    /// `@@toStringTag` property to the given string value (non-writable,
    /// non-enumerable, configurable — per WebIDL §3.7.6).
    to_string_tag: Option<String>,
    /// Bare flag: `#[webidl_interface(hidden)]` marks an interface
    /// that's not installed as a property on the global object. It remains
    /// reachable through the `constructor` property on instances' prototype,
    /// so a hidden interface that declares a `#[constructor]` is still
    /// constructible that way; omit the `#[constructor]` to prevent it.
    hidden: bool,
    /// Bare flag: `#[getter(unforgeable)]` marks a `[LegacyUnforgeable]`
    /// accessor — installed per-instance as an own property, not on the prototype.
    unforgeable: bool,
    /// `#[jsmodule(default = "Name")]` makes the module's named export `Name` its `default`
    /// export too, with every named export set as a property on it.
    default: Option<String>,
    /// Bare flag: `#[jsmodule(default_namespace)]` gives the module a `default` export holding
    /// every named export as a property.
    default_namespace: bool,
    /// `#[jsmodule(default_namespace, self_export = "name")]`, or with `default = "..."`, adds a
    /// named export `name` holding the `default` object, which is also a property of it.
    self_export: Option<String>,
    /// `#[jsmodule(default = "...", mirror = "a, b")]` adds named exports `a` and `b`, initialized
    /// from the `default` export's properties of the same names.
    mirror: Option<String>,
    /// Bare flag: `#[jsmodule(node_args)]` and `#[jsnamespace(node_args)]` make every argument of
    /// the functions optional, as Node.js functions treat a missing argument as `undefined`.
    /// Without it, a missing required argument throws a `TypeError`, as for WebIDL operations.
    node_args: bool,
    /// `#[jsclass(proto_parent = "path::to::fn_class")]` chains the class's prototype and
    /// constructor to those of a `#[jsfunction_class]`, whose module path it names.
    proto_parent: Option<syn::Path>,
    /// `#[jsclass(invalid_this = "path::to::fn")]` makes a method or accessor called on a `this`
    /// that isn't an instance throw by calling `fn(scope, class_name) -> ExnThrown`.
    invalid_this: Option<syn::Path>,
    /// `#[jsclass(templates = "A, B")]` makes `ClassDef::property_count` the number of keys of
    /// the `PropertyTemplate`s `A` and `B`.
    templates: Vec<syn::Path>,
}

/// The comma-separated paths of `PropertyTemplate`s in a `templates = "..."` option.
fn parse_template_paths(list: &LitStr) -> syn::Result<Vec<syn::Path>> {
    list.parse_with(syn::punctuated::Punctuated::<syn::Path, Token![,]>::parse_terminated)
        .map(|paths| paths.into_iter().collect())
}

/// The total number of keys of the `PropertyTemplate`s `templates`.
fn template_property_count(templates: &[syn::Path]) -> proc_macro2::TokenStream {
    quote! { 0usize #(+ #templates.len())* }
}

impl Parse for AttrOpts {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut opts = Self::default();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            // Bare flags take no `= value`.
            if key == "hidden" {
                opts.hidden = true;
                if !input.is_empty() {
                    let _: Token![,] = input.parse()?;
                }
                continue;
            }
            if key == "unforgeable" {
                opts.unforgeable = true;
                if !input.is_empty() {
                    let _: Token![,] = input.parse()?;
                }
                continue;
            }
            if key == "default_namespace" {
                opts.default_namespace = true;
                if !input.is_empty() {
                    let _: Token![,] = input.parse()?;
                }
                continue;
            }
            if key == "node_args" {
                opts.node_args = true;
                if !input.is_empty() {
                    let _: Token![,] = input.parse()?;
                }
                continue;
            }
            if input.parse::<Token![=]>().is_err() {
                return Err(syn::Error::new(key.span(), "unknown option"));
            }

            match key.to_string().as_str() {
                "name" => opts.name = Some(input.parse::<LitStr>()?.value()),
                "length" => opts.length = Some(input.parse::<LitInt>()?.base10_parse()?),
                "extends" => opts.extends = Some(input.parse()?),
                "js_proto" => opts.js_proto = Some(input.parse::<LitStr>()?.value()),
                "to_string_tag" => opts.to_string_tag = Some(input.parse::<LitStr>()?.value()),
                "default" => opts.default = Some(input.parse::<LitStr>()?.value()),
                "self_export" => opts.self_export = Some(input.parse::<LitStr>()?.value()),
                "mirror" => opts.mirror = Some(input.parse::<LitStr>()?.value()),
                "proto_parent" => opts.proto_parent = Some(input.parse::<LitStr>()?.parse()?),
                "invalid_this" => opts.invalid_this = Some(input.parse::<LitStr>()?.parse()?),
                "templates" => opts.templates = parse_template_paths(&input.parse::<LitStr>()?)?,
                _ => return Err(syn::Error::new(key.span(), "unknown option")),
            }
            if !input.is_empty() {
                let _: Token![,] = input.parse()?;
            }
        }
        if opts.self_export.is_some() && !opts.default_namespace && opts.default.is_none() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`self_export` requires `default_namespace` or `default`",
            ));
        }
        if opts.default.is_some() && opts.default_namespace {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`default` and `default_namespace` are mutually exclusive",
            ));
        }
        // Validate: js_proto, extends and proto_parent are mutually exclusive
        let parents = [
            opts.js_proto.is_some(),
            opts.extends.is_some(),
            opts.proto_parent.is_some(),
        ];
        if parents.into_iter().filter(|set| *set).count() > 1 {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`js_proto`, `extends` and `proto_parent` are mutually exclusive",
            ));
        }
        Ok(opts)
    }
}

/// Parse the optional arguments of a method-marker attribute such as
/// `#[method(...)]` or `#[getter(...)]`.
///
/// A bare marker (`#[method]`, no parentheses) yields default options.
/// Malformed arguments — or options that aren't valid in this context (a
/// typo'd key, `#[getter(length = 2)]`, a class-level option on a method) —
/// produce a spanned compile error rather than being silently dropped, which
/// would otherwise register the member under the wrong JS name or `.length`.
fn parse_marker_opts(attr: &syn::Attribute, allowed: &[&str]) -> syn::Result<AttrOpts> {
    let opts = match &attr.meta {
        syn::Meta::Path(_) => return Ok(AttrOpts::default()),
        syn::Meta::List(_) => attr.parse_args::<AttrOpts>()?,
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "expected parenthesized arguments, e.g. `(name = \"...\")`",
            ));
        }
    };
    opts.reject_all_but(allowed, attr)?;
    Ok(opts)
}

impl AttrOpts {
    /// Reject every option set on this attribute that isn't in `allowed`.
    ///
    /// Every attribute parses the same option set, so each one has to turn away
    /// the ones that mean nothing to it — silently ignoring `#[getter(length =
    /// 2)]` or `#[webidl_dictionary(hidden)]` would register the member under
    /// the wrong shape. Listing the fields once here means a new option can't
    /// be quietly accepted by every attribute that forgot to name it.
    fn reject_all_but(&self, allowed: &[&str], span: impl quote::ToTokens) -> syn::Result<()> {
        let set: [(bool, &str); 15] = [
            (self.default.is_some(), "default"),
            (self.mirror.is_some(), "mirror"),
            (self.self_export.is_some(), "self_export"),
            (self.default_namespace, "default_namespace"),
            (self.name.is_some(), "name"),
            (self.length.is_some(), "length"),
            (self.extends.is_some(), "extends"),
            (self.js_proto.is_some(), "js_proto"),
            (self.to_string_tag.is_some(), "to_string_tag"),
            (self.hidden, "hidden"),
            (self.unforgeable, "unforgeable"),
            (self.node_args, "node_args"),
            (self.proto_parent.is_some(), "proto_parent"),
            (self.invalid_this.is_some(), "invalid_this"),
            (!self.templates.is_empty(), "templates"),
        ];
        for (present, key) in set {
            if present && !allowed.contains(&key) {
                return Err(syn::Error::new_spanned(
                    &span,
                    format!("`{key}` is not a valid option for this attribute"),
                ));
            }
        }
        Ok(())
    }
}

// ============================================================================
// #[jsclass] attribute macro
// ============================================================================

/// Attribute macro that derives `ClassDef` for a struct and generates a
/// stack newtype for ergonomic use.
///
/// Given `struct Foo { ... }`, this macro:
/// 1. Renames the data struct to `FooImpl` (hidden, implements `ClassDef`)
/// 2. Generates `Foo<'s>` — a `#[repr(transparent)]` newtype wrapping
///    `Stack<'s, FooImpl>` (inherits handle access via deref chain)
///
/// # Usage
///
/// ```rust,ignore
/// #[jsclass]
/// struct MyClass {
///     data: String,
/// }
/// // Generates:
/// //   MyClassImpl { data: String }    — the data struct (ClassDef)
/// //   MyClass<'s>                     — stack newtype (Stack<FooImpl> wrapper)
/// ```
///
/// `templates = "A, B"` gives new instances fixed slots for the keys of the
/// `js::object::PropertyTemplate`s `A` and `B`.
#[proc_macro_attribute]
pub fn jsclass(attr: TokenStream, item: TokenStream) -> TokenStream {
    process_class_def(attr, item, ClassConfig::JSCLASS)
}

/// Attribute macro for WebIDL interface definitions.
///
/// Identical to `#[jsclass]` but with WebIDL-specific defaults:
/// - `Symbol.toStringTag` is automatically set to the class name
///   (unless explicitly overridden via `to_string_tag = "..."`)
/// - `pub const` items in `#[jsmethods]` are installed on **both** the
///   constructor and the prototype (per WebIDL §3.7.3)
///
/// # Usage
///
/// ```rust,ignore
/// #[webidl_interface]
/// struct DOMException {
///     name: String,
///     message: String,
/// }
/// ```
#[proc_macro_attribute]
pub fn webidl_interface(attr: TokenStream, item: TokenStream) -> TokenStream {
    process_class_def(attr, item, ClassConfig::WEBIDL_INTERFACE)
}

// ============================================================================
// Class definition configuration
// ============================================================================

/// Controls codegen differences between `#[jsclass]` and `#[webidl_interface]`.
struct ClassConfig {
    /// When `true` and no explicit `to_string_tag` is set, automatically
    /// use the JS class name as `Symbol.toStringTag`.
    auto_to_string_tag: bool,
    /// When `true`, generate `const CONSTANTS_ON_PROTOTYPE: bool = true;`
    /// so that `pub const` items are installed on both constructor and
    /// prototype (per WebIDL §3.7.3).
    constants_on_prototype: bool,
    /// JS builtins' methods aren't enumerable, but WebIDL interfaces' are, so we have to use different flags for them.
    method_flags: u16,
    /// The attribute this configures, for use in error messages.
    attribute: &'static str,
}

impl ClassConfig {
    /// Configuration for plain `#[jsclass]`: no auto-tag, constants on
    /// constructor only.
    const JSCLASS: Self = Self {
        auto_to_string_tag: false,
        constants_on_prototype: false,
        method_flags: 0,
        attribute: "jsclass",
    };

    /// Configuration for `#[webidl_interface]`: auto Symbol.toStringTag,
    /// constants on both constructor and prototype.
    const WEBIDL_INTERFACE: Self = Self {
        auto_to_string_tag: true,
        constants_on_prototype: true,
        method_flags: 1, // js::class_spec::JSPROP_ENUMERATE
        attribute: "webidl_interface",
    };
}

/// Shared implementation for `#[jsclass]` and `#[webidl_interface]`.
///
/// Processes the attributed struct and generates all ClassDef machinery
/// and stack newtypes.
fn process_class_def(attr: TokenStream, item: TokenStream, config: ClassConfig) -> TokenStream {
    let opts = parse_macro_input!(attr as AttrOpts);
    let mut input = parse_macro_input!(item as ItemStruct);
    let struct_name = input.ident.clone();
    let inner_name = format_ident!("{}Impl", struct_name);
    let js_name = opts.name.unwrap_or_else(|| struct_name.to_string());

    // Generate identifiers for the static JSClass and JSClassOps
    let class_ops_static = format_ident!("__{}_CLASS_OPS", struct_name.to_string().to_uppercase());
    let class_static = format_ident!("__{}_CLASS", struct_name.to_string().to_uppercase());
    let js_name_cstr_lit = cstr_literal(&js_name);

    // If extends is set, compute the inner parent name and rewrite the parent field type
    let opts_extends_ident = opts.extends.clone();
    let inner_parent = opts.extends.as_ref().map(|p| format_ident!("{}Impl", p));

    if let Some(ref inner_parent_name) = inner_parent {
        // A class embeds its parent's data, so the `parent` field holds the parent's `Impl`.
        let extends = opts_extends_ident.as_ref().expect("extends is set");
        if let Fields::Named(ref fields) = input.fields {
            if let Err(e) = parent_field(
                &struct_name,
                &fields.named,
                extends,
                inner_parent_name,
                config.attribute,
            ) {
                return e.to_compile_error().into();
            }
        }
    }

    // Rename the struct to FooImpl. It needs to be `pub` because it's used in
    // trait impls and as the Heap/Stack type parameter, but `#[doc(hidden)]`
    // keeps it out of generated documentation.
    input.ident = inner_name.clone();
    input.vis = syn::Visibility::Public(syn::token::Pub::default());
    input.attrs.push(syn::parse_quote! { #[doc(hidden)] });
    // The inner struct is stored in SpiderMonkey reserved slots and traced
    // via `generic_class_trace`, so its fields don't need independent rooting.
    // The struct itself must still be rooted properly.
    // `allow_self_return`: associated fns may return the bare Impl (`-> Self`
    // constructors and factory methods). The generated trampolines immediately
    // wrap such returns in a new JS object.
    input
        .attrs
        .push(syn::parse_quote! { #[::js::must_root(allow_self_return)] });

    // Generate parent_prototype / register_inheritance / ensure_parent_registered
    // methods if extends or js_proto is set.
    let parent_classdef_methods = if let Some(ref inner_parent_name) = inner_parent {
        quote! {
            // `extends` is real WebIDL interface inheritance (unlike `js_proto`,
            // which only borrows a built-in prototype), so the interface object's
            // [[Prototype]] chains to the parent interface object.
            const INHERITS_INTERFACE: bool = true;

            // Returns a raw prototype pointer that the caller passes straight
            // to SpiderMonkey; it is never held across an allocation.
            #[cfg_attr(crown, allow(crown::unrooted_must_root))]
            fn parent_prototype(scope: &::js::gc::scope::Scope<'_>) -> *mut ::js::native::JSObject {
                ::js::class::get_prototype_for::<#inner_parent_name>(scope)
                    .unwrap_or(::std::ptr::null_mut())
            }

            fn register_inheritance() {
                ::js::class::register_parent_info::<Self>();
            }

            fn ensure_parent_registered(
                scope: &::js::gc::scope::Scope<'_>,
                global: ::js::Object<'_>,
            ) {
                unsafe {
                    // SAFETY: register_class is safe to call if scope and global are valid.
                    ::js::class::register_class::<#inner_parent_name>(scope, global);
                }
            }
        }
    } else if let Some(ref fn_class) = opts.proto_parent {
        quote! {
            const INHERITS_INTERFACE: bool = true;

            // Returns a raw prototype pointer that the caller passes straight
            // to SpiderMonkey. It is never held across an allocation.
            #[cfg_attr(crown, allow(crown::unrooted_must_root))]
            fn parent_prototype(scope: &::js::gc::scope::Scope<'_>) -> *mut ::js::native::JSObject {
                #fn_class::prototype_object(scope)
                    .map(|proto| unsafe { proto.as_raw() })
                    .unwrap_or(::std::ptr::null_mut())
            }

            fn ensure_parent_registered(
                scope: &::js::gc::scope::Scope<'_>,
                _global: ::js::Object<'_>,
            ) {
                #fn_class::register(scope).expect("registering a class succeeds outside of OOM");
            }
        }
    } else if let Some(ref proto_name) = opts.js_proto {
        // js_proto = "Error" → use the built-in JS prototype via JSProtoKey.
        let proto_key = format_ident!("JSProto_{}", proto_name);
        quote! {
            // Returns a raw prototype pointer that the caller passes straight
            // to SpiderMonkey; it is never held across an allocation.
            #[cfg_attr(crown, allow(crown::unrooted_must_root))]
            fn parent_prototype(scope: &::js::gc::scope::Scope<'_>) -> *mut ::js::native::JSObject {
                ::js::class::get_class_prototype(scope, ::js::class_spec::JSProtoKey::#proto_key)
                    .map(|h| h.get())
                    .unwrap_or(::std::ptr::null_mut())
            }
        }
    } else {
        quote! {}
    };

    // A class embeds its parent's data, so the parent's destructor is called after this
    // class's own. Without this it would never run, because finalization dispatches only on
    // the most derived class.
    let parent_destructor = if let Some(ref inner_parent_name) = inner_parent {
        quote! {
            <#inner_parent_name as ::js::class::ClassDef>::destructor(&mut self.parent, object);
        }
    } else {
        quote! {}
    };

    // Generate TO_STRING_TAG const override.
    // Explicit `to_string_tag = "..."` always wins. Otherwise, when
    // `config.auto_to_string_tag` is true (webidl_interface), default
    // to the JS class name.
    let effective_tag = opts
        .to_string_tag
        .as_deref()
        .or({
            if config.auto_to_string_tag {
                Some(js_name.as_str())
            } else {
                None
            }
        })
        .map(|t| t.to_owned());
    let to_string_tag_const = if let Some(ref tag) = effective_tag {
        quote! {
            const TO_STRING_TAG: &'static str = #tag;
        }
    } else {
        quote! {}
    };

    // Generate CONSTANTS_ON_PROTOTYPE override for webidl_interface.
    let constants_on_prototype_const = if config.constants_on_prototype {
        quote! {
            const CONSTANTS_ON_PROTOTYPE: bool = true;
        }
    } else {
        quote! {}
    };

    // Generate HAS_ERROR_DATA const when js_proto = "Error".
    let has_error_data_const = if opts.js_proto.as_deref() == Some("Error") {
        quote! {
            const HAS_ERROR_DATA: bool = true;
            const RESERVED_SLOTS: u32 = ::js::class::ERROR_STACK_SLOT + 1;
        }
    } else {
        quote! {}
    };

    // Generate HIDDEN override when `hidden` is set.
    let hidden_const = if opts.hidden {
        quote! {
            const HIDDEN: bool = true;
        }
    } else {
        quote! {}
    };

    // Generate the `define_unforgeable` and `has_unforgeable` ClassDef methods.
    // They chain to the parent interface (for inherited unforgeable accessors)
    // and delegate own accessors to the `__UnforgeableRegistrar` provided by
    // `#[jsmethods]`.
    let install_unforgeable_method = {
        let (parent_define, parent_has) = if let Some(ref inner_parent_name) = inner_parent {
            (
                quote! {
                    <#inner_parent_name as ::js::class::ClassDef>::define_unforgeable(scope, obj)?;
                },
                quote! {
                    <#inner_parent_name as ::js::class::ClassDef>::has_unforgeable() ||
                },
            )
        } else {
            (quote! {}, quote! {})
        };
        quote! {
            fn define_unforgeable(
                scope: &::js::gc::scope::Scope<'_>,
                obj: ::js::Object<'_>,
            ) -> ::std::result::Result<(), ::js::error::ExnThrown> {
                #parent_define
                use ::js::class::__UnforgeableRegistrar;
                let reg = ::js::class::__UnforgeableReg::<Self>::new();
                (&reg).install(scope, obj)
            }

            fn has_unforgeable() -> bool {
                use ::js::class::__UnforgeableRegistrar;
                let reg = ::js::class::__UnforgeableReg::<Self>::new();
                #parent_has (&reg).has()
            }
        }
    };

    // Generate debug_assert_fully_initialized override if the struct has
    // bare Heap<T> fields (not Option<Heap<T>>, which is legitimately nullable).
    let heap_field_assertions: Vec<_> = if let Fields::Named(ref fields) = input.fields {
        fields
            .named
            .iter()
            .filter_map(|f| {
                let ident = f.ident.as_ref()?;
                if is_bare_heap_type(&f.ty) {
                    let name = ident.to_string();
                    Some(quote! {
                        debug_assert!(
                            self.#ident.is_initialized(),
                            "Heap field `{}::{}` not initialized after construction",
                            stringify!(#inner_name),
                            #name,
                        );
                    })
                } else {
                    None
                }
            })
            .collect()
    } else {
        Vec::new()
    };

    let debug_assert_method = if heap_field_assertions.is_empty() {
        quote! {}
    } else {
        quote! {
            fn debug_assert_fully_initialized(&self) {
                #(#heap_field_assertions)*
            }
        }
    };

    let cast_error_lit = cstr_literal(&format!("Value is not an instance of {}", struct_name));
    let not_type_error_lit = cstr_literal(&format!("'this' is not of type {}", js_name));
    let no_new_error = if config.attribute == "jsclass" {
        let message = cstr_literal(&format!(
            "Class constructor {js_name} cannot be invoked without 'new'"
        ));
        quote! { const CALL_WITHOUT_NEW_ERROR: &'static ::core::ffi::CStr = #message; }
    } else {
        quote! {}
    };
    let prototype_call_error_lit = cstr_literal(&format!(
        "\"{}\" getter/method called on prototype or uninitialized object",
        js_name
    ));
    let throw_invalid_this = match &opts.invalid_this {
        Some(path) => quote! {
            fn throw_invalid_this(
                scope: &::js::gc::scope::Scope<'_>,
                _site: &::js::error::BindingSite,
                _message: &'static ::core::ffi::CStr,
            ) -> ::js::error::ExnThrown {
                #path(scope, #js_name)
            }
        },
        None => quote! {},
    };
    let property_count = if opts.templates.is_empty() {
        quote! {}
    } else {
        let count = template_property_count(&opts.templates);
        quote! {
            fn property_count() -> usize {
                #count
            }
        }
    };

    let output = quote! {
        #[doc(hidden)]
        #[derive(Default, ::js::macros::Traceable)]
        #input

        // Static JSClassOps for this type — unique per ClassDef.
        #[doc(hidden)]
        static #class_ops_static: ::js::class_spec::JSClassOps = ::js::class_spec::JSClassOps {
            addProperty: None,
            delProperty: None,
            enumerate: None,
            newEnumerate: None,
            resolve: None,
            mayResolve: None,
            finalize: Some(::js::class::generic_class_finalize::<#inner_name>),
            call: None,
            construct: None,
            trace: Some(::js::class::generic_class_trace::<#inner_name>),
        };

        // Static JSClass for this type — its address serves as the type tag.
        #[doc(hidden)]
        static #class_static: ::js::class_spec::JSClass = {
            // Ensure at least MIN_CLASS_RESERVED_SLOTS for private data (slot 0).
            // Use a const block so the max() is evaluated at compile time.
            const SLOTS: u32 = if <#inner_name as ::js::class::ClassDef>::RESERVED_SLOTS
                > ::js::class::MIN_CLASS_RESERVED_SLOTS
            {
                <#inner_name as ::js::class::ClassDef>::RESERVED_SLOTS
            } else {
                ::js::class::MIN_CLASS_RESERVED_SLOTS
            };

            ::js::class_spec::JSClass {
                name: #js_name_cstr_lit.as_ptr(),
                flags: ::js::class_spec::JSCLASS_FOREGROUND_FINALIZE
                    | ((SLOTS & ::js::class_spec::JSCLASS_RESERVED_SLOTS_MASK)
                        << ::js::class_spec::JSCLASS_RESERVED_SLOTS_SHIFT),
                cOps: &#class_ops_static as *const ::js::class_spec::JSClassOps,
                spec: ::std::ptr::null(),
                ext: ::std::ptr::null(),
                oOps: ::std::ptr::null(),
            }
        };

        // Generated ClassDef impl using autoref specialization.
        // The constructor and method registration delegate to companion types
        // that are populated by #[jsmethods].
        #[::js::must_root]
        impl ::js::class::ClassDef for #inner_name {
            type Rooted<'s> = #struct_name<'s>;
            const NAME: &'static str = #js_name;
            const NAME_CSTR: &'static ::core::ffi::CStr = #js_name_cstr_lit;
            const NOT_TYPE_ERROR: &'static ::core::ffi::CStr = #not_type_error_lit;
            const PROTOTYPE_CALL_ERROR: &'static ::core::ffi::CStr = #prototype_call_error_lit;
            #no_new_error
            #throw_invalid_this
            #property_count

            fn class() -> &'static ::js::class_spec::JSClass {
                &#class_static
            }

            fn constructor(
                scope: &::js::gc::scope::Scope<'_>,
                args: &::js::native::CallArgs,
            ) -> ::std::result::Result<Self, ::js::error::ExnThrown> {
                use ::js::class::__ConstructorRegistrar;
                let reg = ::js::class::__CtorReg::<Self>::new();
                (&reg).construct(scope, args)
            }

            fn constructor_nargs() -> u32 {
                use ::js::class::__ConstructorRegistrar;
                let reg = ::js::class::__CtorReg::<Self>::new();
                (&reg).nargs()
            }

            fn has_js_constructor() -> bool {
                use ::js::class::__ConstructorRegistrar;
                let reg = ::js::class::__CtorReg::<Self>::new();
                (&reg).defined()
            }

            fn register_class_methods(
                builder: ::js::class::ClassBuilder<Self>,
            ) -> ::js::class::ClassBuilder<Self> {
                use ::js::class::__MethodRegistrar;
                let reg = ::js::class::__MethodReg::<Self>::new();
                (&reg).register(builder)
            }

            fn register_static_methods(
                builder: ::js::class::ClassBuilder<Self>,
            ) -> ::js::class::ClassBuilder<Self> {
                use ::js::class::__StaticMethodRegistrar;
                let reg = ::js::class::__StaticMethodReg::<Self>::new();
                (&reg).register(builder)
            }

            #[cfg_attr(crown, allow(crown::unrooted_must_root))]
            fn destructor(&mut self, object: *mut ::js::native::JSObject) {
                use ::js::class::__DestructorRegistrar;
                let reg = ::js::class::__DtorReg::<Self>::new();
                (&reg).destruct(self, object);
                #parent_destructor
            }

            fn register_constants(
                builder: ::js::class::ClassBuilder<Self>,
            ) -> ::js::class::ClassBuilder<Self> {
                use ::js::class::__ConstantRegistrar;
                let reg = ::js::class::__ConstantReg::<Self>::new();
                (&reg).register(builder)
            }

            fn post_init(
                scope: &::js::gc::scope::Scope<'_>,
                obj: ::js::Object<'_>,
                args: &::js::native::CallArgs,
            ) -> ::std::result::Result<(), ::js::error::ExnThrown> {
                use ::js::class::__PostInitRegistrar;
                let reg = ::js::class::__PostInitReg::<Self>::new();
                (&reg).post_init(scope, obj, args)
            }

            #parent_classdef_methods
            #to_string_tag_const
            #has_error_data_const
            #constants_on_prototype_const
            #hidden_const
            #install_unforgeable_method
            #debug_assert_method
        }

        // Reflexive DerivedFrom: every class derives from itself
        impl ::js::class::DerivedFrom<#inner_name> for #inner_name {}

        // ================================================================
        // Foo<'s> — stack newtype wrapping Stack<'s, FooImpl>
        // ================================================================
        #[repr(transparent)]
        #[derive(Copy, Clone)]
        pub struct #struct_name<'s>(::js::gc::handle::Stack<'s, #inner_name>);

        impl<'s> ::js::class::StackType<'s> for #struct_name<'s> {
            type Inner = #inner_name;

            unsafe fn from_handle_unchecked(
                h: ::js::native::GCHandle<'s, *mut ::js::native::JSObject>,
            ) -> Self {
                #struct_name(::js::gc::handle::Stack::from_handle_unchecked(h))
            }

            fn js_handle(self) -> ::js::native::GCHandle<'s, *mut ::js::native::JSObject> {
                self.0.handle()
            }
        }

        impl<'s> ::js::class::InstanceState<'s> for #struct_name<'s> {
            type Class = #inner_name;

            fn of_instance(instance: ::js::Object<'s>) -> ::std::option::Option<Self> {
                instance.cast::<#struct_name<'s>>().ok()
            }
        }

        impl<'s> ::js::builtins::CastTarget<'s> for #struct_name<'s> {
            type Output = #struct_name<'s>;

            const TARGET_NAME: &'static str = <#inner_name as ::js::class::ClassDef>::NAME;

            // The raw pointer is only inspected, never held across an
            // allocation.
            #[cfg_attr(crown, allow(crown::unrooted_must_root))]
            #[inline]
            unsafe fn is_instance(obj: *mut ::js::native::JSObject) -> bool {
                unsafe { <#inner_name as ::js::builtins::JSType>::is_instance(obj) }
            }

            unsafe fn construct_unchecked(
                h: ::js::native::GCHandle<'s, *mut ::js::native::JSObject>,
            ) -> #struct_name<'s> {
                #struct_name(unsafe { ::js::gc::handle::Stack::from_handle_unchecked(h) })
            }
        }

        impl<'s> #struct_name<'s> {
            /// Get the raw `*mut JSObject` pointer.
            // Raw-pointer escape hatch: the caller takes responsibility for
            // not holding the pointer across an allocation.
            #[cfg_attr(crown, allow(crown::unrooted_must_root))]
            pub unsafe fn as_raw(self) -> *mut ::js::native::JSObject {
                self.0.as_raw()
            }

            /// Upcast to the untyped `Object` handle, reusing this rooting.
            pub fn as_object(self) -> ::js::Object<'s> {
                // SAFETY: every class instance is a JS object.
                unsafe { ::js::gc::handle::Stack::from_handle_unchecked(self.0.handle()) }
            }

            pub fn eq_stack(&self, other: &::js::gc::handle::Stack<'_, #inner_name>) -> bool {
                self.0.as_raw() == other.as_raw()
            }

            pub fn eq_heap(&self, other: &::js::gc::handle::Heap<#inner_name>) -> bool {
                unsafe { self.0.as_raw() == other.as_ptr() }
            }

            /// Borrow the private Rust data (guard dereferencing to `&data`).
            ///
            /// In debug builds, panics if the data is already mutably borrowed
            /// (a reentrant access to the same object). See
            /// `js::class::Stack::data`.
            #[inline]
            pub fn data(&self) -> ::js::class::Ref<'_, #inner_name> {
                self.0.data()
            }

            /// Mutably borrow the private Rust data (guard dereferencing to
            /// `&mut data`).
            ///
            /// In debug builds, panics if the data is already borrowed (a
            /// reentrant access to the same object). See
            /// `js::class::Stack::data_mut`.
            #[inline]
            pub fn data_mut(&self) -> ::js::class::RefMut<'_, #inner_name> {
                self.0.data_mut()
            }
        }

        impl<'s> ::js::conversion::ToJSVal<'s> for #struct_name<'s> {
            #[inline]
            fn to_jsval_raw(&self, scope: &'s ::js::prelude::Scope<'_>) -> ::std::result::Result<::js::value::Value, ::js::conversion::ConversionError> {
                self.0.to_jsval_raw(scope)
            }
        }

        impl<'s, 'v> ::js::conversion::FromJSVal<'s, 'v> for #struct_name<'s> {
            type Config = ();

            fn from_jsval(
                scope: &'s ::js::prelude::Scope<'s>,
                val: ::js::prelude::HandleValue<'v>,
                _option: (),
            ) -> ::std::result::Result<Self, ::js::conversion::ConversionError> {
                let obj = ::js::Object::from_jsval(scope, val, ())?;
                obj.cast::<#struct_name<'s>>().map_err(|_| ::js::conversion::ConversionError::Failure(::std::borrow::Cow::Borrowed(#cast_error_lit)))
            }
        }

        impl<'s> ::std::convert::From<::js::gc::handle::Stack<'s, #inner_name>> for #struct_name<'s> {
            fn from(stack: ::js::gc::handle::Stack<'s, #inner_name>) -> Self {
                #struct_name(stack)
            }
        }

        impl<'s> ::std::convert::From<#struct_name<'s>> for ::js::gc::handle::Stack<'s, #inner_name> {
            fn from(val: #struct_name<'s>) -> Self {
                val.0
            }
        }

        impl<'s> ::std::convert::From<#struct_name<'s>> for ::js::gc::handle::Heap<#inner_name> {
            fn from(val: #struct_name<'s>) -> Self {
                ::js::gc::handle::Heap::from(val.0)
            }
        }
    };

    // If extends is specified, append inheritance impls and set Deref target
    // to the parent type. Otherwise, Deref targets Object.
    let output = if let Some(ref inner_parent_name) = inner_parent {
        let parent_name = opts_extends_ident.as_ref().unwrap();
        quote! {
            #output

            // Deref: Foo<'s> -> Parent<'s>
            impl<'s> ::std::ops::Deref for #struct_name<'s> {
                type Target = #parent_name<'s>;
                fn deref(&self) -> &Self::Target {
                    unsafe { ::std::mem::transmute(self) }
                }
            }

            // Deref: FooImpl -> ParentImpl
            impl ::std::ops::Deref for #inner_name {
                type Target = #inner_parent_name;
                fn deref(&self) -> &Self::Target {
                    &self.parent
                }
            }

            // Deref: FooImpl -> ParentImpl
            impl ::std::ops::DerefMut for #inner_name {
                fn deref_mut(&mut self) -> &mut Self::Target {
                    &mut self.parent
                }
            }

            impl ::js::class::HasParent for #inner_name {
                type Parent = #inner_parent_name;
                fn as_parent(&self) -> &#inner_parent_name { &self.parent }
                fn as_parent_mut(&mut self) -> &mut #inner_parent_name { &mut self.parent }
            }

            impl ::js::class::DerivedFrom<#inner_parent_name> for #inner_name {}
        }
    } else {
        quote! {
            #output

            // Deref: Foo<'s> -> Object<'s> (base case, no parent)
            impl<'s> ::std::ops::Deref for #struct_name<'s> {
                type Target = ::js::Object<'s>;
                fn deref(&self) -> &Self::Target {
                    unsafe { ::std::mem::transmute(self) }
                }
            }
        }
    };

    output.into()
}

// ============================================================================
// #[jsmethods] attribute macro
// ============================================================================

/// Classification of a method in the impl block.
enum MethodKind {
    Constructor,
    Destructor,
    Method {
        js_name: String,
    },
    StaticMethod {
        js_name: String,
    },
    /// Property getter — becomes a JSPropertySpec accessor, or a per-instance
    /// own accessor when `unforgeable` (`[LegacyUnforgeable]`).
    Getter {
        js_name: String,
        unforgeable: bool,
    },
    /// Property setter — becomes a JSPropertySpec accessor.
    Setter {
        js_name: String,
    },
}

/// How the return value of a method should be handled.
enum ReturnStyle {
    /// No return value (or returns `()`)
    Void,
    /// Returns a value that implements `ToJSVal`
    Value,
    /// Returns `Result<(), impl Display>` — error becomes JS exception
    ResultVoid,
    /// Returns `Result<T, impl Display>` — Ok value set as return, Err becomes exception
    ResultValue,
    /// Raw method returning `Result<(), ExnThrown>` with manual exception handling
    Raw,
    /// Returns `PromiseFuture` — creates a JS Promise and spawns the async future
    Promise,
    /// Returns `Result<Promise<'_>, E>` — a synchronous WebIDL operation whose
    /// return type is a promise. The `Ok` promise is returned to JS; an `Err`
    /// (from argument conversion or the method body) is surfaced as a rejected
    /// promise rather than a synchronous throw, per WebIDL §3.7.7 ("Operations":
    /// when an operation whose return type is a promise type throws, return
    /// `! Call(%Promise.reject%, %Promise%, « E »)`).
    ResultPromise,
    /// Returns `Self` (or the class type) from a method/static_method —
    /// the result is wrapped into a new JS object via `create_instance`.
    InstanceValue,
}

/// Info about a parsed method.
struct MethodInfo {
    kind: MethodKind,
    fn_item: ImplItemFn,
    /// Parameter names and types (excluding self/cx/args)
    params: Vec<(Ident, Type)>,
    /// Number of required arguments: the explicit `length = N` override when
    /// given, otherwise the count of non-`Option` (and non-`RestArgs`) params.
    /// This is the JS-visible `.length`.
    nargs: u32,
    /// The threshold at and beyond which `any`-typed (`HandleValue`/`Value`)
    /// params are extracted as optional (missing → `undefined`) rather than
    /// throwing: `nargs`, or 0 with `#[jsmethods(node_args)]`.
    required: u32,
    /// How the return value should be handled
    return_style: ReturnStyle,
    /// Whether the method takes &self
    has_self: bool,
    /// Whether the method takes &mut self
    has_mut_self: bool,
    /// Whether the method takes cx: &mut JSContext
    has_cx: bool,
    /// Whether the method has raw cx/args params for low-level access
    is_raw: bool,
    /// Whether the method has a variadic rest parameter (last param)
    has_rest_args: bool,
    /// Name of the rest parameter, if any
    rest_arg_name: Option<Ident>,
    /// Inner type of RestArgs<T>, or None for bare RestArgs (defaults to Value)
    rest_inner_type: Option<Type>,
}

/// Attribute macro for an `impl` block that generates JSNative wrappers.
///
/// The impl block is written on the user-visible type name (e.g. `impl Foo`),
/// but is rewritten to target the inner data struct (`impl FooImpl`).
/// Forwarding methods and constructors are generated on the stack newtype
/// `Foo<'s>`.
///
/// # Instantiation
///
/// To make a class constructible from JS, at most one function can be
/// annotated with `#[constructor]`. Otherwise, attempting to instantiate the
/// class will throw a `TypeError`. `#[constructor(length = N)]` sets the
/// constructor's `length` and required argument count, as `length = N` does
/// for `#[method]`.
///
/// Independently of that, every constructor-shaped fn in the `impl` block is
/// added to the stack newtype as a constructor, with the return type
/// `Result<Foo<'s>, ExnThrown>`, that creates the instance's JS object
/// and runs the fn to initialize it.
///
/// Constructor-shaped means either of the two constructor forms:
///
/// ```rust,ignore
/// fn anything() -> Self                          // no receiver, returns the class
/// fn new(&self, ...) -> Result<(), E>            // setup-style, recognized by name
/// ```
///
/// Examples:
///
/// ```rust,ignore
/// #[jsmethods]
/// impl Foo {
///     #[constructor]
///     fn new(n: i32) -> Self { Self { n } }   // Foo::new(scope, n)  + `new Foo(n)` in JS
///     fn doubled(n: i32) -> Self { Self { n: n * 2 } }  // Foo::doubled(scope, n), Rust only
/// }
/// ```
///
/// See [`is_implicit_constructor`].
///
/// # Usage
///
/// ```rust,ignore
/// #[jsmethods]
/// impl MyClass {
///     #[constructor]
///     fn new(data: String) -> Self {
///         Self { data }
///     }
///
///     #[method(name = "toString")]
///     fn to_string(&self) -> String {
///         format!("MyClass({})", self.data)
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn jsmethods(attr: TokenStream, item: TokenStream) -> TokenStream {
    process_methods(attr, item, ClassConfig::JSCLASS)
}

/// Attribute macro for an `impl` block that generates JSNative wrappers for
/// WebIDL interfaces' methods and properties.
///
/// Identical to `#[jsmethods]`, except that methods are enumerable.
#[proc_macro_attribute]
pub fn webidl_methods(attr: TokenStream, item: TokenStream) -> TokenStream {
    process_methods(attr, item, ClassConfig::WEBIDL_INTERFACE)
}

fn process_methods(attr: TokenStream, item: TokenStream, config: ClassConfig) -> TokenStream {
    let opts = parse_macro_input!(attr as AttrOpts);
    let mut input = parse_macro_input!(item as ItemImpl);
    if let Err(e) = opts.reject_all_but(&["node_args"], &input.self_ty) {
        return e.to_compile_error().into();
    }

    let self_ty = &input.self_ty;

    // Extract the type name for generating function names
    let type_name = match self_ty.as_ref() {
        Type::Path(tp) => tp
            .path
            .segments
            .last()
            .map(|s| s.ident.clone())
            .expect("Expected a named type"),
        _ => panic!("#[jsmethods] requires a named type"),
    };

    // Compute the inner data struct name
    let inner_name = format_ident!("{}Impl", type_name);

    // Find the `#[constructor]`-annotated function to use as the JS constructor.
    // Finding more than one such function results in an error.
    let js_ctor_names: Vec<Ident> = input
        .items
        .iter()
        .filter_map(|item| match item {
            ImplItem::Fn(f) if f.attrs.iter().any(|a| a.path().is_ident("constructor")) => {
                Some(f.sig.ident.clone())
            }
            _ => None,
        })
        .collect();
    if let [first, second, ..] = js_ctor_names.as_slice() {
        return syn::Error::new_spanned(
            second,
            format!(
                "a class can have at most one `#[constructor]`, but `{first}` and `{second}` are \
                 both marked; JS exposes a single constructor. Drop the attribute from one — an \
                 unannotated fn returning `Self` is still a Rust-side factory."
            ),
        )
        .to_compile_error()
        .into();
    }

    let js_ctor_name = js_ctor_names.first();
    let mut js_ctor_index = None;

    let mut methods: Vec<MethodInfo> = Vec::new();
    let mut ctor_indices = Vec::new();

    // Set when a method-marker attribute has malformed or out-of-context
    // arguments. Collected during the per-item scan and turned into a spanned
    // compile error after the loop, rather than silently dropping the options.
    let mut attr_error: Option<syn::Error> = None;
    let mut constant_builder_calls: Vec<proc_macro2::TokenStream> = Vec::new();
    // Names of unannotated `fn`s in the impl block. These are plain Rust
    // helpers (not JS-exposed), but like `#[method]`s they operate on the
    // rooted object, so they are moved onto the stack newtype `Foo<'s>`
    // alongside the registered methods — letting them use the newtype API
    // (`self.data()`, sibling methods) and be called as `self.helper(..)`.
    let mut helper_fn_names: Vec<Ident> = Vec::new();

    // Parse each item and classify it
    for item in &mut input.items {
        // Handle `pub const NAME: Type = value;` items — generate constant builder calls.
        if let ImplItem::Const(const_item) = item {
            if matches!(const_item.vis, Visibility::Public(_)) {
                // Class constants are registered through an `i32` slot, so the
                // value is `as i32`-cast. Only integer types that round-trip
                // losslessly are allowed — an `f64`, `bool`, or a wider integer
                // would be silently truncated or wrapped.
                const I32_SAFE: &[&str] = &["i8", "u8", "i16", "u16", "i32"];
                if !I32_SAFE.iter().any(|t| last_segment_is(&const_item.ty, t)) {
                    attr_error.get_or_insert(syn::Error::new_spanned(
                        &const_item.ty,
                        "a class `pub const` must have an integer type that fits in i32 \
                         (i8, u8, i16, u16, or i32); other types would be truncated by the \
                         constant's i32 representation",
                    ));
                    continue;
                }

                let const_name = const_item.ident.to_string();
                let const_name_bytes = format!("{const_name}\0");
                let const_name_cstr =
                    proc_macro2::Literal::byte_string(const_name_bytes.as_bytes());
                let const_ident = &const_item.ident;

                constant_builder_calls.push(quote! {
                    .constant(
                        unsafe { ::std::ffi::CStr::from_bytes_with_nul_unchecked(#const_name_cstr) },
                        #inner_name::#const_ident as i32,
                    )
                });
            }
            continue;
        }

        if let ImplItem::Fn(fn_item) = item {
            let mut kind = None;
            let mut custom_rename = None;
            let mut custom_nargs = None;

            // Check for our attributes
            fn_item.attrs.retain(|attr| {
                if attr.path().is_ident("constructor") {
                    match parse_marker_opts(attr, &["length"]) {
                        Ok(opts) => custom_nargs = opts.length,
                        Err(e) => {
                            attr_error.get_or_insert(e);
                        }
                    }
                    kind = Some(MethodKind::Constructor);
                    assert_eq!(&fn_item.sig.ident, js_ctor_name.unwrap());
                    ctor_indices.push(methods.len());
                    js_ctor_index = Some(methods.len());
                    false
                } else if attr.path().is_ident("method") {
                    match parse_marker_opts(attr, &["name", "length"]) {
                        Ok(opts) => {
                            custom_rename = opts.name;
                            custom_nargs = opts.length;
                        }
                        Err(e) => {
                            attr_error.get_or_insert(e);
                        }
                    }
                    kind = Some(MethodKind::Method {
                        js_name: String::new(), // filled below
                    });
                    false
                } else if attr.path().is_ident("static_method") {
                    match parse_marker_opts(attr, &["name", "length"]) {
                        Ok(opts) => {
                            custom_rename = opts.name;
                            custom_nargs = opts.length;
                        }
                        Err(e) => {
                            attr_error.get_or_insert(e);
                        }
                    }
                    kind = Some(MethodKind::StaticMethod {
                        js_name: String::new(), // filled below
                    });
                    false
                } else if attr.path().is_ident("getter") {
                    let mut unforgeable = false;
                    match parse_marker_opts(attr, &["name", "unforgeable"]) {
                        Ok(opts) => {
                            custom_rename = opts.name;
                            unforgeable = opts.unforgeable;
                        }
                        Err(e) => {
                            attr_error.get_or_insert(e);
                        }
                    }
                    kind = Some(MethodKind::Getter {
                        js_name: String::new(), // filled below
                        unforgeable,
                    });
                    false
                } else if attr.path().is_ident("setter") {
                    match parse_marker_opts(attr, &["name"]) {
                        Ok(opts) => {
                            custom_rename = opts.name;
                        }
                        Err(e) => {
                            attr_error.get_or_insert(e);
                        }
                    }
                    kind = Some(MethodKind::Setter {
                        js_name: String::new(), // filled below
                    });
                    false
                } else if attr.path().is_ident("destructor") {
                    kind = Some(MethodKind::Destructor);
                    false
                } else {
                    true // keep other attrs
                }
            });

            // A malformed marker attribute taints this method; stop scanning and
            // report it below rather than building codegen from partial options.
            if attr_error.is_some() {
                break;
            }

            let kind = match kind {
                Some(k) => k,
                // An unannotated constructor-shaped fn is a Rust-side factory:
                // it allocates a JS object like `#[constructor]` does, but is
                // not exposed to JS. A class may have any number of them.
                None if is_implicit_constructor(&fn_item.sig, &type_name) => {
                    ctor_indices.push(methods.len());
                    MethodKind::Constructor
                }
                None => {
                    helper_fn_names.push(fn_item.sig.ident.clone());
                    continue;
                }
            };

            // A destructor taking the finalized object names a raw `JSObject` pointer, and must
            // not allocate anything or otherwise trigger a GC, so opting out of the crown check
            // is safe.
            if matches!(kind, MethodKind::Destructor) && fn_item.sig.inputs.len() > 1 {
                fn_item.attrs.push(
                    syn::parse_quote! { #[cfg_attr(crown, allow(crown::unrooted_must_root))] },
                );
            }

            let mut info = parse_method_info(
                fn_item.clone(),
                kind,
                custom_rename,
                custom_nargs,
                &type_name,
            );
            if opts.node_args {
                info.required = 0;
            }

            // Rewrite RestArgs<T> in the function signature to use the
            // fully-qualified type path so the impl block compiles.
            let fn_name = fn_item.sig.ident.clone();
            if let Err(e) = rewrite_rest_args_in_sig(&mut fn_item.sig, &fn_name) {
                attr_error.get_or_insert(e);
                break;
            }

            methods.push(info);
        }
    }

    if let Some(e) = attr_error {
        return e.to_compile_error().into();
    }

    // Rewrite the impl block's self type to FooImpl
    *input.self_ty = syn::parse_quote! { #inner_name };

    // Suppress clippy warnings for generated impl (e.g. inherent to_string methods)
    input
        .attrs
        .push(syn::parse_quote! { #[allow(clippy::inherent_to_string)] });

    // Generate JSNative wrappers for non-constructor methods
    let mut native_fns = Vec::new();
    let mut builder_calls = Vec::new();
    let mut static_builder_calls = Vec::new();
    let mut constructor_body = None;
    let mut destructor_fn_name = None;
    // A destructor may take the object being finalized as a second parameter.
    let mut destructor_takes_object = false;
    // Setup-style constructor: detected when #[constructor] has &self/&mut self.
    // The constructor body runs on the stack newtype after allocation + boxing.
    let mut setup_ctor_info: Option<usize> = None; // index into `methods`

    // Collect property accessors indexed by JS name for pairing
    struct PropertyEntry {
        js_name: String,
        getter_native: Option<Ident>,
        setter_native: Option<Ident>,
    }
    let mut property_map: Vec<PropertyEntry> = Vec::new();
    // `[LegacyUnforgeable]` getters: (JS name, native getter ident). Installed
    // per-instance via `install_unforgeable` rather than on the prototype.
    let mut unforgeable_getters: Vec<(String, Ident)> = Vec::new();

    fn find_or_create_property<'a>(
        map: &'a mut Vec<PropertyEntry>,
        js_name: &str,
    ) -> &'a mut PropertyEntry {
        if let Some(pos) = map.iter().position(|e| e.js_name == js_name) {
            &mut map[pos]
        } else {
            map.push(PropertyEntry {
                js_name: js_name.to_string(),
                getter_native: None,
                setter_native: None,
            });
            map.last_mut().unwrap()
        }
    }

    for (i, method) in methods.iter().enumerate() {
        let on_newtype = is_method_on_newtype(method);
        match &method.kind {
            // Only the explicit `#[constructor]` backs the JS constructor.
            MethodKind::Constructor if Some(i) == js_ctor_index => {
                if method.has_self || method.has_mut_self {
                    // Setup-style: constructor runs on the stack newtype after
                    // allocation. ConstructorRegistrar returns FooImpl::default(),
                    // and PostInitRegistrar handles arg extraction + calling
                    // the user's constructor body.
                    setup_ctor_info = Some(i);
                } else {
                    // Old-style: constructor returns Self directly.
                    constructor_body = Some(gen_constructor_body(method, &inner_name));
                }
            }
            MethodKind::Constructor => {}
            MethodKind::Destructor => {
                destructor_takes_object = method.fn_item.sig.inputs.len() > 1;
                destructor_fn_name = Some(method.fn_item.sig.ident.clone());
            }
            MethodKind::Method { js_name } => {
                let (native_fn, builder_call) = gen_method_native(
                    method,
                    &inner_name,
                    &type_name,
                    js_name,
                    config.method_flags,
                    on_newtype,
                );
                native_fns.push(native_fn);
                builder_calls.push(builder_call);
            }
            MethodKind::StaticMethod { js_name } => {
                let (native_fn, builder_call) = gen_method_native(
                    method,
                    &inner_name,
                    &type_name,
                    js_name,
                    config.method_flags,
                    false,
                );
                native_fns.push(native_fn);
                static_builder_calls.push(builder_call);
            }
            MethodKind::Getter {
                js_name,
                unforgeable,
            } => {
                let native_fn =
                    gen_accessor_native(method, &inner_name, &type_name, js_name, true, on_newtype);
                let native_name =
                    format_ident!("__getter_{inner_name}_{}", unraw(&method.fn_item.sig.ident));
                native_fns.push(native_fn);
                if *unforgeable {
                    // Installed per-instance as an own property, not on the
                    // prototype, so it is not added to `property_map`.
                    unforgeable_getters.push((js_name.clone(), native_name));
                } else {
                    let entry = find_or_create_property(&mut property_map, js_name);
                    entry.getter_native = Some(native_name);
                }
            }
            MethodKind::Setter { js_name } => {
                let native_fn = gen_accessor_native(
                    method,
                    &inner_name,
                    &type_name,
                    js_name,
                    false,
                    on_newtype,
                );
                let native_name =
                    format_ident!("__setter_{inner_name}_{}", unraw(&method.fn_item.sig.ident));
                native_fns.push(native_fn);
                let entry = find_or_create_property(&mut property_map, js_name);
                entry.setter_native = Some(native_name);
            }
        }
    }

    // Generate .property() builder calls for all accessor entries
    for entry in &property_map {
        let js_name = &entry.js_name;
        let js_name_cstr_lit = cstr_literal(js_name);

        let getter = match &entry.getter_native {
            Some(name) => quote! { Some(#name) },
            None => quote! { None },
        };
        let setter = match &entry.setter_native {
            Some(name) => quote! { Some(#name) },
            None => quote! { None },
        };

        let flags = config.method_flags;
        builder_calls.push(quote! {
            .property(
                #js_name_cstr_lit,
                #getter,
                #setter,
                #flags,
            )
        });
    }

    // ================================================================
    // Remove setup-style ctors and stack newtype methods from impl FooImpl.
    // ================================================================
    // Setup-style ctor bodies run on the stack newtype (as `__ctor_setup_*`),
    // so they are dropped from the FooImpl block; old-style ctors stay there.
    let remove_fn_names: Vec<Ident> = ctor_indices
        .iter()
        .map(|i| &methods[*i])
        .filter(|m| m.has_self || m.has_mut_self)
        .map(|m| m.fn_item.sig.ident.clone())
        .collect();

    // Collect names of methods that go on the newtype.
    let newtype_method_names: Vec<Ident> = methods
        .iter()
        .filter(|m| is_method_on_newtype(m))
        .map(|m| m.fn_item.sig.ident.clone())
        .collect();

    // Remove setup ctors and stack newtype methods from the FooImpl impl block.
    let mut newtype_items: Vec<ImplItem> = Vec::new();
    {
        let remove_names: Vec<&Ident> = remove_fn_names.iter().collect();

        let mut retained = Vec::new();
        for item in input.items.drain(..) {
            if let ImplItem::Fn(ref fn_item) = item {
                let ident = &fn_item.sig.ident;
                if remove_names.iter().any(|n| *ident == **n) {
                    continue; // setup ctors are dropped entirely
                }
                if newtype_method_names.contains(ident) || helper_fn_names.contains(ident) {
                    // Registered methods and unannotated helpers alike move to
                    // the newtype impl.
                    newtype_items.push(item);
                    continue;
                }
            }
            retained.push(item);
        }
        input.items = retained;
    }

    let ctor_nargs: u32 = js_ctor_index.map_or(0, |i| methods[i].nargs);
    let ctor_nargs_method = quote! {
        fn nargs(&self) -> u32 {
            #ctor_nargs
        }
    };

    let has_js_ctor = js_ctor_name.is_some();
    let ctor_defined_method = quote! {
        fn defined(&self) -> bool {
            #has_js_ctor
        }
    };

    // Generate the ConstructorRegistrar impl (on FooImpl)
    let ctor_impl = if setup_ctor_info.is_some() {
        // Setup-style: return FooImpl::default(). The real constructor logic
        // runs in the PostInitRegistrar after the object is boxed and rooted.
        quote! {
            impl ::js::class::__ConstructorRegistrar<#inner_name> for ::js::class::__CtorReg<#inner_name> {
                fn construct(
                    &self,
                    _scope: &::js::gc::scope::Scope<'_>,
                    _args: &::js::native::CallArgs,
                ) -> ::std::result::Result<#inner_name, ::js::error::ExnThrown> {
                    Ok(#inner_name::default())
                }
                #ctor_nargs_method
                #ctor_defined_method
            }
        }
    } else if let Some(body) = constructor_body {
        quote! {
            impl ::js::class::__ConstructorRegistrar<#inner_name> for ::js::class::__CtorReg<#inner_name> {
                fn construct(
                    &self,
                    scope: &::js::gc::scope::Scope<'_>,
                    args: &::js::native::CallArgs,
                ) -> ::std::result::Result<#inner_name, ::js::error::ExnThrown> {
                    unsafe { #body }
                }
                #ctor_nargs_method
                #ctor_defined_method
            }
        }
    } else {
        quote! {
            impl ::js::class::__ConstructorRegistrar<#inner_name> for ::js::class::__CtorReg<#inner_name> {
                fn construct(
                    &self,
                    scope: &::js::gc::scope::Scope<'_>,
                    _args: &::js::native::CallArgs,
                ) -> ::std::result::Result<#inner_name, ::js::error::ExnThrown> {
                    Err(::js::error::throw_binding_error(
                        scope,
                        &::js::class::constructor_site::<#inner_name>(),
                        ::js::error::BindingError::IllegalConstructor,
                    ))
                }
                #ctor_nargs_method
                #ctor_defined_method
            }
        }
    };

    // Generate the MethodRegistrar impl (on FooImpl)
    let method_impl = quote! {
        impl ::js::class::__MethodRegistrar<#inner_name> for ::js::class::__MethodReg<#inner_name> {
            fn register(
                &self,
                builder: ::js::class::ClassBuilder<#inner_name>,
            ) -> ::js::class::ClassBuilder<#inner_name> {
                builder #(#builder_calls)*
            }
        }
    };

    // Generate the StaticMethodRegistrar impl (only if static methods exist)
    let static_method_impl = if !static_builder_calls.is_empty() {
        quote! {
            impl ::js::class::__StaticMethodRegistrar<#inner_name> for ::js::class::__StaticMethodReg<#inner_name> {
                fn register(
                    &self,
                    builder: ::js::class::ClassBuilder<#inner_name>,
                ) -> ::js::class::ClassBuilder<#inner_name> {
                    builder #(#static_builder_calls)*
                }
            }
        }
    } else {
        quote! {}
    };

    // Generate the ConstantRegistrar impl (only if constants exist)
    let constant_impl = if !constant_builder_calls.is_empty() {
        quote! {
            impl ::js::class::__ConstantRegistrar<#inner_name> for ::js::class::__ConstantReg<#inner_name> {
                fn register(
                    &self,
                    builder: ::js::class::ClassBuilder<#inner_name>,
                ) -> ::js::class::ClassBuilder<#inner_name> {
                    builder #(#constant_builder_calls)*
                }
            }
        }
    } else {
        quote! {}
    };

    // Generate the DestructorRegistrar impl
    let dtor_impl = if let Some(fn_name) = destructor_fn_name {
        let call = if destructor_takes_object {
            quote! { #inner_name::#fn_name(this, object); }
        } else {
            quote! { let _ = object; #inner_name::#fn_name(this); }
        };
        quote! {
            impl ::js::class::__DestructorRegistrar<#inner_name> for ::js::class::__DtorReg<#inner_name> {
                #[cfg_attr(crown, allow(crown::unrooted_must_root))]
                fn destruct(&self, this: &mut #inner_name, object: *mut ::js::native::JSObject) {
                    #call
                }
            }
        }
    } else {
        quote! {}
    };

    // Generate the PostInitRegistrar impl for a setup-style constructor: extract the
    // arguments and run the constructor body on the rooted newtype. Classes without one
    // fall back to the no-op registrar through autoref specialization.
    let post_init_impl = if let Some(idx) = setup_ctor_info {
        let info = &methods[idx];
        let setup_fn_name = setup_fn_ident(&info.fn_item.sig.ident);
        let arg_extractions = gen_arg_extractions(
            &info.params,
            quote!(args),
            true,
            quote!(scope),
            info.required,
        );
        let arg_names: Vec<_> = info.params.iter().map(|(name, _)| quote!(#name)).collect();
        let call = if info.has_cx {
            quote! { #type_name::#setup_fn_name(&__typed, scope, #(#arg_names),*) }
        } else {
            quote! { #type_name::#setup_fn_name(&__typed, #(#arg_names),*) }
        };
        let site = binding_site(class_interface(&inner_name), "constructor", "Constructor");
        quote! {
            #[allow(clippy::not_unsafe_ptr_arg_deref)]
            impl ::js::class::__PostInitRegistrar<#inner_name> for ::js::class::__PostInitReg<#inner_name> {
                fn post_init(
                    &self,
                    scope: &::js::gc::scope::Scope<'_>,
                    obj: ::js::Object<'_>,
                    args: &::js::native::CallArgs,
                ) -> ::std::result::Result<(), ::js::error::ExnThrown> {
                    #site
                    let __typed = obj.cast::<#type_name>().unwrap();
                    #(#arg_extractions)*
                    #call.map_err(|__e| ::js::error::ThrowException::throw(__e, scope))
                }
            }
        }
    } else {
        quote! {}
    };

    let add_to_global_fn = quote! {
        /// Register this class on a global object, making it available
        /// as a constructor in JavaScript.
        ///
        /// If the class is marked as hidden `#[{jsclass,webidl_interface}(hidden)]`, the
        /// constructor isn't installed on the global object, so it is not visible to content.
        pub fn add_to_global<'scope>(scope: &'scope ::js::gc::scope::Scope<'_>, global: ::js::Object<'scope>) {
            if <#inner_name as ::js::class::ClassDef>::HIDDEN {
                unsafe { ::js::class::register_hidden_class::<#inner_name>(scope); }
            } else {
                unsafe { ::js::class::register_class::<#inner_name>(scope, global); }
            }
        }

        /// This class's constructor in the current realm, or a `TypeError` if the class isn't
        /// registered in it.
        pub fn class_constructor<'scope>(
            scope: &'scope ::js::gc::scope::Scope<'_>,
        ) -> ::std::result::Result<::js::Object<'scope>, ::js::error::ExnThrown> {
            ::js::class::class_constructor::<#inner_name>(scope)
        }
    };

    // Generate `impl<'s> Foo<'s>` containing constructors and add_to_global().
    let mut ctor_items: Vec<proc_macro2::TokenStream> = Vec::new();
    for i in &ctor_indices {
        let method = &methods[*i];
        if method.has_self || method.has_mut_self {
            ctor_items.push(gen_setup_factory(method, &type_name, &inner_name));
        } else if !method.is_raw {
            // A raw (`&CallArgs`) constructor is callable only from a JSNative
            // wrapper, so it gets no Rust-side factory.
            ctor_items.push(gen_instance_factory(
                &method.fn_item.sig.ident,
                method,
                &type_name,
                &inner_name,
            ));
        }
    }

    let ctor_new_impl = quote! {
        impl<'s> #type_name<'s> {
            #(#ctor_items)*
            #add_to_global_fn
        }
    };

    // Generate forwarding methods on Foo<'s> for InstanceValue methods
    // (which stay on FooImpl). All other instance methods are now directly
    // on the newtype via `newtype_items`.
    let mut forwarding_methods: Vec<proc_macro2::TokenStream> = Vec::new();

    for method in &methods {
        match &method.kind {
            MethodKind::Method { .. }
                if matches!(method.return_style, ReturnStyle::InstanceValue) =>
            {
                if !method.has_self && !method.has_mut_self {
                    continue;
                }

                let fn_name = &method.fn_item.sig.ident;
                let fn_generics = &method.fn_item.sig.generics;
                let param_decls: Vec<_> = method
                    .params
                    .iter()
                    .map(|(name, ty)| quote! { #name: #ty })
                    .collect();
                let param_names: Vec<_> = method
                    .params
                    .iter()
                    .map(|(name, _)| quote! { #name })
                    .collect();

                // `data()`/`data_mut()` return borrow guards; reborrow through
                // them when passing the receiver to the inner method.
                let (get_inner, inner_arg) = if method.has_mut_self {
                    (
                        quote! { let mut inner = self.data_mut(); },
                        quote! { &mut *inner },
                    )
                } else {
                    (quote! { let inner = self.data(); }, quote! { &*inner })
                };

                // InstanceValue: always needs a scope to create the JS object.
                let cx_param = quote! { scope: &'s ::js::gc::scope::Scope<'_>, };
                let cx_arg = if method.has_cx {
                    quote! { scope, }
                } else {
                    quote! {}
                };

                forwarding_methods.push(quote! {
                    pub fn #fn_name #fn_generics (&self, #cx_param #(#param_decls),*)
                        -> ::std::result::Result<#type_name<'s>, ::js::error::ExnThrown>
                    {
                        #get_inner
                        unsafe {
                            let __typed = ::js::class::create_instance_with::<#inner_name>(scope, |_| {
                                #inner_name::#fn_name(#inner_arg, #cx_arg #(#param_names),*)
                            })?;
                            Ok(__typed)
                        }
                    }
                });
            }
            _ => continue,
        }
    }

    // Generate the UnforgeableRegistrar impl when the interface has
    // `#[getter(unforgeable)]` accessors (e.g. Event.isTrusted). These are
    // defined on the class's template object by `ClassDef::define_unforgeable`.
    let unforgeable_impl = if unforgeable_getters.is_empty() {
        quote! {}
    } else {
        let installs: Vec<_> = unforgeable_getters
            .iter()
            .map(|(js_name, native_name)| {
                let name_bytes = format!("{js_name}\0");
                let name_cstr = proc_macro2::Literal::byte_string(name_bytes.as_bytes());
                quote! {
                    ::js::class::define_unforgeable_accessor(
                        scope,
                        obj,
                        unsafe { ::std::ffi::CStr::from_bytes_with_nul_unchecked(#name_cstr) },
                        Some(#native_name),
                    )?;
                }
            })
            .collect();
        quote! {
            impl ::js::class::__UnforgeableRegistrar<#inner_name>
                for ::js::class::__UnforgeableReg<#inner_name>
            {
                fn has(&self) -> bool {
                    true
                }
                fn install(
                    &self,
                    scope: &::js::gc::scope::Scope<'_>,
                    obj: ::js::Object<'_>,
                ) -> ::std::result::Result<(), ::js::error::ExnThrown> {
                    #(#installs)*
                    Ok(())
                }
            }
        }
    };

    // Emit stack newtype methods in a separate `impl<'s> Foo<'s>` block.
    // These methods receive `self` as the stack newtype, giving direct
    // access to the rooted JS object handle.
    let newtype_impl = if !newtype_items.is_empty() || !forwarding_methods.is_empty() {
        quote! {
            #[allow(clippy::inherent_to_string)]
            impl<'s> #type_name<'s> {
                #(#newtype_items)*
                #(#forwarding_methods)*
            }
        }
    } else if !forwarding_methods.is_empty() {
        quote! {
            impl<'s> #type_name<'s> {
                #(#forwarding_methods)*
            }
        }
    } else {
        quote! {}
    };

    let output = quote! {
        #input

        // Generated JSNative wrapper functions
        #(#native_fns)*

        // Generated constructor registrar
        #ctor_impl

        // Generated method registrar
        #method_impl

        // Generated static method registrar
        #static_method_impl

        // Generated constant registrar
        #constant_impl

        // Generated destructor registrar
        #dtor_impl

        // Generated post-init registrar
        #post_init_impl

        // Generated unforgeable-accessor registrar
        #unforgeable_impl

        // Generated Rust-side instantiation functions + add_to_global on stack newtype
        #ctor_new_impl

        // Generated newtype instance methods + forwarding methods
        #newtype_impl
    };

    output.into()
}

// ============================================================================
// Method analysis
// ============================================================================

fn parse_method_info(
    fn_item: ImplItemFn,
    mut kind: MethodKind,
    custom_rename: Option<String>,
    custom_nargs: Option<usize>,
    type_name: &Ident,
) -> MethodInfo {
    let method_name = unraw(&fn_item.sig.ident);

    // Determine self receiver
    let has_self = fn_item
        .sig
        .inputs
        .first()
        .map(|a| matches!(a, FnArg::Receiver(r) if r.mutability.is_none()))
        .unwrap_or(false);
    let has_mut_self = fn_item
        .sig
        .inputs
        .first()
        .map(|a| matches!(a, FnArg::Receiver(r) if r.mutability.is_some()))
        .unwrap_or(false);

    // Collect non-self parameters, detecting cx and raw params
    let mut params = Vec::new();
    let mut required_args = 0;
    let mut has_cx = false;
    let mut is_raw = false;
    let mut has_rest_args = false;
    let mut rest_arg_name = None;
    let mut rest_inner_type = None;
    let skip_first = if has_self || has_mut_self { 1 } else { 0 };

    for arg in fn_item.sig.inputs.iter().skip(skip_first) {
        if let FnArg::Typed(pat_ty) = arg {
            if is_cx_param_type(&pat_ty.ty) {
                has_cx = true;
                continue;
            }
            if is_callargs_param_type(&pat_ty.ty) {
                is_raw = true;
                continue;
            }
            // Check for RestArgs marker type
            if is_rest_args_type(&pat_ty.ty) {
                if let Pat::Ident(pat_ident) = pat_ty.pat.as_ref() {
                    has_rest_args = true;
                    rest_arg_name = Some(pat_ident.ident.clone());
                    rest_inner_type = extract_rest_args_inner_type(&pat_ty.ty);
                }
                continue;
            }
            if let Pat::Ident(pat_ident) = pat_ty.pat.as_ref() {
                params.push((pat_ident.ident.clone(), (*pat_ty.ty).clone()));
                if !is_option_type(&pat_ty.ty) {
                    required_args += 1;
                }
            }
        }
    }

    // Determine return style
    let is_constructor = matches!(kind, MethodKind::Constructor);
    let return_style =
        classify_return_style(&fn_item.sig.output, Some(type_name), is_constructor, is_raw);

    // Compute JS name: custom name overrides, otherwise default to camelCase.
    // For setters, derive the property name by stripping "set_" prefix.
    let js_name = custom_rename.unwrap_or_else(|| {
        if matches!(kind, MethodKind::Setter { .. }) {
            let stripped = method_name.strip_prefix("set_").unwrap_or(&method_name);
            stripped.to_lower_camel_case()
        } else {
            method_name.to_lower_camel_case()
        }
    });

    match &mut kind {
        MethodKind::Method { js_name: n } => {
            *n = js_name;
        }
        MethodKind::StaticMethod { js_name: n } => {
            *n = js_name;
        }
        MethodKind::Getter { js_name: n, .. } => {
            *n = js_name;
        }
        MethodKind::Setter { js_name: n } => {
            *n = js_name;
        }
        _ => {}
    }

    MethodInfo {
        kind,
        fn_item,
        params,
        nargs: custom_nargs.unwrap_or(required_args) as u32,
        required: custom_nargs.unwrap_or(required_args) as u32,
        return_style,
        has_self,
        has_mut_self,
        has_cx,
        is_raw,
        has_rest_args,
        rest_arg_name,
        rest_inner_type,
    }
}

/// Whether a method should live on the stack newtype (`Foo<'s>`) rather than
/// on `FooImpl`. Methods on the newtype receive `self` as the rooted stack
/// newtype, giving access to both the JS object and the private data.
fn is_method_on_newtype(method: &MethodInfo) -> bool {
    // Constructors, destructors, and static methods stay on FooImpl.
    if matches!(
        method.kind,
        MethodKind::Constructor | MethodKind::Destructor | MethodKind::StaticMethod { .. }
    ) {
        return false;
    }
    // Must be an instance method (has self receiver).
    if !method.has_self && !method.has_mut_self {
        return false;
    }
    // InstanceValue methods return Self (= FooImpl), so they stay on FooImpl.
    if matches!(method.return_style, ReturnStyle::InstanceValue) {
        return false;
    }
    // Methods named data/data_mut would conflict with the generated accessors
    // on the newtype.
    let name = method.fn_item.sig.ident.to_string();
    if matches!(name.as_str(), "data" | "data_mut") {
        return false;
    }
    true
}

fn last_segment_is(ty: &Type, name: &str) -> bool {
    let ty = match ty {
        Type::Reference(r) => &*r.elem,
        other => other,
    };
    matches!(ty, Type::Path(tp) if tp.path.segments.last().is_some_and(|s| s.ident == name))
}

/// The identifier's name with any raw-identifier prefix stripped
/// (`r#type` → `type`), so a method/function named with a keyword maps to the
/// intended JS name instead of e.g. `rType` after camel-casing.
fn unraw(ident: &Ident) -> String {
    let s = ident.to_string();
    s.strip_prefix("r#").map(str::to_owned).unwrap_or(s)
}

fn is_cx_param_type(ty: &Type) -> bool {
    last_segment_is(ty, "Scope") || last_segment_is(ty, "JSContext")
}

fn is_callargs_param_type(ty: &Type) -> bool {
    last_segment_is(ty, "CallArgs")
}

fn is_rest_args_type(ty: &Type) -> bool {
    let s = quote!(#ty).to_string();
    s == "RestArgs"
        || s.starts_with("RestArgs <")
        || s.starts_with("RestArgs<")
        || s.ends_with(":: RestArgs")
        || s.ends_with("::RestArgs")
        || s.contains(":: RestArgs <")
        || s.contains("::RestArgs<")
}

/// Extract the inner type from `RestArgs<T>`. Returns `None` for a bare
/// `RestArgs`.
fn extract_rest_args_inner_type(ty: &Type) -> Option<Type> {
    if let Type::Path(type_path) = ty {
        let last_seg = type_path.path.segments.last()?;
        let ident = last_seg.ident.to_string();
        if ident == "RestArgs" {
            if let syn::PathArguments::AngleBracketed(args) = &last_seg.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    return Some(inner.clone());
                }
            }
        }
    }
    None
}

/// The element type a `RestArgs<T>` collects into.
///
/// [`rewrite_rest_args_in_sig`] rejects both a bare `RestArgs` and an owned
/// `Value` element before any codegen runs, so every callable that reaches this
/// point declared a usable element type. A `None` here would mean that check was
/// bypassed, which is a bug in this macro rather than in the user's code.
/// (Changing the callsites to unwrap the `Option` would create more noise than
/// it's worth, so an unwrap here it is.)
fn rest_args_element_type(declared: Option<&Type>) -> Type {
    declared
        .cloned()
        .expect("bare `RestArgs` must be rejected by `rewrite_rest_args_in_sig` before codegen")
}

/// Rewrite every `RestArgs<T>` parameter in `sig` to the fully-qualified
/// `::js::class::RestArgs<T>`, and reject the element types that can't work:
/// a missing one (bare `RestArgs`) and the GC-unsafe owned `Value`.
///
/// Shared by `#[jsmethods]` and by the `mod`-block macros (`#[jsmodule]`,
/// `#[jsglobals]`, `#[jsnamespace]`, `#[webidl_namespace]`).
///
/// Errors are spanned on the name of the function or method whose signature this
/// is, because the parameter type itself has usually already been replaced by
/// the time a caller reports one.
fn rewrite_rest_args_in_sig(sig: &mut syn::Signature, name: &Ident) -> syn::Result<()> {
    let declared = sig.inputs.iter().find_map(|arg| match arg {
        FnArg::Typed(pat_ty) if is_rest_args_type(&pat_ty.ty) => {
            Some(extract_rest_args_inner_type(&pat_ty.ty))
        }
        _ => None,
    });
    let Some(declared) = declared else {
        return Ok(());
    };

    // `&CallArgs` already exposes every argument, including the ones a tail
    // would collect, and the raw call path passes it instead of the extracted
    // parameters, so a `RestArgs` alongside it would be silently dropped.
    if sig
        .inputs
        .iter()
        .any(|arg| matches!(arg, FnArg::Typed(pat_ty) if is_callargs_param_type(&pat_ty.ty)))
    {
        return Err(syn::Error::new_spanned(
            name,
            "`RestArgs` cannot be combined with `&CallArgs`: `&CallArgs` already \
             provides every argument. Use one or the other.",
        ));
    }

    let Some(inner_ty) = declared else {
        return Err(syn::Error::new_spanned(
            name,
            "`RestArgs` needs an element type. Use `RestArgs<HandleValue<'_>>` to \
             collect untyped arguments, or name a converted type such as \
             `RestArgs<f64>`.",
        ));
    };

    if last_segment_is(&inner_ty, "Value") {
        return Err(syn::Error::new_spanned(
            name,
            "`RestArgs<Value>` is not GC-safe. Use `RestArgs<HandleValue<'_>>` or \
             `&CallArgs` instead.",
        ));
    }

    for arg in sig.inputs.iter_mut() {
        if let FnArg::Typed(pat_ty) = arg {
            if is_rest_args_type(&pat_ty.ty) {
                *pat_ty.ty = syn::parse_quote! { ::js::class::RestArgs<#inner_ty> };
            }
        }
    }
    Ok(())
}

/// Emit the statement that collects a variadic tail into the `RestArgs<T>` bound
/// to `rest_name`, converting each argument from index `start_idx` onwards with
/// `FromJSVal`. Returns an empty token stream when the callable doesn't use rest
/// args.
///
/// Takes the same context parameters as [`gen_arg_extractions`], and with the
/// same meaning: a JSNative trampoline names its argument vector `__args` and
/// owns its `scope`, while a constructor registrar names it `args` and receives
/// `scope` by reference; `use_question_mark` picks the early return that
/// context needs (`Err(ExnThrown)` for the registrar, whose body is a `Result`,
/// versus `false` for the trampoline, which returns `bool` to SpiderMonkey).
fn gen_rest_setup(
    rest_name: Option<&Ident>,
    rest_inner_type: Option<&Type>,
    start_idx: usize,
    args_expr: &proc_macro2::TokenStream,
    use_question_mark: bool,
    scope_expr: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let Some(rest_name) = rest_name else {
        return quote! {};
    };
    let start_idx = start_idx as u32;
    let inner_ty = rest_args_element_type(rest_inner_type);
    // Integer element types (and integer-element containers) convert with
    // `ConversionBehavior`, everything else uses `()`. Mirrors the config
    // selection in `gen_arg_extractions` for ordinary parameters.
    let rest_config = if is_integer_type(&inner_ty) || is_int_container_type(&inner_ty) {
        quote! { ::js::conversion::ConversionBehavior::Default }
    } else {
        quote! { () }
    };
    let fail = if use_question_mark {
        quote! { return Err(::js::error::ExnThrown); }
    } else {
        quote! { return false; }
    };
    quote! {
        let #rest_name = {
            let __argc: u32 = #args_expr.argc_;
            let mut __rest_vec = ::std::vec::Vec::with_capacity(
                (__argc.saturating_sub(#start_idx)) as usize,
            );
            for __i in #start_idx..__argc {
                let __handle = unsafe {
                    ::js::native::Handle::from_raw(#args_expr.get(__i))
                };
                match <#inner_ty as ::js::conversion::FromJSVal<'_, '_>>::from_jsval(
                    #scope_expr,
                    __handle,
                    #rest_config
                ) {
                    Ok(__v) => __rest_vec.push(__v),
                    Err(e) => {
                        if let ::js::conversion::ConversionError::Failure(reason) = e {
                            ::js::error::throw_binding_error(
                                #scope_expr,
                                &__SITE,
                                ::js::error::BindingError::InvalidArgument {
                                    index: __i,
                                    value: __handle,
                                    message: &*reason,
                                },
                            );
                        }
                        #fail
                    },
                }
            }
            ::js::class::RestArgs::new(__rest_vec)
        };
    }
}

/// Walk a `UseTree` to find the leaf `Ident` (e.g., `super::Vec2` → `Vec2`).
/// Collect the class identifiers a `pub use` brings into scope, for
/// registration on the global. `use A`, `use path::A`, `use path::A as B`
/// (registers the alias `B`, which is what's in scope), and `use path::{A, B}`
/// are all supported. A glob (`use path::*`) can't be enumerated, so its span
/// is returned as an error rather than silently registering nothing.
fn collect_use_class_idents(
    tree: &syn::UseTree,
    out: &mut Vec<Ident>,
) -> ::std::result::Result<(), proc_macro2::Span> {
    match tree {
        syn::UseTree::Name(name) => out.push(name.ident.clone()),
        syn::UseTree::Rename(rename) => out.push(rename.rename.clone()),
        syn::UseTree::Path(path) => collect_use_class_idents(&path.tree, out)?,
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_use_class_idents(item, out)?;
            }
        }
        syn::UseTree::Glob(glob) => return Err(glob.star_token.span),
    }
    Ok(())
}

fn is_promise_type(ty: &Type) -> bool {
    let Type::Path(tp) = ty else {
        return false;
    };
    tp.path
        .segments
        .last()
        .is_some_and(|s| s.ident == "Promise" || s.ident == "PromiseFuture")
}

/// Whether the return type is `Result<Promise<...>, _>`: a synchronous WebIDL
/// operation whose declared return is a promise. The `Ok` type is the first
/// generic argument of the `Result`; it counts as a promise when its final path
/// segment is `Promise`/`PromiseFuture` (the wrapper carries a lifetime argument,
/// e.g. `Promise<'r>`, which `is_promise_type`'s bare-identifier match rejects).
fn is_result_promise_type(ty: &Type) -> bool {
    let Type::Path(tp) = ty else {
        return false;
    };
    let Some(seg) = tp.path.segments.last() else {
        return false;
    };
    if seg.ident != "Result" {
        return false;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return false;
    };
    let Some(syn::GenericArgument::Type(ok_ty)) = args.args.first() else {
        return false;
    };
    is_promise_type(ok_ty)
}

/// The `T` of a `&T` parameter.
fn reference_target(ty: &Type) -> Option<&Type> {
    match ty {
        Type::Reference(r) if r.mutability.is_none() => Some(&r.elem),
        _ => None,
    }
}

/// How a `&T` parameter is converted, and how the parameter's own name is then
/// bound to a borrow of the converted value.
///
/// `FromJSVal` always produces an owned value, so the reference is taken from
/// a local the trampoline holds for the duration of the call. In most cases,
/// the local's type is `T`, but for `&str` it's `String`, and for `&[T]` it's
/// `Vec<T>`.
struct BorrowedParam {
    /// The type actually converted.
    convert_ty: Type,
    /// Whether reaching the referent from it goes through a `Deref`.
    deref: bool,
}

impl BorrowedParam {
    /// `None` for a parameter taken by value, which converts as itself and
    /// binds its own name directly.
    fn of(ty: &Type) -> Option<Self> {
        let referent = reference_target(ty)?;
        Some(match referent {
            Type::Slice(slice) => {
                let element = &slice.elem;
                Self {
                    convert_ty: syn::parse_quote!(::std::vec::Vec<#element>),
                    deref: true,
                }
            }
            Type::Path(tp) if tp.path.is_ident("str") => Self {
                convert_ty: syn::parse_quote!(::std::string::String),
                deref: true,
            },
            _ => Self {
                convert_ty: referent.clone(),
                deref: false,
            },
        })
    }

    /// Bind `name` to a borrow of the converted value in `value`.
    fn rebind(&self, name: &Ident, value: &Ident, optional: bool) -> proc_macro2::TokenStream {
        match (optional, self.deref) {
            (true, true) => quote! { let #name = ::std::option::Option::as_deref(&#value); },
            (true, false) => quote! { let #name = ::std::option::Option::as_ref(&#value); },
            (false, true) => quote! { let #name = &*#value; },
            (false, false) => quote! { let #name = &#value; },
        }
    }

    fn annotation(&self, optional: bool) -> proc_macro2::TokenStream {
        let convert_ty = &self.convert_ty;
        if optional {
            quote! { : ::std::option::Option<#convert_ty> }
        } else {
            quote! { : #convert_ty }
        }
    }
}

/// Whether an input is a WebIDL `Promise<T>` whose `T` needs no check, which
/// includes `any` and `undefined`.
fn is_unchecked_promise_type(ty: &Type) -> bool {
    last_segment_is(ty, "Promise")
}

/// Rewrite every lifetime in a type to `'_`.
///
/// A value type is lifted out of the method's signature into the trampoline,
/// where the method's own lifetime parameters aren't in scope: `PromiseOf<'r,
/// Payload<'r>>` would emit a `Payload<'r>` naming an undeclared `'r`. Every
/// use site of the rewritten type is a position where `'_` means "infer", and
/// what it infers to is the scope the value is rooted in — the same scope the
/// method's lifetime was bound to.
fn elide_lifetimes(ty: &Type) -> Type {
    struct Elide;
    impl syn::visit_mut::VisitMut for Elide {
        fn visit_lifetime_mut(&mut self, lifetime: &mut syn::Lifetime) {
            *lifetime = syn::Lifetime::new("'_", lifetime.apostrophe);
        }
    }
    let mut ty = ty.clone();
    syn::visit_mut::VisitMut::visit_type_mut(&mut Elide, &mut ty);
    ty
}

/// How to name a type in a message to script: the last path segment's
/// identifier, which for a rooted handle (`web_fetch::response::Response<'_>`)
/// is the interface name script knows it by. Anything not a path falls back to
/// its source text.
fn type_display_name(ty: &Type) -> String {
    match ty {
        Type::Path(tp) => tp.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
    .unwrap_or_else(|| quote!(#ty).to_string())
}

/// The `T` of a `PromiseOf<'_, T>` input — a WebIDL `Promise<T>` whose eventual
/// value is checked against `T`. `T` is the first type argument; the leading
/// lifetime is the handle's own.
fn promise_value_type(ty: &Type) -> Option<&Type> {
    let Type::Path(tp) = ty else {
        return None;
    };
    let seg = tp.path.segments.last()?;
    if seg.ident != "PromiseOf" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    })
}

/// The fulfillment reaction that checks a `PromiseOf<'_, T>`'s value against
/// `T`, for the two places WebIDL converts an input to `Promise<T>`: an
/// operation's argument and a dictionary member.
fn promise_value_check(value_ty: &Type) -> proc_macro2::TokenStream {
    let type_name = type_display_name(value_ty);
    let value_ty = elide_lifetimes(value_ty);
    quote! {
        |__scope, __args, _| {
            ::js::promise::check_promise_value_type::<#value_ty>(__scope, __args.get(0), #type_name)
        }
    }
}

fn is_integer_type(ty: &Type) -> bool {
    let s = quote!(#ty).to_string();
    matches!(
        s.as_str(),
        "i8" | "i16" | "i32" | "i64" | "u8" | "u16" | "u32" | "u64" | "isize" | "usize"
    )
}

/// Whether a type is a primitive WebIDL type whose `null` value should be
/// converted through the normal path rather than treated as absent.
/// Detect `Vec<T>` types (WebIDL sequence parameters).
fn is_vec_type(ty: &Type) -> bool {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            return seg.ident == "Vec"
                && matches!(seg.arguments, syn::PathArguments::AngleBracketed(_));
        }
    }
    false
}

/// Extract the inner type from `Vec<T>`.
fn extract_vec_inner_type(ty: &Type) -> Option<Type> {
    if let Type::Path(type_path) = ty {
        let last_seg = type_path.path.segments.last()?;
        if last_seg.ident == "Vec" {
            if let syn::PathArguments::AngleBracketed(args) = &last_seg.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    return Some(inner.clone());
                }
            }
        }
    }
    None
}

/// Detect `Record<K, V>` types (WebIDL record parameters).
fn is_record_type(ty: &Type) -> bool {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            return seg.ident == "Record"
                && matches!(seg.arguments, syn::PathArguments::AngleBracketed(_));
        }
    }
    false
}

/// Extract the value type from `Record<K, V>` (the last type argument).
fn extract_record_inner_type(ty: &Type) -> Option<Type> {
    if let Type::Path(type_path) = ty {
        let last_seg = type_path.path.segments.last()?;
        if last_seg.ident == "Record" {
            if let syn::PathArguments::AngleBracketed(args) = &last_seg.arguments {
                return args.args.iter().rev().find_map(|arg| match arg {
                    syn::GenericArgument::Type(inner) => Some(inner.clone()),
                    _ => None,
                });
            }
        }
    }
    None
}

/// Check whether a container type (`Vec<T>` or `Record<T>`) has an integer
/// inner type, requiring `ConversionBehavior` config instead of `()`.
fn is_int_container_type(ty: &Type) -> bool {
    if is_vec_type(ty) {
        extract_vec_inner_type(ty).is_some_and(|inner| is_integer_type(&inner))
    } else if is_record_type(ty) {
        extract_record_inner_type(ty).is_some_and(|inner| is_integer_type(&inner))
    } else {
        false
    }
}

/// Detect `Option<T>` types for optional parameter handling.
fn is_option_type(ty: &Type) -> bool {
    if let Type::Path(tp) = ty {
        if let Some(seg) = tp.path.segments.last() {
            return seg.ident == "Option"
                && matches!(seg.arguments, syn::PathArguments::AngleBracketed(_));
        }
    }
    false
}

/// JS-visible `.length` for a free function exported by `#[jsmodule]`,
/// `#[jsglobals]`, `#[webidl_methods]`, or a namespace: the number of required arguments, i.e.
/// parameters that aren't `Option<T>`.
fn required_arg_count(params: &[(Ident, Type)]) -> u32 {
    params.iter().filter(|(_, ty)| !is_option_type(ty)).count() as u32
}

/// Check whether a type is a none-optional `Heap<T>`.
fn is_bare_heap_type(ty: &Type) -> bool {
    if let Type::Path(tp) = ty {
        // Only match single-segment `Heap<T>` (our js::gc::handle::Heap),
        // not multi-segment paths like `js::gc::handle::MozHeap<Value>` (mozjs Heap).
        if tp.path.segments.len() == 1 {
            if let Some(seg) = tp.path.segments.last() {
                return seg.ident == "Heap"
                    && matches!(seg.arguments, syn::PathArguments::AngleBracketed(_));
            }
        }
    }
    false
}

/// Extract the inner type from `Option<T>`.
fn extract_option_inner_type(ty: &Type) -> Option<Type> {
    if let Type::Path(type_path) = ty {
        let last_seg = type_path.path.segments.last()?;
        if last_seg.ident == "Option" {
            if let syn::PathArguments::AngleBracketed(args) = &last_seg.arguments {
                if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                    return Some(inner.clone());
                }
            }
        }
    }
    None
}

/// Extract the error type `E` from a `Result<T, E>` return type.
fn extract_result_error_type(output: &ReturnType) -> Option<Type> {
    if let ReturnType::Type(_, ty) = output {
        if let Type::Path(tp) = ty.as_ref() {
            if let Some(seg) = tp.path.segments.last() {
                if seg.ident == "Result" {
                    if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                        if let Some(syn::GenericArgument::Type(err_ty)) = args.args.iter().nth(1) {
                            return Some(err_ty.clone());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Detect stack newtype types like `Item<'_>` or `Item<'s>`.
///
/// Stack newtypes are generated by `#[jsclass]` and always carry a single
/// Check if a type string is exactly `Result < () , ::js::error::ExnThrown >`
fn is_result_unit_jserror(ty_str: &str) -> bool {
    let normalized: String = ty_str.chars().filter(|c| !c.is_whitespace()).collect();
    normalized.starts_with("Result<()") && normalized.ends_with("ExnThrown>")
}

/// Check if a type string is a `Result<T, E>` type.
/// Returns `Some(true)` if Result has a non-() Ok type, `Some(false)` if Ok is ().
/// Returns `None` if not a Result type.
fn is_result_type(ty_str: &str) -> Option<bool> {
    let normalized: String = ty_str.chars().filter(|c| !c.is_whitespace()).collect();
    result_ok_type(&normalized).map(|ok_type| ok_type != "()")
}

/// Extract the Ok type from a whitespace-normalized `Result<T, E>` string.
/// Returns `None` if the string is not a two-parameter `Result`.
fn result_ok_type(normalized: &str) -> Option<&str> {
    let inner = normalized.strip_prefix("Result<")?.strip_suffix('>')?;
    // Find the Ok type (before the first comma at depth 0)
    let mut depth = 0;
    for (i, c) in inner.char_indices() {
        match c {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => return Some(&inner[..i]),
            _ => {}
        }
    }
    None
}

/// Whether an unannotated `fn` in a `#[jsmethods]` block is one of the class's
/// Rust-only instantiation functions. Such a fn is not exposed to JS, but gets
/// a wrapper with the same name installed on the stack newtype that allocates
/// a JS object around the data it produces.
///
/// The receiver selects which of the two constructor shapes to look for:
///
/// - **No receiver returning `Self`, `Foo`, `Foo<'s>`, or `Result<Self, E>`**
///   — constructor-shaped regardless of name. The fallible form is detected
///   only for a literal `Self` Ok type: a fn returning `Result<Foo<'s>, E>`
///   is a helper that allocates the JS object itself and returns the rooted
///   newtype (whose lifetime `Self` could not express), e.g. built on top of
///   another factory.
/// - **`fn new` taking `&self` or `&mut self` and returning `Result<(), E>` or `()`**
///   — the setup-style shape, which runs on an already-allocated instance.
fn is_implicit_constructor(sig: &syn::Signature, type_name: &Ident) -> bool {
    if matches!(sig.inputs.first(), Some(FnArg::Receiver(_))) {
        return unraw(&sig.ident) == "new";
    }
    match &sig.output {
        ReturnType::Default => false,
        ReturnType::Type(_, ty) => {
            let ty_str = quote!(#ty).to_string();
            let normalized: String = ty_str.chars().filter(|c| !c.is_whitespace()).collect();
            match result_ok_type(&normalized) {
                Some(ok_type) => ok_type == "Self",
                None => is_self_or_instance_type(&normalized, &type_name.to_string()),
            }
        }
    }
}

/// Check if a type string is exactly `Self` or the class name (with optional
/// lifetime). Matches `Self`, `Foo`, `Foo<'s>`, `Foo<'_>` but not types where
/// the class name is nested inside wrappers like `Result<Option<Foo<'s>>, E>`.
fn is_self_or_instance_type(ty_str: &str, type_name: &str) -> bool {
    let normalized: String = ty_str.chars().filter(|c| !c.is_whitespace()).collect();
    if normalized == "Self" {
        return true;
    }
    // Match `TypeName` or `TypeName<'lifetime>`
    if normalized == type_name {
        return true;
    }
    if let Some(rest) = normalized.strip_prefix(type_name) {
        // Must be followed by `<'...>` and nothing else
        if rest.starts_with("<'") && rest.ends_with('>') && rest.matches('<').count() == 1 {
            return true;
        }
    }
    false
}

/// Classify the return type of a function into a `ReturnStyle`.
/// If `type_name` is provided and `is_constructor` is true, `Self` returns become `Void`
/// (constructors handle object creation separately). For non-constructor methods,
/// `Self` returns become `InstanceValue` so the macro auto-wraps them.
fn classify_return_style(
    output: &ReturnType,
    type_name: Option<&Ident>,
    is_constructor: bool,
    is_raw: bool,
) -> ReturnStyle {
    match output {
        ReturnType::Default => ReturnStyle::Void,
        ReturnType::Type(_, ty) => {
            let ty_str = quote!(#ty).to_string();
            if let Some(tn) = type_name {
                if is_self_or_instance_type(&ty_str, &tn.to_string()) {
                    if is_constructor {
                        return ReturnStyle::Void;
                    }
                    return ReturnStyle::InstanceValue;
                }
            }
            if is_promise_type(ty) {
                ReturnStyle::Promise
            } else if is_result_promise_type(ty) {
                ReturnStyle::ResultPromise
            } else if is_result_unit_jserror(&ty_str) {
                // A `Result<(), ExnThrown>` method that takes `&CallArgs` sets its
                // own return value (Raw); otherwise the `Ok(())` value is the
                // undefined return, like any other `Result<(), E>`.
                if is_raw {
                    ReturnStyle::Raw
                } else {
                    ReturnStyle::ResultVoid
                }
            } else if let Some(has_inner_value) = is_result_type(&ty_str) {
                if has_inner_value {
                    ReturnStyle::ResultValue
                } else {
                    ReturnStyle::ResultVoid
                }
            } else {
                ReturnStyle::Value
            }
        }
    }
}

// ============================================================================
// Code generation
// ============================================================================

/// Build a C-string literal token from `s`, for a JS-visible name or an error
/// message passed across the C API. Panics only if `s` contains an interior
/// NUL, which never happens for the identifiers and fixed messages used here.
fn cstr_literal(s: &str) -> proc_macro2::Literal {
    let cstr = std::ffi::CString::new(s).expect("string for a C literal must not contain NUL");
    proc_macro2::Literal::c_string(&cstr)
}

/// The `__SITE` constant that the binding code of a member passes to the argument extractors and
/// brand checks: a `::js::error::BindingSite` with the class name `interface` (an expression),
/// the member name `member` and the `::js::error::MemberKind` variant `kind`. Every block that
/// uses [`gen_arg_extractions`], [`gen_rest_setup`] or a `get_this*` call starts with one.
fn binding_site(
    interface: proc_macro2::TokenStream,
    member: &str,
    kind: &str,
) -> proc_macro2::TokenStream {
    let kind = format_ident!("{kind}");
    quote! {
        #[allow(dead_code)]
        const __SITE: ::js::error::BindingSite = ::js::error::BindingSite {
            interface: #interface,
            member: #member,
            kind: ::js::error::MemberKind::#kind,
        };
    }
}

/// The interface expression of [`binding_site`] for the class whose private data is `type_name`.
fn class_interface(type_name: &Ident) -> proc_macro2::TokenStream {
    quote! { <#type_name as ::js::class::ClassDef>::NAME }
}

/// Generate argument extraction code for a list of typed parameters.
///
/// When `use_question_mark` is true, extraction errors propagate via `?`;
/// otherwise they `return false` (for use inside JSNative wrappers).
///
/// `required` is the number of leading parameters a call must pass: the method's argument count
/// (its JS `.length`) for WebIDL members, and 0 for Node.js functions. A parameter at or beyond
/// that index is extracted as optional: a missing argument converts as `undefined` instead of
/// throwing "Not enough arguments". This lets `#[method(length = N)]` make trailing `any`
/// params optional without wrapping them in `Option<…>`, matching how WebIDL
/// `optional any` defaults to `undefined`.
///
/// A call without one of the leading parameters that must be passed throws the missing argument's
/// error before any argument is converted, as WebIDL's overload resolution checks the number of
/// arguments first.
fn gen_arg_extractions(
    params: &[(Ident, Type)],
    args_expr: proc_macro2::TokenStream,
    use_question_mark: bool,
    scope_expr: proc_macro2::TokenStream,
    required: u32,
) -> Vec<proc_macro2::TokenStream> {
    let must_pass =
        |(i, (_, ty)): &(usize, &(Ident, Type))| !is_option_type(ty) && (*i as u32) < required;
    let leading = params.iter().enumerate().take_while(must_pass).count() as u32;
    let arity_check = (leading > 0).then(|| {
        let fail = if use_question_mark {
            quote! { return Err(__e); }
        } else {
            quote! { let ::js::error::ExnThrown = __e; return false; }
        };
        quote! {
            if (#args_expr).argc_ < #leading {
                let __argc = (#args_expr).argc_;
                let __e = ::js::error::throw_binding_error(
                    #scope_expr,
                    &__SITE,
                    ::js::error::BindingError::MissingArgument { index: __argc, given: __argc },
                );
                #fail
            }
        }
    });
    let extractions = params.iter().enumerate().map(|(i, (name, ty))| {
        let idx = i as u32;

        let optional = is_option_type(ty);
        let inner =
            optional.then(|| extract_option_inner_type(ty).expect("Option<T> has an inner type"));
        let declared = inner.as_ref().unwrap_or(ty);
        let borrow = BorrowedParam::of(declared);
        let convert_ty = borrow.as_ref().map_or(declared, |b| &b.convert_ty);
        let hidden = format_ident!("__arg_owned_{}", idx);
        let value = if borrow.is_some() { &hidden } else { name };
        let annotation = borrow.as_ref().map(|b| b.annotation(optional));
        let rebind = borrow.as_ref().map(|b| b.rebind(name, value, optional));

        if optional {
            let inner_extract =
                gen_typed_arg_getter(convert_ty, &scope_expr, &args_expr, idx, true);
            let fail = if use_question_mark {
                quote! { return Err(::js::error::ExnThrown); }
            } else {
                quote! { return false; }
            };

            // Per WebIDL, only a missing argument or `undefined` maps to `None` for an
            // `optional` argument without a default; `null` is a present value and is converted
            // through the normal path. For primitives that means e.g. `null` → "null" / `0`, for a
            // dictionary the conversion itself treats `null` as an empty dictionary, and for
            // a union or sequence (e.g. `HeadersInit`) converting `null` throws.
            return quote! {
                let __val = #args_expr.get(#idx);
                let #value #annotation = if __val.is_undefined() {
                    None
                } else {
                    match #inner_extract {
                        Ok(__v) => Some(__v),
                        Err(::js::error::ExnThrown) => { #fail }
                    }
                };
                #rebind
            };
        }

        let extract =
            gen_typed_arg_getter(convert_ty, &scope_expr, &args_expr, idx, idx < required);
        if use_question_mark {
            quote! {
                let #value #annotation = #extract?;
                #rebind
            }
        } else {
            quote! {
                let #value #annotation = match #extract {
                    Ok(v) => v,
                    Err(::js::error::ExnThrown) => return false,
                };
                #rebind
            }
        }
    });
    arity_check.into_iter().chain(extractions).collect()
}

/// How an input of a given type is converted from JS.
enum InputConversion<'a> {
    /// WebIDL `Promise<T>`, doing a type check on the result of awaiting
    /// `Promise.resolve(value)` instead of on `value` itself.
    /// See `js::PromiseOf`.
    CheckedPromise(&'a Type),
    /// `Promise<any>`/`Promise<undefined>`, wrapped in `Promise.resolve(value)
    /// as above, but without a type check.
    UncheckedPromise,
    /// An integer, whose conversion needs an out-of-range behavior.
    Integer,
    /// A container of integers, which forwards that behavior to its elements.
    IntegerContainer,
    /// Everything else, converted through `FromJSVal` with no configuration.
    Plain,
}

impl<'a> InputConversion<'a> {
    fn of(ty: &'a Type) -> Self {
        if let Some(value_ty) = promise_value_type(ty) {
            Self::CheckedPromise(value_ty)
        } else if is_unchecked_promise_type(ty) {
            Self::UncheckedPromise
        } else if is_integer_type(ty) {
            Self::Integer
        } else if is_int_container_type(ty) {
            Self::IntegerContainer
        } else {
            Self::Plain
        }
    }
}

/// Build the `get_*` extraction expression for a single typed argument, one per
/// [`InputConversion`]. Each yields a `Result<T, ExnThrown>`. A missing argument
/// throws "Not enough arguments" if `required` is set, and converts as
/// `undefined` otherwise.
fn gen_typed_arg_getter(
    ty: &Type,
    scope_expr: &proc_macro2::TokenStream,
    args_expr: &proc_macro2::TokenStream,
    idx: u32,
    required: bool,
) -> proc_macro2::TokenStream {
    match InputConversion::of(ty) {
        InputConversion::CheckedPromise(value_ty) => {
            let check = promise_value_check(value_ty);
            quote! {
                unsafe { ::js::class::get_checked_promise_arg(#scope_expr, &__SITE, #args_expr, #idx, #required, #check) }
            }
        }
        InputConversion::UncheckedPromise => quote! {
            unsafe { ::js::class::get_unchecked_promise_arg(#scope_expr, &__SITE, #args_expr, #idx, #required) }
        },
        InputConversion::Integer => quote! {
            unsafe { ::js::class::get_int_arg(#scope_expr, &__SITE, #args_expr, #idx, #required,
                ::js::conversion::ConversionBehavior::Default) }
        },
        InputConversion::IntegerContainer => quote! {
            unsafe { ::js::class::get_arg_with_config::<#ty>(#scope_expr, &__SITE, #args_expr, #idx,
                #required, ::js::conversion::ConversionBehavior::Default) }
        },
        InputConversion::Plain if required => {
            quote! { unsafe { ::js::class::get_arg(#scope_expr, &__SITE, #args_expr, #idx) } }
        }
        InputConversion::Plain => {
            quote! { unsafe { ::js::class::get_arg_or_undefined(#scope_expr, &__SITE, #args_expr, #idx) } }
        }
    }
}

/// The declarations and names of a constructor's parameters, with a trailing
/// `RestArgs<T>` appended when the constructor is variadic.
fn ctor_param_lists(
    info: &MethodInfo,
) -> (Vec<proc_macro2::TokenStream>, Vec<proc_macro2::TokenStream>) {
    let mut decls: Vec<_> = info
        .params
        .iter()
        .map(|(name, ty)| quote! { #name: #ty })
        .collect();
    let mut names: Vec<_> = info
        .params
        .iter()
        .map(|(name, _)| quote! { #name })
        .collect();
    if let Some(rest_name) = info.rest_arg_name.as_ref() {
        let inner_ty = rest_args_element_type(info.rest_inner_type.as_ref());
        decls.push(quote! { #rest_name: ::js::class::RestArgs<#inner_ty> });
        names.push(quote! { #rest_name });
    }
    (decls, names)
}

/// The call to an old-style constructor fn on the inner data struct, which
/// yields the `FooImpl` value to wrap in a fresh JS object.
fn ctor_call(
    info: &MethodInfo,
    inner_name: &Ident,
    param_names: &[proc_macro2::TokenStream],
) -> proc_macro2::TokenStream {
    let ctor_fn = &info.fn_item.sig.ident;
    if info.has_cx {
        quote! { #inner_name::#ctor_fn(scope, #(#param_names),*) }
    } else {
        quote! { #inner_name::#ctor_fn(#(#param_names),*) }
    }
}

/// The `__ctor_setup_*` name a setup-style constructor's body is moved to on
/// the stack newtype.
fn setup_fn_ident(ident: &Ident) -> Ident {
    format_ident!("__ctor_setup_{}", unraw(ident))
}

/// A Rust-side instantiation function on the stack newtype for a setup-style
/// constructor (`&self` receiver): allocate a default-initialized JS object,
/// run the moved constructor body (`__ctor_setup_*`, emitted alongside) on the
/// rooted newtype.
fn gen_setup_factory(
    info: &MethodInfo,
    type_name: &Ident,
    inner_name: &Ident,
) -> proc_macro2::TokenStream {
    let factory_name = &info.fn_item.sig.ident;
    let setup_ident = setup_fn_ident(factory_name);
    let param_decls: Vec<_> = info
        .params
        .iter()
        .map(|(name, ty)| quote! { #name: #ty })
        .collect();
    let param_names: Vec<_> = info
        .params
        .iter()
        .map(|(name, _)| quote! { #name })
        .collect();

    // Always returns Result<Foo<'s>, ExnThrown> because create_instance_with
    // allocates a JS object, which is fallible.
    let err_ty = extract_result_error_type(&info.fn_item.sig.output);
    let setup_call = if err_ty.is_some() {
        if info.has_cx {
            quote! { #type_name::#setup_ident(&__typed, scope, #(#param_names),*).map_err(|e| {
                ::js::error::ThrowException::throw(e, scope)
            })?; }
        } else {
            quote! { #type_name::#setup_ident(&__typed, #(#param_names),*).map_err(|e| {
                ::js::error::ThrowException::throw(e, scope)
            })?; }
        }
    } else if info.has_cx {
        quote! { #type_name::#setup_ident(&__typed, scope, #(#param_names),*); }
    } else {
        quote! { #type_name::#setup_ident(&__typed, #(#param_names),*); }
    };

    // Extract the constructor body and rename it.
    let mut setup_fn = info.fn_item.clone();
    setup_fn.sig.ident = setup_ident;
    setup_fn.vis = Visibility::Inherited; // private
    setup_fn.attrs.retain(|a| !a.path().is_ident("constructor"));

    quote! {
        /// Construct a new instance and return the stack newtype.
        pub fn #factory_name(scope: &'s ::js::gc::scope::Scope<'_>, #(#param_decls),*)
            -> ::std::result::Result<#type_name<'s>, ::js::error::ExnThrown>
        {
            unsafe {
                let __typed = ::js::class::create_instance_with::<#inner_name>(scope, |_| {
                    #inner_name::default()
                })?;
                #setup_call
                #[cfg(debug_assertions)]
                if let Some(__data) = ::js::class::get_private::<#inner_name>(__typed.0.as_raw()) {
                    ::js::class::ClassDef::debug_assert_fully_initialized(__data);
                }
                Ok(__typed)
            }
        }

        #setup_fn
    }
}

/// A Rust-side instantiation function on the stack newtype for an old-style
/// constructor (no receiver, returns `Self` or `Result<Self, E>`): build the
/// data and allocate the JS object around it. A fallible constructor's error
/// is thrown as a JS exception via `ThrowException`, as in the JS constructor
/// path.
fn gen_instance_factory(
    factory_name: &Ident,
    info: &MethodInfo,
    type_name: &Ident,
    inner_name: &Ident,
) -> proc_macro2::TokenStream {
    let (param_decls, param_names) = ctor_param_lists(info);
    let call = ctor_call(info, inner_name, &param_names);
    let create = if extract_result_error_type(&info.fn_item.sig.output).is_some() {
        quote! {
            ::js::class::try_create_instance_with::<#inner_name>(scope, |_| {
                #call.map_err(|e| ::js::error::ThrowException::throw(e, scope))
            })?
        }
    } else {
        quote! {
            ::js::class::create_instance_with::<#inner_name>(scope, |_| {
                #call
            })?
        }
    };
    quote! {
        /// Construct a new instance and return the stack newtype.
        pub fn #factory_name(scope: &'s ::js::gc::scope::Scope<'_>, #(#param_decls),*)
            -> ::std::result::Result<#type_name<'s>, ::js::error::ExnThrown>
        {
            unsafe {
                let __typed = #create;
                #[cfg(debug_assertions)]
                if let Some(__data) = ::js::class::get_private::<#inner_name>(__typed.0.as_raw()) {
                    ::js::class::ClassDef::debug_assert_fully_initialized(__data);
                }
                Ok(__typed)
            }
        }
    }
}

/// Generate the constructor body that extracts args and calls the Rust constructor fn.
fn gen_constructor_body(info: &MethodInfo, type_name: &Ident) -> proc_macro2::TokenStream {
    let ctor_fn = &info.fn_item.sig.ident;
    let arg_extractions = gen_arg_extractions(
        &info.params,
        quote!(args),
        true,
        quote!(scope),
        info.required,
    );
    let mut arg_names: Vec<_> = info.params.iter().map(|(name, _)| quote!(#name)).collect();

    let rest_setup = gen_rest_setup(
        info.rest_arg_name.as_ref(),
        info.rest_inner_type.as_ref(),
        info.params.len(),
        &quote!(args),
        true,
        &quote!(scope),
    );
    if let Some(rest_name) = info.rest_arg_name.as_ref() {
        arg_names.push(quote!(#rest_name));
    }

    // Build the constructor call, passing scope and/or args if the Rust
    // constructor requested them via `scope: &Scope<'_>` or `args: &CallArgs`.
    let call = if info.is_raw {
        quote! { #type_name::#ctor_fn(scope, args) }
    } else if info.has_cx {
        quote! { #type_name::#ctor_fn(scope, #(#arg_names),*) }
    } else {
        quote! { #type_name::#ctor_fn(#(#arg_names),*) }
    };

    // Constructors returning `Result<Self, E>` need error conversion:
    // the error is thrown as a JS exception via `ThrowException`.
    // In the construct function, `scope` is `&Scope<'_>` (not owned),
    // so we pass it directly to throw (unlike native functions which
    // own the scope and pass `&scope`).
    // `map_err` rather than a `match`: matching on `Ok(data)` would bind the
    // still-unrooted inner data on the stack, which crown rejects.
    let wrapped = match &info.return_style {
        ReturnStyle::ResultVoid | ReturnStyle::ResultValue => {
            quote! {
                #call.map_err(|e| ::js::error::ThrowException::throw(e, scope))
            }
        }
        _ => quote! { Ok(#call) },
    };

    let site = binding_site(class_interface(type_name), "constructor", "Constructor");
    quote! {
        #site
        #(#arg_extractions)*
        #rest_setup
        #wrapped
    }
}

/// Generate a JSNative wrapper function and the corresponding ClassBuilder call.
///
/// When `on_newtype` is true, the method lives on the stack newtype `Foo<'s>`.
/// The wrapper extracts `this` as the rooted newtype and calls the method
/// via method-call syntax.
/// Emit the complete JSNative trampoline (`extern "C" fn`) for a callable: a
/// method, a static method, or a free function exposed by a module, the global,
/// or a namespace. This is the single place where argument extraction, return-
/// value handling, and exception checking happen, so every JS-exposed function
/// behaves identically regardless of the macro that declared it.
///
/// The caller supplies the already-built `call` expression (which differs by
/// receiver and call path) plus the `this` extraction snippets; everything from
/// argument extraction through the return-style dispatch lives here. The
/// receiver is named `__args` internally to avoid clashing with user parameters.
///
/// - `this_extraction` runs before the body for receiver-taking callables (empty
///   for free functions, and for `ResultPromise` where it moves into the closure).
/// - `this_in_closure` is the `?`-style receiver extraction for `ResultPromise`.
/// - `rest_arg` names the variadic tail's parameter and element type, if there is
///   one. The collection is emitted here rather than by the caller so that it
///   lands in the same place as the fixed-argument extractions.
/// - `instance_type` is the class to mint for `InstanceValue`; `None` for free
///   functions, which `classify_return_style` never classifies as `InstanceValue`.
/// - `required` is the number of leading `any` parameters whose absence throws
///   "Not enough arguments". A missing `any` argument past them is `undefined`.
/// - `site` is the [`binding_site`] constant of the callable.
#[allow(clippy::too_many_arguments)]
fn emit_native_fn(
    native_name: &Ident,
    name_str: &str,
    site: &proc_macro2::TokenStream,
    return_style: &ReturnStyle,
    params: &[(Ident, Type)],
    required: u32,
    call: &proc_macro2::TokenStream,
    this_extraction: &proc_macro2::TokenStream,
    this_in_closure: &proc_macro2::TokenStream,
    rest_arg: Option<(&Ident, Option<&Type>)>,
    instance_type: Option<&Ident>,
) -> proc_macro2::TokenStream {
    let gen_rest = |use_question_mark: bool| {
        gen_rest_setup(
            rest_arg.map(|(name, _)| name),
            rest_arg.and_then(|(_, ty)| ty),
            params.len(),
            &quote!(__args),
            use_question_mark,
            &quote!(&scope),
        )
    };

    // For `ResultPromise` the argument extractions live inside the rejecting
    // closure built in the body (so a conversion failure becomes a rejected
    // promise, not a synchronous throw); nothing is emitted here.
    let is_result_promise = matches!(return_style, ReturnStyle::ResultPromise);
    let arg_extractions = if is_result_promise {
        Vec::new()
    } else {
        gen_arg_extractions(params, quote!(&__args), false, quote!(&scope), required)
    };
    let rest_setup = if is_result_promise {
        quote! {}
    } else {
        gen_rest(false)
    };

    let body = match return_style {
        ReturnStyle::Raw => quote! {
            match #call {
                Ok(()) => ::js::exception::check_fn_return(&scope, true, &#name_str),
                Err(::js::error::ExnThrown) => ::js::exception::check_fn_return(&scope, false, &#name_str),
            }
        },
        ReturnStyle::Value => quote! {
            let __result = #call;
            let __set_ok = ::js::class::set_return(&scope, &__args, &__result);
            ::js::exception::check_fn_return(&scope, __set_ok, &#name_str)
        },
        ReturnStyle::Void => quote! {
            #call;
            __args.rval().set(::js::value::undefined());
            ::js::exception::check_fn_return(&scope, true, &#name_str)
        },
        ReturnStyle::ResultVoid => quote! {
            match #call {
                Ok(()) => {
                    __args.rval().set(::js::value::undefined());
                    ::js::exception::check_fn_return(&scope, true, &#name_str)
                }
                Err(__e) => {
                    ::js::error::ThrowException::throw(__e, &scope);
                    ::js::exception::check_fn_return(&scope, false, &#name_str)
                }
            }
        },
        ReturnStyle::ResultValue => quote! {
            match #call {
                Ok(__v) => {
                    let __set_ok = ::js::class::set_return(&scope, &__args, &__v);
                    ::js::exception::check_fn_return(&scope, __set_ok, &#name_str)
                }
                Err(__e) => {
                    ::js::error::ThrowException::throw(__e, &scope);
                    ::js::exception::check_fn_return(&scope, false, &#name_str)
                }
            }
        },
        ReturnStyle::ResultPromise => {
            // Run the brand check, argument extraction, and the call inside a
            // closure so that any failure (a wrong-`this` brand check, a
            // missing/invalid argument, or an `Err` from the body) yields a
            // rejected promise instead of a synchronous throw — WebIDL §3.7.7
            // ("Operations") for a promise-returning operation. All three use the
            // `?`-propagating variant; `#call` returns the `Result`.
            let extractions =
                gen_arg_extractions(params, quote!(&__args), true, quote!(&scope), required);
            let rest_setup = gen_rest(true);
            quote! {
                let __result = (|| -> ::core::result::Result<_, ::js::error::ExnThrown> {
                    #this_in_closure
                    #(#extractions)*
                    #rest_setup
                    #call.map_err(|__e| ::js::error::ThrowException::throw(__e, &scope))
                })();
                match __result {
                    Ok(__v) => {
                        let __set_ok = ::js::class::set_return(&scope, &__args, &__v);
                        ::js::exception::check_fn_return(&scope, __set_ok, &#name_str)
                    }
                    Err(::js::error::ExnThrown) => {
                        // The pending exception is the conversion/body error;
                        // adopt it as the rejection reason.
                        match ::js::Promise::new_rejected_with_pending_error(&scope) {
                            Ok(__rejected) => {
                                __args.rval().set(unsafe {
                                    ::js::value::from_object(__rejected.as_raw())
                                });
                                ::js::exception::check_fn_return(&scope, true, &#name_str)
                            }
                            Err(_) => ::js::exception::check_fn_return(&scope, false, &#name_str),
                        }
                    }
                }
            }
        }
        ReturnStyle::Promise => quote! {
            // Create a bare JS Promise (no executor)
            let __promise = match ::js::Promise::new_pending(&scope) {
                Ok(p) => p,
                Err(_) => return ::js::exception::check_fn_return(&scope, false, &#name_str),
            };
            // Return the promise object to JS immediately
            __args.rval().set(unsafe { ::js::value::from_object(__promise.as_raw()) });
            // Call the user's function to get the PromiseFuture (containing the future)
            let __js_promise = #call;
            // Spawn the future, which will resolve/reject the promise later (driven by
            // the event loop via `js::promise::drive_pending_futures`).
            __promise.spawn(__js_promise);
            ::js::exception::check_fn_return(&scope, true, &#name_str)
        },
        ReturnStyle::InstanceValue => {
            let type_name =
                instance_type.expect("InstanceValue return style requires a class type");
            quote! {
                let __typed = match ::js::class::create_instance_with::<#type_name>(&scope, |_| {
                    #call
                }) {
                    Ok(o) => o,
                    Err(_) => return ::js::exception::check_fn_return(&scope, false, &#name_str),
                };
                // Assert full initialization, so an instance created by a JS method call
                // matches one built by the constructor or the Rust-side factory.
                #[cfg(debug_assertions)]
                if let Some(__data) = unsafe { ::js::class::get_private::<#type_name>(__typed.as_raw()) } {
                    ::js::class::ClassDef::debug_assert_fully_initialized(__data);
                }
                __args.rval().set(unsafe { ::js::value::from_object(__typed.as_raw()) });
                ::js::exception::check_fn_return(&scope, true, &#name_str)
            }
        }
    };

    quote! {
        #[allow(non_snake_case)]
        unsafe extern "C" fn #native_name(
            raw_cx: *mut ::js::native::RawJSContext,
            argc: u32,
            vp: *mut ::js::native::Value,
        ) -> bool {
            #site
            let scope = unsafe { ::js::gc::scope::RootScope::from_current_realm(raw_cx) };
            let __args = ::js::native::CallArgs::from_vp(vp, argc);
            #this_extraction
            #(#arg_extractions)*
            #rest_setup
            #body
        }
    }
}

fn gen_method_native(
    info: &MethodInfo,
    type_name: &Ident,
    struct_name: &Ident,
    js_name: &str,
    flags: u16,
    on_newtype: bool,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
    let fn_name = &info.fn_item.sig.ident;
    let native_name = format_ident!("__native_{type_name}_{}", unraw(fn_name));
    let name_str = format!("{}::{}", struct_name, fn_name);
    let nargs_u32 = info.nargs;

    // Create the C string literal for the JS name
    let js_name_cstr_lit = cstr_literal(js_name);

    // Use __args internally to avoid shadowing user's rest param names.
    //
    // The brand check (`this`-type validation) `get_this`s the receiver. For a
    // promise-returning operation (`ResultPromise`), a brand-check failure must
    // surface as a rejected promise, not a synchronous throw (WebIDL §3.7.7),
    // so it runs inside the rejecting closure (`this_in_closure`, `?`-style) and
    // nothing is emitted here. For every other return style it throws
    // synchronously before the body.
    let has_this = info.has_self || info.has_mut_self;
    let self_mut = if info.has_mut_self {
        quote! { mut }
    } else {
        quote! {}
    };
    let this_getter_expr = if on_newtype && has_this {
        quote! { ::js::class::get_this::<#struct_name<'_>>(&scope, &__SITE, &__args) }
    } else if info.has_self {
        quote! { ::js::class::get_this_data::<#type_name>(&scope, &__SITE, &__args) }
    } else if info.has_mut_self {
        quote! { ::js::class::get_this_data_mut::<#type_name>(&scope, &__SITE, &__args) }
    } else {
        quote! {}
    };
    let site = binding_site(
        class_interface(type_name),
        js_name,
        if has_this { "Method" } else { "StaticMethod" },
    );
    let is_result_promise = matches!(info.return_style, ReturnStyle::ResultPromise);
    let this_extraction = if !has_this || is_result_promise {
        quote! {}
    } else {
        quote! {
            let #self_mut __self = match #this_getter_expr {
                Ok(v) => v,
                Err(::js::error::ExnThrown) => {
                    return ::js::exception::check_fn_return(&scope, false, #name_str);
                },
            };
        }
    };
    let this_in_closure = if has_this && is_result_promise {
        quote! { let #self_mut __self = #this_getter_expr?; }
    } else {
        quote! {}
    };

    let call_args: Vec<_> = info.params.iter().map(|(name, _)| quote!(#name)).collect();
    let rest_arg = info
        .rest_arg_name
        .as_ref()
        .map(|name| (name, info.rest_inner_type.as_ref()));

    // Build rest arg value for method call
    let rest_arg_expr: Vec<proc_macro2::TokenStream> = if info.has_rest_args {
        let rest_name = info.rest_arg_name.as_ref().unwrap();
        vec![quote! { #rest_name }]
    } else {
        vec![]
    };

    // When `this` is the private data (not the newtype), `__self` is a borrow
    // guard; reborrow through it to pass the receiver to the inner method.
    let self_arg = if info.has_mut_self {
        quote! { &mut *__self }
    } else {
        quote! { &*__self }
    };
    let call = if on_newtype && (info.has_self || info.has_mut_self) {
        // Method lives on the stack newtype — use method-call syntax.
        if info.is_raw {
            quote! { __self.#fn_name(&scope, &__args) }
        } else if info.has_cx {
            let all_args: Vec<_> = call_args.iter().chain(rest_arg_expr.iter()).collect();
            quote! { __self.#fn_name(&scope, #(#all_args),*) }
        } else {
            let all_args: Vec<_> = call_args.iter().chain(rest_arg_expr.iter()).collect();
            quote! { __self.#fn_name(#(#all_args),*) }
        }
    } else if info.has_self || info.has_mut_self {
        if info.is_raw {
            quote! { #type_name::#fn_name(#self_arg, &scope, &__args) }
        } else if info.has_cx {
            let all_args: Vec<_> = call_args.iter().chain(rest_arg_expr.iter()).collect();
            quote! { #type_name::#fn_name(#self_arg, &scope, #(#all_args),*) }
        } else {
            let all_args: Vec<_> = call_args.iter().chain(rest_arg_expr.iter()).collect();
            quote! { #type_name::#fn_name(#self_arg, #(#all_args),*) }
        }
    } else if info.is_raw {
        quote! { #type_name::#fn_name(&scope, &__args) }
    } else if info.has_cx {
        let all_args: Vec<_> = call_args.iter().chain(rest_arg_expr.iter()).collect();
        quote! { #type_name::#fn_name(&scope, #(#all_args),*) }
    } else {
        let all_args: Vec<_> = call_args.iter().chain(rest_arg_expr.iter()).collect();
        quote! { #type_name::#fn_name(#(#all_args),*) }
    };
    let native_fn = emit_native_fn(
        &native_name,
        &name_str,
        &site,
        &info.return_style,
        &info.params,
        info.required,
        &call,
        &this_extraction,
        &this_in_closure,
        rest_arg,
        Some(type_name),
    );

    // Generate: .method(c"jsName", nargs, Some(native_fn), flags)
    // We need a &'static CStr. Use an unsafe trick with a byte string literal.
    let builder_call = quote! {
        .method(
            #js_name_cstr_lit,
            #nargs_u32,
            Some(#native_name),
            #flags,
        )
    };

    (native_fn, builder_call)
}

/// Generate a JSNative wrapper for a property getter or setter.
///
/// - Getter: `fn name(&self) -> T` — reads `this`, calls method, sets return value.
/// - Setter: `fn set_name(&mut self, val: T)` — reads `this` mutably, reads `args[0]`, calls method.
///
/// When `on_newtype` is true, the accessor lives on the stack newtype and the
/// wrapper extracts `this` as the rooted newtype rather than raw private data.
fn gen_accessor_native(
    info: &MethodInfo,
    type_name: &Ident,
    struct_name: &Ident,
    js_name: &str,
    is_getter: bool,
    on_newtype: bool,
) -> proc_macro2::TokenStream {
    let fn_name = &info.fn_item.sig.ident;
    let native_name = if is_getter {
        format_ident!("__getter_{type_name}_{}", unraw(fn_name))
    } else {
        format_ident!("__setter_{type_name}_{}", unraw(fn_name))
    };
    let name_str = format!("{}::{}", struct_name, fn_name);

    // A getter whose attribute type is a promise must reject on a brand-check
    // failure, not throw, per WebIDL §3.7.7 ("Attributes"). This covers both a
    // bare `-> Promise<'_>` and a fallible `-> ::std::result::Result<Promise<'_>, E>`.
    let is_promise_getter = is_getter
        && (matches!(&info.fn_item.sig.output, syn::ReturnType::Type(_, ty) if is_promise_type(ty))
            || matches!(info.return_style, ReturnStyle::ResultPromise));
    let brand_fail = if is_promise_getter {
        quote! {
            // Adopt the pending brand-check exception (a `TypeError`) as the
            // rejection reason; the getter still returns a value (the rejected
            // promise), so set `rval` and report success.
            return match ::js::Promise::new_rejected_with_pending_error(&scope) {
                Ok(__rejected) => {
                    __args.rval().set(unsafe { ::js::value::from_object(__rejected.as_raw()) });
                    ::js::exception::check_fn_return(&scope, true, #name_str)
                }
                Err(_) => ::js::exception::check_fn_return(&scope, false, #name_str),
            };
        }
    } else {
        quote! { return ::js::exception::check_fn_return(&scope, false, #name_str); }
    };
    let this_getter_expr = if on_newtype {
        quote! { ::js::class::get_this::<#struct_name<'_>>(&scope, &__SITE, &__args) }
    } else if is_getter {
        quote! { ::js::class::get_this_data::<#type_name>(&scope, &__SITE, &__args) }
    } else {
        quote! { ::js::class::get_this_data_mut::<#type_name>(&scope, &__SITE, &__args) }
    };
    let site = binding_site(
        class_interface(type_name),
        js_name,
        if is_getter { "Getter" } else { "Setter" },
    );
    // Getters take `&self`; setters take `&mut self`.
    let self_mut = if is_getter {
        quote! {}
    } else {
        quote! { mut }
    };
    let this_extraction = quote! {
        let #self_mut __self = match #this_getter_expr {
            Ok(v) => v,
            Err(::js::error::ExnThrown) => { #brand_fail }
        };
    };

    // When `this` is the private data (not the newtype), `__self` is a borrow
    // guard; reborrow through it to pass the receiver to the inner method.
    let self_arg = if is_getter {
        quote! { &*__self }
    } else {
        quote! { &mut *__self }
    };

    let body = if is_getter {
        // Getter: call method, set return value
        let call = if on_newtype {
            if info.is_raw {
                quote! { __self.#fn_name(&scope, &__args) }
            } else if info.has_cx {
                quote! { __self.#fn_name(&scope) }
            } else {
                quote! { __self.#fn_name() }
            }
        } else if info.is_raw {
            quote! { #type_name::#fn_name(#self_arg, &scope, &__args) }
        } else if info.has_cx {
            quote! { #type_name::#fn_name(#self_arg, &scope) }
        } else {
            quote! { #type_name::#fn_name(#self_arg) }
        };

        match &info.return_style {
            ReturnStyle::Raw => quote! {
                match #call {
                    Ok(()) => ::js::exception::check_fn_return(&scope, true, &#name_str),
                    Err(::js::error::ExnThrown) => ::js::exception::check_fn_return(&scope, false, &#name_str),
                }
            },
            ReturnStyle::Value => quote! {
                let __result = #call;
                let __set_ok = ::js::class::set_return(&scope, &__args, &__result);
                ::js::exception::check_fn_return(&scope, __set_ok, &#name_str)
            },
            ReturnStyle::ResultValue => quote! {
                match #call {
                    Ok(__v) => {
                        let __set_ok = ::js::class::set_return(&scope, &__args, &__v);
                        ::js::exception::check_fn_return(&scope, __set_ok, &#name_str)
                    }
                    Err(__e) => {
                        ::js::error::ThrowException::throw(__e, &scope);
                        ::js::exception::check_fn_return(&scope, false, &#name_str)
                    }
                }
            },
            // A fallible promise-typed attribute returns the `Ok` promise; an
            // `Err` becomes a rejected promise, not a synchronous throw.
            ReturnStyle::ResultPromise => quote! {
                match #call {
                    Ok(__p) => {
                        __args.rval().set(unsafe { ::js::value::from_object(__p.as_raw()) });
                        ::js::exception::check_fn_return(&scope, true, &#name_str)
                    }
                    Err(__e) => match {
                        ::js::error::ThrowException::throw(__e, &scope);
                        ::js::Promise::new_rejected_with_pending_error(&scope)
                    } {
                        Ok(__rejected) => {
                            __args.rval().set(unsafe { ::js::value::from_object(__rejected.as_raw()) });
                            ::js::exception::check_fn_return(&scope, true, &#name_str)
                        }
                        Err(_) => ::js::exception::check_fn_return(&scope, false, &#name_str),
                    },
                }
            },
            // The catch-all covers `Value` and a bare `-> Promise<'_>` getter,
            // both of which convert through `ToJSVal`.
            _ => quote! {
                let __result = #call;
                let __set_ok = ::js::class::set_return(&scope, &__args, &__result);
                ::js::exception::check_fn_return(&scope, __set_ok, &#name_str)
            },
        }
    } else {
        // Setter: extract arg[0], call method
        let arg_extractions = gen_arg_extractions(
            &info.params,
            quote!(&__args),
            false,
            quote!(&scope),
            info.required,
        );

        let call_args: Vec<_> = info.params.iter().map(|(name, _)| quote!(#name)).collect();
        let call = if on_newtype {
            if info.is_raw {
                quote! { __self.#fn_name(&scope, &__args) }
            } else if info.has_cx {
                quote! { __self.#fn_name(&scope, #(#call_args),*) }
            } else {
                quote! { __self.#fn_name(#(#call_args),*) }
            }
        } else if info.is_raw {
            quote! { #type_name::#fn_name(#self_arg, &scope, &__args) }
        } else if info.has_cx {
            quote! { #type_name::#fn_name(#self_arg, &scope, #(#call_args),*) }
        } else {
            quote! { #type_name::#fn_name(#self_arg, #(#call_args),*) }
        };

        match &info.return_style {
            ReturnStyle::Raw => quote! {
                #(#arg_extractions)*
                match #call {
                    Ok(()) => ::js::exception::check_fn_return(&scope, true, &#name_str),
                    Err(::js::error::ExnThrown) => ::js::exception::check_fn_return(&scope, false, &#name_str),
                }
            },
            ReturnStyle::ResultVoid => quote! {
                #(#arg_extractions)*
                match #call {
                    Ok(()) => {
                        __args.rval().set(::js::value::undefined());
                        ::js::exception::check_fn_return(&scope, true, &#name_str)
                    }
                    Err(__e) => {
                        ::js::error::ThrowException::throw(__e, &scope);
                        ::js::exception::check_fn_return(&scope, false, &#name_str)
                    }
                }
            },
            // A setter's return value is ignored (the JS setter yields
            // undefined), but a fallible `Result<T, E>` body must still surface
            // its `Err` as a thrown exception rather than swallow it.
            // TODO: this should probably be a compilation error instead of silently ignoring the value and returning undefined.
            ReturnStyle::ResultValue => quote! {
                #(#arg_extractions)*
                match #call {
                    Ok(_) => {
                        __args.rval().set(::js::value::undefined());
                        ::js::exception::check_fn_return(&scope, true, &#name_str)
                    }
                    Err(__e) => {
                        ::js::error::ThrowException::throw(__e, &scope);
                        ::js::exception::check_fn_return(&scope, false, &#name_str)
                    }
                }
            },
            // The catch-all covers a plain value-returning setter; the value is
            // discarded and the JS setter yields undefined.
            _ => quote! {
                #(#arg_extractions)*
                #call;
                __args.rval().set(::js::value::undefined());
                ::js::exception::check_fn_return(&scope, true, &#name_str)
            },
        }
    };

    quote! {
        #[allow(non_snake_case)]
        unsafe extern "C" fn #native_name(
            raw_cx: *mut ::js::native::RawJSContext,
            argc: u32,
            vp: *mut ::js::native::Value,
        ) -> bool {
            #site
            let scope = unsafe { ::js::gc::scope::RootScope::from_current_realm(raw_cx) };
            let __args = ::js::native::CallArgs::from_vp(vp, argc);
            #this_extraction
            #body
        }
    }
}

// ============================================================================
// #[derive(Traceable)] proc macro
// ============================================================================

/// Derive macro that generates an `unsafe impl Traceable` for a struct.
///
/// Each field is traced by calling `self.field.trace(trc)` unless annotated
/// with `#[no_trace]`, in which case it is skipped.
///
/// # Usage
///
/// ```rust,ignore
/// #[derive(Traceable)]
/// struct MyStruct {
///     js_val: Heap<*mut JSObject>,
///     #[no_trace]
///     plain_data: String,
/// }
/// ```
#[proc_macro_derive(Traceable, attributes(no_trace))]
pub fn derive_traceable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;

    // Bound a type parameter by `Trace` only if it appears in a *traced* field
    // (one not marked `#[no_trace]`), so the generated `field.trace(trc)` resolves.
    // A parameter used solely behind `#[no_trace]` is never traced and must not be
    // constrained. E.g. `struct Cache<K> { #[no_trace] key: K, val: Heap<Value> }`
    // must compile for a `K` that is not `Trace`.
    let mut generics = input.generics.clone();
    let all_params: std::collections::HashSet<syn::Ident> =
        generics.type_params().map(|tp| tp.ident.clone()).collect();
    let mut traced_params = std::collections::HashSet::new();
    for ty in traced_field_types(&input.data) {
        collect_param_idents(&ty, &all_params, &mut traced_params);
    }
    for tp in generics.type_params_mut() {
        if traced_params.contains(&tp.ident) {
            tp.bounds.push(syn::parse_quote!(::js::heap::Trace));
        }
    }
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let trace_body = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(fields) => {
                let field_traces: Vec<_> = fields
                    .named
                    .iter()
                    .filter(|f| !has_no_trace_attr(f))
                    .map(|f| {
                        let field_name = f.ident.as_ref().unwrap();
                        quote! { self.#field_name.trace(trc); }
                    })
                    .collect();
                quote! { #(#field_traces)* }
            }
            Fields::Unnamed(fields) => {
                let field_traces: Vec<_> = fields
                    .unnamed
                    .iter()
                    .enumerate()
                    .filter(|(_, f)| !has_no_trace_attr(f))
                    .map(|(i, _)| {
                        let idx = syn::Index::from(i);
                        quote! { self.#idx.trace(trc); }
                    })
                    .collect();
                quote! { #(#field_traces)* }
            }
            Fields::Unit => quote! {},
        },
        Data::Enum(data) => {
            let arms: Vec<_> = data
                .variants
                .iter()
                .map(|v| trace_variant_arm(name, &v.ident, &v.fields))
                .collect();
            quote! { match self { #(#arms)* } }
        }
        Data::Union(u) => {
            return syn::Error::new_spanned(
                u.union_token,
                "#[derive(Traceable)] is not supported for unions",
            )
            .to_compile_error()
            .into();
        }
    };

    let output = quote! {
        unsafe impl #impl_generics ::js::heap::Trace for #name #ty_generics #where_clause {
            #[inline]
            unsafe fn trace(&self, trc: *mut ::js::native::JSTracer) {
                #trace_body
            }
        }
    };

    output.into()
}

fn has_no_trace_attr(field: &syn::Field) -> bool {
    field
        .attrs
        .iter()
        .any(|attr| attr.path().is_ident("no_trace"))
}

/// The types of every field that will be traced (i.e. not `#[no_trace]`), across
/// a struct's fields or all of an enum's variants.
fn traced_field_types(data: &Data) -> Vec<syn::Type> {
    let mut tys = Vec::new();
    let push_fields = |fields: &Fields, tys: &mut Vec<syn::Type>| {
        for f in fields.iter() {
            if !has_no_trace_attr(f) {
                tys.push(f.ty.clone());
            }
        }
    };
    match data {
        Data::Struct(s) => push_fields(&s.fields, &mut tys),
        Data::Enum(e) => {
            for v in &e.variants {
                push_fields(&v.fields, &mut tys);
            }
        }
        Data::Union(_) => {}
    }
    tys
}

/// Collect into `out` every type-parameter ident (from `params`) that appears
/// anywhere in `ty`. A type parameter is "used" iff its ident appears in the
/// type's token stream, so any traced field that mentions the parameter marks it
/// as needing a `Trace` bound.
fn collect_param_idents(
    ty: &syn::Type,
    params: &std::collections::HashSet<syn::Ident>,
    out: &mut std::collections::HashSet<syn::Ident>,
) {
    use quote::ToTokens;
    fn walk(
        stream: proc_macro2::TokenStream,
        params: &std::collections::HashSet<syn::Ident>,
        out: &mut std::collections::HashSet<syn::Ident>,
    ) {
        for tok in stream {
            match tok {
                proc_macro2::TokenTree::Ident(id) => {
                    if params.contains(&id) {
                        out.insert(id);
                    }
                }
                proc_macro2::TokenTree::Group(g) => walk(g.stream(), params, out),
                _ => {}
            }
        }
    }
    walk(ty.to_token_stream(), params, out);
}

/// Generate one `match` arm tracing a single enum variant's fields. Fields
/// marked `#[no_trace]` are bound to `_` (tuple) or dropped behind `..`
/// (struct) so they aren't traced; a unit variant matches with no bindings.
fn trace_variant_arm(
    enum_name: &Ident,
    vname: &Ident,
    fields: &Fields,
) -> proc_macro2::TokenStream {
    match fields {
        Fields::Named(named) => {
            let mut binds = Vec::new();
            let mut traces = Vec::new();
            for f in &named.named {
                let id = f.ident.as_ref().unwrap();
                if has_no_trace_attr(f) {
                    continue;
                }
                binds.push(quote!(#id));
                traces.push(quote! { #id.trace(trc); });
            }
            let rest = if binds.len() < named.named.len() {
                quote!(..)
            } else {
                quote!()
            };
            quote! { #enum_name::#vname { #(#binds,)* #rest } => { #(#traces)* } }
        }
        Fields::Unnamed(unnamed) => {
            let mut binds = Vec::new();
            let mut traces = Vec::new();
            for (i, f) in unnamed.unnamed.iter().enumerate() {
                if has_no_trace_attr(f) {
                    binds.push(quote!(_));
                } else {
                    let b = format_ident!("__{i}");
                    binds.push(quote!(#b));
                    traces.push(quote! { #b.trace(trc); });
                }
            }
            quote! { #enum_name::#vname( #(#binds),* ) => { #(#traces)* } }
        }
        Fields::Unit => quote! { #enum_name::#vname => {} },
    }
}

// ============================================================================
// #[derive(ScopeRoot)] — scope-rooted mirror for `#[must_root]` aggregates
// ============================================================================

/// If `ty` is `Heap<INNER>`, returns `INNER`.
fn heap_inner_ty(ty: &Type) -> Option<&Type> {
    let Type::Path(tp) = ty else { return None };
    let seg = tp.path.segments.last()?;
    if seg.ident != "Heap" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        syn::GenericArgument::Type(inner) => Some(inner),
        _ => None,
    }
}

/// Whether `ty`'s final path segment is `Value` (the `js::native::Value` JSVal).
fn is_value_ty(ty: &Type) -> bool {
    matches!(ty, Type::Path(tp) if tp.path.segments.last().is_some_and(|s| s.ident == "Value"))
}

/// One field's classification for `#[derive(ScopeRoot)]`.
struct RootField {
    /// Binding identifier used in destructuring patterns.
    bind: Ident,
    /// `Some(ident)` for a named field, `None` for a tuple field.
    name: Option<Ident>,
    /// `true` for any `Heap<…>` field (object or value).
    is_heap: bool,
    /// `true` for `Heap<T>` where `T: JSType` (rooted via `take`); `false` for
    /// `Heap<Value>` and plain fields.
    is_object_heap: bool,
    /// The field type in the rooted mirror.
    mirror_ty: proc_macro2::TokenStream,
}

fn classify_root_fields(fields: &Fields) -> Vec<RootField> {
    fields
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let (is_heap, is_object_heap, mirror_ty) = match heap_inner_ty(&f.ty) {
                Some(inner) if is_value_ty(inner) => {
                    (true, false, quote! { ::js::prelude::HandleValue<'s> })
                }
                Some(inner) => (
                    true,
                    true,
                    quote! { <#inner as ::js::builtins::JSType>::Rooted<'s> },
                ),
                None => {
                    let ty = &f.ty;
                    (false, false, quote! { #ty })
                }
            };
            RootField {
                bind: f.ident.clone().unwrap_or_else(|| format_ident!("__f{}", i)),
                name: f.ident.clone(),
                is_heap,
                is_object_heap,
                mirror_ty,
            }
        })
        .collect()
}

/// Build, for one field group (an enum variant's fields or a struct's fields):
/// the mirror field declarations, a by-value destructuring pattern, and the two
/// rooted constructor argument lists (fast `take`-based and traced `get`-based).
fn build_root_group(
    fields: &Fields,
) -> (
    proc_macro2::TokenStream, // mirror field declarations (with braces/parens)
    proc_macro2::TokenStream, // destructuring pattern (with braces/parens)
    proc_macro2::TokenStream, // fast-path constructor args (with braces/parens)
    proc_macro2::TokenStream, // traced-path constructor args (with braces/parens)
    usize,                    // number of `Heap` fields
    bool,                     // single object-heap field (fast path eligible)
) {
    let infos = classify_root_fields(fields);
    let heap_count = infos.iter().filter(|f| f.is_heap).count();
    let single_object =
        heap_count == 1 && infos.iter().filter(|f| f.is_heap).all(|f| f.is_object_heap);

    let binds: Vec<&Ident> = infos.iter().map(|f| &f.bind).collect();

    let mirror_decls = infos.iter().map(|f| {
        let ty = &f.mirror_ty;
        match &f.name {
            Some(n) => quote! { #n: #ty },
            None => quote! { #ty },
        }
    });
    let fast_args = infos.iter().map(|f| {
        let bind = &f.bind;
        let expr = if f.is_heap {
            quote! { #bind.take(scope) }
        } else {
            quote! { #bind }
        };
        match &f.name {
            Some(n) => quote! { #n: #expr },
            None => quote! { #expr },
        }
    });
    let traced_args = infos.iter().map(|f| {
        let bind = &f.bind;
        let expr = if f.is_heap {
            quote! { #bind.get(scope) }
        } else {
            quote! { *#bind }
        };
        match &f.name {
            Some(n) => quote! { #n: #expr },
            None => quote! { #expr },
        }
    });

    let (mirror, pattern, fast, traced) = match fields {
        Fields::Named(_) => (
            quote! { { #(#mirror_decls),* } },
            quote! { { #(#binds),* } },
            quote! { { #(#fast_args),* } },
            quote! { { #(#traced_args),* } },
        ),
        Fields::Unnamed(_) => (
            quote! { ( #(#mirror_decls),* ) },
            quote! { ( #(#binds),* ) },
            quote! { ( #(#fast_args),* ) },
            quote! { ( #(#traced_args),* ) },
        ),
        // A unit variant/struct has no fields, so it carries no
        // braces/parens — matching `Variant()` against a unit variant is
        // a hard error (E0532).
        Fields::Unit => (quote! {}, quote! {}, quote! {}, quote! {}),
    };
    (mirror, pattern, fast, traced, heap_count, single_object)
}

/// Generates a scope-rooted mirror of a `#[must_root]` aggregate plus a
/// `root(self, scope)` method that produces it.
///
/// For a type `Foo` whose fields are `Heap<T>` / `Heap<Value>` / plain `Copy`
/// values, this emits `StackFoo<'s>` with each `Heap<T>` replaced by its rooted
/// handle (`<T as JSType>::Rooted<'s>`), each `Heap<Value>` by `HandleValue<'s>`,
/// and plain fields unchanged. `StackFoo` holds only scope-rooted handles, so it
/// is **not** `#[must_root]` — methods can hold it across allocations freely.
///
/// `Foo::root` is safe by construction. A field group with a single `Heap<T:
/// JSType>` field roots it with [`Heap::take`](js::gc::handle::Heap::take)
/// (drop-before-root, no allocation). A group with two or more `Heap` fields (or
/// a `Heap<Value>`) is first moved into a `RootedTraceableBox` and each field
/// rooted with [`Heap::get`](js::gc::handle::Heap::get) while traced, so rooting
/// one field can never stale another. The single generated `root` carries the one
/// `allow_unrooted` these types need (`self` is `#[must_root]`).
///
/// Requires the type to be `Trace` (the traced path boxes `self`). Use for
/// settled-exactly-once consume-leaf types (read requests, queue entries, promise
/// slots) — not for types mutated and re-queued in place.
#[proc_macro_derive(ScopeRoot)]
pub fn derive_scope_root(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let vis = &input.vis;
    let rooted_name = format_ident!("Stack{}", name);

    let (mirror_def, root_body) = match &input.data {
        Data::Enum(data) => {
            let mut mirror_variants = Vec::new();
            let mut arms = Vec::new();
            for v in &data.variants {
                let vname = &v.ident;
                let (mirror, pattern, fast, traced, heap_count, single_object) =
                    build_root_group(&v.fields);
                mirror_variants.push(quote! { #vname #mirror });
                if single_object || heap_count == 0 {
                    arms.push(quote! {
                        #name::#vname #pattern => #rooted_name::#vname #fast,
                    });
                } else {
                    let wildcard = if matches!(v.fields, Fields::Named(_)) {
                        quote! { #name::#vname { .. } }
                    } else {
                        quote! { #name::#vname ( .. ) }
                    };
                    arms.push(quote! {
                        __req @ #wildcard => {
                            let __boxed = ::js::heap::RootedTraceableBox::new(__req);
                            match &*__boxed {
                                #name::#vname #pattern => #rooted_name::#vname #traced,
                                _ => ::std::unreachable!(),
                            }
                        }
                    });
                }
            }
            (
                quote! {
                    #vis enum #rooted_name<'s> {
                        #(#mirror_variants),*
                    }
                },
                quote! {
                    match self {
                        #(#arms)*
                    }
                },
            )
        }
        Data::Struct(data) => {
            let (mirror, pattern, fast, traced, heap_count, single_object) =
                build_root_group(&data.fields);
            let semi = if matches!(data.fields, Fields::Named(_)) {
                quote! {}
            } else {
                quote! { ; }
            };
            let body = if single_object || heap_count == 0 {
                quote! {
                    let #name #pattern = self;
                    #rooted_name #fast
                }
            } else {
                quote! {
                    let __boxed = ::js::heap::RootedTraceableBox::new(self);
                    let #name #pattern = &*__boxed;
                    #rooted_name #traced
                }
            };
            (quote! { #vis struct #rooted_name<'s> #mirror #semi }, body)
        }
        Data::Union(_) => panic!("#[derive(ScopeRoot)] is not supported for unions"),
    };

    quote! {
        #mirror_def

        impl #name {
            /// Root this `#[must_root]` value into `scope`, yielding its
            /// scope-rooted (non-`must_root`) mirror. Generated by
            /// `#[derive(ScopeRoot)]`.
            #[cfg_attr(crown, allow(crown::unrooted_must_root))]
            #vis fn root<'s>(self, scope: &'s ::js::gc::scope::Scope<'_>) -> #rooted_name<'s> {
                #root_body
            }
        }
    }
    .into()
}

// ============================================================================
// Shared free-function / const export codegen
//
// `#[jsmodule]`, `#[jsglobals]`, `#[jsnamespace]`, and `#[webidl_namespace]` all
// expose public free functions and consts from a `mod` block. They share the
// parameter/return parsing and the JSNative trampoline below, differing only in
// where the resulting functions and consts are installed.
// ============================================================================

/// Collect a public free function into a `ModuleFnExport`, filtering out the
/// optional `scope: &Scope` and `args: &CallArgs` parameters (which are passed
/// through rather than extracted from JS arguments) and a trailing
/// `RestArgs<T>` (which is collected from the arguments past the fixed ones).
///
/// The function name is camelCased.
fn parse_free_fn_export(fn_item: &syn::ItemFn) -> ModuleFnExport {
    let fn_name = &fn_item.sig.ident;
    let js_name = unraw(fn_name).to_lower_camel_case();

    let mut params: Vec<(Ident, Type)> = Vec::new();
    let mut has_cx = false;
    let mut is_raw = false;
    let mut rest_arg_name = None;
    let mut rest_inner_type = None;
    for arg in &fn_item.sig.inputs {
        if let FnArg::Typed(pat_ty) = arg {
            if is_cx_param_type(&pat_ty.ty) {
                has_cx = true;
                continue;
            }
            if is_callargs_param_type(&pat_ty.ty) {
                is_raw = true;
                continue;
            }
            // A variadic tail is not an ordinary parameter: it is collected from
            // whatever arguments follow the fixed ones, so it must stay out of
            // `params` for the index arithmetic in `gen_rest_setup` to line up.
            if is_rest_args_type(&pat_ty.ty) {
                if let Pat::Ident(pat_ident) = pat_ty.pat.as_ref() {
                    rest_arg_name = Some(pat_ident.ident.clone());
                    rest_inner_type = extract_rest_args_inner_type(&pat_ty.ty);
                }
                continue;
            }
            if let Pat::Ident(pat_ident) = pat_ty.pat.as_ref() {
                params.push((pat_ident.ident.clone(), (*pat_ty.ty).clone()));
            }
        }
    }

    let return_style = classify_return_style(&fn_item.sig.output, None, false, is_raw);

    ModuleFnExport {
        fn_name: fn_name.clone(),
        js_name,
        params,
        return_style,
        has_cx,
        is_raw,
        rest_arg_name,
        rest_inner_type,
        length: None,
    }
}

/// Collect a public const into a `ModuleConstExport`.
///
/// Constants keep their declared Rust name, because JS and Rust conventions
/// match on their naming.
fn parse_const_export(const_item: &syn::ItemConst) -> ModuleConstExport {
    let const_name = &const_item.ident;
    let js_name = unraw(const_name);
    let is_ref_type = matches!(&*const_item.ty, Type::Reference(_));
    ModuleConstExport {
        const_name: const_name.clone(),
        js_name,
        is_ref_type,
    }
}

/// Remove a `#[js_length = N]` attribute from `fn_item`, and return the `.length` it gives, if it
/// has one.
fn take_js_length_attr(fn_item: &mut syn::ItemFn) -> syn::Result<Option<u32>> {
    let mut length = None;
    let mut error = None;
    fn_item.attrs.retain(|attr| {
        if !attr.path().is_ident("js_length") {
            return true;
        }
        let parsed = match &attr.meta {
            syn::Meta::NameValue(syn::MetaNameValue {
                value:
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Int(lit),
                        ..
                    }),
                ..
            }) => lit.base10_parse::<u32>(),
            other => Err(syn::Error::new_spanned(
                other,
                "expected `#[js_length = N]`",
            )),
        };
        match parsed {
            Ok(value) => length = Some(value),
            Err(e) => error = Some(e),
        }
        false
    });
    match error {
        Some(error) => Err(error),
        None => Ok(length),
    }
}

/// Remove a `#[js_name = "..."]` attribute from `fn_item`, and return the JS name it gives, if
/// it has one.
fn take_js_name_attr(fn_item: &mut syn::ItemFn) -> syn::Result<Option<String>> {
    let mut name = None;
    let mut error = None;
    fn_item.attrs.retain(|attr| {
        if !attr.path().is_ident("js_name") {
            return true;
        }
        match &attr.meta {
            syn::Meta::NameValue(syn::MetaNameValue {
                value:
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(lit),
                        ..
                    }),
                ..
            }) => name = Some(lit.value()),
            other => {
                error = Some(syn::Error::new_spanned(
                    other,
                    "expected `#[js_name = \"...\"]`",
                ))
            }
        }
        false
    });
    match error {
        Some(error) => Err(error),
        None => Ok(name),
    }
}

/// What a `mod`-block macro found to expose, plus the module body to re-emit.
#[derive(Default)]
struct ModExports {
    fns: Vec<ModuleFnExport>,
    consts: Vec<ModuleConstExport>,
    /// Classes named by `pub use`, collected only when the caller asks for them.
    class_reexports: Vec<Ident>,
    /// Every item of the original `mod`, re-emitted as written apart from the
    /// `RestArgs` signature rewrite applied to public functions.
    items: Vec<proc_macro2::TokenStream>,
}

/// Walk a `mod` block and collect the public items it exposes to JS.
///
/// `#[jsmodule]`, `#[jsglobals]`, `#[jsnamespace]`, and `#[webidl_namespace]`
/// differ in *where* they install what they find, not in what counts as an
/// export or how its JS name and signature are derived, so they share this
/// walk.
///
/// `collect_classes` enables the `pub use Foo;` arm, which only `#[jsglobals]`
/// acts on; elsewhere a `use` is passed through as an ordinary item.
/// `macro_name` is the spelling used in diagnostics, e.g. `"#[jsmodule]"`.
fn collect_mod_exports(
    input: &syn::ItemMod,
    macro_name: &str,
    collect_classes: bool,
) -> syn::Result<ModExports> {
    let Some((_, items)) = &input.content else {
        return Err(syn::Error::new_spanned(
            input,
            format!("{macro_name} requires an inline mod block"),
        ));
    };

    let mut out = ModExports::default();
    for item in items {
        match item {
            // `pub use SomeClass;` / `pub use super::{A, B};` / `pub use X as Y;`
            // — register the named classes on the global.
            syn::Item::Use(use_item)
                if collect_classes && matches!(use_item.vis, syn::Visibility::Public(_)) =>
            {
                collect_use_class_idents(&use_item.tree, &mut out.class_reexports).map_err(
                    |span| {
                        syn::Error::new(
                            span,
                            format!(
                                "{macro_name} cannot register classes from a glob \
                                 `use ...::*`; name each class explicitly"
                            ),
                        )
                    },
                )?;
                // Keep the use item in the module output for Rust visibility.
                out.items.push(quote! { #use_item });
            }
            syn::Item::Fn(fn_item) if matches!(fn_item.vis, syn::Visibility::Public(_)) => {
                let mut fn_item = fn_item.clone();
                let fn_name = fn_item.sig.ident.clone();
                let js_name = take_js_name_attr(&mut fn_item)?;
                let length = take_js_length_attr(&mut fn_item)?;
                rewrite_rest_args_in_sig(&mut fn_item.sig, &fn_name)?;
                let mut export = parse_free_fn_export(&fn_item);
                if let Some(js_name) = js_name {
                    export.js_name = js_name;
                }
                export.length = length;
                out.fns.push(export);
                out.items.push(quote! { #fn_item });
            }
            syn::Item::Const(const_item)
                if matches!(const_item.vis, syn::Visibility::Public(_)) =>
            {
                out.consts.push(parse_const_export(const_item));
                out.items.push(quote! { #const_item });
            }
            other => out.items.push(quote! { #other }),
        }
    }
    Ok(out)
}

/// Generate the JSNative trampoline for a free function exported by a module,
/// the global object, or a namespace. `call_path` is the path to the user's
/// function's containing module (e.g. `super::my_mod`).
/// `required` is as for [`emit_native_fn`].
fn gen_free_fn_native(
    exp: &ModuleFnExport,
    native_name: &Ident,
    call_path: &proc_macro2::TokenStream,
    required: u32,
) -> proc_macro2::TokenStream {
    let fn_name = &exp.fn_name;
    let name_str = fn_name.to_string();
    let mut call_args: Vec<_> = exp.params.iter().map(|(name, _)| quote!(#name)).collect();
    let rest_arg = exp
        .rest_arg_name
        .as_ref()
        .map(|name| (name, exp.rest_inner_type.as_ref()));
    if let Some(rest_name) = exp.rest_arg_name.as_ref() {
        call_args.push(quote!(#rest_name));
    }

    let call = if exp.is_raw {
        quote! { #call_path::#fn_name(&scope, &__args) }
    } else if exp.has_cx {
        quote! { #call_path::#fn_name(&scope, #(#call_args),*) }
    } else {
        quote! { #call_path::#fn_name(#(#call_args),*) }
    };

    let site = binding_site(quote! { "" }, &exp.js_name, "Function");
    emit_native_fn(
        native_name,
        &name_str,
        &site,
        &exp.return_style,
        &exp.params,
        required,
        &call,
        // Free functions don't have a receiver, so `this` extraction is empty
        &quote! {},
        &quote! {},
        rest_arg,
        None,
    )
}

// ============================================================================
// #[jsmodule] attribute macro
// ============================================================================

/// Attribute macro that transforms a `mod` block into a native ES module.
///
/// Public functions become callable JS exports renamed to camelCase while
/// remaining callable from Rust under their original name. `#[js_name = "..."]`
/// on a function sets its JS name instead. Their `.length` is the count of
/// parameters that aren't `Option<T>`, or `N` with `#[js_length = N]`.
/// Public `const` items become value exports without camelCasing:
/// `PI` is exported as `PI`, not `pi`.
///
/// `pub use SomeClass;` exports a `#[jsclass]` class's constructor under the class's Rust name,
/// without installing the class on the global object.
///
/// The import specifier is the `mod` name camelCased, unless overridden with
/// `#[jsmodule(name = "...")]`. `#[jsmodule(default_namespace)]` adds a `default` export holding
/// every named export, and `#[jsmodule(default = "Name")]` makes the named export `Name` the
/// `default` export, with every named export set as a property on it.
///
/// # Usage
///
/// ```rust,ignore
/// #[::core_runtime::jsmodule]
/// mod my_math {
///     pub const PI: f64 = 3.14159;
///     pub fn add(a: f64, b: f64) -> f64 { a + b }
/// }
///
/// // Register:
/// core_runtime::module::register_module::<my_math::js_module>(scope);
///
/// // Call from Rust:
/// assert_eq!(my_math::add(1.0, my_math::PI), 4.14159);
/// ```
///
/// // Call from JS:
/// ```js
/// import { PI, add } from "myMath";
/// console.log(add(1, PI)); // prints 4.14159
/// ```
#[proc_macro_attribute]
pub fn jsmodule(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = parse_macro_input!(attr as AttrOpts);
    let input = parse_macro_input!(item as syn::ItemMod);
    if let Err(e) = opts.reject_all_but(
        &[
            "name",
            "default",
            "default_namespace",
            "self_export",
            "mirror",
            "node_args",
        ],
        &input.ident,
    ) {
        return e.to_compile_error().into();
    }

    let mod_name = &input.ident;
    let mod_vis = &input.vis;
    let js_module_name = opts
        .name
        .unwrap_or_else(|| mod_name.to_string().to_lower_camel_case());

    let ModExports {
        fns: fn_exports,
        consts: const_exports,
        class_reexports,
        items: original_items,
    } = match collect_mod_exports(&input, "#[jsmodule]", true) {
        Ok(exports) => exports,
        Err(e) => return e.to_compile_error().into(),
    };

    // Generate JSNative wrappers for each function export
    let mut native_fns: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut declaration_entries: Vec<proc_macro2::TokenStream> = Vec::new();

    for exp in &fn_exports {
        let native_name = format_ident!("__native_module_{}", unraw(&exp.fn_name));
        let js_name = &exp.js_name;
        let nargs = required_arg_count(&exp.params);
        let length = exp.length.unwrap_or(nargs);

        native_fns.push(gen_free_fn_native(
            exp,
            &native_name,
            &quote!(super::#mod_name),
            if opts.node_args { 0 } else { nargs },
        ));

        declaration_entries.push(quote! {
            ::core_runtime::module::ModuleExport::Function {
                js_name: #js_name,
                native: Some(#native_name),
                nargs: #length,
            }
        });
    }

    // Generate declaration entries for constants
    for exp in &const_exports {
        let js_name = &exp.js_name;
        declaration_entries.push(quote! {
            ::core_runtime::module::ModuleExport::Value {
                js_name: #js_name,
            }
        });
    }

    // Classes named by `pub use` are value exports holding their constructors.
    for class_name in &class_reexports {
        let js_name = unraw(class_name);
        declaration_entries.push(quote! {
            ::core_runtime::module::ModuleExport::Value {
                js_name: #js_name,
            }
        });
    }

    // Generate the evaluate function that sets constant values
    let mut value_setters: Vec<proc_macro2::TokenStream> = Vec::new();
    for class_name in &class_reexports {
        let js_name = unraw(class_name);
        value_setters.push(quote! {
            let Ok(__ctor) = ::js::class::IntoConstructor::into_constructor(
                #class_name::class_constructor(scope),
            ) else {
                return false;
            };
            if !::core_runtime::module::set_module_export(scope, env, #js_name, &__ctor) {
                return false;
            }
        });
    }
    for exp in &const_exports {
        let const_name = &exp.const_name;
        let js_name = &exp.js_name;
        // For reference-type constants (e.g. `&str`), pass directly.
        let value_expr = if exp.is_ref_type {
            quote! { super::#mod_name::#const_name }
        } else {
            quote! { &super::#mod_name::#const_name }
        };
        value_setters.push(quote! {
            if !::core_runtime::module::set_module_export(
                scope, env, #js_name, #value_expr,
            ) {
                return false;
            }
        });
    }

    let js_module_name_str = &js_module_name;
    let mirrored: Vec<String> = opts
        .mirror
        .iter()
        .flat_map(|names| names.split(','))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    let self_export = match &opts.self_export {
        Some(name) => quote! { Some(#name) },
        None => quote! { None },
    };
    let default_export = match (&opts.default, opts.default_namespace) {
        (Some(name), _) => quote! {
            ::core_runtime::module::DefaultExport::Export { name: #name, self_export: #self_export }
        },
        (None, true) => {
            quote! { ::core_runtime::module::DefaultExport::Namespace { self_export: #self_export } }
        }
        (None, false) => quote! { ::core_runtime::module::DefaultExport::None },
    };

    let output = quote! {
        #mod_vis mod #mod_name {
            #(#original_items)*

            /// Generated struct implementing `NativeModule` for this module.
            pub struct js_module;

            #(#native_fns)*

            impl ::core_runtime::module::NativeModule for js_module {
                const NAME: &'static str = #js_module_name_str;
                const DEFAULT_EXPORT: ::core_runtime::module::DefaultExport = #default_export;
                const MIRRORED_EXPORTS: &'static [&'static str] = &[#(#mirrored),*];

                fn declarations() -> Vec<::core_runtime::module::ModuleExport> {
                    vec![
                        #(#declaration_entries),*
                    ]
                }

                unsafe fn evaluate(
                    scope: &::js::gc::scope::Scope<'_>,
                    env: ::js::native::HandleObject,
                ) -> bool {
                    #(#value_setters)*
                    true
                }
            }

            /// Register this native module so it can be imported from JS.
            ///
            /// Equivalent to `register_module::<js_module>(scope)`.
            pub fn register(
                scope: &::js::gc::scope::Scope<'_>,
            ) -> bool {
                ::core_runtime::module::register_module::<js_module>(scope)
            }
        }
    };

    output.into()
}

struct ModuleFnExport {
    fn_name: Ident,
    js_name: String,
    params: Vec<(Ident, Type)>,
    return_style: ReturnStyle,
    has_cx: bool,
    is_raw: bool,
    /// The variadic tail's parameter name and `RestArgs<T>` element type, if the
    /// function declared one.
    rest_arg_name: Option<Ident>,
    rest_inner_type: Option<Type>,
    /// The JS-visible `.length` from `#[js_length = N]`, which replaces the count of required
    /// arguments.
    length: Option<u32>,
}

struct ModuleConstExport {
    const_name: Ident,
    js_name: String,
    is_ref_type: bool,
}

// ============================================================================
// #[jsfunction_class] attribute macro
// ============================================================================

/// Options of `#[jsfunction_class]`.
#[derive(Default)]
struct FunctionClassOpts {
    /// The JS name of the constructor. Defaults to the `mod` name in UpperCamelCase.
    name: Option<String>,
    /// The path of the parent class's `#[jsfunction_class]` module.
    extends: Option<syn::Path>,
    /// Whether the methods are non-enumerable, as those of a JS `class` are.
    class_syntax: bool,
    /// Whether a call without `new` whose `this` isn't an instance returns a new instance.
    self_construct: bool,
    /// The [`js::class::InstanceState`] of the class's instances.
    state: Option<syn::Path>,
    /// The function `fn(&Scope, &str) -> ExnThrown` that throws the error for a `this` without
    /// state.
    invalid_this: Option<syn::Path>,
    /// The `PropertyTemplate`s whose keys instances get when they are created.
    templates: Vec<syn::Path>,
}

impl Parse for FunctionClassOpts {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut opts = Self::default();
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            if key == "class_syntax" || key == "self_construct" {
                if key == "class_syntax" {
                    opts.class_syntax = true;
                } else {
                    opts.self_construct = true;
                }
                if !input.is_empty() {
                    let _: Token![,] = input.parse()?;
                }
                continue;
            }
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "name" => opts.name = Some(input.parse::<LitStr>()?.value()),
                "extends" => opts.extends = Some(input.parse::<LitStr>()?.parse()?),
                "state" => opts.state = Some(input.parse::<LitStr>()?.parse()?),
                "invalid_this" => opts.invalid_this = Some(input.parse::<LitStr>()?.parse()?),
                "templates" => opts.templates = parse_template_paths(&input.parse::<LitStr>()?)?,
                _ => return Err(syn::Error::new(key.span(), "unknown option")),
            }
            if !input.is_empty() {
                let _: Token![,] = input.parse()?;
            }
        }
        if opts.class_syntax && opts.self_construct {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`class_syntax` and `self_construct` are mutually exclusive",
            ));
        }
        if opts.invalid_this.is_some() && opts.state.is_none() {
            return Err(syn::Error::new(
                proc_macro2::Span::call_site(),
                "`invalid_this` requires `state`",
            ));
        }
        Ok(opts)
    }
}

/// What a function in a `#[jsfunction_class]` module becomes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FunctionClassMember {
    Constructor,
    Method,
    StaticMethod,
    Setup,
}

/// Remove the marker attribute that classifies a `#[jsfunction_class]` function, and return what
/// it marks and the JS name its `name = "..."` option gives, if any. A function without a marker
/// is a method, and `#[method(name = "...")]` names one.
fn take_function_class_marker(
    fn_item: &mut syn::ItemFn,
) -> syn::Result<(FunctionClassMember, Option<String>, Option<usize>)> {
    let mut member = FunctionClassMember::Method;
    let mut name = None;
    let mut length = None;
    let mut error = None;
    fn_item.attrs.retain(|attr| {
        let marker = if attr.path().is_ident("constructor") {
            FunctionClassMember::Constructor
        } else if attr.path().is_ident("static_method") {
            FunctionClassMember::StaticMethod
        } else if attr.path().is_ident("method") {
            FunctionClassMember::Method
        } else if attr.path().is_ident("setup") {
            FunctionClassMember::Setup
        } else {
            return true;
        };
        member = marker;
        match parse_marker_opts(attr, &["name", "length"]) {
            Ok(opts) => {
                name = opts.name;
                length = opts.length;
            }
            Err(e) => error = Some(e),
        }
        false
    });
    match error {
        Some(error) => Err(error),
        None => Ok((member, name, length)),
    }
}

/// Attribute macro that turns a `mod` block into a constructor function whose prototype methods
/// work on any object, the shape of Node.js classes that ES5-style code calls without `new` and
/// subclasses with `util.inherits`.
///
/// Any object can be an instance: one created by `new`, one a subclass constructor passed to the
/// constructor as `this`, or one the prototype's methods were copied onto.
///
/// With the `state = "Type"` option, the instances hold Rust state of the
/// [`js::class::InstanceState`] `Type`, usually a `#[jsclass(hidden)]` that the global registers:
///
/// - `new` and `super()` create instances of `Type::Class`, whose private data holds all of the
///   instance's state. Their prototype is that of `new.target`.
/// - A call without `new` whose `this` holds no state, as in `C.call(this)` from an ES5-style
///   subclass constructor, creates a handle: an instance of `Type::Class` that `this` holds in a
///   property under the private name symbol [`js::class::STATE_HANDLE_KEY`], which JavaScript
///   can't read, list, copy or delete. It replaces a handle of another class. For a `Proxy`, its
///   innermost target holds the handle, as [`js::class::set_state_handle`] defines it.
/// - The state's data starts out as its default, which the `#[constructor]` function fills in.
/// - The generated `try_state_of(scope, this) -> Result<Option<Type>, ExnThrown>` returns the
///   state of `this` as [`js::class::find_instance_state`] finds it: the private data of `this`,
///   of its handle, of a `Proxy`'s target, or of an object on its prototype chain, without running
///   JavaScript. `state_of(scope, this)` throws
///   for a `this` without state, with the `invalid_this = "path"` function if the class names one,
///   and a `TypeError` otherwise.
///
/// A class that `extends` a class with state either has a `state` whose class extends the
/// parent's, or creates its instances and handles as the parent does.
///
/// - The `#[constructor]` function initializes `this`, its first parameter after an optional
///   `scope: &Scope`, and returns `Result<(), E>` with `E: ThrowException`. `new C(...)` calls it
///   with a new object whose prototype is `new.target.prototype`, and returns that object.
///   `C.call(obj, ...)` calls it with `obj` and returns `undefined`. A constructor without a
///   `this` parameter is a factory instead: `C(...)` and `new C(...)` both return its result.
/// - Every other public function is a prototype method, named in camelCase unless
///   `#[method(name = "...")]` names it. A parameter named `this` of type `Object` receives the
///   receiver, which must be an object.
/// - `#[static_method]` functions are methods of the constructor. `#[static_method(name = "...")]`
///   names one.
/// - The constructor's and each method's `.length` is the count of its parameters that aren't
///   `Option<T>`, or `N` with `length = N` in its `#[constructor]`, `#[method]` or
///   `#[static_method]` attribute.
/// - A `#[setup]` function `fn(scope, constructor: Object, prototype: Object) -> Result<(),
///   ExnThrown>` runs once after the constructor and prototype are created, for members the other
///   forms don't cover.
///
/// The macro generates `pub fn register(scope) -> Result<Object, ExnThrown>`, which creates the
/// constructor in the current realm unless it exists, and returns it. A global registers its
/// classes when it is set up. The generated `pub fn class_constructor(scope)` returns the
/// constructor, and throws a `TypeError` if the class isn't registered. It also generates
/// `pub fn prototype_object(scope)`, `pub fn is_instance(scope, value)` and
/// `pub fn new_instance(scope)`, which creates an instance as `new` does before the constructor
/// initializes it. They use the prototype the constructor had when it was created. A `prototype`
/// assigned to the constructor later is not used.
/// `extends = "path::to::parent_module"` chains the constructor and prototype to those of another
/// `#[jsfunction_class]`.
///
/// Methods and static methods are enumerable, as properties assigned to a function and its
/// prototype are. With the `class_syntax` option, they are non-enumerable, as a JS `class`'s are,
/// and a call without `new` throws a `TypeError`.
/// With the `self_construct` option, a call without `new` whose `this` isn't an instance of the
/// class initializes and returns a new instance, as Node.js's
/// `if (!(this instanceof C)) return new C(...)` does.
/// With `templates = "A, B"`, the instances `new_instance` creates get fixed slots for the keys of
/// the `js::object::PropertyTemplate`s `A` and `B`, and so do those `new` creates if they are
/// plain objects.
///
/// ```rust,ignore
/// #[jsfunction_class(name = "Counter")]
/// pub mod counter {
///     #[constructor]
///     pub fn init(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> {
///         this.set_property(scope, c"count", 0)
///     }
///
///     pub fn increment(scope: &Scope<'_>, this: Object<'_>) -> Result<(), ExnThrown> { ... }
/// }
/// ```
#[proc_macro_attribute]
pub fn jsfunction_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = parse_macro_input!(attr as FunctionClassOpts);
    let input = parse_macro_input!(item as syn::ItemMod);
    match gen_function_class(opts, input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn gen_function_class(
    opts: FunctionClassOpts,
    input: syn::ItemMod,
) -> syn::Result<proc_macro2::TokenStream> {
    let mod_name = &input.ident;
    let mod_vis = &input.vis;
    let self_construct = opts.self_construct;
    let class_syntax = opts.class_syntax;
    let property_count = template_property_count(&opts.templates);
    let member_attrs = if opts.class_syntax {
        quote! { 0 }
    } else {
        quote! { ::js::class_spec::JSPROP_ENUMERATE as u32 }
    };
    let js_name = opts
        .name
        .unwrap_or_else(|| mod_name.to_string().to_upper_camel_case());
    let no_new_message = cstr_literal(&format!(
        "Class constructor {js_name} cannot be invoked without 'new'"
    ));
    let not_registered_message =
        cstr_literal(&format!("{js_name} is not registered on this global"));
    let Some((_, items)) = &input.content else {
        return Err(syn::Error::new_spanned(
            &input,
            "#[jsfunction_class] requires an inline mod block",
        ));
    };

    let mut out_items = Vec::new();
    let mut natives = Vec::new();
    let mut methods = Vec::new();
    let mut statics = Vec::new();
    let mut constructor = None;
    let mut setup = None;
    for item in items {
        let syn::Item::Fn(fn_item) = item else {
            out_items.push(quote! { #item });
            continue;
        };
        if !matches!(fn_item.vis, syn::Visibility::Public(_)) {
            out_items.push(quote! { #item });
            continue;
        }
        let mut fn_item = fn_item.clone();
        let (member, js_name_override, length_override) = take_function_class_marker(&mut fn_item)?;
        let fn_name = fn_item.sig.ident.clone();
        if member == FunctionClassMember::Setup {
            setup = Some(fn_name);
            out_items.push(quote! { #fn_item });
            continue;
        }
        rewrite_rest_args_in_sig(&mut fn_item.sig, &fn_name)?;
        let mut export = parse_free_fn_export(&fn_item);
        if let Some(name) = js_name_override {
            export.js_name = name;
        }
        let has_this = export
            .params
            .first()
            .is_some_and(|(name, _)| name == "this");
        if has_this {
            export.params.remove(0);
        }
        out_items.push(quote! { #fn_item });

        let mut call_args: Vec<_> = Vec::new();
        if export.has_cx || export.is_raw {
            call_args.push(quote!(&scope));
        }
        if export.is_raw {
            call_args.push(quote!(&__args));
        }
        if has_this {
            call_args.push(quote!(this));
        }
        call_args.extend(export.params.iter().map(|(name, _)| quote!(#name)));
        if let Some(rest_name) = &export.rest_arg_name {
            call_args.push(quote!(#rest_name));
        }
        let call = quote! { super::#mod_name::#fn_name(#(#call_args),*) };
        let nargs =
            length_override.map_or_else(|| required_arg_count(&export.params), |n| n as u32);
        let native_name = format_ident!("__native_fnclass_{}", unraw(&fn_name));

        if member == FunctionClassMember::Constructor && !has_this {
            // A factory: both a call and a construction return what the function returns.
            let rest_arg = export
                .rest_arg_name
                .as_ref()
                .map(|name| (name, export.rest_inner_type.as_ref()));
            natives.push(emit_native_fn(
                &native_name,
                &fn_name.to_string(),
                &binding_site(quote! { #js_name }, "constructor", "Constructor"),
                &export.return_style,
                &export.params,
                0,
                &call,
                &quote! {},
                &quote! {},
                rest_arg,
                None,
            ));
            constructor = Some((native_name, nargs));
            continue;
        }
        if member == FunctionClassMember::Constructor {
            let arg_extractions =
                gen_arg_extractions(&export.params, quote!(&__args), false, quote!(&scope), 0);
            let rest_setup = gen_rest_setup(
                export.rest_arg_name.as_ref(),
                export.rest_inner_type.as_ref(),
                export.params.len(),
                &quote!(__args),
                false,
                &quote!(&scope),
            );
            let site = binding_site(quote! { #js_name }, "constructor", "Constructor");
            natives.push(quote! {
                #[allow(non_snake_case)]
                unsafe extern "C" fn #native_name(
                    raw_cx: *mut ::js::native::RawJSContext,
                    argc: u32,
                    vp: *mut ::js::native::Value,
                ) -> bool {
                    #site
                    let scope = unsafe { ::js::gc::scope::RootScope::from_current_realm(raw_cx) };
                    let __args = ::js::native::CallArgs::from_vp(vp, argc);
                    let mut __constructing = __args.is_constructing();
                    if #class_syntax && !__constructing {
                        ::js::error::throw_type_error(&scope, #no_new_message);
                        return false;
                    }
                    let this = if __constructing {
                        match __new_for_constructor(&scope, &__args, #property_count) {
                            Ok(__o) => __o,
                            Err(_) => return false,
                        }
                    } else if #self_construct && !match is_instance(&scope, scope.root_value(__args.thisv().get())) {
                        Ok(__instance) => __instance,
                        Err(_) => return false,
                    } {
                        __constructing = true;
                        match new_instance(&scope) {
                            Ok(__o) => __o,
                            Err(_) => return false,
                        }
                    } else {
                        let __this = match ::js::Object::from_value(&scope, __args.thisv().get()) {
                            Ok(__o) => __o,
                            Err(_) => {
                                ::js::error::throw_type_error(
                                    &scope,
                                    c"Class constructor called on a value that isn't an object",
                                );
                                return false;
                            }
                        };
                        if __ensure_state(&scope, __this).is_err() {
                            return false;
                        }
                        __this
                    };
                    #(#arg_extractions)*
                    #rest_setup
                    if let Err(__e) = #call {
                        ::js::error::ThrowException::throw(__e, &scope);
                        return false;
                    }
                    if __constructing {
                        __args.rval().set(unsafe { ::js::value::from_object(this.as_raw()) });
                    } else {
                        __args.rval().set(::js::value::undefined());
                    }
                    true
                }
            });
            constructor = Some((native_name, nargs));
            continue;
        }

        let this_extraction = if has_this {
            let message = format!(
                "{js_name}.prototype.{} called on a value that isn't an object\0",
                export.js_name
            );
            let message = proc_macro2::Literal::byte_string(message.as_bytes());
            quote! {
                let this = match ::js::Object::from_value(&scope, __args.thisv().get()) {
                    Ok(__o) => __o,
                    Err(_) => {
                        ::js::error::throw_type_error(&scope, unsafe {
                            ::std::ffi::CStr::from_bytes_with_nul_unchecked(#message)
                        });
                        return false;
                    }
                };
            }
        } else {
            quote! {}
        };
        let rest_arg = export
            .rest_arg_name
            .as_ref()
            .map(|name| (name, export.rest_inner_type.as_ref()));
        let kind = if member == FunctionClassMember::StaticMethod {
            "StaticMethod"
        } else {
            "Method"
        };
        natives.push(emit_native_fn(
            &native_name,
            &fn_name.to_string(),
            &binding_site(quote! { #js_name }, &export.js_name, kind),
            &export.return_style,
            &export.params,
            0,
            &call,
            &this_extraction,
            &quote! {},
            rest_arg,
            None,
        ));
        let js_member = cstr_literal(&export.js_name);
        let entry = quote! { (#js_member, #native_name as unsafe extern "C" fn(*mut ::js::native::RawJSContext, u32, *mut ::js::native::Value) -> bool, #nargs) };
        if member == FunctionClassMember::StaticMethod {
            statics.push(entry);
        } else {
            methods.push(entry);
        }
    }

    let Some((ctor_native, ctor_nargs)) = constructor else {
        return Err(syn::Error::new_spanned(
            &input,
            "#[jsfunction_class] requires a #[constructor] function",
        ));
    };
    let js_name_cstr = cstr_literal(&js_name);
    let (parent_setup, proto_parent) = match &opts.extends {
        Some(parent) => (
            quote! {
                let __parent = #parent::register(scope)?;
                let __parent_proto = ::js::Object::from_value(
                    scope,
                    __parent.get_property(scope, c"prototype")?,
                ).map_err(|__e| __e.throw(scope))?;
                __ctor.set_prototype(scope, __parent.handle())?;
            },
            quote! { __proto.set_prototype(scope, __parent_proto.handle())?; },
        ),
        None => (quote! {}, quote! {}),
    };
    let setup_call = match &setup {
        Some(setup) => quote! { super::#mod_name::#setup(scope, __ctor, __proto)?; },
        None => quote! {},
    };
    let (new_for_constructor, new_with_proto, ensure_state) = match (&opts.state, &opts.extends) {
        (Some(state), _) => (
            quote! {
                let __state = ::js::class::create_instance_for_constructor::<
                    <#state<'s> as ::js::class::InstanceState<'s>>::Class,
                >(scope, args, ::std::default::Default::default(), property_count)?;
                Ok(__state.as_object())
            },
            quote! {
                let __state = ::js::class::create_instance_with_proto::<
                    <#state<'s> as ::js::class::InstanceState<'s>>::Class,
                >(scope, proto, ::std::default::Default::default(), property_count)?;
                Ok(__state.as_object())
            },
            quote! {
                if ::js::class::find_instance_state::<#state<'_>>(scope, this, false)?.is_some() {
                    return Ok(());
                }
                let __handle = ::js::class::create_instance_with::<
                    <#state<'_> as ::js::class::InstanceState<'_>>::Class,
                >(scope, |_| ::std::default::Default::default())?;
                ::js::class::set_state_handle(scope, this, __handle.as_object())
            },
        ),
        (None, Some(parent)) => (
            quote! { #parent::__new_for_constructor(scope, args, property_count) },
            quote! { #parent::__new_with_proto(scope, proto, property_count) },
            quote! { #parent::__ensure_state(scope, this) },
        ),
        (None, None) => (
            quote! { ::js::Object::new_plain_for_constructor(scope, args, property_count) },
            quote! {
                if property_count == 0 {
                    ::js::Object::new_plain_with_proto(scope, proto.handle())
                } else {
                    ::js::Object::new_with_property_capacity(scope, None, Some(proto), property_count)
                }
            },
            quote! { Ok(()) },
        ),
    };
    let invalid_this = match &opts.invalid_this {
        Some(path) => quote! { #path(scope, #js_name) },
        None => {
            let message = cstr_literal(&format!("Value of \"this\" must be of type {js_name}"));
            quote! { ::js::error::throw_type_error(scope, #message) }
        }
    };
    let state_items = match &opts.state {
        Some(state) => quote! {
            /// The Rust state of `this`, as `js::class::find_instance_state` finds it, including
            /// through the prototype chain.
            #[allow(dead_code)]
            pub fn try_state_of<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
                this: ::js::Object<'s>,
            ) -> ::std::result::Result<::std::option::Option<#state<'s>>, ::js::error::ExnThrown> {
                ::js::class::find_instance_state(scope, this, true)
            }

            /// The Rust state of `this`, as `try_state_of` finds it, or an exception for a `this`
            /// without state.
            #[allow(dead_code)]
            pub fn state_of<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
                this: ::js::Object<'s>,
            ) -> ::std::result::Result<#state<'s>, ::js::error::ExnThrown> {
                try_state_of(scope, this)?.ok_or_else(|| #invalid_this)
            }
        },
        None => quote! {},
    };

    Ok(quote! {
        #mod_vis mod #mod_name {
            #(#out_items)*

            #(#natives)*

            /// Marker keying this class's constructor in the realm's class registry.
            #[doc(hidden)]
            pub struct __FunctionClassMarker;

            /// Marker keying this class's prototype in the realm's class registry.
            #[doc(hidden)]
            pub struct __PrototypeMarker;

            #state_items

            /// A new instance for the constructor call `args`, whose prototype is `new.target`'s.
            /// A plain object gets fixed slots for `property_count` properties.
            #[doc(hidden)]
            pub fn __new_for_constructor<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
                args: &::js::native::CallArgs,
                property_count: usize,
            ) -> ::std::result::Result<::js::Object<'s>, ::js::error::ExnThrown> {
                #new_for_constructor
            }

            /// A new instance whose prototype is `proto`, with fixed slots for `property_count`
            /// properties.
            #[doc(hidden)]
            pub fn __new_with_proto<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
                proto: ::js::Object<'s>,
                property_count: usize,
            ) -> ::std::result::Result<::js::Object<'s>, ::js::error::ExnThrown> {
                #new_with_proto
            }

            /// Give `this`, which the constructor initializes without having created it, a handle
            /// that holds its state, unless it holds state already.
            #[doc(hidden)]
            #[allow(unused_variables)]
            pub fn __ensure_state(
                scope: &::js::gc::scope::Scope<'_>,
                this: ::js::Object<'_>,
            ) -> ::std::result::Result<(), ::js::error::ExnThrown> {
                #ensure_state
            }

            /// The class's prototype in the current realm, or a `TypeError` if the class isn't
            /// registered in it.
            #[allow(dead_code)]
            pub fn prototype_object<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
            ) -> ::std::result::Result<::js::Object<'s>, ::js::error::ExnThrown> {
                ::js::class::realm_object::<__PrototypeMarker>(scope)
                    .ok_or_else(|| __not_registered(scope))
            }

            /// Throw the `TypeError` for a class that isn't registered in the current realm.
            #[cold]
            fn __not_registered(scope: &::js::gc::scope::Scope<'_>) -> ::js::error::ExnThrown {
                ::js::error::throw_type_error(scope, #not_registered_message)
            }

            /// Whether `value` is an object whose prototype chain contains the class's prototype.
            #[allow(dead_code)]
            pub fn is_instance(
                scope: &::js::gc::scope::Scope<'_>,
                value: ::js::prelude::HandleValue<'_>,
            ) -> ::std::result::Result<bool, ::js::error::ExnThrown> {
                let ::std::result::Result::Ok(__object) = ::js::Object::from_value(scope, *value) else {
                    return Ok(false);
                };
                let __prototype = prototype_object(scope)?;
                let mut __current = __object.get_prototype(scope)?;
                while let ::std::option::Option::Some(__link) = __current {
                    if __link.as_raw() == __prototype.as_raw() {
                        return Ok(true);
                    }
                    __current = __link.get_prototype(scope)?;
                }
                Ok(false)
            }

            /// A new instance whose prototype is the class's prototype, as `new` creates it before
            /// the constructor initializes it.
            #[allow(dead_code)]
            pub fn new_instance<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
            ) -> ::std::result::Result<::js::Object<'s>, ::js::error::ExnThrown> {
                __new_with_proto(scope, prototype_object(scope)?, #property_count)
            }

            /// The class's constructor in the current realm, or a `TypeError` if the class isn't
            /// registered in it.
            pub fn class_constructor<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
            ) -> ::std::result::Result<::js::Object<'s>, ::js::error::ExnThrown> {
                ::js::class::realm_object::<__FunctionClassMarker>(scope)
                    .ok_or_else(|| __not_registered(scope))
            }

            /// Create the class's constructor and prototype in the current realm, unless they
            /// exist, and return the constructor. A global registers its classes when it is set
            /// up.
            pub fn register<'s>(
                scope: &'s ::js::gc::scope::Scope<'_>,
            ) -> ::std::result::Result<::js::Object<'s>, ::js::error::ExnThrown> {
                if let Some(__ctor) = ::js::class::realm_object::<__FunctionClassMarker>(scope) {
                    return Ok(__ctor);
                }
                let __ctor_fn = ::js::Function::new(
                    scope,
                    Some(#ctor_native),
                    #ctor_nargs,
                    ::js::native::JSFUN_CONSTRUCTOR,
                    #js_name_cstr,
                )?;
                let __ctor: ::js::Object<'s> = *__ctor_fn;
                let __proto = ::js::Object::new_plain(scope)?;
                #parent_setup
                #proto_parent
                for (__name, __native, __nargs) in [#(#methods),*] {
                    ::js::Function::define(scope, __proto.handle(), __name, Some(__native), __nargs, #member_attrs)?;
                }
                for (__name, __native, __nargs) in [#(#statics),*] {
                    ::js::Function::define(scope, __ctor.handle(), __name, Some(__native), __nargs, #member_attrs)?;
                }
                // The function's own `prototype` and the prototype's `constructor` are writable
                // and non-enumerable, as for a function declaration.
                __ctor.define_property(scope, c"prototype", __proto, ::js::class_spec::JSPROP_PERMANENT as u32)?;
                __proto.define_property(scope, c"constructor", __ctor, 0)?;
                #setup_call
                ::js::class::register_realm_object::<__FunctionClassMarker>(scope, __ctor);
                ::js::class::register_realm_object::<__PrototypeMarker>(scope, __proto);
                Ok(__ctor)
            }
        }
    })
}

// ============================================================================
// #[jsglobals] attribute macro
// ============================================================================

/// Attribute macro that transforms a `mod` block into a set of global JS definitions.
///
/// Public functions become callable JS functions on the global object, renamed
/// to camelCase. `#[js_name = "..."]` on a function sets its JS name instead.
/// Public `const` items become properties on the global object
/// under their declared name: `PI` stays `PI`, not `pi`.
/// `pub use ClassName;` items register `#[jsclass]` classes on the global.
///
/// # Usage
///
/// ```rust,ignore
/// #[jsglobals]
/// mod my_math {
///     pub use super::MyExtendedMath; // registers MyExtendedMath on the global
///     pub const PI: f64 = 3.14159;
///     pub fn add(a: f64, b: f64) -> f64 { a + b }
/// }
///
/// // Install on global:
/// my_math::add_to_global(&scope, global);
///
/// // Call from Rust:
/// assert_eq!(my_math::add(1.0, my_math::PI), 4.14159);
/// ```
///
/// Call from JS:
///
/// ```js
/// console.log(add(1, PI)); // prints 4.14159
/// ```
///
/// The attribute takes no options.
#[proc_macro_attribute]
pub fn jsglobals(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::TokenStream::from(attr)
                .into_iter()
                .next()
                .unwrap()
                .span(),
            "#[jsglobals] takes no options",
        )
        .to_compile_error()
        .into();
    }
    let input = parse_macro_input!(item as syn::ItemMod);

    let mod_name = &input.ident;
    let mod_vis = &input.vis;

    let ModExports {
        fns: fn_exports,
        consts: const_exports,
        class_reexports,
        items: original_items,
    } = match collect_mod_exports(&input, "#[jsglobals]", true) {
        Ok(exports) => exports,
        Err(e) => return e.to_compile_error().into(),
    };

    // Generate JSNative wrappers for each function export
    let mut native_fns: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut install_calls: Vec<proc_macro2::TokenStream> = Vec::new();

    for exp in &fn_exports {
        let native_name = format_ident!("__native_global_{}", unraw(&exp.fn_name));
        let js_name = &exp.js_name;
        let nargs = required_arg_count(&exp.params);
        let length = exp.length.unwrap_or(nargs);

        native_fns.push(gen_free_fn_native(
            exp,
            &native_name,
            &quote!(super::#mod_name),
            nargs,
        ));

        let js_name_cstr_lit = cstr_literal(js_name);

        install_calls.push(quote! {
            ::js::Function::define(
                scope,
                global.handle(),
                #js_name_cstr_lit,
                Some(#native_name),
                #length,
                ::js::class_spec::JSPROP_ENUMERATE as u32
            ).unwrap();
        });
    }

    // Generate install calls for constants
    for exp in &const_exports {
        let const_name = &exp.const_name;
        let js_name_cstr_lit = cstr_literal(&exp.js_name);

        // For reference-type constants (e.g. `&str`), pass the value directly
        // since it's already a reference. For value types, add `&`.
        let value_expr = if exp.is_ref_type {
            quote! { super::#mod_name::#const_name }
        } else {
            quote! { &super::#mod_name::#const_name }
        };

        install_calls.push(quote! {
            global.define_property(
                scope,
                #js_name_cstr_lit,
                #value_expr,
                ::js::class_spec::JSPROP_ENUMERATE as u32
            ).unwrap();
        });
    }

    // Generate class registration calls — classes are accessed via the
    // `pub use` items that bring them into this module's scope.
    let class_install_calls: Vec<proc_macro2::TokenStream> = class_reexports
        .iter()
        .map(|class_name| {
            quote! {
                #class_name::add_to_global(scope, global);
            }
        })
        .collect();

    let output = quote! {
        #mod_vis mod #mod_name {
            #(#original_items)*

            #(#native_fns)*

            /// Install all global functions, constants, and classes onto the given global object.
            pub fn add_to_global<'scope>(
                scope: &'scope ::js::gc::scope::Scope<'_>,
                global: ::js::Object<'scope>,
            ) {
                #(#class_install_calls)*
                #(#install_calls)*
            }
        }
    };

    output.into()
}

// ============================================================================
// #[jsnamespace] / #[webidl_namespace] attribute macros
// ============================================================================

/// Controls codegen differences between `#[jsnamespace]` and `#[webidl_namespace]`.
struct NamespaceConfig {
    /// When `true`, automatically set `Symbol.toStringTag` to the namespace
    /// name (per WebIDL §3.13).
    auto_to_string_tag: bool,
}

impl NamespaceConfig {
    /// Configuration for plain `#[jsnamespace]`: no Symbol.toStringTag.
    const JSNAMESPACE: Self = Self {
        auto_to_string_tag: false,
    };

    /// Configuration for `#[webidl_namespace]`: auto Symbol.toStringTag.
    const WEBIDL_NAMESPACE: Self = Self {
        auto_to_string_tag: true,
    };
}

/// Attribute macro that transforms a `mod` block into a namespace object
/// installed on the global.
///
/// Public functions become methods on the namespace object, renamed to
/// camelCase. `#[js_name = "..."]` on a function sets its JS name instead.
/// No constructor, no prototype chain.
///
/// # Usage
///
/// ```rust,ignore
/// #[jsnamespace(name = "console")]
/// mod console_ns {
///     pub fn log(scope: &Scope<'_>, args: RestArgs<HandleValue<'_>>) {
///         // ...
///     }
/// }
///
/// // Install on global:
/// console_ns::add_to_global(&scope, global);
/// ```
#[proc_macro_attribute]
pub fn jsnamespace(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = parse_macro_input!(attr as AttrOpts);
    let input = parse_macro_input!(item as syn::ItemMod);
    process_namespace(opts, input, NamespaceConfig::JSNAMESPACE)
}

/// Attribute macro for WebIDL namespace definitions.
///
/// Identical to `#[jsnamespace]` but with WebIDL §3.13 semantics:
/// - `Symbol.toStringTag` is automatically set to the namespace name
///
/// # Usage
///
/// ```rust,ignore
/// #[webidl_namespace(name = "CSS")]
/// mod css_ns {
///     pub fn escape(value: String) -> String {
///         // ...
///     }
/// }
/// ```
#[proc_macro_attribute]
pub fn webidl_namespace(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = parse_macro_input!(attr as AttrOpts);
    let input = parse_macro_input!(item as syn::ItemMod);
    process_namespace(opts, input, NamespaceConfig::WEBIDL_NAMESPACE)
}

/// Shared implementation for `#[jsnamespace]` and `#[webidl_namespace]`.
fn process_namespace(opts: AttrOpts, input: syn::ItemMod, config: NamespaceConfig) -> TokenStream {
    let mod_name = &input.ident;
    let mod_vis = &input.vis;
    let js_ns_name = opts
        .name
        .unwrap_or_else(|| mod_name.to_string().to_lower_camel_case());

    let ModExports {
        fns: fn_exports,
        consts: const_exports,
        items: original_items,
        ..
    } = match collect_mod_exports(&input, "#[jsnamespace]", false) {
        Ok(exports) => exports,
        Err(e) => return e.to_compile_error().into(),
    };

    // Generate JSNative wrappers and install calls for each function
    let mut native_fns: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut install_calls: Vec<proc_macro2::TokenStream> = Vec::new();

    for exp in &fn_exports {
        let native_name = format_ident!("__native_ns_{}", unraw(&exp.fn_name));
        let nargs = required_arg_count(&exp.params);
        let length = exp.length.unwrap_or(nargs);

        native_fns.push(gen_free_fn_native(
            exp,
            &native_name,
            &quote!(super::#mod_name),
            if opts.node_args { 0 } else { nargs },
        ));

        let js_name_cstr_lit = cstr_literal(&exp.js_name);

        install_calls.push(quote! {
            ::js::Function::define(
                scope,
                ns_handle,
                #js_name_cstr_lit,
                Some(#native_name),
                #length,
                ::js::class_spec::JSPROP_ENUMERATE as u32
            ).unwrap();
        });
    }

    // Install constants on the namespace object. Namespace members are
    // enumerable, configurable, writable data properties (WebIDL §3.13).
    for exp in &const_exports {
        let const_name = &exp.const_name;
        let js_name_cstr_lit = cstr_literal(&exp.js_name);
        let value_expr = if exp.is_ref_type {
            quote! { #const_name }
        } else {
            quote! { &#const_name }
        };
        install_calls.push(quote! {
            ns_obj.define_property(
                scope,
                #js_name_cstr_lit,
                #value_expr,
                ::js::class_spec::JSPROP_ENUMERATE as u32,
            ).unwrap();
        });
    }

    // Generate Symbol.toStringTag installation for webidl_namespace
    let to_string_tag_install = if config.auto_to_string_tag {
        quote! {
            ::js::class::define_to_string_tag(scope, ns_handle, #js_ns_name);
        }
    } else {
        quote! {}
    };

    let js_ns_name_bytes = format!("{js_ns_name}\0");
    let js_ns_name_cstr = proc_macro2::Literal::byte_string(js_ns_name_bytes.as_bytes());

    let output = quote! {
        #mod_vis mod #mod_name {
            #(#original_items)*

            #(#native_fns)*

            /// Install this namespace onto the given global object.
            pub fn add_to_global<'scope>(
                scope: &'scope ::js::gc::scope::Scope<'_>,
                global: ::js::Object<'scope>,
            ) {
                // Create a plain object for the namespace.
                let ns_obj = ::js::Object::new_plain(scope)
                    .expect("failed to create namespace object");
                let ns_handle = ns_obj.handle();

                // Install functions on the namespace object.
                #(#install_calls)*

                // Install Symbol.toStringTag if applicable.
                #to_string_tag_install

                // Install the namespace object on the global as a
                // non-enumerable, configurable, writable data property (WebIDL
                // §3.13 — defined, not assigned via [[Set]]).
                let ns_name = unsafe {
                    ::std::ffi::CStr::from_bytes_with_nul_unchecked(#js_ns_name_cstr)
                };
                global.define_property(scope, ns_name, ns_obj, 0)
                    .expect("failed to define namespace on global");
            }
        }
    };

    output.into()
}

// ============================================================================
// #[webidl_dictionary] attribute macro
// ============================================================================

/// Attribute macro that makes a struct usable as a WebIDL dictionary.
///
/// Generates a `FromJSVal` implementation that extracts named properties from
/// a JS object, following the [WebIDL dictionary conversion semantics]
/// (https://webidl.spec.whatwg.org/#es-dictionary).
///
/// # Field mapping
///
/// Each field's Rust `snake_case` name is converted to `camelCase` for the
/// JS property lookup. Override the JS name with `#[webidl(name = "...")]`.
///
/// # Lifetimes
///
/// If the struct contains GC references, it must carry a lifetime parameter
/// (e.g. `'a`) that is tied to the scope. Any field types that borrow from
/// the JS engine (`HandleValue<'a>`, `Object<'a>`, stack newtypes) use this
/// lifetime.
///
/// # Default values
///
/// Annotate a field with `#[webidl(default = <expr>)]` to provide a default
/// value used when the property is missing or `undefined`. Only valid on
/// non-`Option` fields (use `Option<T>` for "absent means `None`").
///
/// # Inheritance
///
/// `#[webidl_dictionary(extends = Parent)]` declares an inherited dictionary.
/// The struct holds its parent in a field named `parent`, and the members it
/// inherits are reached through it. `Deref` makes that transparent, so
/// `init.cancelable` works however deep the chain is.
///
/// The parent converts first, per [WebIDL's dictionary conversion][conv]:
/// inherited dictionaries are converted in order from least to most derived,
/// each one's own members in lexicographic order. Since the parent's own
/// conversion does the same for *its* parent, the whole chain lands in spec
/// order.
///
/// [conv]: https://webidl.spec.whatwg.org/#es-dictionary
///
/// # Usage
///
/// ```rust,ignore
/// #[webidl_dictionary]
/// pub struct QueuingStrategyInit<'a> {
///     pub high_water_mark: f64,
///     pub size: Option<Object<'a>>,
/// }
///
/// #[webidl_dictionary(extends = EventInit)]
/// pub struct CustomEventInit<'a> {
///     parent: EventInit,
///     pub detail: Option<HandleValue<'a>>,
/// }
/// ```
///
/// Then use as a parameter in a `#[jsmethods]` method:
///
/// ```rust,ignore
/// #[constructor]
/// fn new(init: QueuingStrategyInit<'_>) -> Self {
///     Self { high_water_mark: init.high_water_mark }
/// }
/// ```
#[proc_macro_attribute]
pub fn webidl_dictionary(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr: proc_macro2::TokenStream = attr.into();
    let extends = match parse_dictionary_extends(attr) {
        Ok(extends) => extends,
        Err(e) => return e.to_compile_error().into(),
    };
    let input = parse_macro_input!(item as ItemStruct);
    let struct_name = &input.ident;
    let vis = &input.vis;
    let attrs = &input.attrs;
    let generics = &input.generics;
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    // Determine the lifetime parameter. WebIDL dictionaries that contain
    // scope-rooted types must have exactly one lifetime parameter.
    let lifetime = generics.lifetimes().next().map(|lt| &lt.lifetime);

    let fields = match &input.fields {
        Fields::Named(f) => &f.named,
        _ => {
            return syn::Error::new_spanned(
                &input,
                "#[webidl_dictionary] requires a struct with named fields",
            )
            .to_compile_error()
            .into();
        }
    };

    // The `parent` field holds the inherited members and is not looked up as a property of its
    // own; the parent's conversion fills it.
    let parent_ty = match extends
        .as_ref()
        .map(|extends| {
            parent_field(&input, fields, extends, extends, "webidl_dictionary").map(|f| &f.ty)
        })
        .transpose()
    {
        Ok(ty) => ty,
        Err(e) => return e.to_compile_error().into(),
    };

    // Parse each field into a DictionaryMember.
    let mut members: Vec<DictMember> = Vec::new();
    // Set on a malformed or unknown `#[webidl(...)]` field option, turned into
    // a spanned error after the loop. Swallowing it would silently drop a
    // `name`/`default` override — e.g. a typo'd key would make a defaulted
    // member required and throw "Missing required dictionary member" at runtime.
    let mut dict_error: Option<syn::Error> = None;
    for field in fields {
        let ident = field.ident.as_ref().unwrap().clone();
        if parent_ty.is_some() && ident == "parent" {
            continue;
        }
        let ty = field.ty.clone();

        // Parse #[webidl(...)] attributes on this field.
        let mut js_name_override: Option<String> = None;
        let mut default_expr: Option<syn::Expr> = None;
        for attr in &field.attrs {
            if attr.path().is_ident("webidl") {
                if let Err(e) = attr.parse_nested_meta(|meta| {
                    if meta.path.is_ident("name") {
                        let value = meta.value()?;
                        let lit: LitStr = value.parse()?;
                        js_name_override = Some(lit.value());
                    } else if meta.path.is_ident("default") {
                        let value = meta.value()?;
                        let expr: syn::Expr = value.parse()?;
                        default_expr = Some(expr);
                    } else {
                        return Err(meta.error(
                            "unknown `webidl` option; expected `name = \"...\"` or `default = ...`",
                        ));
                    }
                    Ok(())
                }) {
                    dict_error.get_or_insert(e);
                }
            }
        }

        let js_name = js_name_override.unwrap_or_else(|| {
            let name = ident.to_string();
            // Strip raw identifier prefix (e.g. `r#type` → `type`).
            let name = name.strip_prefix("r#").unwrap_or(&name);
            name.to_lower_camel_case()
        });
        let optional = is_option_type(&ty);
        let inner_ty = if optional {
            extract_option_inner_type(&ty)
        } else {
            None
        };

        members.push(DictMember {
            ident,
            ty,
            inner_ty,
            js_name,
            optional,
            default_expr,
        });
    }

    if let Some(e) = dict_error {
        return e.to_compile_error().into();
    }

    // Sort members alphabetically by JS name (per WebIDL spec §3.2.17).
    members.sort_by(|a, b| a.js_name.cmp(&b.js_name));

    // Generate the property extraction code for each member.
    let member_extractions: Vec<proc_macro2::TokenStream> =
        members.iter().map(gen_dict_member_extraction).collect();

    // Generate field initializers in struct declaration order (not sorted).
    let field_idents: Vec<&Ident> = members.iter().map(|m| &m.ident).collect();

    // WebIDL converts the inherited dictionaries first, least derived first, and the parent's own
    // conversion does the same for its parent, so the chain arrives in that order.
    let parent_extraction = parent_ty.map(|ty| {
        quote! {
            let parent = <#ty as ::js::conversion::FromJSVal>::from_jsval(scope, val, ())?;
        }
    });
    let parent_init = parent_ty.map(|_| quote! { parent, });
    let parent_deref = parent_ty.map(|ty| {
        quote! {
            impl #impl_generics ::std::ops::Deref for #struct_name #ty_generics #where_clause {
                type Target = #ty;
                fn deref(&self) -> &Self::Target {
                    &self.parent
                }
            }

            impl #impl_generics ::std::ops::DerefMut for #struct_name #ty_generics #where_clause {
                fn deref_mut(&mut self) -> &mut Self::Target {
                    &mut self.parent
                }
            }
        }
    });

    let (impl_generics_for_conv, trait_lifetimes, self_ty, scope_lt, value_lt) = match lifetime {
        Some(lt) => (
            quote!(<#lt, 'v>),
            quote!(#lt, 'v),
            quote!(#struct_name<#lt>),
            quote!(#lt),
            quote!('v),
        ),
        None => (
            quote!(),
            quote!('_, '_),
            quote!(#struct_name),
            quote!('_),
            quote!('_),
        ),
    };
    let from_jsval_impl = quote! {
        impl #impl_generics_for_conv ::js::conversion::FromJSVal<#trait_lifetimes> for #self_ty {
            type Config = ();

            fn from_jsval(
                scope: &#scope_lt ::js::prelude::Scope<#scope_lt>,
                val: ::js::prelude::HandleValue<#value_lt>,
                _option: (),
            ) -> ::std::result::Result<Self, ::js::conversion::ConversionError> {
                // WebIDL §3.2.17: If V is undefined or null, treat as empty dict. If V is not an
                // object, throw a TypeError — which is what `from_value` reports, so it is the
                // only object test needed.
                let __obj = if val.get().is_null_or_undefined() {
                    None
                } else {
                    Some(
                        ::js::Object::from_value(scope, *val)
                            .map_err(|_| ::js::conversion::ConversionError::Failure(
                                ::std::borrow::Cow::Borrowed(c"dictionary value is not an object"),
                            ))?
                    )
                };

                #parent_extraction
                #(#member_extractions)*

                Ok(#struct_name {
                    #parent_init
                    #(#field_idents),*
                })
            }
        }
    };
    // Strip #[webidl(...)] attributes from the output struct's fields.
    let mut output_struct = input.clone();
    if let Fields::Named(ref mut named) = output_struct.fields {
        for field in &mut named.named {
            field.attrs.retain(|a| !a.path().is_ident("webidl"));
        }
    }

    let cleaned_fields = if let Fields::Named(ref named) = output_struct.fields {
        let field_tokens: Vec<_> = named
            .named
            .iter()
            .map(|f| {
                let field_attrs: Vec<_> = f
                    .attrs
                    .iter()
                    .filter(|a| !a.path().is_ident("webidl"))
                    .collect();
                let field_vis = &f.vis;
                let field_ident = &f.ident;
                let field_ty = &f.ty;
                quote! {
                    #(#field_attrs)*
                    #field_vis #field_ident: #field_ty
                }
            })
            .collect();
        field_tokens
    } else {
        vec![]
    };

    let output = quote! {
        #(#attrs)*
        #vis struct #struct_name #impl_generics #where_clause {
            #(#cleaned_fields),*
        }

        #from_jsval_impl
        #parent_deref
    };

    output.into()
}

/// Parse `#[webidl_dictionary]`'s optional `extends = Parent` argument.
///
/// The options share [`AttrOpts`] with the class attributes, so every other key
/// parses, and has to be rejected here, or `#[webidl_dictionary(hidden)]`
/// would be silently accepted and do nothing.
fn parse_dictionary_extends(attr: proc_macro2::TokenStream) -> syn::Result<Option<Ident>> {
    if attr.is_empty() {
        return Ok(None);
    }
    let opts: AttrOpts = syn::parse2(attr.clone())?;
    opts.reject_all_but(&["extends"], attr)?;
    Ok(opts.extends)
}

/// The `parent` field that a type declared with `extends = Parent` holds its
/// parent in, checked to be an `expected`.
///
/// Every `extends`, class, interface, or dictionary, embeds the parent in a
/// field of that name with the parent's type. Checking them here reports a
/// mismatch against the declaration that caused it, rather than as a missing-field
/// or type error inside a generated `Deref`/`HasParent`/`FromJSVal` impl.
fn parent_field<'a>(
    span: impl quote::ToTokens,
    fields: &'a syn::punctuated::Punctuated<syn::Field, Token![,]>,
    extends: &Ident,
    expected: &Ident,
    attribute: &str,
) -> syn::Result<&'a syn::Field> {
    let Some(field) = fields
        .iter()
        .find(|f| f.ident.as_ref().is_some_and(|i| i == "parent"))
    else {
        return Err(syn::Error::new_spanned(
            span,
            format!(
                "`#[{attribute}(extends = {extends})]` requires a field `parent: {expected}` to \
                 hold the inherited state"
            ),
        ));
    };
    let names_parent = match &field.ty {
        Type::Path(tp) => tp
            .path
            .segments
            .last()
            .is_some_and(|s| &s.ident == expected),
        _ => false,
    };
    if !names_parent {
        return Err(syn::Error::new_spanned(
            &field.ty,
            format!("the `parent` field must be a `{expected}`, matching `extends = {extends}`"),
        ));
    }
    Ok(field)
}

/// Information about a single dictionary member.
struct DictMember {
    ident: Ident,
    ty: Type,
    /// The inner type if this is `Option<T>`.
    inner_ty: Option<Type>,
    /// The JS property name (camelCase).
    js_name: String,
    /// Whether this is an `Option<T>` field (optional member).
    optional: bool,
    /// Default expression, if any (from `#[webidl(default = ...)]`).
    default_expr: Option<syn::Expr>,
}

/// Generate the extraction code for a single dictionary member.
fn gen_dict_member_extraction(member: &DictMember) -> proc_macro2::TokenStream {
    let ident = &member.ident;
    let js_name_cstr_lit = cstr_literal(&member.js_name);

    // Every arm reads `obj[js_name]` and branches on whether it is present
    // (not `undefined`). The three member kinds differ only in the binding (a
    // type annotation drives `from_jsval` inference for non-optional members),
    // the conversion type, and what an absent property yields.
    let (binding, conv_ty, absent) = if member.optional {
        // Optional member (Option<T>): None when property is missing/undefined.
        let inner_ty = member.inner_ty.as_ref().unwrap_or(&member.ty);
        (quote! { let #ident }, inner_ty, quote! { None })
    } else if let Some(default_expr) = &member.default_expr {
        // Required member with a default: use the default when missing/undefined.
        let ty = &member.ty;
        (quote! { let #ident: #ty }, ty, quote! { #default_expr })
    } else {
        // Required member without a default: TypeError when missing.
        let ty = &member.ty;
        let err_lit = cstr_literal(&format!(
            "Missing required dictionary member '{}'",
            member.js_name
        ));
        let absent = quote! {
            return Err(::js::conversion::ConversionError::Failure(
                ::std::borrow::Cow::Borrowed(#err_lit),
            ));
        };
        (quote! { let #ident: #ty }, ty, absent)
    };

    // The same conversions an argument gets, in the shape a dictionary member needs: a value
    // expression yielding `Result<_, ConversionError>` rather than a `get_*_arg` call. Optional
    // members wrap the converted value in `Some`.
    let core = match InputConversion::of(conv_ty) {
        // Wrapping here rather than at each read is what lets a promise attribute fed from this
        // member return the same object every time.
        InputConversion::CheckedPromise(value_ty) => {
            let check = promise_value_check(value_ty);
            quote! {
                ::js::PromiseOf::new(scope, __prop, #check)
                    .map_err(|_| ::js::conversion::ConversionError::ExnPending)?
            }
        }
        InputConversion::UncheckedPromise => quote! {
            ::js::Promise::new_resolved_with_value(scope, __prop)
                .map_err(|_| ::js::conversion::ConversionError::ExnPending)?
        },
        // A container forwards the behavior to its elements, so both spell it the same way.
        InputConversion::Integer | InputConversion::IntegerContainer => quote! {
            <#conv_ty as ::js::conversion::FromJSVal<'_, '_>>::from_jsval(
                scope, __prop, ::js::conversion::ConversionBehavior::Default,
            )?
        },
        InputConversion::Plain => {
            quote! { ::js::conversion::FromJSVal::from_jsval(scope, __prop, ())? }
        }
    };
    let convert = if member.optional {
        quote! { Some(#core) }
    } else {
        core
    };

    quote! {
        #binding = if let Some(ref __obj) = __obj {
            let __id = ::js::class::get_or_init_property_id(
                scope,
                #js_name_cstr_lit,
            )
            .map_err(|_| ::js::conversion::ConversionError::ExnPending)?;
            let __prop = __obj.get_property_by_id(scope, __id)
                .map_err(|_| ::js::conversion::ConversionError::ExnPending)?;
            if __prop.get().is_undefined() {
                #absent
            } else {
                #convert
            }
        } else {
            #absent
        };
    }
}

#[proc_macro_attribute]
pub fn must_root(attr: TokenStream, item: TokenStream) -> TokenStream {
    let attr: proc_macro2::TokenStream = attr.into();
    let item: proc_macro2::TokenStream = item.into();
    if attr.is_empty() {
        quote! {
            #[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
            #item
        }
    } else {
        // Forward parameters to crown, e.g. `must_root(allow_self_return)`
        // permits associated fns of the marked type to return it bare (the
        // class macros' trampolines immediately wrap such returns in a new
        // JS object).
        quote! {
            #[cfg_attr(crown, crown::unrooted_must_root_lint::must_root(#attr))]
            #item
        }
    }
    .into()
}

#[proc_macro_attribute]
pub fn allow_unrooted(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item: proc_macro2::TokenStream = item.into();
    quote! {
        #[cfg_attr(crown, allow(crown::unrooted_must_root))]
        #item
    }
    .into()
}

#[proc_macro_attribute]
pub fn allow_unrooted_interior(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item: proc_macro2::TokenStream = item.into();
    quote! {
        #[cfg_attr(crown, crown::unrooted_must_root_lint::allow_unrooted_interior)]
        #item
    }
    .into()
}

#[proc_macro_attribute]
pub fn allow_unrooted_interior_in_rc(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item: proc_macro2::TokenStream = item.into();
    quote! {
        #[cfg_attr(crown, crown::unrooted_must_root_lint::allow_unrooted_interior_in_rc)]
        #item
    }
    .into()
}

// ============================================================================
// WebIDL Union types — `#[webidl_union]`
// ============================================================================

/// Attribute macro for WebIDL union type definitions.
///
/// Applied to a Rust `enum` where each variant has exactly one unnamed field.
/// Generates `FromJSVal` and `ToJSVal` implementations following the WebIDL
/// §3.2.25 union type conversion algorithm.
///
/// Variant inner types are classified automatically:
/// - `bool` → boolean branch
/// - integer types (`i8`..`u64`) → numeric branch
/// - `f32`, `f64` → numeric branch
/// - `String` → string branch
/// - `Vec<T>` → sequence branch (checks `Symbol.iterator`)
/// - Other types → object/interface branch (uses `FromJSVal`)
///
/// The enum may carry a single lifetime parameter when its variants hold
/// scope-rooted types; that lifetime is bound to the scope's lifetime in the
/// generated impls.
///
/// # Example
///
/// ```rust,ignore
/// #[webidl_union]
/// pub enum StringOrUnsignedLong {
///     String(String),
///     UnsignedLong(u32),
/// }
///
/// #[webidl_union]
/// pub enum BufferOrString<'a> {
///     Buffer(ArrayBuffer<'a>),
///     Str(String),
/// }
/// ```
#[proc_macro_attribute]
pub fn webidl_union(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemEnum);
    process_webidl_union(input)
}

/// Classification of a union variant's inner type for conversion priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnionCategory {
    Boolean,
    Numeric,
    String,
    Sequence,
    Object,
}

fn classify_union_variant(ty: &Type) -> UnionCategory {
    let s = quote!(#ty).to_string();
    if s == "bool" {
        return UnionCategory::Boolean;
    }
    if is_integer_type(ty) || matches!(s.as_str(), "f32" | "f64") {
        return UnionCategory::Numeric;
    }
    if s == "String" {
        return UnionCategory::String;
    }
    if is_vec_type(ty) {
        return UnionCategory::Sequence;
    }
    UnionCategory::Object
}

fn process_webidl_union(input: ItemEnum) -> TokenStream {
    let enum_name = &input.ident;
    let vis = &input.vis;
    let attrs = &input.attrs;
    let generics = &input.generics;
    let where_clause = &generics.where_clause;

    // Unions whose variants reference scope-rooted types declare a single
    // lifetime parameter. Reject anything beyond one lifetime / type parameter
    // so the generated impl shape stays predictable.
    if generics.type_params().next().is_some() || generics.const_params().next().is_some() {
        return syn::Error::new_spanned(
            generics,
            "#[webidl_union] does not support type or const generic parameters",
        )
        .to_compile_error()
        .into();
    }
    let mut lifetimes = generics.lifetimes();
    let lifetime = lifetimes.next().map(|lt| lt.lifetime.clone());
    if lifetimes.next().is_some() {
        return syn::Error::new_spanned(
            generics,
            "#[webidl_union] accepts at most one lifetime parameter",
        )
        .to_compile_error()
        .into();
    }

    // Collect variant info: (variant_ident, inner_type, category).
    let mut variants_info = Vec::new();
    for variant in &input.variants {
        let fields = match &variant.fields {
            Fields::Unnamed(f) if f.unnamed.len() == 1 => f,
            _ => {
                return syn::Error::new_spanned(
                    variant,
                    "#[webidl_union] variants must have exactly one unnamed field",
                )
                .to_compile_error()
                .into();
            }
        };
        let inner_ty = &fields.unnamed[0].ty;
        let category = classify_union_variant(inner_ty);
        variants_info.push((&variant.ident, inner_ty, category));
    }

    // Build the conversion branches in WebIDL §3.2.25 priority order.
    // Priority within an object value:
    //   1. Sequence types (check Symbol.iterator)
    //   2. Interface/object types (try FromJSVal)
    // Priority for primitives:
    //   3. Boolean
    //   4. Numeric
    //   5. String (fallback — any value can be stringified)

    let mut sequence_branches = Vec::new();
    let mut object_only_branches = Vec::new();
    let mut boolean_branch = None;
    let mut numeric_branch = None;
    let mut string_branch = None;

    for (ident, inner_ty, category) in &variants_info {
        // A union has at most one member of each primitive category (WebIDL
        // distinguishability). Two would silently overwrite each other here,
        // leaving the earlier variant unreachable — reject it instead.
        let dup = |kind: &str| -> TokenStream {
            syn::Error::new_spanned(
                ident,
                format!(
                    "#[webidl_union] has more than one {kind} variant; union member \
                     types must be distinguishable"
                ),
            )
            .to_compile_error()
            .into()
        };
        match category {
            UnionCategory::Boolean => {
                if boolean_branch.is_some() {
                    return dup("boolean");
                }
                boolean_branch = Some((ident, inner_ty));
            }
            UnionCategory::Numeric => {
                if numeric_branch.is_some() {
                    return dup("numeric");
                }
                numeric_branch = Some((ident, inner_ty));
            }
            UnionCategory::String => {
                if string_branch.is_some() {
                    return dup("string");
                }
                string_branch = Some((ident, inner_ty));
            }
            UnionCategory::Sequence => {
                sequence_branches.push((ident, inner_ty));
            }
            UnionCategory::Object => {
                object_only_branches.push((ident, inner_ty));
            }
        }
    }

    // Generate the FromJSVal body.
    // The algorithm checks object branches first (when value is an object),
    // then primitive branches.
    let mut from_body = Vec::new();

    let has_object_branches = !sequence_branches.is_empty() || !object_only_branches.is_empty();
    if has_object_branches {
        let mut obj_checks = Vec::new();

        // WebIDL §3.2.25: when the value is an Object and the union includes a
        // sequence type, the sequence branch is selected iff the value has a
        // non-null `@@iterator`.
        if !sequence_branches.is_empty() {
            // Try sequence variants in declaration order. Within the iterable
            // arm, propagate `ExnPending` immediately and only fall through on
            // a soft `Failure` (so the rare case of multiple sequence variants
            // with different element types still works in declaration order).
            let mut seq_attempts = Vec::new();
            for (ident, inner_ty) in &sequence_branches {
                seq_attempts.push(quote! {
                    match <#inner_ty as ::js::conversion::FromJSVal>::from_jsval(
                        scope, val, Default::default(),
                    ) {
                        Ok(v) => return Ok(#enum_name::#ident(v)),
                        Err(::js::conversion::ConversionError::ExnPending) => {
                            return Err(::js::conversion::ConversionError::ExnPending);
                        }
                        // A soft failure falls through to the next sequence
                        // variant (declaration order).
                        Err(_) => {}
                    }
                });
            }

            // An iterable value belongs to a sequence member (WebIDL §3.2.25):
            // once `@@iterator` is present we commit to the sequence types and
            // do not fall back to the object/primitive members. If no sequence
            // variant accepted the value, that is a conversion failure.
            obj_checks.push(quote! {
                if ::js::conversion::is_iterable_value(scope, val)? {
                    #(#seq_attempts)*
                    return Err(::js::conversion::ConversionError::Failure(
                        c"Iterable value cannot be converted to the union's sequence member".into(),
                    ));
                }
            });
        }

        // Interface / record / object types: try in declaration order. Always
        // propagate `ExnPending` (a pending JS exception cannot be silently
        // recovered from); fall through only on soft `Failure` so the next
        // variant gets a chance.
        for (ident, inner_ty) in &object_only_branches {
            obj_checks.push(quote! {
                match <#inner_ty as ::js::conversion::FromJSVal>::from_jsval(
                    scope, val, Default::default(),
                ) {
                    Ok(v) => return Ok(#enum_name::#ident(v)),
                    Err(::js::conversion::ConversionError::ExnPending) => {
                        return Err(::js::conversion::ConversionError::ExnPending);
                    }
                    Err(_) => {}
                }
            });
        }

        // Object/sequence members are only attempted for actual objects.
        // WebIDL §3.2.25 also routes `null`/`undefined` to a dictionary member
        // when the union has one, but none of our unions do, and routing them
        // here would change the behavior of object-only unions like
        // `HeadersInit` (where `null` must fall through to the terminal error,
        // not become an empty record). Left unimplemented deliberately.
        // TODO: "none of our unions do" is a very temporary claim.
        from_body.push(quote! {
            if val.get().is_object() {
                #(#obj_checks)*
            }
        });
    }

    // Boolean branch. The fast path matches an actual boolean value. When no
    // string or numeric member can serve as the §3.2.25 fallback, a boolean
    // member is the fallback: any value coerces to it via `ToBoolean` (which
    // never fails), so it becomes the terminal expression below rather than a
    // guarded branch.
    let boolean_is_fallback =
        boolean_branch.is_some() && string_branch.is_none() && numeric_branch.is_none();
    if let Some((ident, _inner_ty)) = &boolean_branch {
        if !boolean_is_fallback {
            from_body.push(quote! {
                if val.get().is_boolean() {
                    return Ok(#enum_name::#ident(val.get().to_boolean()));
                }
            });
        }
    }

    // Numeric branch. The fast path matches an actual number; when there is no
    // string member, a numeric member is the §3.2.25 fallback and any value
    // coerces to it via `ToNumber`.
    if let Some((ident, inner_ty)) = &numeric_branch {
        let numeric_conversion = if is_integer_type(inner_ty) {
            quote! {
                <#inner_ty as ::js::conversion::FromJSVal>::from_jsval(
                    scope, val, ::js::conversion::ConversionBehavior::Default,
                )
            }
        } else {
            quote! {
                <#inner_ty as ::js::conversion::FromJSVal>::from_jsval(
                    scope, val, Default::default(),
                )
            }
        };
        if string_branch.is_none() {
            from_body.push(quote! {
                match #numeric_conversion {
                    Ok(v) => return Ok(#enum_name::#ident(v)),
                    Err(::js::conversion::ConversionError::ExnPending) => {
                        return Err(::js::conversion::ConversionError::ExnPending);
                    }
                    // A non-numeric value that `ToNumber` rejects without a
                    // pending exception falls through to the terminal union
                    // error below.
                    Err(_) => {}
                }
            });
        } else {
            from_body.push(quote! {
                if val.get().is_number() || val.get().is_int32() {
                    match #numeric_conversion {
                        Ok(v) => return Ok(#enum_name::#ident(v)),
                        Err(::js::conversion::ConversionError::ExnPending) => {
                            return Err(::js::conversion::ConversionError::ExnPending);
                        }
                        Err(_) => {}
                    }
                }
            });
        }
    }

    // String branch (fallback — any value can be converted to string).
    if let Some((ident, inner_ty)) = &string_branch {
        from_body.push(quote! {
            match <#inner_ty as ::js::conversion::FromJSVal>::from_jsval(
                scope, val, Default::default(),
            ) {
                Ok(v) => return Ok(#enum_name::#ident(v)),
                Err(::js::conversion::ConversionError::ExnPending) => {
                    return Err(::js::conversion::ConversionError::ExnPending);
                }
                Err(_) => {}
            }
        });
    }

    // Terminal: a boolean fallback coerces any remaining value via `ToBoolean`
    // (WebIDL §3.2.25); otherwise no member matched and conversion fails.
    let terminal = match (boolean_is_fallback, &boolean_branch) {
        (true, Some((ident, _))) => quote! {
            Ok(#enum_name::#ident(
                <bool as ::js::conversion::FromJSVal>::from_jsval(scope, val, ())?,
            ))
        },
        _ => quote! {
            Err(::js::conversion::ConversionError::Failure(
                c"Value cannot be converted to the expected union type".into(),
            ))
        },
    };

    // Generate ToJSVal arms.
    let to_arms: Vec<_> = variants_info
        .iter()
        .map(|(ident, _inner_ty, _cat)| {
            quote! {
                #enum_name::#ident(inner) => inner.to_jsval_raw(scope),
            }
        })
        .collect();

    // Rebuild the enum variants for the output.
    let variant_defs: Vec<_> = input
        .variants
        .iter()
        .map(|v| {
            let id = &v.ident;
            let fields = &v.fields;
            let attrs = &v.attrs;
            quote! { #(#attrs)* #id #fields }
        })
        .collect();

    // When the enum carries a lifetime parameter, reuse it as the scope
    // lifetime so variants like `Stack<'a, T>` line up with the trait's `'s`.
    let scope_lt = match &lifetime {
        Some(lt) => quote! { #lt },
        None => quote! { 's },
    };
    let enum_ty = match &lifetime {
        Some(_) => quote! { #enum_name<#scope_lt> },
        None => quote! { #enum_name },
    };

    let output = quote! {
        #(#attrs)*
        #vis enum #enum_name #generics #where_clause {
            #(#variant_defs,)*
        }

        impl<#scope_lt, 'v> ::js::conversion::FromJSVal<#scope_lt, 'v> for #enum_ty {
            type Config = ();
            fn from_jsval(
                scope: &#scope_lt ::js::prelude::Scope<#scope_lt>,
                val: ::js::prelude::HandleValue<'v>,
                _: (),
            ) -> ::std::result::Result<Self, ::js::conversion::ConversionError> {
                #(#from_body)*

                #terminal
            }
        }

        impl<#scope_lt> ::js::conversion::ToJSVal<#scope_lt> for #enum_ty {
            fn to_jsval_raw(
                &self,
                scope: &#scope_lt ::js::prelude::Scope<#scope_lt>,
            ) -> ::std::result::Result<::js::value::Value, ::js::conversion::ConversionError> {
                match self {
                    #(#to_arms)*
                }
            }
        }
    };

    output.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a type from source, for the type-shape predicates below.
    fn parse_ty(source: &str) -> Type {
        syn::parse_str::<Type>(source).expect("test input parses as a type")
    }

    /// A type as source text, with `quote!`'s inter-token spaces removed so the
    /// expectations below can be written the way the type is spelled.
    fn ty_string(ty: &Type) -> String {
        quote!(#ty).to_string().replace(' ', "")
    }

    /// Without a receiver, the return type alone marks a constructor — the
    /// name is irrelevant.
    #[test]
    fn implicit_constructor_detects_self_returns_by_shape() {
        let sig = |s: &str| syn::parse_str::<syn::Signature>(s).unwrap();
        let url = format_ident!("URL");
        assert!(is_implicit_constructor(&sig("fn new() -> Self"), &url));
        assert!(is_implicit_constructor(
            &sig("fn from_parts(a: String) -> Self"),
            &url
        ));
        assert!(is_implicit_constructor(&sig("fn make() -> URL<'s>"), &url));
    }

    /// A fallible factory returning `Result<Self, E>` is constructor-shaped
    /// too — but only for a literal `Self` Ok type. A named-newtype Ok type
    /// (`Result<URL<'s>, E>`) marks a helper that allocates the JS object
    /// itself (e.g. `ReadableStream::new_native`) and returns the rooted
    /// newtype, whose lifetime `Self` could not express.
    #[test]
    fn implicit_constructor_detects_fallible_self_returns() {
        let sig = |s: &str| syn::parse_str::<syn::Signature>(s).unwrap();
        let url = format_ident!("URL");
        assert!(is_implicit_constructor(
            &sig("fn from_incoming(a: String) -> Result<Self, ExnThrown>"),
            &url
        ));
        assert!(!is_implicit_constructor(
            &sig("fn make() -> Result<URL<'s>, Error>"),
            &url
        ));
        assert!(!is_implicit_constructor(
            &sig("fn all() -> Result<Vec<URL<'s>>, Error>"),
            &url
        ));
        assert!(!is_implicit_constructor(
            &sig("fn flush() -> Result<(), ExnThrown>"),
            &url
        ));
    }

    /// A no-receiver fn that returns anything else is a plain helper.
    #[test]
    fn implicit_constructor_rejects_non_instance_returns() {
        let sig = |s: &str| syn::parse_str::<syn::Signature>(s).unwrap();
        let url = format_ident!("URL");
        assert!(!is_implicit_constructor(&sig("fn new() -> i32"), &url));
        assert!(!is_implicit_constructor(&sig("fn helper()"), &url));
        assert!(!is_implicit_constructor(
            &sig("fn all() -> Vec<URL<'s>>"),
            &url
        ));
    }

    /// The setup-style shape (a receiver, no instance return) is
    /// indistinguishable from a helper method, so it is keyed on the name.
    #[test]
    fn implicit_constructor_detects_setup_style_by_name() {
        let sig = |s: &str| syn::parse_str::<syn::Signature>(s).unwrap();
        let url = format_ident!("URL");
        assert!(is_implicit_constructor(
            &sig("fn new(&self, url: String) -> Result<(), ExnThrown>"),
            &url
        ));
        assert!(is_implicit_constructor(
            &sig("fn new(&mut self) -> Result<(), ExnThrown>"),
            &url
        ));
        // Same shape, different name: an ordinary helper.
        assert!(!is_implicit_constructor(
            &sig("fn flush(&self) -> Result<(), ExnThrown>"),
            &url
        ));
    }

    #[test]
    fn is_self_or_instance_type_matches_bare_name() {
        assert!(is_self_or_instance_type("Self", "URL"));
        assert!(is_self_or_instance_type("URL", "URL"));
        assert!(is_self_or_instance_type("URL<'s>", "URL"));
        assert!(is_self_or_instance_type("URL<'_>", "URL"));
    }

    #[test]
    fn is_self_or_instance_type_rejects_nested() {
        assert!(!is_self_or_instance_type("Option<URL<'s>>", "URL"));
        assert!(!is_self_or_instance_type(
            "Result<Option<URL<'s>>,ExnThrown>",
            "URL"
        ));
        assert!(!is_self_or_instance_type("Vec<URL<'s>>", "URL"));
    }

    #[test]
    fn is_self_or_instance_type_rejects_substring_match() {
        assert!(!is_self_or_instance_type("URLSearchParams", "URL"));
        assert!(!is_self_or_instance_type("URLSearchParams<'s>", "URL"));
    }

    #[test]
    fn is_result_promise_type_detects_promise_ok() {
        let parse = |s: &str| syn::parse_str::<Type>(s).unwrap();
        // The `Ok` type carries a lifetime argument, which the bare-identifier
        // `is_promise_type` rejects — the dedicated check must still match.
        assert!(is_result_promise_type(&parse(
            "Result<Promise<'r>, ExnThrown>"
        )));
        assert!(is_result_promise_type(&parse(
            "Result<js::Promise<'r>, ExnThrown>"
        )));
        assert!(is_result_promise_type(&parse(
            "Result<PromiseFuture, ExnThrown>"
        )));
    }

    #[test]
    fn is_result_promise_type_rejects_non_promise() {
        let parse = |s: &str| syn::parse_str::<Type>(s).unwrap();
        assert!(!is_result_promise_type(&parse("Result<String, ExnThrown>")));
        assert!(!is_result_promise_type(&parse("Result<(), ExnThrown>")));
        assert!(!is_result_promise_type(&parse("Promise<'r>")));
        // A `Promise` nested inside another `Ok` type is not a promise return.
        assert!(!is_result_promise_type(&parse(
            "Result<Vec<Promise<'r>>, ExnThrown>"
        )));
    }

    #[test]
    fn promise_params_match_both_spellings() {
        let parse = |s: &str| syn::parse_str::<Type>(s).unwrap();
        assert!(is_unchecked_promise_type(&parse("Promise<'_>")));
        assert!(is_unchecked_promise_type(&parse("js::Promise<'r>")));
        // A future is a return type, never an argument, so it must not pick up
        // the `Promise<T>` argument conversion the way `is_promise_type` does.
        assert!(!is_unchecked_promise_type(&parse("PromiseFuture")));
        assert!(!is_unchecked_promise_type(&parse(
            "PromiseOf<'_, Response<'_>>"
        )));
        assert!(!is_unchecked_promise_type(&parse("Vec<Promise<'_>>")));
    }

    #[test]
    fn promise_value_type_skips_the_handle_lifetime() {
        let value_ty = |s: &str| promise_value_type(&parse_ty(s)).map(ty_string);
        assert_eq!(
            value_ty("PromiseOf<'_, Response<'_>>").as_deref(),
            Some("Response<'_>")
        );
        assert_eq!(value_ty("js::PromiseOf<'r, i32>").as_deref(), Some("i32"));
        // Not a checked promise: no value type to extract.
        assert_eq!(value_ty("Promise<'_>"), None);
        assert_eq!(value_ty("PromiseOfSorts<'_, i32>"), None);
    }

    #[test]
    fn reference_params_expose_their_referent() {
        let target = |s: &str| reference_target(&parse_ty(s)).map(ty_string);
        assert_eq!(target("&EventInit").as_deref(), Some("EventInit"));
        assert_eq!(
            target("&FetchEventInit<'_>").as_deref(),
            Some("FetchEventInit<'_>")
        );
        assert_eq!(target("EventInit"), None);
        // A conversion hands back a value nothing else holds, so there is nothing to mutate
        // through — `&mut` is left to fail as an ordinary type error.
        assert_eq!(target("&mut EventInit"), None);
    }

    #[test]
    fn unsized_referents_convert_as_what_owns_them() {
        let converts_as =
            |s: &str| BorrowedParam::of(&parse_ty(s)).map(|b| (ty_string(&b.convert_ty), b.deref));
        // An unsized referent converts as what owns it, and is reached back through its `Deref`.
        assert_eq!(
            converts_as("&str"),
            Some(("::std::string::String".to_string(), true))
        );
        assert_eq!(
            converts_as("&[u8]"),
            Some(("::std::vec::Vec<u8>".to_string(), true))
        );
        // Everything else converts as itself, and is reached by a plain borrow.
        assert_eq!(
            converts_as("&EventInit"),
            Some(("EventInit".to_string(), false))
        );
        assert_eq!(converts_as("&String"), Some(("String".to_string(), false)));
        // Not a reference at all: converts as itself and binds its own name.
        assert!(converts_as("EventInit").is_none());
    }

    #[test]
    fn dictionary_takes_extends_and_nothing_else() {
        let parse = |s: &str| parse_dictionary_extends(s.parse().unwrap());
        assert!(parse("").unwrap().is_none());
        assert_eq!(
            parse("extends = EventInit").unwrap().map(|i| i.to_string()),
            Some("EventInit".to_string())
        );
        // The options are shared with the class attributes, so the ones that mean nothing for a
        // dictionary have to be turned away rather than silently ignored.
        assert!(parse("hidden").is_err());
        assert!(parse("js_proto = \"Error\"").is_err());
        assert!(parse("name = \"Other\"").is_err());
        assert!(parse("extends = EventInit, hidden").is_err());
    }

    #[test]
    fn cx_and_callargs_params_match_exactly() {
        let parse = |s: &str| syn::parse_str::<Type>(s).unwrap();
        // The real context / raw-args parameter spellings are recognized.
        assert!(is_cx_param_type(&parse("&Scope<'_>")));
        assert!(is_cx_param_type(&parse("&js::gc::scope::Scope<'_>")));
        assert!(is_cx_param_type(&parse("&mut JSContext")));
        assert!(is_callargs_param_type(&parse("&CallArgs")));
        // A user parameter whose type merely contains the substring is not.
        assert!(!is_cx_param_type(&parse("ScopeOptions")));
        assert!(!is_cx_param_type(&parse("Telescope")));
        assert!(!is_cx_param_type(&parse("Vec<ScopeId>")));
        assert!(!is_callargs_param_type(&parse("MyCallArgs")));
        assert!(!is_callargs_param_type(&parse("CallArgsView")));
    }
}
