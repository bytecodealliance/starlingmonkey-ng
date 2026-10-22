// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! This crate's thread-local state, held in a single key.
//!
//! Each module that needs thread-local state owns a struct of it here and
//! reaches it through [`with`]. One key for the crate means one destructor, so
//! the fields below drop in the order they are declared.
//!
//! Every field keeps its own `RefCell` or `Cell`, so a borrow covers one piece
//! of state rather than all of it.

use crate::class::ClassTls;
use crate::gc::pool::PoolTls;
use crate::gc::rooted::RootedTls;
use crate::module::ModuleTls;
use crate::promise::PromiseTls;

/// The whole crate's thread-local state.
#[crate::allow_unrooted_interior]
pub(crate) struct JsTls {
    pub(crate) promise: PromiseTls,
    pub(crate) class: ClassTls,
    pub(crate) pool: PoolTls,
    pub(crate) module: ModuleTls,
    pub(crate) rooted: RootedTls,
}

crate::instance_local! {
    static JS_TLS: JsTls = const {
        JsTls {
            promise: PromiseTls::new(),
            class: ClassTls::new(),
            pool: PoolTls::new(),
            module: ModuleTls::new(),
            rooted: RootedTls::new(),
        }
    };
}

/// Run `f` against this thread's state.
pub(crate) fn with<R>(f: impl FnOnce(&JsTls) -> R) -> R {
    JS_TLS.with(f)
}

/// Run `f` against this thread's state, or return `None` once the thread destroyed it.
pub(crate) fn try_with<R>(f: impl FnOnce(&JsTls) -> R) -> Option<R> {
    JS_TLS.try_with(f).ok()
}
