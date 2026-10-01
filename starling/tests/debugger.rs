// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! End-to-end tests of `starlingmonkey --debug=stdio`, driven by a DAP client.

#![cfg(not(target_arch = "wasm32"))]

mod support;

use std::path::{Path, PathBuf};

use serde_json::json;
use support::dap::{values, Dap};
use support::{write_script, AWAIT, INSPECT, LOOP};

#[test]
fn session_runs_the_script_after_configuration_and_reports_its_exit() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["console.log('hello');", "console.warn('careful');"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let seen = dap.events_until("terminated");
    let category = |output: &str| {
        seen.iter()
            .find(|m| m["event"] == "output" && m["body"]["output"] == output)
            .map(|m| m["body"]["category"].clone())
    };
    assert_eq!(category("hello\n"), Some(json!("stdout")), "{seen:?}");
    assert_eq!(
        category("Warn: careful\n"),
        Some(json!("stderr")),
        "{seen:?}"
    );
    let exited = seen.iter().find(|m| m["event"] == "exited");
    assert_eq!(
        exited.map(|m| m["body"]["exitCode"].clone()),
        Some(json!(0))
    );
    dap.close_stdin();
    assert!(dap.wait_for_exit().success());
}

#[test]
fn threads_lists_one_javascript_thread() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["1;"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    let threads = dap.request("threads", json!({}));
    assert_eq!(
        threads["threads"],
        json!([{ "id": 1, "name": "JavaScript" }])
    );
    dap.request("configurationDone", json!({}));
    dap.run_to_end();
}

#[test]
fn launch_starts_a_session_like_attach() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('launched');"]);
    let mut dap = Dap::spawn(&script);
    dap.request("initialize", json!({ "adapterID": "starling" }));
    dap.request("launch", json!({}));
    dap.event("initialized");
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.run_to_end().output, "launched\n");
}

#[test]
fn initialized_follows_the_attach_response() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["1;"]);
    let mut dap = Dap::spawn(&script);
    dap.request("initialize", json!({ "adapterID": "starling" }));
    let attach = dap.send_request("attach", json!({}));
    let seen = dap.events_until("initialized");
    assert!(
        seen.iter()
            .any(|m| m["type"] == "response" && m["request_seq"] == attach),
        "`initialized` preceded the `attach` response: {seen:?}"
    );
    dap.request("configurationDone", json!({}));
    dap.run_to_end();
}

#[test]
fn unhandled_rejections_are_reported_as_output() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["Promise.reject(new Error('rejected-late'));"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let finished = dap.run_to_end();
    assert_eq!(
        finished.output.matches("rejected-late").count(),
        1,
        "{}",
        finished.output
    );
}

#[test]
fn unknown_requests_fail_without_ending_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('ran');"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    let seq = dap.send_request("stepBack", json!({ "threadId": 1 }));
    let response = dap.response(seq);
    assert_eq!(response["success"], false);
    assert!(
        response["message"]
            .as_str()
            .unwrap()
            .contains("Unsupported request 'stepBack'"),
        "{response}"
    );
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.run_to_end().output, "ran\n");
}

#[test]
fn top_level_throw_is_reported_before_exit() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["throw new Error('boom');"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let finished = dap.run_to_end();
    assert!(finished.output.contains("boom"), "{}", finished.output);
    assert_eq!(finished.exit_code, Some(1));
    assert_eq!(finished.status.code(), Some(1));
}

#[test]
fn disconnect_with_terminate_ends_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["setTimeout(() => {}, 60000);"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.request("disconnect", json!({ "terminateDebuggee": true }));
    dap.event("terminated");
    assert!(dap.wait_for_exit().success());
}

#[test]
fn breakpoint_stops_each_time_its_line_runs() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", LOOP);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    let breakpoints = dap.set_breakpoints(&script, &[3]);
    assert_eq!(
        breakpoints[0]["verified"], false,
        "the module is not loaded yet"
    );
    dap.request("configurationDone", json!({}));
    let bound = dap.event("breakpoint");
    assert_eq!(bound["breakpoint"]["verified"], true);
    assert_eq!(bound["breakpoint"]["line"], 3);
    for _ in 0..3 {
        let stopped = dap.event("stopped");
        assert_eq!(stopped["reason"], "breakpoint");
        assert_eq!(stopped["threadId"], 1);
        assert_eq!(stopped["hitBreakpointIds"], json!([breakpoints[0]["id"]]));
        dap.request("continue", json!({ "threadId": 1 }));
    }
    assert_eq!(dap.run_to_end().output, "total 3\n");
}

