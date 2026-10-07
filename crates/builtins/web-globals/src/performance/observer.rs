// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `PerformanceObserver` and `PerformanceObserverEntryList` interfaces of
//! [Performance Timeline].
//!
//! [Performance Timeline]: https://w3c.github.io/performance-timeline/

use core_runtime::{webidl_dictionary, webidl_interface, webidl_methods};
use js::error::{ExnThrown, ThrowException};
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::{Callable, Object};

use super::entry::{PerformanceEntry, PerformanceEntryImpl};
use crate::dom_exception::throw_dom_exception;

/// <https://w3c.github.io/performance-timeline/#dfn-observer-type>
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ObserverType {
    #[default]
    Undefined,
    Single,
    Multiple,
}

/// <https://w3c.github.io/performance-timeline/#dom-performanceobserverinit>
#[derive(Default)]
#[webidl_dictionary]
pub struct PerformanceObserverInit {
    pub entry_types: Option<Vec<String>>,
    #[webidl(name = "type")]
    pub kind: Option<String>,
    pub buffered: Option<bool>,
}

/// <https://w3c.github.io/performance-timeline/#the-performanceobserver-interface>
#[webidl_interface]
pub struct PerformanceObserver {
    /// <https://w3c.github.io/performance-timeline/#dfn-observer-callback>
    callback: Heap<js::function::Callable>,
    /// <https://w3c.github.io/performance-timeline/#dfn-observer-buffer>
    buffer: Vec<Heap<PerformanceEntryImpl>>,
    #[no_trace]
    observer_type: ObserverType,
    /// The entry types of the observer's options list, each once, in the order they were added.
    #[no_trace]
    entry_types: Vec<&'static str>,
    /// <https://w3c.github.io/performance-timeline/#dfn-requires-dropped-entries>
    #[no_trace]
    requires_dropped_entries: bool,
    /// Whether the observer is in the list of registered performance observer objects.
    #[no_trace]
    registered: bool,
}

impl PerformanceObserverImpl {
    /// Whether the observer's options list has `entry_type`.
    pub(crate) fn observes(&self, entry_type: &str) -> bool {
        self.entry_types.contains(&entry_type)
    }

    /// Append `entry` to the observer buffer.
    pub(crate) fn push(&mut self, entry: PerformanceEntry<'_>) {
        self.buffer.push(Heap::from(entry));
    }
}

