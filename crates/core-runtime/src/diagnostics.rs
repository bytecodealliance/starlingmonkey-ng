// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Reports of script errors nothing caught: exceptions a task or microtask threw, and promises
//! rejected with no handler.

use std::cell::{Cell, RefCell};

/// Receives each report in place of stderr. The message has no trailing newline.
pub type DiagnosticsSink = fn(message: &str);

js::instance_local! {
    static SINK: Cell<Option<DiagnosticsSink>> = const { Cell::new(None) };

    /// The messages [`report_later`] queued, in the order it queued them.
    static QUEUED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Route reports to `sink`, or back to stderr with `None`.
pub fn set_sink(sink: Option<DiagnosticsSink>) {
    SINK.with(|slot| slot.set(sink));
}

/// Pass `message` to the sink [`set_sink`] installed, or write it and a newline to stderr if none
/// is installed.
pub(crate) fn report(message: &str) {
    match SINK.with(Cell::get) {
        Some(sink) => sink(message),
        None => eprintln!("{message}"),
    }
}

/// Queue `message` to be reported as [`report`] reports it by the next
/// [`run_microtasks`](crate::event_loop::run_microtasks). It neither runs JavaScript nor allocates
/// GC things, so a finalizer can call it. A message queued while the instance is torn down is
/// dropped.
pub fn report_later(message: String) {
    let _ = QUEUED.try_with(|queued| queued.borrow_mut().push(message));
}

/// Report the messages [`report_later`] queued.
pub(crate) fn report_queued() {
    for message in QUEUED.with(RefCell::take) {
        report(&message);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::config::RuntimeConfig;
    use crate::event_loop::run_microtasks;
    use crate::runtime::Runtime;

    thread_local! {
        static REPORTS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn record(message: &str) {
        REPORTS.with(|reports| reports.borrow_mut().push(message.to_string()));
    }

    #[test]
    fn unhandled_rejections_are_reported_to_the_sink() {
        let runtime = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = runtime.default_global();
        set_sink(Some(record));
        let evaluated = js::compile::evaluate_with_filename(
            &scope,
            "Promise.reject(new Error('rejected'));",
            "test.js",
            1,
        );
        run_microtasks(&scope);
        set_sink(None);
        assert!(evaluated.is_ok());
        let reports = REPORTS.with(RefCell::take);
        assert_eq!(reports.len(), 1, "{reports:?}");
        assert!(
            reports[0].starts_with("Uncaught (in promise) rejected"),
            "{reports:?}"
        );
    }

    #[test]
    fn queued_messages_are_reported_by_the_next_microtask_checkpoint() {
        let runtime = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = runtime.default_global();
        set_sink(Some(record));
        report_later("first".to_string());
        report_later("second".to_string());
        let before = REPORTS.with(|reports| reports.borrow().len());
        run_microtasks(&scope);
        run_microtasks(&scope);
        set_sink(None);
        assert_eq!(before, 0);
        let reports = REPORTS.with(RefCell::take);
        assert_eq!(reports, ["first", "second"]);
    }
}
