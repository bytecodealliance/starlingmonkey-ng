// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! StarlingMonkey — a JavaScript runtime built on SpiderMonkey.
//!
//! Parses command-line arguments into a [`RuntimeConfig`](libstarling::config::RuntimeConfig)
//! and delegates execution to [`libstarling::run`]. The wasm build is the `starling` cdylib in
//! `lib.rs`. On wasm targets this binary is built but does nothing.

#[cfg(not(target_arch = "wasm32"))]
use std::process::exit;

#[cfg(not(target_arch = "wasm32"))]
use libstarling::config::RuntimeConfig;

/// The environment variable whose options, separated by spaces, the command line gets after the
/// program name, each unless the command line has it already. A `--node-compat` process sets it in
/// its own environment, which its child processes inherit.
#[cfg(not(target_arch = "wasm32"))]
const NODE_OPTIONS_VARIABLE: &str = "STARLINGMONKEY_NODE_OPTIONS";

/// The short options refused in [`NODE_OPTIONS_VARIABLE`]: `-e`, `-p`, `-i` and `-d`.
#[cfg(not(target_arch = "wasm32"))]
const REFUSED_SHORT_OPTIONS: &[char] = &['e', 'p', 'i', 'd'];

/// The long options refused in [`NODE_OPTIONS_VARIABLE`], with or without a `=value`.
#[cfg(not(target_arch = "wasm32"))]
const REFUSED_LONG_OPTIONS: &[&str] = &[
    "--eval",
    "--print",
    "--initializer-script",
    "--debug",
    "--serve",
    "--serve-isolated",
];

/// The options of `variable`, the value of [`NODE_OPTIONS_VARIABLE`], or the first refused one.
/// Options that run code, start a debugger or change what runs are refused, as Node.js refuses
/// them in `NODE_OPTIONS`: the refused short and long options, a cluster of short options that
/// contains a refused one, and arguments that aren't options.
#[cfg(not(target_arch = "wasm32"))]
fn node_options(variable: &str) -> Result<Vec<&str>, &str> {
    variable
        .split_whitespace()
        .map(|option| {
            let refused = match option.strip_prefix("--") {
                Some(_) => {
                    let name = option.split_once('=').map_or(option, |(name, _)| name);
                    REFUSED_LONG_OPTIONS.contains(&name)
                }
                None => match option.strip_prefix('-') {
                    Some(cluster) => cluster.chars().any(|c| REFUSED_SHORT_OPTIONS.contains(&c)),
                    None => true,
                },
            };
            if refused {
                Err(option)
            } else {
                Ok(option)
            }
        })
        .collect()
}

/// The command line, with the options of [`NODE_OPTIONS_VARIABLE`] added after the program name.
/// Exits with status 9, as Node.js does, if the variable has a refused option.
#[cfg(not(target_arch = "wasm32"))]
fn args() -> Vec<String> {
    let mut args: Vec<String> = std::env::args().collect();
    if let Ok(variable) = std::env::var(NODE_OPTIONS_VARIABLE) {
        let options = match node_options(&variable) {
            Ok(options) => options,
            Err(option) => {
                let program = args.first().map_or("starlingmonkey", String::as_str);
                eprintln!("{program}: {option} is not allowed in {NODE_OPTIONS_VARIABLE}");
                exit(9);
            }
        };
        let at = 1.min(args.len());
        for option in options.into_iter().rev() {
            if !args.iter().any(|arg| arg == option) {
                args.insert(at, option.to_string());
            }
        }
    }
    args
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    let config = match RuntimeConfig::from_args(args()) {
        Ok(config) => config,
        // `--help` and `--version` print to stdout and exit with 0. A usage error prints to
        // stderr and exits with 2.
        Err(e) => e.exit(),
    };

    if let Err(e) = libstarling::run(config) {
        eprintln!("{e}");
        exit(1);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    unreachable!("On wasm targets the `starling` cdylib is the entry point, not this binary");
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[test]
fn cli_runs() {
    let config = libstarling::config::RuntimeConfig::from_args(
        ["starlingmonkey", "-e", "1 + 1"]
            .iter()
            .map(|s| s.to_string()),
    )
    .unwrap();
    libstarling::run(config)
        .map_err(|e| println!("{e}"))
        .expect("Run failed");
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[test]
fn refused_node_options_fail() {
    assert_eq!(
        node_options(" --node-compat  --allow-node-runtime-flags --node-http-listen "),
        Ok(vec![
            "--node-compat",
            "--allow-node-runtime-flags",
            "--node-http-listen"
        ])
    );
    for refused in [
        "-e",
        "--eval=1",
        "-p",
        "--print",
        "-i",
        "--initializer-script",
        "--debug=listen:0.0.0.0:9229",
        "-d",
        "-ve",
        "--serve=8080",
        "--serve-isolated",
        "app.js",
    ] {
        assert_eq!(
            node_options(&format!("--node-compat {refused}")),
            Err(refused)
        );
    }
}