#[webidl_methods]
impl PerformanceObserver {
    /// <https://w3c.github.io/performance-timeline/#dom-performanceobserver-constructor>
    #[constructor]
    pub fn create(callback: Callable<'_>) -> Self {
        // Step 1: Create a new PerformanceObserver object, with its observer callback set to
        //         callback.
        PerformanceObserverImpl {
            callback: Heap::from(callback),
            buffer: Vec::new(),
            observer_type: ObserverType::Undefined,
            entry_types: Vec::new(),
            requires_dropped_entries: false,
            registered: false,
        }
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceobserver-observe>
    ///
    /// Deviates from the specification as Node.js does: `buffered` is ignored with `entryTypes`
    /// rather than rejected, and `entryTypes` without any supported type disconnects the
    /// observer.
    #[method]
    pub fn observe(
        &self,
        scope: &Scope<'_>,
        options: Option<PerformanceObserverInit>,
    ) -> Result<(), ExnThrown> {
        let options = options.unwrap_or_default();
        // Step 2: If options's entryTypes and type members are both omitted, then throw a
        //         TypeError.
        if options.entry_types.is_none() && options.kind.is_none() {
            return Err(js::error::OperationError {
                site: js::class::member_site::<PerformanceObserverImpl>(
                    "observe",
                    js::error::MemberKind::Method,
                ),
                kind: js::error::JSExnType::JSEXN_TYPEERR,
                message: "PerformanceObserver.observe needs entryTypes or type".into(),
                code: "ERR_MISSING_ARGS",
                args: vec!["options.entryTypes".into(), "options.type".into()],
            }
            .throw(scope));
        }
        // Step 3: If options's entryTypes is present and any other member is also present, then
        //         throw a TypeError.
        if options.entry_types.is_some() && options.kind.is_some() {
            return Err(js::error::throw_type_error(
                scope,
                c"PerformanceObserver.observe can't take both entryTypes and type",
            ));
        }
        // Step 4: Update or check this's observer type by running these steps:
        let observer_type = self.data().observer_type;
        match observer_type {
            // Step 4.1: If this's observer type is "undefined":
            ObserverType::Undefined => {
                // Step 4.1.1: If options's entryTypes member is present, then set this's observer
                //             type to "multiple".
                // Step 4.1.2: If options's type member is present, then set this's observer type
                //             to "single".
                self.data_mut().observer_type = if options.entry_types.is_some() {
                    ObserverType::Multiple
                } else {
                    ObserverType::Single
                };
            }
            // Step 4.2: If this's observer type is "single" and options's entryTypes member is
            //           present, then throw an "InvalidModificationError".
            ObserverType::Single if options.entry_types.is_some() => {
                return Err(throw_dom_exception(
                    scope,
                    "InvalidModificationError",
                    "PerformanceObserver can not change to multiple observations",
                ));
            }
            // Step 4.3: If this's observer type is "multiple" and options's type member is
            //           present, then throw an "InvalidModificationError".
            ObserverType::Multiple if options.kind.is_some() => {
                return Err(throw_dom_exception(
                    scope,
                    "InvalidModificationError",
                    "PerformanceObserver can not change to single observation",
                ));
            }
            _ => {}
        }
        // Step 5: Set this's requires dropped entries to true.
        self.data_mut().requires_dropped_entries = true;
        let supported = super::supported_entry_types();
        if let Some(entry_types) = options.entry_types {
            // Step 6: If this's observer type is "multiple", run the following steps:
            // Step 6.1: Let entry types be options's entryTypes sequence.
            // Step 6.2: Remove all types from entry types that are not contained in
            //           relevantGlobal's frozen array of supported entry types.
            let types: Vec<&'static str> = entry_types
                .iter()
                .filter_map(|requested| supported.iter().copied().find(|t| t == requested))
                .fold(Vec::new(), |mut types, entry_type| {
                    if !types.contains(&entry_type) {
                        types.push(entry_type);
                    }
                    types
                });
            // Step 6.3: If the resulting entry types sequence is an empty sequence, abort these
            //           steps.
            if types.is_empty() {
                return self.disconnect(scope);
            }
            // Step 6.4: If the list of registered performance observer objects of relevantGlobal
            //           contains a registered performance observer whose observer is this,
            //           replace its options list with a list containing options as its only
            //           item.
            // Step 6.5: Otherwise, create and append a registered performance observer object to
            //           the list of registered performance observer objects of relevantGlobal.
            let previous = std::mem::replace(&mut self.data_mut().entry_types, types.clone());
            for entry_type in previous.iter().filter(|t| !types.contains(t)) {
                super::observed(scope, entry_type, false);
            }
            for entry_type in types.iter().filter(|t| !previous.contains(t)) {
                super::observed(scope, entry_type, true);
            }
            return super::register_observer(scope, self);
        }
        // Step 7: Otherwise, run the following steps:
        let Some(kind) = options.kind else {
            return Ok(());
        };
        // Step 7.1: Assert that this's observer type is "single".
        // Step 7.2: If options's type is not contained in the relevantGlobal's frozen array of
        //           supported entry types, abort these steps.
        let Some(entry_type) = supported.iter().copied().find(|t| *t == kind) else {
            return Ok(());
        };
        // Step 7.3: If the list of registered performance observer objects of relevantGlobal
        //           contains a registered performance observer obs whose observer is this:
        // Step 7.3.1: If obs's options list contains a PerformanceObserverInit item whose type is
        //             equal to options's type, replace currentOptions with options in obs's
        //             options list.
        // Step 7.3.2: Otherwise, append options to obs's options list.
        // Step 7.4: Otherwise, create and append a registered performance observer object to the
        //           list of registered performance observer objects of relevantGlobal.
        if !self.data().observes(entry_type) {
            self.data_mut().entry_types.push(entry_type);
            super::observed(scope, entry_type, true);
        }
        super::register_observer(scope, self)?;
        // Step 7.5: If options's buffered flag is set:
        if options.buffered == Some(true) {
            // Step 7.5.1: Let tuple be the relevant performance entry tuple of options's type
            //             and relevantGlobal.
            // Step 7.5.2: For each entry in tuple's performance entry buffer, append entry to
            //             the observer buffer.
            let entries = super::buffered_entries(scope, None, Some(entry_type))?;
            {
                let mut data = self.data_mut();
                for entry in entries {
                    data.push(entry);
                }
            }
            // Step 7.5.3: Queue a PerformanceObserver task with relevantGlobal as input.
            super::queue_observer_task(scope)?;
        }
        Ok(())
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceobserver-disconnect>
    ///
    /// As in Node.js, the observer type is reset, so the observer can be used with either form
    /// of `observe` again.
    #[method]
    pub fn disconnect(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 1: Remove this from the list of registered performance observer objects of
        //         relevant global object.
        super::unregister_observer(scope, self);
        // Step 2: Set this's observer buffer to empty.
        // Step 3: Set this's options list to empty.
        let entry_types = {
            let mut data = self.data_mut();
            data.buffer.clear();
            data.observer_type = ObserverType::Undefined;
            data.requires_dropped_entries = false;
            std::mem::take(&mut data.entry_types)
        };
        for entry_type in entry_types {
            super::observed(scope, entry_type, false);
        }
        Ok(())
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceobserver-takerecords>
    #[method]
    pub fn take_records<'r>(&self, scope: &'r Scope<'_>) -> Vec<PerformanceEntry<'r>> {
        // Step 1: Let entries be a copy of this's observer buffer.
        // Step 2: Set this's observer buffer to empty.
        let entries = self
            .data()
            .buffer
            .iter()
            .map(|entry| entry.get(scope))
            .collect();
        self.data_mut().buffer.clear();
        // Step 3: Return entries.
        entries
    }
}

impl<'s> PerformanceObserver<'s> {
    /// Whether the observer is registered with its global's `Performance` object.
    pub(crate) fn is_registered(&self) -> bool {
        self.data().registered
    }

    /// Record whether the observer is registered with its global's `Performance` object.
    pub(crate) fn set_registered(&self, registered: bool) {
        self.data_mut().registered = registered;
    }

    /// Run the observer's callback with the entries in its buffer, and empty the buffer, as a
    /// step of the PerformanceObserver task. Does nothing if the buffer is empty.
    ///
    /// <https://w3c.github.io/performance-timeline/#dfn-queue-a-performanceobserver-task>
    pub(crate) fn notify(&self, scope: &Scope<'_>) -> Result<(), ExnThrown> {
        // Step 3.1: Let entries be a copy of po's observer buffer.
        // Step 3.2: If entries is empty, return.
        // Step 3.3: Empty po's observer buffer.
        let (entries, callback, requires_dropped_entries) = {
            let mut data = self.data_mut();
            if data.buffer.is_empty() {
                return Ok(());
            }
            let entries: Vec<PerformanceEntry<'_>> =
                data.buffer.iter().map(|entry| entry.get(scope)).collect();
            data.buffer.clear();
            let requires_dropped_entries = std::mem::take(&mut data.requires_dropped_entries);
            (entries, data.callback.get(scope), requires_dropped_entries)
        };
        // Step 3.4: Let observerEntryList be a new PerformanceObserverEntryList, with its entry
        //           list set to entries.
        let list = PerformanceObserverEntryList::from_entries(scope, entries)?;
        // Step 3.5-3.6: Let droppedEntriesCount be null. If po's requires dropped entries is
        //               set, set droppedEntriesCount to the number of entries dropped from the
        //               buffers of the observed types, and unset it.
        // Step 3.7: Let callbackOptions be a PerformanceObserverCallbackOptions with its
        //           droppedEntriesCount set to droppedEntriesCount if droppedEntriesCount is not
        //           null, otherwise unset.
        let callback_options = Object::new_plain(scope)?;
        if requires_dropped_entries {
            callback_options.set_property(scope, c"droppedEntriesCount", 0)?;
        }
        // Step 3.8: Call po's observer callback with « observerEntryList, po, callbackOptions »,
        //           "report", and po.
        let this = self.as_object();
        let args = [
            js::conversion::ToJSVal::to_jsval_throwing(&list, scope)?,
            js::conversion::ToJSVal::to_jsval_throwing(&this, scope)?,
            js::conversion::ToJSVal::to_jsval_throwing(&callback_options, scope)?,
        ];
        js::Function::call(scope, this, callback, &args)?;
        Ok(())
    }
}

/// <https://w3c.github.io/performance-timeline/#performanceobserverentrylist-interface>
#[webidl_interface]
pub struct PerformanceObserverEntryList {
    /// <https://w3c.github.io/performance-timeline/#dfn-entry-list>, sorted by `startTime`.
    entries: Vec<Heap<PerformanceEntryImpl>>,
}

impl<'s> PerformanceObserverEntryList<'s> {
    /// A new list of `entries`, sorted by `startTime`.
    pub fn from_entries(
        scope: &'s Scope<'_>,
        mut entries: Vec<PerformanceEntry<'_>>,
    ) -> Result<Self, ExnThrown> {
        sort_by_start_time(&mut entries);
        js::class::create_instance_with::<PerformanceObserverEntryListImpl>(scope, |_| {
            PerformanceObserverEntryListImpl {
                entries: entries.into_iter().map(Heap::from).collect(),
            }
        })
    }
}

/// Sort `entries` by `startTime`, keeping the order of entries with the same `startTime`.
pub(crate) fn sort_by_start_time(entries: &mut [PerformanceEntry<'_>]) {
    entries.sort_by(|a, b| a.data().start_time().total_cmp(&b.data().start_time()));
}

#[webidl_methods]
impl PerformanceObserverEntryList {
    /// <https://w3c.github.io/performance-timeline/#dom-performanceobserverentrylist-getentries>
    #[method]
    pub fn get_entries<'r>(&self, scope: &'r Scope<'_>) -> Vec<PerformanceEntry<'r>> {
        self.filter(scope, None, None)
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceobserverentrylist-getentriesbytype>
    #[method]
    pub fn get_entries_by_type<'r>(
        &self,
        scope: &'r Scope<'_>,
        entry_type: String,
    ) -> Vec<PerformanceEntry<'r>> {
        self.filter(scope, None, Some(&entry_type))
    }

    /// <https://w3c.github.io/performance-timeline/#dom-performanceobserverentrylist-getentriesbyname>
    #[method]
    pub fn get_entries_by_name<'r>(
        &self,
        scope: &'r Scope<'_>,
        name: String,
        entry_type: Option<String>,
    ) -> Vec<PerformanceEntry<'r>> {
        self.filter(scope, Some(&name), entry_type.as_deref())
    }

    /// <https://w3c.github.io/performance-timeline/#dfn-filter-buffer-by-name-and-type>: the
    /// entries with `name` and of `entry_type`, each if given, in the list's order.
    fn filter<'r>(
        &self,
        scope: &'r Scope<'_>,
        name: Option<&str>,
        entry_type: Option<&str>,
    ) -> Vec<PerformanceEntry<'r>> {
        let entries: Vec<PerformanceEntry<'r>> = self
            .data()
            .entries
            .iter()
            .map(|entry| entry.get(scope))
            .collect();
        entries
            .into_iter()
            .filter(|entry| matches(entry, name, entry_type))
            .collect()
    }
}

