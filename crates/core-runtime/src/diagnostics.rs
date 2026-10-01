// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Reports of script errors nothing caught: exceptions a task or microtask threw, and promises
//! rejected with no handler.

use std::cell::Cell;

/// Receives each report in place of stderr. The message has no trailing newline.
pub type DiagnosticsSink = fn(message: &str);

js::instance_local! {
    static SINK: Cell<Option<DiagnosticsSink>> = const { Cell::new(None) };
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
}
