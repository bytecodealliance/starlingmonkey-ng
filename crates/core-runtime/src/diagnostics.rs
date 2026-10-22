// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Messages that are written to stderr at the next microtask checkpoint instead of where they
//! arise, for code such as finalizers that can't report anything itself.

use std::cell::RefCell;

js::instance_local! {
    /// The messages [`report_later`] queued, in the order it queued them.
    static QUEUED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Queue `message` to be written to stderr by the next
/// [`run_microtasks`](crate::event_loop::run_microtasks). It neither runs JavaScript nor allocates
/// GC things, so a finalizer can call it. A message queued while the instance is torn down is
/// dropped. Messages queued after the last checkpoint, such as by finalizers in the final GC, are
/// written only if a later checkpoint runs in the same instance.
pub fn report_later(message: String) {
    let _ = QUEUED.try_with(|queued| queued.borrow_mut().push(message));
}

/// Write the messages [`report_later`] queued to stderr, each followed by a newline.
pub(crate) fn report_queued() {
    for message in take_queued() {
        eprintln!("{message}");
    }
}

fn take_queued() -> Vec<String> {
    QUEUED.with(RefCell::take)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;
    use crate::event_loop::run_microtasks;
    use crate::runtime::Runtime;

    #[test]
    fn queued_messages_are_reported_by_the_next_microtask_checkpoint() {
        let runtime = Runtime::init(&RuntimeConfig::default()).expect("runtime init");
        let scope = runtime.default_global();
        report_later("first".to_string());
        report_later("second".to_string());
        assert_eq!(
            QUEUED.with(|queued| queued.borrow().clone()),
            ["first", "second"]
        );
        run_microtasks(&scope);
        assert!(take_queued().is_empty());
    }
}
