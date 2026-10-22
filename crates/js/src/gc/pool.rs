// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Allocator for GC root handles.
//!
//! Every scope ([`RootScope`] or [`InnerScope`]) roots values through a [`ScopeAlloc`]. The
//! innermost live scope bump-allocates on the [`HandlePool`]'s shared root stack. A scope that
//! roots a value while a scope nested in it is alive gets [`PrivatePages`] instead, since the
//! nested scope's drop truncates the stack back to where it started.
//!
//! # Architecture
//!
//! - **[`HandlePool`]**: owns the root stack (chunks of [`Page`]s, whose slots never move) and
//!   one [`Level`] per live scope, a freelist of pages for private allocations, and an intrusive
//!   linked list of the live [`PrivatePages`] for GC tracing.
//! - **[`ScopeAlloc`]**: a scope's handle on its level, and its private pages if it has any.
//!   Dropping it truncates the stack to the level's base once every scope created after it has
//!   dropped too.
//! - **[`Page`]**: a fixed-size array of 128 tagged `u64` slots.
//!
//! The pool is wrapped in a [`PoolRooter`], a `#[repr(C)]` `CustomAutoRooter` that sits on the
//! `autoGCRooters` stack for the entire lifetime of the [`Runtime`]. During GC, it traces the
//! root stack up to its top and every live private allocation.
//!
//! [`RootScope`]: crate::gc::scope::RootScope
//! [`InnerScope`]: crate::gc::scope::InnerScope
//! [`Runtime`]: mozjs::rust::Runtime

use std::cell::{Cell, RefCell, UnsafeCell};
use std::ptr;

use mozjs::context::JSContext;
use mozjs::gc::{CustomAutoRooter, CustomTrace};
use mozjs::glue::{
    CallBigIntRootTracer, CallFunctionRootTracer, CallIdRootTracer, CallObjectRootTracer,
    CallScriptRootTracer, CallStringRootTracer, CallSymbolRootTracer, CallValueRootTracer,
};
use mozjs::jsapi::JSTracer;

/// Number of slots per page. 128 gives ~1.1 KB per page (128 × 8 bytes for
/// values + 128 bytes for tags), which fits comfortably in L1 cache and covers
/// most operations without a second allocation.
const PAGE_SIZE: usize = 128;

/// Type tag for a slot in the handle pool.
///
/// Used during GC tracing to dispatch to the correct `CallXxxRootTracer`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum SlotTag {
    Object = 0,
    Value = 1,
    String = 2,
    Script = 3,
    Id = 4,
    Symbol = 5,
    Function = 6,
    BigInt = 7,
}

/// A fixed-size page of root slots.
///
/// Tags and values are stored in parallel arrays to avoid alignment padding
/// that would waste memory with a per-slot struct.
pub struct Page {
    tags: [Cell<SlotTag>; PAGE_SIZE],
    values: [UnsafeCell<u64>; PAGE_SIZE],
}