#[test]
fn breakpoint_on_a_line_without_code_moves_to_the_next_line_with_code() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["// comment", "", "console.log('a');"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[1]);
    dap.request("configurationDone", json!({}));
    let bound = dap.event("breakpoint");
    assert_eq!(bound["breakpoint"]["line"], 3);
    dap.event("stopped");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "a\n");
}

#[test]
fn breakpoint_in_a_dynamically_imported_module_binds_when_it_loads() {
    let dir = tempfile::tempdir().unwrap();
    let dep = write_script(
        dir.path(),
        "dep.mjs",
        &["export function f() {", "  return 42;", "}"],
    );
    let main = write_script(
        dir.path(),
        "main.mjs",
        &[
            "const { f } = await import('./dep.mjs');",
            "console.log(f());",
        ],
    );
    let mut dap = Dap::spawn(&main);
    dap.start(json!({}));
    dap.set_breakpoints(&dep, &[2]);
    dap.request("configurationDone", json!({}));
    let bound = dap.event("breakpoint");
    assert_eq!(bound["breakpoint"]["verified"], true);
    assert_eq!(bound["breakpoint"]["line"], 2);
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "42\n");
}

#[test]
fn console_output_before_a_breakpoint_arrives_before_the_stop() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["console.log('before');", "console.log('after');"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[2]);
    dap.request("configurationDone", json!({}));
    let seen = dap.events_until("stopped");
    let before = seen
        .iter()
        .position(|m| m["event"] == "output" && m["body"]["output"] == "before\n");
    assert!(
        before.is_some(),
        "`before` did not precede the stop: {seen:?}"
    );
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn closing_the_connection_while_paused_lets_the_script_finish() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('x');"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[1]);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    dap.close_stdin();
    assert!(dap.wait_for_exit().success());
}

#[test]
fn closing_both_pipes_while_paused_lets_the_script_finish() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &[
            "debugger;",
            "setTimeout(() => { console.log('a'); Promise.reject(new Error('r1')); }, 300);",
            "setTimeout(() => Promise.reject(new Error('r2')), 600);",
            "setTimeout(() => Promise.reject(new Error('r3')), 900);",
            "setTimeout(() => Promise.reject(new Error('r4')), 1200);",
        ],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    dap.close_stdout();
    dap.close_stdin();
    assert_eq!(dap.wait_for_exit().code(), Some(0));
}

#[test]
fn breakpoint_locations_lists_the_positions_on_a_line() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["const a = 1, b = 2;", "console.log(a + b);"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[1]);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    let locations = dap.request(
        "breakpointLocations",
        json!({ "source": { "path": script }, "line": 2 }),
    );
    let locations = locations["breakpoints"].as_array().unwrap();
    assert!(!locations.is_empty());
    assert!(locations.iter().all(|l| l["line"] == 2), "{locations:?}");
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn terminate_request_ends_the_process() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["setTimeout(() => {}, 60000);"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.request("terminate", json!({}));
    assert_eq!(dap.event("exited")["exitCode"], 0);
    dap.event("terminated");
    assert!(dap.wait_for_exit().success());
}

#[test]
fn requests_are_answered_while_the_script_waits_on_a_timer() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["setTimeout(() => console.log('done'), 3000);"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let threads = dap.request("threads", json!({}));
    assert_eq!(threads["threads"][0]["id"], 1);
    assert_eq!(dap.run_to_end().output, "done\n");
}

#[test]
fn breakpoint_set_while_running_applies_to_a_later_callback() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["setTimeout(() => {", "  console.log('fired');", "}, 1000);"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let breakpoints = dap.set_breakpoints(&script, &[2]);
    assert_eq!(breakpoints[0]["verified"], true);
    assert_eq!(dap.event("stopped")["reason"], "breakpoint");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "fired\n");
}

#[test]
fn disconnect_before_configuration_lets_the_script_run() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('ran');"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("disconnect", json!({}));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "ran\n");
    assert!(finished.status.success());
}

#[test]
fn closing_the_connection_after_disconnect_sends_no_further_response() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["setTimeout(() => console.log('late'), 1000);"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.request("disconnect", json!({}));
    dap.request("disconnect", json!({}));
    dap.close_stdin();
    let seen = dap.events_until("terminated");
    assert!(
        !seen.iter().any(|m| m["type"] == "response"),
        "unexpected response: {seen:?}"
    );
    assert!(dap.wait_for_exit().success());
}