/// Whether `entry` has `name` and is of `entry_type`, each if given.
pub(crate) fn matches(
    entry: &PerformanceEntry<'_>,
    name: Option<&str>,
    entry_type: Option<&str>,
) -> bool {
    let data = entry.data();
    name.is_none_or(|name| data.name() == name)
        && entry_type.is_none_or(|entry_type| data.entry_type() == entry_type)
}

/// Define the static `PerformanceObserver.supportedEntryTypes` getter, which returns the same
/// frozen array for every call in a global.
///
/// <https://w3c.github.io/performance-timeline/#supportedentrytypes-attribute>
pub(crate) fn define_supported_entry_types(scope: &Scope<'_>) -> Result<(), ExnThrown> {
    let constructor = js::class::class_constructor::<PerformanceObserverImpl>(scope)?;
    let getter = js::Function::new_callback(
        scope,
        c"supportedEntryTypes",
        0,
        supported_entry_types_getter,
        (),
    )?;
    let attrs = js::class_spec::JSPROP_ENUMERATE as u32;
    constructor.define_accessor(scope, c"supportedEntryTypes", Some(*getter), None, attrs)
}

/// The `PerformanceObserver.supportedEntryTypes` getter.
fn supported_entry_types_getter(
    scope: &Scope<'_>,
    _args: js::function::CallbackArgs<'_>,
    _payload: HandleValue,
) -> Result<js::native::Value, ExnThrown> {
    let key = supported_entry_types_getter as *const () as usize;
    let array = js::class::get_or_init_shared_object(scope, key, |scope| {
        let types = super::supported_entry_types();
        let array = js::Array::with_contents(scope, types)?;
        array.freeze(scope)?;
        Ok(*array)
    })?;
    js::conversion::ToJSVal::to_jsval_raw_throwing(&array, scope)
}
