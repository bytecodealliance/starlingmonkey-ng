// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! `#[derive(ScopeRoot)]` handles a unit enum variant. A `Variant()` pattern for one fails to
//! compile (E0532).

// This file contains nothing platform-specific, so skip it on wasm32.
#![cfg(not(target_arch = "wasm32"))]

use core_runtime::config::RuntimeConfig;
use core_runtime::runtime::Runtime;
use core_runtime::Traceable;
use js::gc::handle::Heap;
use js::native::Value;
use js::ScopeRoot;

#[derive(Traceable, ScopeRoot)]
#[allow(dead_code)]
enum Reply {
    // Unit variant — previously produced an uncompilable `Reply::Empty ()` pattern.
    Empty,
    // Single `Heap<Value>` field (fast path).
    One(Heap<Value>),
    // Two `Heap` fields (traced path: boxed before rooting).
    Two { a: Heap<Value>, b: Heap<Value> },
}

#[test]
fn scope_root_enum_unit_variant_roots() {
    let rt = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
    let scope = rt.default_global();

    let rooted = Reply::Empty.root(&scope);
    assert!(matches!(rooted, StackReply::Empty));
}