/// Start a session on `INSPECT`, stopped at line 3.
fn stopped_in_add(dir: &Path) -> (Dap, PathBuf) {
    let script = write_script(dir, "main.mjs", INSPECT);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[3]);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    (dap, script)
}

#[test]
fn stack_trace_names_frames_and_positions() {
    let dir = tempfile::tempdir().unwrap();
    let (mut dap, _) = stopped_in_add(dir.path());
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let frames = trace["stackFrames"].as_array().unwrap();
    assert_eq!(frames[0]["name"], "add");
    assert_eq!(frames[0]["line"], 3);
    assert!(frames[0]["source"]["path"]
        .as_str()
        .unwrap()
        .ends_with("main.mjs"));
    assert_eq!(frames[1]["line"], 6);
    assert_eq!(trace["totalFrames"], frames.len());
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn local_scope_lists_parameters_and_locals() {
    let dir = tempfile::tempdir().unwrap();
    let (mut dap, _) = stopped_in_add(dir.path());
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let frame_id = trace["stackFrames"][0]["id"].clone();
    let scopes = dap.request("scopes", json!({ "frameId": frame_id }));
    assert_eq!(scopes["scopes"][0]["name"], "Local");
    let reference = scopes["scopes"][0]["variablesReference"].clone();
    let locals = values(&dap.request("variables", json!({ "variablesReference": reference })));
    for expected in [("a", "1"), ("b", "41"), ("sum", "42")] {
        assert!(
            locals.contains(&(expected.0.to_string(), expected.1.to_string())),
            "{locals:?}"
        );
    }
    let last = scopes["scopes"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(last["name"], "Global");
    assert_eq!(last["expensive"], true);
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn objects_expand_into_their_properties() {
    let dir = tempfile::tempdir().unwrap();
    let (mut dap, _) = stopped_in_add(dir.path());
    let point = dap.request(
        "evaluate",
        json!({ "expression": "point", "context": "watch" }),
    );
    assert_eq!(point["result"], "{…}");
    let properties = dap.request(
        "variables",
        json!({ "variablesReference": point["variablesReference"] }),
    );
    let entries = values(&properties);
    assert!(entries.contains(&("x".into(), "1".into())), "{entries:?}");
    assert!(
        entries.contains(&("y".into(), "Array(2)".into())),
        "{entries:?}"
    );
    let y = properties["variables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["name"] == "y")
        .unwrap()["variablesReference"]
        .clone();
    let elements = values(&dap.request("variables", json!({ "variablesReference": y })));
    assert!(elements.contains(&("0".into(), "2".into())), "{elements:?}");
    assert!(
        elements.contains(&("length".into(), "2".into())),
        "{elements:?}"
    );
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn set_variable_changes_what_the_script_sees() {
    let dir = tempfile::tempdir().unwrap();
    let (mut dap, _) = stopped_in_add(dir.path());
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let scopes = dap.request(
        "scopes",
        json!({ "frameId": trace["stackFrames"][0]["id"] }),
    );
    let local = scopes["scopes"][0]["variablesReference"].clone();
    let set = dap.request(
        "setVariable",
        json!({ "variablesReference": local, "name": "sum", "value": "100" }),
    );
    assert_eq!(set["value"], "100");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "100\n");
}

#[test]
fn evaluate_reports_thrown_exceptions() {
    let dir = tempfile::tempdir().unwrap();
    let (mut dap, _) = stopped_in_add(dir.path());
    let seq = dap.send_request(
        "evaluate",
        json!({ "expression": "nope.x", "context": "repl" }),
    );
    let failed = dap.response(seq);
    assert_eq!(failed["success"], false);
    assert!(
        failed["message"].as_str().unwrap().contains("not defined"),
        "{failed}"
    );
    let sum = dap.request(
        "evaluate",
        json!({ "expression": "sum", "context": "repl" }),
    );
    assert_eq!(sum["result"], "42");
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn evaluate_while_running_can_use_timers() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["setTimeout(() => console.log('late'), 2000);"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    let result = dap.request(
        "evaluate",
        json!({
            "expression": "setTimeout(() => console.log('from eval'), 0); 1 + 1",
            "context": "repl",
        }),
    );
    assert_eq!(result["result"], "2");
    let output = dap.run_to_end().output;
    assert!(
        output.contains("from eval\n") && output.contains("late\n"),
        "{output}"
    );
}

#[test]
fn set_variable_on_a_const_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &[
            "function f() {",
            "  const fixed = 1;",
            "  return fixed;",
            "}",
            "console.log(f());",
        ],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[3]);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let scopes = dap.request(
        "scopes",
        json!({ "frameId": trace["stackFrames"][0]["id"] }),
    );
    let local = scopes["scopes"][0]["variablesReference"].clone();
    let seq = dap.send_request(
        "setVariable",
        json!({ "variablesReference": local, "name": "fixed", "value": "2" }),
    );
    let failed = dap.response(seq);
    assert_eq!(failed["success"], false);
    assert!(
        failed["message"].as_str().unwrap().contains("const"),
        "{failed}"
    );
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "1\n");
}

/// `outer` calls `inner` on line 5.
const STEPS: &[&str] = &[
    "function inner() {",
    "  return 1;",
    "}",
    "function outer() {",
    "  const v = inner();",
    "  return v + 1;",
    "}",
    "console.log(outer());",
];

/// Start a session on `lines`, stopped at a breakpoint on `line`.
fn stopped_at(dir: &Path, lines: &[&str], line: u32) -> Dap {
    let script = write_script(dir, "main.mjs", lines);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[line]);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    dap
}

/// Send `command` and return the `stopped` event it leads to.
fn step(dap: &mut Dap, command: &str) -> serde_json::Value {
    dap.request(command, json!({ "threadId": 1 }));
    dap.event("stopped")
}

#[test]
fn next_moves_to_the_next_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut dap = stopped_at(dir.path(), STEPS, 5);
    assert_eq!(step(&mut dap, "next")["reason"], "step");
    assert_eq!(dap.top_frame(), ("outer".to_string(), 6));
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "2\n");
}