impl Page {
    fn new() -> Self {
        // SAFETY: Cell<SlotTag> where SlotTag is repr(u8), and UnsafeCell<u64>,
        // are both safe to zero-initialize. Unused slots are never read by the
        // tracer (bounded by cursor).
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// ScopeAlloc: a scope's root allocation
// ---------------------------------------------------------------------------

/// A scope's root allocation: its level on the pool's root stack, and the private pages it
/// roots into while a scope nested in it is alive.
pub struct ScopeAlloc {
    /// The pool this allocator roots in.
    pool: *const HandlePool,

    /// Index of this scope's [`Level`] in the pool.
    level: usize,

    /// Pages for values rooted while this scope was not the innermost one. `None` until needed.
    private: Option<Box<PrivatePages>>,
}

impl ScopeAlloc {
    /// Create an allocator for a new scope, nested in every scope that is alive.
    ///
    /// # Safety
    ///
    /// `pool` must point to a valid `HandlePool` that outlives this `ScopeAlloc`.
    pub unsafe fn new(pool: *const HandlePool) -> Self {
        // SAFETY: guaranteed by the caller.
        let level = unsafe { &*pool }.push_level();
        ScopeAlloc {
            pool,
            level,
            private: None,
        }
    }

    /// Allocate a slot for a rooted value.
    ///
    /// Returns a stable pointer to the value's `u64` storage. The pointer
    /// remains valid until this `ScopeAlloc` is dropped.
    #[inline]
    pub fn alloc(&mut self, tag: SlotTag, value: u64) -> *mut u64 {
        // SAFETY: the pool outlives every allocator created from it.
        let pool = unsafe { &*self.pool };
        if pool.is_innermost(self.level) {
            return pool.stack_alloc(tag, value);
        }
        self.private_alloc(tag, value)
    }

    /// Allocate a slot in this scope's private pages, for a scope that is not the innermost one.
    #[cold]
    #[inline(never)]
    fn private_alloc(&mut self, tag: SlotTag, value: u64) -> *mut u64 {
        self.private
            // SAFETY: the pool outlives every allocator created from it.
            .get_or_insert_with(|| unsafe { PrivatePages::new(self.pool) })
            .alloc(tag, value)
    }

    /// Get the pool pointer that this allocator roots in.
    pub fn pool(&self) -> *const HandlePool {
        self.pool
    }
}

impl Drop for ScopeAlloc {
    fn drop(&mut self) {
        if let Some(private) = self.private.take() {
            PrivatePages::recycle(private);
        }
        // SAFETY: the pool outlives every allocator created from it.
        unsafe { &*self.pool }.pop_level(self.level);
    }
}

/// A scope's private root pages, for values it roots while a scope nested in it is alive.
///
/// Pages are obtained from the [`HandlePool`] freelist and returned on drop. `PrivatePages` is
/// registered in the pool's intrusive linked list of active allocators so that the GC tracer can
/// discover its slots.
pub struct PrivatePages {
    /// The pool from which this allocator borrows pages.
    pool: *const HandlePool,

    /// Current page being allocated into. `None` until the first allocation.
    current_page: Option<Box<Page>>,

    /// Slot index within the current page (0..PAGE_SIZE).
    cursor: usize,

    /// Previously filled pages owned by this scope.
    #[allow(clippy::vec_box)]
    full_pages: Vec<Box<Page>>,

    /// Intrusive linked-list pointers for the pool's active-allocator list.
    /// These are raw pointers because the list is manually managed.
    next: *mut PrivatePages,
    prev: *mut PrivatePages,

    /// Whether this allocator is in the pool's active list. False once
    /// [`release`](Self::release) has run, and while the box waits in the
    /// pool's allocator freelist.
    active: bool,
}

impl PrivatePages {
    /// Create private pages, boxed for address stability, and register them
    /// with the pool's active list.
    ///
    /// Returns a `Box<PrivatePages>` because the intrusive linked list requires
    /// a stable address. The box must not be moved out of its allocation.
    ///
    /// No page is allocated until the first `alloc` call (lazy initialization).
    /// The box comes from the pool's allocator freelist when one is available.
    /// Pass a box that is done with to [`recycle`](Self::recycle) to return it
    /// there.
    ///
    /// # Safety
    ///
    /// `pool` must point to a valid `HandlePool` that outlives this `PrivatePages`.
    pub unsafe fn new(pool: *const HandlePool) -> Box<Self> {
        // SAFETY: guaranteed by the caller.
        let pool_ref = unsafe { &*pool };
        let mut alloc = pool_ref.take_alloc().unwrap_or_else(|| {
            Box::new(PrivatePages {
                pool,
                current_page: None,
                cursor: 0,
                full_pages: Vec::new(),
                next: ptr::null_mut(),
                prev: ptr::null_mut(),
                active: false,
            })
        });
        debug_assert!(!alloc.active && alloc.current_page.is_none() && alloc.full_pages.is_empty());
        alloc.pool = pool;
        alloc.cursor = 0;
        alloc.active = true;
        // Register using the heap address (stable across moves of the Box pointer).
        pool_ref.register(&mut *alloc);
        alloc
    }

    /// Release `alloc`'s slots and pages, and keep the box in the pool's
    /// allocator freelist for a later [`new`](Self::new).
    pub fn recycle(mut alloc: Box<Self>) {
        alloc.release();
        // SAFETY: the pool outlives every allocator created from it.
        let pool = unsafe { &*alloc.pool };
        pool.return_alloc(alloc);
    }

    /// Unregister from the pool and return this allocator's pages. Does nothing
    /// if already released.
    fn release(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        // SAFETY: pool pointer is valid for the lifetime of the Runtime,
        // which outlives all scopes.
        let pool = unsafe { &*self.pool };

        // Unregister from the active allocator list.
        pool.unregister(self);

        // Return all pages to the pool freelist.
        if let Some(page) = self.current_page.take() {
            pool.return_page(page);
        }
        for page in self.full_pages.drain(..) {
            pool.return_page(page);
        }
    }

    /// Allocate a slot for a rooted value.
    ///
    /// Returns a stable pointer to the value's `u64` storage. The pointer
    /// remains valid until this `PrivatePages` is dropped.
    pub fn alloc(&mut self, tag: SlotTag, value: u64) -> *mut u64 {
        // Ensure we have a page to allocate from.
        if self.current_page.is_none() {
            // SAFETY: pool pointer is valid for the lifetime of the Runtime.
            let pool = unsafe { &*self.pool };
            self.current_page = Some(pool.take_page());
        }

        // If the current page is full, move it to full_pages and get a new one.
        if self.cursor == PAGE_SIZE {
            let full = self.current_page.take().unwrap();
            self.full_pages.push(full);
            // SAFETY: pool pointer is valid for the lifetime of the Runtime.
            let pool = unsafe { &*self.pool };
            self.current_page = Some(pool.take_page());
            self.cursor = 0;
        }

        let page = self.current_page.as_ref().unwrap();
        page.tags[self.cursor].set(tag);
        let ptr = page.values[self.cursor].get();
        // SAFETY: This slot is at our current cursor position, not yet in use.
        unsafe { *ptr = value };
        self.cursor += 1;
        ptr
    }

    /// Trace all live slots for GC.
    ///
    /// # Safety
    ///
    /// Must only be called during GC tracing. `trc` must be a valid `JSTracer`.
    unsafe fn trace(&self, trc: *mut JSTracer) {
        // Trace all full pages (all PAGE_SIZE slots are live).
        for page in &self.full_pages {
            trace_page(trc, page, PAGE_SIZE);
        }
        // Trace the current page up to the cursor.
        if let Some(page) = &self.current_page {
            trace_page(trc, page, self.cursor);
        }
    }
}

impl Drop for PrivatePages {
    fn drop(&mut self) {
        self.release();
    }
}

/// Trace `count` slots in a page.
///
/// # Safety
///
/// `trc` must be a valid `JSTracer`. `count` must be <= PAGE_SIZE.
unsafe fn trace_page(trc: *mut JSTracer, page: &Page, count: usize) {
    for i in 0..count {
        let tag = page.tags[i].get();
        let ptr = page.values[i].get();

        match tag {
            SlotTag::Object => {
                let obj_ptr = ptr as *mut *mut mozjs::jsapi::JSObject;
                if !(*obj_ptr).is_null() {
                    CallObjectRootTracer(trc, obj_ptr, c"pool-object".as_ptr());
                }
            }
            SlotTag::Value => {
                let val_ptr = ptr as *mut mozjs::jsapi::Value;
                CallValueRootTracer(trc, val_ptr, c"pool-value".as_ptr());
            }
            SlotTag::String => {
                let str_ptr = ptr as *mut *mut mozjs::jsapi::JSString;
                if !(*str_ptr).is_null() {
                    CallStringRootTracer(trc, str_ptr, c"pool-string".as_ptr());
                }
            }
            SlotTag::Script => {
                let scr_ptr = ptr as *mut *mut mozjs::jsapi::JSScript;
                if !(*scr_ptr).is_null() {
                    CallScriptRootTracer(trc, scr_ptr, c"pool-script".as_ptr());
                }
            }
            SlotTag::Id => {
                let id_ptr = ptr as *mut mozjs::jsapi::jsid;
                CallIdRootTracer(trc, id_ptr, c"pool-id".as_ptr());
            }
            SlotTag::Symbol => {
                let sym_ptr = ptr as *mut *mut mozjs::jsapi::JS::Symbol;
                if !(*sym_ptr).is_null() {
                    CallSymbolRootTracer(trc, sym_ptr, c"pool-symbol".as_ptr());
                }
            }
            SlotTag::Function => {
                let fun_ptr = ptr as *mut *mut mozjs::jsapi::JSFunction;
                if !(*fun_ptr).is_null() {
                    CallFunctionRootTracer(trc, fun_ptr, c"pool-function".as_ptr());
                }
            }
            SlotTag::BigInt => {
                let bi_ptr = ptr as *mut *mut mozjs::jsapi::JS::BigInt;
                if !(*bi_ptr).is_null() {
                    CallBigIntRootTracer(trc, bi_ptr, c"pool-bigint".as_ptr());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// HandlePool: root stack, page freelist, and private allocation registry
// ---------------------------------------------------------------------------

/// A scope's entry on the root stack: the stack's height when the scope was created, and whether
/// the scope is still alive.
struct Level {
    base: usize,
    alive: bool,
}

/// The root stack shared by all scopes, a freelist of pages for private allocations, and a
/// registry of the live [`PrivatePages`].
///
/// During GC, the tracer traces the root stack up to its top and walks the registry to discover
/// the private slots.
///
/// # Thread safety
///
/// `HandlePool` uses interior mutability (`Cell`/`UnsafeCell`) and is
/// single-threaded, matching SpiderMonkey's threading model.
#[derive(Default)]
pub struct HandlePool {
    /// The root stack's pages. Slot `i` is slot `i % PAGE_SIZE` of page `i / PAGE_SIZE`. Pages are
    /// added as the stack grows and kept when it shrinks, so slots never move.
    #[allow(clippy::vec_box)]
    stack: UnsafeCell<Vec<Box<Page>>>,

    /// The number of slots in use on the root stack.
    top: Cell<usize>,

    /// One entry per scope, innermost last. Entries of dropped scopes are kept until every scope
    /// created after them has dropped.
    levels: UnsafeCell<Vec<Level>>,

    /// Freelist of reusable pages for private allocations.
    #[allow(clippy::vec_box)]
    freelist: UnsafeCell<Vec<Box<Page>>>,

    /// Head of the intrusive doubly-linked list of live `PrivatePages`.
    /// Null when there are none.
    active_head: Cell<*mut PrivatePages>,

    /// Released private allocations kept for reuse, at most [`MAX_FREE_ALLOCS`].
    #[allow(clippy::vec_box)]
    alloc_freelist: UnsafeCell<Vec<Box<PrivatePages>>>,

    /// Tracers registered with [`crate::gc::add_root_tracer`], run whenever the pool is traced.
    root_tracers: UnsafeCell<Vec<unsafe fn(*mut JSTracer)>>,
}

/// The maximum number of released private allocations the pool keeps.
const MAX_FREE_ALLOCS: usize = 32;

/// This module's share of the crate's thread-local state. See [`crate::tls`].
pub(crate) struct PoolTls {
    handle_pool: RefCell<Option<Box<PoolRooter>>>,
}

impl PoolTls {
    pub(crate) const fn new() -> Self {
        Self {
            handle_pool: RefCell::new(None),
        }
    }
}

fn handle_pool<R>(f: impl FnOnce(&RefCell<Option<Box<PoolRooter>>>) -> R) -> R {
    crate::tls::with(|tls| f(&tls.pool.handle_pool))
}

/// Create the handle pool for scope-based rooting.
///
/// The pool is inside a PoolRooter (CustomAutoRooter) so it is traced during
/// both minor and major GC via the autoGCRooters chain.
pub(crate) fn init_pool(cx: &mut JSContext) {
    let mut pool_rooter = Box::new(PoolRooter::new(HandlePool::new()));
    // SAFETY: `raw_cx()` requires unsafe. The Box gives us a stable
    // address so the autoGCRooters stack entry remains valid.
    unsafe {
        pool_rooter.add_to_root_stack(cx.raw_cx());
    }

    handle_pool(|hp| {
        let mut borrow = hp.borrow_mut();
        assert!(
            borrow.is_none(),
            "HandlePool already initialized on this thread"
        );
        *borrow = Some(pool_rooter);
    });
}

/// Register `tracer` with the current thread's pool, unless it already is.
pub(crate) fn add_root_tracer(tracer: unsafe fn(*mut JSTracer)) {
    // SAFETY: the pool outlives this call, and single-threaded access leaves no other reference to
    // its tracer list.
    let tracers = unsafe { &mut *(*current_pool()).root_tracers.get() };
    if !tracers
        .iter()
        .any(|&registered| ptr::fn_addr_eq(registered, tracer))
    {
        tracers.push(tracer);
    }
}

/// Get a raw pointer to the current thread's HandlePool.
///
/// The returned pointer is valid for the lifetime of the Runtime (until
/// [`shutdown`] is called). It is safe to store in `ScopeAlloc` because
/// the pool outlives all scopes.
///
/// # Panics
///
/// Panics if no pool has been initialized on this thread.
pub(crate) fn current_pool() -> *const HandlePool {
    handle_pool(|hp| {
        let borrow = hp.borrow();
        let rooter = borrow
            .as_ref()
            .expect("No HandlePool on this thread — has a Runtime been created?");
        // Deref through Box<PoolRooter> → PoolRooter → HandlePool
        let pool: &HandlePool = rooter;
        pool as *const HandlePool
    })
}

impl HandlePool {
    /// Create a new empty pool.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a level for a new scope, nested in every live scope, and return its index.
    #[inline]
    fn push_level(&self) -> usize {
        // SAFETY: Single-threaded access, and no reference to `levels` outlives this call.
        let levels = unsafe { &mut *self.levels.get() };
        levels.push(Level {
            base: self.top.get(),
            alive: true,
        });
        levels.len() - 1
    }

    /// Whether `level` belongs to the innermost live scope.
    #[inline]
    fn is_innermost(&self, level: usize) -> bool {
        // SAFETY: Single-threaded access, and no reference to `levels` outlives this call.
        let levels = unsafe { &*self.levels.get() };
        level == levels.len() - 1
    }

    /// Mark `level`'s scope dropped. If no live scope was created after it, truncate the root stack
    /// to the base of the oldest of the dropped scopes on top of the level stack, and remove their
    /// levels.
    #[inline]
    fn pop_level(&self, level: usize) {
        // SAFETY: Single-threaded access, and no reference to `levels` outlives this call.
        let levels = unsafe { &mut *self.levels.get() };
        levels[level].alive = false;
        while let Some(last) = levels.last() {
            if last.alive {
                break;
            }
            self.top.set(last.base);
            levels.pop();
        }
    }

    /// Push a slot for the innermost scope onto the root stack, and return its storage.
    #[inline]
    fn stack_alloc(&self, tag: SlotTag, value: u64) -> *mut u64 {
        let index = self.top.get();
        // SAFETY: Single-threaded access, and no reference to `stack` outlives this call.
        let stack = unsafe { &mut *self.stack.get() };
        let page_index = index / PAGE_SIZE;
        if page_index == stack.len() {
            Self::grow_stack(stack);
        }
        let page = &stack[page_index];
        let slot = index % PAGE_SIZE;
        page.tags[slot].set(tag);
        let ptr = page.values[slot].get();
        // SAFETY: the slot is above the stack's top, so no handle refers to it.
        unsafe { *ptr = value };
        self.top.set(index + 1);
        ptr
    }

    /// Add a page to the root stack.
    #[cold]
    #[inline(never)]
    #[allow(clippy::vec_box)]
    fn grow_stack(stack: &mut Vec<Box<Page>>) {
        stack.push(Box::new(Page::new()));
    }

    /// Take a page from the freelist, or allocate a new one.
    fn take_page(&self) -> Box<Page> {
        // SAFETY: Single-threaded access.
        let fl = unsafe { &mut *self.freelist.get() };
        fl.pop().unwrap_or_else(|| Box::new(Page::new()))
    }

    /// Return a page to the freelist for reuse.
    fn return_page(&self, page: Box<Page>) {
        // SAFETY: Single-threaded access.
        let fl = unsafe { &mut *self.freelist.get() };
        fl.push(page);
    }

    /// Take a released allocator from the freelist, if there is one.
    fn take_alloc(&self) -> Option<Box<PrivatePages>> {
        // SAFETY: Single-threaded access.
        let fl = unsafe { &mut *self.alloc_freelist.get() };
        fl.pop()
    }

    /// Keep a released allocator for reuse, or drop it if the freelist is full.
    fn return_alloc(&self, alloc: Box<PrivatePages>) {
        debug_assert!(!alloc.active);
        // SAFETY: Single-threaded access.
        let fl = unsafe { &mut *self.alloc_freelist.get() };
        if fl.len() < MAX_FREE_ALLOCS {
            fl.push(alloc);
        }
    }

    /// Register a `PrivatePages` in the active allocator list.
    ///
    /// Pushes to the head of the doubly-linked list.
    fn register(&self, alloc: *mut PrivatePages) {
        let head = self.active_head.get();
        // SAFETY: alloc is a valid pointer to a PrivatePages being constructed.
        unsafe {
            (*alloc).next = head;
            (*alloc).prev = ptr::null_mut();
            if !head.is_null() {
                (*head).prev = alloc;
            }
        }
        self.active_head.set(alloc);
    }

    /// Unregister a `PrivatePages` from the active allocator list.
    fn unregister(&self, alloc: *const PrivatePages) {
        // SAFETY: alloc is a valid pointer to a PrivatePages being dropped.
        // The linked-list pointers are valid because they were set by register().
        unsafe {
            let prev = (*alloc).prev;
            let next = (*alloc).next;
            if !prev.is_null() {
                (*prev).next = next;
            } else {
                // alloc was the head.
                self.active_head.set(next);
            }
            if !next.is_null() {
                (*next).prev = prev;
            }
        }
    }
}

unsafe impl CustomTrace for HandlePool {
    /// Trace the root stack up to its top, and the slots of all live private allocations.
    ///
    /// # Safety
    ///
    /// Must only be called during GC tracing (from a `JSTraceDataOp` callback).
    /// `trc` must be a valid `JSTracer` pointer.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    fn trace(&self, trc: *mut JSTracer) {
        // SAFETY: Single-threaded access. Tracing runs while no allocation is in progress.
        let stack = unsafe { &*self.stack.get() };
        let top = self.top.get();
        for (index, page) in stack.iter().enumerate() {
            let start = index * PAGE_SIZE;
            if start >= top {
                break;
            }
            // SAFETY: `trc` is valid per this function's contract, and the slots below `top` hold
            // values of their tags.
            unsafe { trace_page(trc, page, (top - start).min(PAGE_SIZE)) };
        }
        // SAFETY: Single-threaded access. Registration does not run during tracing.
        for tracer in unsafe { &*self.root_tracers.get() } {
            // SAFETY: `trc` is valid per this function's contract.
            unsafe { tracer(trc) };
        }
        let mut current = self.active_head.get();
        while !current.is_null() {
            // SAFETY: The linked list contains only valid PrivatePages pointers
            // because register/unregister maintain the invariant.
            unsafe {
                (*current).trace(trc);
                current = (*current).next;
            }
        }
    }
}

pub type PoolRooter = CustomAutoRooter<HandlePool>;

pub(crate) fn shutdown() {
    handle_pool(|hp| {
        let mut borrow = hp.borrow_mut();
        // Clear the pool rooter from the autoGCRooters stack.
        // SAFETY: This reverses the add_to_root_stack call in init_pool().
        unsafe {
            if let Some(rooter) = borrow.as_mut() {
                rooter.remove_from_root_stack();
            }
        }
        // Drop the pool rooter, which drops the HandlePool and all pages.
        // This must be done after removing from the root stack to avoid
        // use-after-free during GC tracing.
        *borrow = None;
    });
}
