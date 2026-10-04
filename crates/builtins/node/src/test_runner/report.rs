// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The output of `node:test`, in the format of Node.js's `spec` reporter without colors: a line
//! per finished test, nested under a line per suite or test with subtests, then the counts of the
//! results and the errors of the failed tests. See Node.js's
//! `lib/internal/test_runner/reporter/spec.js`.

use std::ffi::CStr;

use js::conversion::FromJSVal;
use js::error::ExnThrown;
use js::gc::scope::Scope;
use js::prelude::HandleValue;
use js::Object;

use super::{elements, harness, kind_of, objects, parent_of, push, ROOT, SUITE};
use crate::stream::{as_object, call_method, rust_string, string, Props};

/// The harness's counters of finished tests, in the order the summary lists them.
pub(super) const COUNTERS: [&CStr; 7] = [
    c"tests",
    c"suites",
    c"passed",
    c"failed",
    c"cancelled",
    c"skipped",
    c"todo",
];

/// The labels the summary gives [`COUNTERS`].
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
    let process = scope.global().get_property(scope, c"process")?;
    let stdout = match as_object(scope, process) {
        Some(process) => process.obj(scope, c"stdout")?,
        None => None,
    };
    match stdout {
        Some(stdout) => {
            call_method(scope, stdout, c"write", &[string(scope, text)?])?;
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// Two spaces per level of `nesting`.
fn indent(nesting: f64) -> String {
    "  ".repeat(nesting.max(0.0) as usize)
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

/// The line of the finished `record`, without indentation: its symbol, name, duration, and its
/// `SKIP`, `TODO` or `EXPECTED FAILURE` directive.
fn title(scope: &Scope<'_>, record: Object<'_>) -> Result<String, ExnThrown> {
    let passed = record.flag(scope, c"passed")?;
    let mut symbol = if passed { "\u{2714} " } else { "\u{2716} " };
    let name = display(scope, record.get_property(scope, c"name")?);
    let duration = record.num(scope, c"endTime")? - record.num(scope, c"startTime")?;
    let mut title = name;
    if duration != 0.0 {
        let rounded = (duration * 1e6).round() / 1e6;
        title.push_str(&format!(
            " ({}ms)",
            crate::errors::number_to_string(rounded)
        ));
    }
    let message = rust_string(scope, record.get_property(scope, c"message")?)
        .filter(|message| !message.is_empty());
    if record.flag(scope, c"skipped")? {
        title.push_str(&format!(" # {}", message.as_deref().unwrap_or("SKIP")));
        symbol = "\u{fe63} ";
    } else if record.flag(scope, c"todo")? {
        title.push_str(&format!(" # {}", message.as_deref().unwrap_or("TODO")));
        if !passed {
            symbol = "\u{26a0} ";
        }
    } else if record.flag(scope, c"expectFailure")? {
        title.push_str(" # EXPECTED FAILURE");
    }
    Ok(format!("{symbol}{title}"))
}

/// The error of the failed `record`, as the lines below its title show it.
fn error_lines(scope: &Scope<'_>, record: Object<'_>, nesting: f64) -> Result<String, ExnThrown> {
    let indentation = indent(nesting);
    let error = record.get_property(scope, c"error")?;
    let text = inspect(scope, error).replace("\r\n", "\n");
    let joined = text
        .split('\n')
        .collect::<Vec<_>>()
        .join(&format!("\n{indentation}  "));
    Ok(format!("\n{indentation}  {joined}\n"))
}

/// Add the finished `record` to the harness's counters, as Node.js's `countCompletedTest` does.
fn count(scope: &Scope<'_>, harness: Object<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    let increment = |name: &'static CStr| -> Result<(), ExnThrown> {
        let value = harness.num(scope, name)?;
        harness.set_property(scope, name, value + 1.0)
    };
    let passed = record.flag(scope, c"passed")?;
    let todo = record.flag(scope, c"todo")?;
    if kind_of(scope, record)? == SUITE {
        increment(c"suites")?;
        if !passed && !todo {
            harness.set_property(scope, c"success", false)?;
        }
        return Ok(());
    }
    if record.flag(scope, c"skipped")? {
        increment(c"skipped")?;
    } else if todo {
        increment(c"todo")?;
    } else if record.flag(scope, c"cancelled")? {
        increment(c"cancelled")?;
        harness.set_property(scope, c"success", false)?;
    } else if !passed {
        increment(c"failed")?;
        harness.set_property(scope, c"success", false)?;
    } else {
        increment(c"passed")?;
    }
    increment(c"tests")
}

/// Report the finished test or suite `record`: count it, and write its line, after a line for
/// each of its ancestors that has none yet, and its diagnostics. A failed record is kept for the
/// summary's list of failures, unless it failed because its subtests did.
pub(super) fn report_test(scope: &Scope<'_>, record: Object<'_>) -> Result<(), ExnThrown> {
    if record.flag(scope, c"filtered")? || kind_of(scope, record)? == ROOT {
        return Ok(());
    }
    let harness = harness(scope)?;
    count(scope, harness, record)?;
    let mut headers = Vec::new();
    let mut current = parent_of(scope, record)?;
    while let Some(ancestor) = current {
        if kind_of(scope, ancestor)? == ROOT || ancestor.flag(scope, c"announced")? {
            break;
        }
        ancestor.set_property(scope, c"announced", true)?;
        let name = display(scope, ancestor.get_property(scope, c"name")?);
        let nesting = ancestor.num(scope, c"nesting")?;
        headers.push(format!("{}\u{25b6} {name}\n", indent(nesting)));
        current = parent_of(scope, ancestor)?;
    }
    headers.reverse();
    let nesting = record.num(scope, c"nesting")?;
    let mut text = headers.concat();
    text.push_str(&format!("{}{}\n", indent(nesting), title(scope, record)?));
    for message in elements(scope, record, c"diagnostics")? {
        text.push_str(&format!(
            "{}\u{2139} {}\n",
            indent(nesting),
            display(scope, message)
        ));
    }
    write(scope, &text)?;
    let failure_type = record.get_property(scope, c"failureType")?;
    if !record.flag(scope, c"passed")?
        && !crate::stream::is_str(scope, failure_type, c"subtestsFailed")?
    {
        let failures = harness.obj(scope, c"failures")?.ok_or(ExnThrown)?;
        push(scope, failures, record)?;
    }
    Ok(())
}

/// Report the failure of one of the root's `after` hooks with `error`: a failed test named after
/// the entry module.
pub(super) fn report_root_hook_failure(
    scope: &Scope<'_>,
    error: HandleValue<'_>,
) -> Result<(), ExnThrown> {
    let harness = harness(scope)?;
    harness.set_property(scope, c"success", false)?;
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
    harness: Object<'_>,
    root: Object<'_>,
) -> Result<(), ExnThrown> {
    let mut text = String::new();
    for message in elements(scope, root, c"diagnostics")? {
        text.push_str(&format!("\u{2139} {}\n", display(scope, message)));
    }
    for (counter, label) in COUNTERS.iter().zip(COUNTER_LABELS) {
        let value = harness.num(scope, counter)?;
        text.push_str(&format!(
            "\u{2139} {label} {}\n",
            crate::errors::number_to_string(value)
        ));
    }
    let duration = root.num(scope, c"endTime")? - root.num(scope, c"startTime")?;
    let rounded = (duration * 1e6).round() / 1e6;
    text.push_str(&format!(
        "\u{2139} duration_ms {}\n",
        crate::errors::number_to_string(rounded)
    ));
    let failures = objects(scope, harness, c"failures")?;
    if !failures.is_empty() {
        let mut parts = vec!["\n\u{2716} failing tests:\n".to_string()];
        for record in failures {
            parts.push(format!(
                "{}{}",
                title(scope, record)?,
                error_lines(scope, record, 0.0)?
            ));
        }
        text.push_str(&parts.join("\n"));
    }
    write(scope, &text)?;
    Ok(())
}