#[test]
fn step_in_enters_the_called_function() {
    let dir = tempfile::tempdir().unwrap();
    let mut dap = stopped_at(dir.path(), STEPS, 5);
    step(&mut dap, "stepIn");
    assert_eq!(dap.top_frame().0, "inner");
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn step_out_returns_to_the_caller() {
    let dir = tempfile::tempdir().unwrap();
    let mut dap = stopped_at(dir.path(), STEPS, 2);
    step(&mut dap, "stepOut");
    assert_eq!(dap.top_frame().0, "outer");
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn next_steps_over_await() {
    let dir = tempfile::tempdir().unwrap();
    let mut dap = stopped_at(dir.path(), AWAIT, 2);
    step(&mut dap, "next");
    assert_eq!(dap.top_frame(), ("work".to_string(), 3));
    step(&mut dap, "next");
    assert_eq!(dap.top_frame(), ("work".to_string(), 4));
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "3\n");
}

#[test]
fn pause_request_stops_the_next_callback() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["setTimeout(() => {", "  console.log('tick');", "}, 1000);"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.request("pause", json!({ "threadId": 1 }));
    assert_eq!(dap.event("stopped")["reason"], "pause");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "tick\n");
}

#[test]
fn debugger_statement_stops() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &["const x = 1;", "debugger;", "console.log(x);"],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "debugger statement");
    assert_eq!(dap.top_frame().1, 2);
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn a_pause_runs_no_content_microtasks() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &[
            "Promise.resolve().then(() => console.log('job'));",
            "debugger;",
            "console.log('after');",
        ],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "debugger statement");
    assert_eq!(dap.evaluate("1 + 1")["result"], "2");
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "after\njob\n");
}

#[test]
fn evaluate_before_configuration_runs_with_an_event_loop_and_drains_microtasks() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('content');"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    let evaluated = dap
        .evaluate("setTimeout(() => {}, 10); Promise.resolve().then(() => console.log('job')); 1");
    assert_eq!(evaluated["result"], "1");
    dap.wait_for_output("job\n");
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.run_to_end().output, "content\n");
}

#[test]
fn jobs_an_evaluate_queues_while_paused_run_after_resume() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &[
            "Promise.resolve().then(() => console.log('content job'));",
            "debugger;",
            "console.log('after');",
        ],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "debugger statement");
    dap.evaluate("Promise.resolve().then(() => console.log('evaluated job'))");
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "after\ncontent job\nevaluated job\n");
}

#[test]
fn jobs_an_evaluate_queues_while_paused_in_a_job_run_after_resume() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(
        dir.path(),
        "main.mjs",
        &[
            "Promise.resolve().then(() => { debugger; });",
            "setTimeout(() => console.log('timer'), 0);",
        ],
    );
    let mut dap = Dap::spawn(&script);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "debugger statement");
    dap.evaluate("Promise.resolve().then(() => console.log('job'))");
    dap.request("continue", json!({ "threadId": 1 }));
    let finished = dap.run_to_end();
    assert_eq!(finished.output, "job\ntimer\n");
    assert_eq!(finished.exit_code, Some(0));
}

