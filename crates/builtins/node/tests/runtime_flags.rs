// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The parsing of the Node.js runtime flags `--allow-node-runtime-flags` accepts.

use node_builtins::runtime_flags::{parse, RuntimeFlags, UnhandledRejections};

fn parse_flags(flags: &[&str]) -> Result<RuntimeFlags, String> {
    let flags: Vec<String> = flags.iter().map(|flag| flag.to_string()).collect();
    parse(&flags, false)
}

#[test]
fn dashes_and_underscores_are_interchangeable() {
    let settings = parse_flags(&["--expose_gc", "--expose-internals", "--no_warnings"]).unwrap();
    assert!(settings.expose_gc);
    assert!(settings.expose_internals);
    assert!(!settings.warnings);
}

#[test]
fn valued_flags_take_their_value_after_an_equals_sign() {
    let settings = parse_flags(&[
        "--title=foo",
        "--network-family-autoselection-attempt-timeout=123",
        "--no-network-family-autoselection",
        "--max-old-space-size=16",
        "--dns-result-order=ipv4first",
        "--unhandled-rejections=warn-with-error-code",
    ])
    .unwrap();
    assert_eq!(settings.title.as_deref(), Some("foo"));
    assert_eq!(settings.network_family_autoselection_attempt_timeout, 123.0);
    assert!(!settings.network_family_autoselection);
    assert_eq!(settings.dns_result_order.as_deref(), Some("ipv4first"));
    assert!(parse_flags(&["--title"]).is_err());
    assert!(parse_flags(&["--expose-gc=1"]).is_err());
    assert!(parse_flags(&["--network-family-autoselection-attempt-timeout=x"]).is_err());
    assert!(parse_flags(&["--dns-result-order=first"]).is_err());
    assert_eq!(
        settings.unhandled_rejections,
        UnhandledRejections::WarnWithErrorCode
    );
    assert_eq!(
        parse_flags(&["--unhandled-rejections=foobar"]).unwrap_err(),
        "invalid value for --unhandled-rejections"
    );
}

#[test]
fn unsupported_flags_are_rejected_with_a_reason() {
    let error = parse_flags(&["--allow-natives-syntax"]).unwrap_err();
    assert!(error.contains("`--allow-natives-syntax`"), "{error}");
    assert!(error.contains("intrinsics"), "{error}");
    assert!(parse_flags(&["--allow-fs-read=*"]).is_err());
    assert!(parse_flags(&["--permission"]).is_err());
    let error = parse_flags(&["--no-such-flag"]).unwrap_err();
    assert_eq!(error, "Unknown Node.js runtime flag `--no-such-flag`");
    assert!(parse_flags(&["-r"]).is_err());
}

/// Every flag `SUPPORTED_FLAGS` in `tests/node-harness/run-node-tests.mjs` lists isn't unknown to
/// the runtime, so the harness runs the tests that have it.
#[test]
fn the_harness_lists_known_flags() {
    let harness = include_str!("../../../../tests/node-harness/run-node-tests.mjs");
    let list = harness
        .split_once("const SUPPORTED_FLAGS = [")
        .and_then(|(_, rest)| rest.split_once("];"))
        .expect("the harness has SUPPORTED_FLAGS")
        .0;
    let names: Vec<&str> = list
        .split(',')
        .map(|name| name.trim().trim_matches('"'))
        .filter(|name| !name.is_empty())
        .collect();
    assert!(names.len() > 10, "{names:?}");
    for name in names {
        if let Err(error) = parse_flags(&[&format!("--{name}")]) {
            assert!(!error.starts_with("Unknown"), "{error}");
        }
    }
}
