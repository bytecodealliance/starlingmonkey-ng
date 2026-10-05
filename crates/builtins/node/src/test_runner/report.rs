// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The output of `node:test`, in the format of Node.js's `spec` reporter without colors: a line
//! per finished test, nested under a line per suite or test with subtests, then the counts of the
//! results and the errors of the failed tests. See Node.js's
//! `lib/internal/test_runner/reporter/spec.js`.

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::handle::Heap;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use super::{harness, parent_of, Harness, TestRecord, ROOT, SUITE};
use crate::support::rust_string;

/// The indices of the harness's counters of finished tests.
const TESTS: usize = 0;
const SUITES: usize = 1;
const PASSED: usize = 2;
const FAILED: usize = 3;
const CANCELLED: usize = 4;
const SKIPPED: usize = 5;
const TODO: usize = 6;

/// The labels the summary gives the harness's counters, in their order.
const COUNTER_LABELS: [&str; 7] = [
    "tests",
    "suites",
    "pass",
    "fail",
    "cancelled",
    "skipped",
    "todo",
];

/// Write `text` with `process.stdout.write`, or to the standard output if there is no
/// `process.stdout`.
fn write(scope: &Scope<'_>, text: &str) -> Result<(), ExnThrown> {
    let process = crate::process::process_value(scope)?;
    let stdout = match Object::from_value(scope, process).ok() {
        Some(process) => Object::from_value(scope, process.get_property(scope, c"stdout")?).ok(),
        None => None,
    };
    match stdout {
        Some(stdout) => {
            js::Function::call_by_name(scope, stdout.handle(), c"write", &[text])?;
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// Two spaces per level of `nesting`.
fn indent(nesting: u32) -> String {
    "  ".repeat(nesting as usize)
}

/// `value` converted to a string, as a template literal converts it, or as `util.inspect` shows
/// it if that conversion throws.
pub(super) fn display(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    if let Some(text) = rust_string(scope, value) {
        return text;
    }
    if value.is_symbol() {
        return crate::inspect::symbol_to_string(scope, value);
    }
    match String::from_jsval(scope, value, ()) {
        Ok(text) => text,
        Err(_) => {
            js::exception::clear(scope);
            inspect(scope, value)
        }
    }
}

/// `value` as `util.inspect` shows it on one line.
fn inspect(scope: &Scope<'_>, value: HandleValue<'_>) -> String {
    let options = crate::inspect::InspectOptions {
        break_length: f64::INFINITY,
        ..Default::default()
    };
    crate::inspect::inspect(scope, value, options).unwrap_or_else(|_| {
        js::exception::clear(scope);
        "[error]".into()
    })
}

/// The duration between `start` and `end` in milliseconds, as the reporter shows it.
fn duration(start: Option<f64>, end: Option<f64>) -> f64 {
    let duration = end.unwrap_or(0.0) - start.unwrap_or(0.0);
    (duration * 1e6).round() / 1e6
}

/// The line of the finished `record`, without indentation: its symbol, name, duration, and its
/// `SKIP`, `TODO` or `EXPECTED FAILURE` directive.
fn title(scope: &Scope<'_>, record: TestRecord<'_>) -> String {
    let data = record.data();
    let mut symbol = if data.passed {
        "\u{2714} "
    } else {
        "\u{2716} "
    };
    let mut title = data.name.clone();
    let duration = duration(data.start_time, data.end_time);
    if duration != 0.0 {
        title.push_str(&format!(
            " ({}ms)",
            crate::errors::number_to_string(duration)
        ));
    }
    let message = rust_string(scope, data.message.get(scope)).filter(|message| !message.is_empty());
    if data.skipped {
        title.push_str(&format!(" # {}", message.as_deref().unwrap_or("SKIP")));
        symbol = "\u{fe63} ";
    } else if data.todo {
        title.push_str(&format!(" # {}", message.as_deref().unwrap_or("TODO")));
        if !data.passed {
            symbol = "\u{26a0} ";
        }
    } else if data.expect_failure {
        title.push_str(" # EXPECTED FAILURE");
    }
    format!("{symbol}{title}")
}

/// The error of the failed `record`, as the lines below its title show it.
fn error_lines(scope: &Scope<'_>, record: TestRecord<'_>, nesting: u32) -> String {
    let indentation = indent(nesting);
    let error = record.data().error.get(scope);
    let text = inspect(scope, error).replace("\r\n", "\n");
    let joined = text
        .split('\n')
        .collect::<Vec<_>>()
        .join(&format!("\n{indentation}  "));
    format!("\n{indentation}  {joined}\n")
}

/// Add the finished `record` to the harness's counters, as Node.js's `countCompletedTest` does.
fn count(harness: Harness<'_>, record: TestRecord<'_>) {
    let record = record.data();
    let mut harness = harness.data_mut();
    if record.kind == SUITE {
        harness.counters[SUITES] += 1;
        if !record.passed && !record.todo {
            harness.success = false;
        }
        return;
    }
    if record.skipped {
        harness.counters[SKIPPED] += 1;
    } else if record.todo {
        harness.counters[TODO] += 1;
    } else if record.cancelled {
        harness.counters[CANCELLED] += 1;
        harness.success = false;
    } else if !record.passed {
        harness.counters[FAILED] += 1;
        harness.success = false;
    } else {
        harness.counters[PASSED] += 1;
    }
    harness.counters[TESTS] += 1;
}

/// Report the finished test or suite `record`: count it, and write its line, after a line for
/// each of its ancestors that has none yet, and its diagnostics. A failed record is kept for the
/// summary's list of failures, unless it failed because its subtests did.
pub(super) fn report_test(scope: &Scope<'_>, record: TestRecord<'_>) -> Result<(), ExnThrown> {
    if record.data().filtered || record.data().kind == ROOT {
        return Ok(());
    }
    let harness = harness(scope)?;
    count(harness, record);
    let mut headers = Vec::new();
    let mut current = parent_of(scope, record);
    while let Some(ancestor) = current {
        {
            let mut data = ancestor.data_mut();
            if data.kind == ROOT || data.announced {
                break;
            }
            data.announced = true;
            headers.push(format!("{}\u{25b6} {}\n", indent(data.nesting), data.name));
        }
        current = parent_of(scope, ancestor);
    }
    headers.reverse();
    let nesting = record.data().nesting;
    let mut text = headers.concat();
    text.push_str(&format!("{}{}\n", indent(nesting), title(scope, record)));
    let diagnostics: Vec<HandleValue<'_>> = record
        .data()
        .diagnostics
        .iter()
        .map(|message| message.get(scope))
        .collect();
    for message in diagnostics {
        text.push_str(&format!(
            "{}\u{2139} {}\n",
            indent(nesting),
            display(scope, message)
        ));
    }
    write(scope, &text)?;
    let listed = {
        let data = record.data();
        !data.passed && data.failure_type != "subtestsFailed"
    };
    if listed {
        harness.data_mut().failures.push(Heap::from(record));
    }
    Ok(())
}

/// Report the failure of one of the root's `after` hooks with `error`: a failed test named after
/// the entry module.
pub(super) fn report_root_hook_failure(
    scope: &Scope<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    harness(scope)?.data_mut().success = false;
    let file = super::entry_file(scope)?;
    let name = display(scope, file);
    let text = format!(
        "\u{2716} {name}\n  {}\n",
        inspect(scope, error).replace('\n', "\n  ")
    );
    write(scope, &text)
}

/// Write the summary: the root's diagnostics, the counters and the duration, then the failed
/// tests with their errors.
pub(super) fn summary(
    scope: &Scope<'_>,
    harness: Harness<'_>,
    root: TestRecord<'_>,
) -> Result<(), ExnThrown> {
    let mut text = String::new();
    let diagnostics: Vec<HandleValue<'_>> = root
        .data()
        .diagnostics
        .iter()
        .map(|message| message.get(scope))
        .collect();
    for message in diagnostics {
        text.push_str(&format!("\u{2139} {}\n", display(scope, message)));
    }
    let counters = harness.data().counters;
    for (value, label) in counters.iter().zip(COUNTER_LABELS) {
        text.push_str(&format!("\u{2139} {label} {value}\n"));
    }
    let (start, end) = {
        let data = root.data();
        (data.start_time, data.end_time)
    };
    text.push_str(&format!(
        "\u{2139} duration_ms {}\n",
        crate::errors::number_to_string(duration(start, end))
    ));
    let failures: Vec<TestRecord<'_>> = harness
        .data()
        .failures
        .iter()
        .map(|record| record.get(scope))
        .collect();
    if !failures.is_empty() {
        let mut parts = vec!["\n\u{2716} failing tests:\n".to_string()];
        for record in failures {
            parts.push(format!(
                "{}{}",
                title(scope, record),
                error_lines(scope, record, 0)
            ));
        }
        text.push_str(&parts.join("\n"));
    }
    write(scope, &text)
}