#[test]
fn stop_on_entry_stops_before_the_first_line() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["console.log('first');"]);
    let mut dap = Dap::spawn(&script);
    dap.start(json!({ "stopOnEntry": true }));
    dap.request("configurationDone", json!({}));
    assert_eq!(dap.event("stopped")["reason"], "entry");
    assert_eq!(dap.top_frame().1, 1);
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "first\n");
}

#[test]
fn source_request_returns_text_for_sources_without_a_path() {
    let mut dap = Dap::spawn_with_args(&["-e", "debugger;"]);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let source = trace["stackFrames"][0]["source"].clone();
    assert!(source.get("path").is_none(), "{source}");
    let reference = source["sourceReference"].as_i64().unwrap();
    assert!(reference > 0);
    let text = dap.request("source", json!({ "sourceReference": reference }));
    assert_eq!(text["content"], "debugger;");
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn loaded_sources_lists_imported_modules_and_not_evaluated_code() {
    let dir = tempfile::tempdir().unwrap();
    write_script(dir.path(), "dep.mjs", &["export const v = 7;"]);
    let main = write_script(
        dir.path(),
        "main.mjs",
        &["import { v } from './dep.mjs';", "console.log(v);"],
    );
    let mut dap = Dap::spawn(&main);
    dap.start(json!({}));
    dap.set_breakpoints(&main, &[2]);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    dap.request(
        "evaluate",
        json!({ "expression": "v + 1", "context": "repl" }),
    );
    let loaded = dap.request("loadedSources", json!({}));
    let names: Vec<&str> = loaded["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"main.mjs") && names.contains(&"dep.mjs"),
        "{names:?}"
    );
    // `Debugger.Frame.eval` names the code it compiles "debugger eval code".
    assert!(!names.contains(&"debugger eval code"), "{names:?}");
    dap.request("continue", json!({ "threadId": 1 }));
    dap.run_to_end();
}

#[test]
fn loaded_source_events_announce_dynamically_imported_modules() {
    let dir = tempfile::tempdir().unwrap();
    write_script(dir.path(), "dep.mjs", &["export const v = 7;"]);
    let main = write_script(
        dir.path(),
        "main.mjs",
        &[
            "const { v } = await import('./dep.mjs');",
            "console.log(v);",
        ],
    );
    let mut dap = Dap::spawn(&main);
    dap.start(json!({}));
    dap.request("configurationDone", json!({}));
    loop {
        let event = dap.event("loadedSource");
        if event["source"]["name"] == "dep.mjs" {
            assert_eq!(event["reason"], "new");
            break;
        }
    }
    dap.run_to_end();
}

#[test]
fn breakpoint_binds_in_an_entry_script_given_by_a_relative_path() {
    let dir = tempfile::tempdir().unwrap();
    let script = write_script(dir.path(), "main.mjs", &["const a = 1;", "console.log(a);"]);
    let mut dap = Dap::spawn_in(dir.path(), &["main.mjs"]);
    dap.start(json!({}));
    dap.set_breakpoints(&script, &[2]);
    dap.request("configurationDone", json!({}));
    let bound = dap.event("breakpoint");
    assert_eq!(bound["breakpoint"]["verified"], true);
    dap.event("stopped");
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    let source = &trace["stackFrames"][0]["source"];
    assert_eq!(source["path"], script.to_str().unwrap(), "{source}");
    assert!(source.get("sourceReference").is_none(), "{source}");
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "1\n");
}

#[test]
fn remote_root_maps_client_paths() {
    let dir = tempfile::tempdir().unwrap();
    // The entry script's URL is its canonical path, so `remoteRoot` names the canonical directory.
    let remote_root = std::fs::canonicalize(dir.path()).unwrap();
    let main = write_script(dir.path(), "main.mjs", &["console.log('mapped');"]);
    let mut dap = Dap::spawn(&main);
    dap.start(json!({ "localRoot": "/client/project", "remoteRoot": remote_root }));
    dap.set_breakpoints(Path::new("/client/project/main.mjs"), &[1]);
    dap.request("configurationDone", json!({}));
    dap.event("stopped");
    let trace = dap.request("stackTrace", json!({ "threadId": 1 }));
    assert_eq!(
        trace["stackFrames"][0]["source"]["path"],
        "/client/project/main.mjs"
    );
    dap.request("continue", json!({ "threadId": 1 }));
    assert_eq!(dap.run_to_end().output, "mapped\n");
}
