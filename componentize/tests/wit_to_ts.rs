// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! Snapshot tests for the WIT-world → guest `.d.ts` generator
//! (`componentize::wit_to_ts`).
//!
//! Each fixture targets one WIT shape group, and the generated declaration is
//! compared against the committed `tests/wit_to_ts/<name>.d.ts.expected`
//! snapshot. Set `BLESS=1` to rewrite the snapshots from the current generator
//! output.
//!
//! Every generated declaration is also type-checked with `tsc --noEmit --strict`,
//! so a snapshot that is structurally invalid TypeScript fails even if it matches
//! its committed form. A fixture may list hand-written guest modules
//! (`tests/wit_to_ts/<name>.*.ts`), which are type-checked against the
//! declaration together with it. The JavaScript guests the componentize suites
//! use are type-checked with `--checkJs` against the declarations of their
//! worlds (`tests/fixtures`). The test fails when no compiler is found.
//!
//! Every compiler process runs with no stdin and a wall-clock limit, and is killed if it
//! exceeds it. `npx` offers to install a package it cannot resolve and waits on stdin for the
//! answer, which hung this test for as long as the run lasted. The `tsc` name on npm belongs
//! to an unrelated package, so what `npx tsc` resolves is only treated as a compiler when it
//! reports a TypeScript version.

#![cfg(not(target_arch = "wasm32"))]

mod shared;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use componentize::wit_to_ts;
use wit_parser::Resolve;

/// Where a fixture's world comes from.
enum Source {
    /// The world named by the second element in `tests/wit_to_ts/<first>.wit`.
    Wit(&'static str, &'static str),
    /// The world named by the second element in `tests/fixtures/<first>.wit`,
    /// a world the componentize suites build components from.
    Suite(&'static str, &'static str),
    /// A built-in world, selected by passing this flag to the
    /// `starling-componentize types` binary.
    Builtin(&'static str),
}

/// One fixture: its name, which names its snapshot `<name>.d.ts.expected`, its
/// world, and the guest modules type-checked against its declaration, as file
/// names in `tests/wit_to_ts`.
struct Fixture {
    name: &'static str,
    source: Source,
    guests: &'static [&'static str],
}

/// A fixture for the world `world` in `tests/wit_to_ts/<name>.wit`, with no
/// guest modules.
const fn wit(name: &'static str, world: &'static str) -> Fixture {
    Fixture {
        name,
        source: Source::Wit(name, world),
        guests: &[],
    }
}

/// The fixtures: one per WIT shape group, the naming shapes, and the two
/// built-in worlds.
const FIXTURES: &[Fixture] = &[
    wit("primitives", "primitives"),
    wit("lists", "lists"),
    wit("option_result", "option-result"),
    wit("records_tuples", "records-tuples"),
    wit("variant_enum_flags", "variant-enum-flags"),
    wit("imports", "imports"),
    wit("world_funcs", "world-funcs"),
    // WIT names escaped with `%` that mangle to JavaScript reserved words.
    wit("reserved_names", "reserved-names"),
    // Parameters named after reserved words, and `this`.
    wit("reserved_params", "reserved-params"),
    // A guest-owned resource, from the world of the `exported_resource` suite.
    Fixture {
        name: "exported_resource",
        source: Source::Suite("exported_resource", "exported-resource"),
        guests: &[],
    },
    // Imported resources at interface and world level. The class lives in the
    // shared-types module and each importing block re-exports it, and an
    // exported interface names an imported resource in its own signature.
    wit("imported_resources", "imported-resources"),
    // An imported resource with async methods and stream parameters, reached
    // only through an exported interface's signatures.
    wit("resource_polarity", "resource-polarity"),
    // An interface both imported and exported: a host class and a guest class
    // for one resource.
    Fixture {
        name: "dual",
        source: Source::Wit("dual", "dual"),
        guests: &["dual.guest.ts"],
    },
    // Types with the same name in several interfaces and packages.
    wit("duplicate_names", "duplicate-names"),
    // WIT types named after the globals the declarations refer to.
    wit("global_names", "global-names"),
    // Error classes of `use`d, renamed and world-level `err` types, and an enum
    // used only as an exported function's `err` type.
    Fixture {
        name: "error_types",
        source: Source::Wit("error_types", "error-types"),
        guests: &["error_types.guest.ts"],
    },
    // Streams, futures and error classes.
    wit("streams", "streams"),
    // The naming shapes of `starling:guest`.
    Fixture {
        name: "naming_unique",
        source: Source::Wit("naming", "naming-unique"),
        guests: &[
            "naming_unique.package.ts",
            "naming_unique.interface.ts",
            "naming_unique.bare.ts",
        ],
    },
    Fixture {
        name: "naming_ambiguous",
        source: Source::Wit("naming", "naming-ambiguous"),
        guests: &["naming_ambiguous.guest.ts"],
    },
    Fixture {
        name: "naming_reserved",
        source: Source::Wit("naming", "naming-reserved"),
        guests: &["naming_reserved.guest.ts"],
    },
    Fixture {
        name: "naming_versions",
        source: Source::Wit("naming", "naming-versions"),
        guests: &["naming_versions.guest.ts"],
    },
    // Exported-resource and handed-over value shapes, with a guest module.
    Fixture {
        name: "guest_shapes",
        source: Source::Wit("guest_shapes", "guest-shapes"),
        guests: &["guest_shapes.guest.ts"],
    },
    // A world declaring `wasi:http/handler` and `wasi:http/types` itself.
    Fixture {
        name: "serve",
        source: Source::Wit("serve", "serve"),
        guests: &["serve.handler.ts"],
    },
    Fixture {
        name: "builtin_cli",
        source: Source::Builtin("--cli"),
        guests: &["builtin_cli.guest.ts"],
    },
    Fixture {
        name: "builtin_serve",
        source: Source::Builtin("--serve"),
        guests: &["builtin_serve.listener.ts", "builtin_serve.handler.ts"],
    },
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wit_to_ts")
}

fn suite_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Generate the `.d.ts` for one fixture.
fn generate(fixture: &Fixture) -> String {
    match fixture.source {
        Source::Wit(stem, world) => generate_wit(&fixtures_dir(), stem, world),
        Source::Suite(stem, world) => generate_wit(&suite_fixtures_dir(), stem, world),
        Source::Builtin(flag) => shared::cli_output(&[flag, "types"]),
    }
}

/// Generate the `.d.ts` for the world `world` in `<dir>/<stem>.wit`.
fn generate_wit(dir: &Path, stem: &str, world: &str) -> String {
    let wit = dir.join(format!("{stem}.wit"));
    let mut resolve = Resolve::default();
    let package = resolve
        .push_file(&wit)
        .unwrap_or_else(|e| panic!("parsing `{}`: {e}", wit.display()));
    let world_id = resolve
        .select_world(&[package], Some(world))
        .unwrap_or_else(|e| panic!("selecting world `{world}` in `{stem}.wit`: {e}"));
    wit_to_ts::generate(&resolve, world_id).expect("generating .d.ts")
}

/// A world the componentize suites build components from, as a `.wit` file or a
/// directory in `tests/fixtures`, and the JavaScript guest modules they
/// componentize against it.
struct SuiteFixture {
    wit: &'static str,
    guests: &'static [&'static str],
}

/// Every suite world with its guests.
const SUITE_FIXTURES: &[SuiteFixture] = &[
    SuiteFixture {
        wit: "async_export.wit",
        guests: &["async_export.js"],
    },
    SuiteFixture {
        wit: "async_import.wit",
        guests: &["async_import.js"],
    },
    SuiteFixture {
        wit: "cli_run.wit",
        guests: &["cli_run.js", "cli_run_no_run.js"],
    },
    SuiteFixture {
        wit: "exported_resource.wit",
        guests: &["exported_resource.js"],
    },
    SuiteFixture {
        wit: "fetch_streams.wit",
        guests: &["fetch_streams.js"],
    },
    SuiteFixture {
        wit: "lowering.wit",
        guests: &["lowering.js"],
    },
    SuiteFixture {
        wit: "parity_demo.wit",
        guests: &["parity_demo.js"],
    },
    SuiteFixture {
        wit: "resources.wit",
        guests: &["resources.js"],
    },
    SuiteFixture {
        wit: "serve.wit",
        guests: &[
            "serve.js",
            "serve_after_await.js",
            "serve_no_listener.js",
            "serve_raw.js",
            "serve_raw_and_listener.js",
            "serve_raw_response.js",
            "serve_raw_throws.js",
        ],
    },
    SuiteFixture {
        wit: "smoke.wit",
        guests: &["smoke.js"],
    },
    SuiteFixture {
        wit: "streams.wit",
        guests: &["streams.js"],
    },
    SuiteFixture {
        wit: "surface.wit",
        guests: &["surface.js"],
    },
    SuiteFixture {
        wit: "sync_suite.wit",
        guests: &["sync_suite.js"],
    },
    SuiteFixture {
        wit: "top_level.wit",
        guests: &[
            "top_level/main.js",
            "top_level/entry.js",
            "top_level/lib/back.js",
            "top_level/lib/late.js",
            "top_level/lib/name.js",
        ],
    },
    SuiteFixture {
        wit: "values.wit",
        guests: &["values.js"],
    },
    SuiteFixture {
        wit: "wit_dir",
        guests: &["wit_dir.js"],
    },
    SuiteFixture {
        wit: "world_resource.wit",
        guests: &["world_resource.js"],
    },
];

#[test]
fn snapshots_match() {
    let bless = std::env::var_os("BLESS").is_some();
    let mut mismatches = Vec::new();

    for fixture in FIXTURES {
        let stem = fixture.name;
        let actual = generate(fixture);
        let snapshot = fixtures_dir().join(format!("{stem}.d.ts.expected"));

        if bless {
            std::fs::write(&snapshot, &actual).expect("writing blessed snapshot");
            continue;
        }

        let expected = std::fs::read_to_string(&snapshot)
            .unwrap_or_else(|e| panic!("reading snapshot `{}`: {e}", snapshot.display()));
        if actual != expected {
            mismatches.push(format!(
                "--- {stem} ---\nEXPECTED:\n{expected}\nACTUAL:\n{actual}"
            ));
        }
    }

    assert!(
        mismatches.is_empty(),
        "{} snapshot(s) differ (run with BLESS=1 to update):\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

/// Type-check every generated declaration with `tsc --noEmit --strict`, together
/// with a consumer module that imports from each module the declaration declares
/// and with the fixture's guest modules.
///
/// The consumer makes the gate meaningful. A declaration file parses on its own
/// however its `declare module` blocks are reached, and only an import from
/// another file distinguishes an ambient module declaration, which resolves,
/// from an augmentation of a module that does not exist, which does not.
#[test]
fn generated_dts_is_valid_typescript() {
    let tsc = find_tsc().unwrap_or_else(|| {
        panic!(
            "no TypeScript compiler found: looked for `node_modules/.bin/tsc`, `tsc` on PATH, \
             and `npx tsc`. Install the pinned one with `npm ci` at the workspace root."
        )
    });

    let out_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wit_to_ts_dts");
    std::fs::create_dir_all(&out_dir).expect("creating tsc scratch dir");

    let mut failures = Vec::new();
    for fixture in FIXTURES {
        let stem = fixture.name;
        let dts = generate(fixture);
        let dts_path = out_dir.join(format!("{stem}.d.ts"));
        std::fs::write(&dts_path, &dts).expect("writing generated .d.ts for tsc");
        let consumer_path = out_dir.join(format!("{stem}_consumer.ts"));
        std::fs::write(&consumer_path, consumer_for(&dts)).expect("writing consumer for tsc");

        let guests: Vec<PathBuf> = fixture
            .guests
            .iter()
            .map(|guest| fixtures_dir().join(guest))
            .collect();
        let mut files: Vec<&Path> = vec![&dts_path, &consumer_path];
        files.extend(guests.iter().map(PathBuf::as_path));
        let output = run_tsc(&tsc, &files, Guests::TypeScript);
        if !output.status.success() {
            failures.push(format!(
                "--- {stem} ---\n{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} generated declaration(s) failed `tsc --noEmit --strict`:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Type-check the JavaScript guests of every suite world against the declarations
/// `starling-componentize types` generates for it, with `tsc --checkJs`.
///
/// Untyped parameters and caught values are left as `any`, since the guests are
/// plain JavaScript. A guest that misuses a type on purpose, or uses an API the
/// TypeScript DOM library lacks, marks the line with `@ts-expect-error`.
#[test]
fn suite_guests_type_check() {
    let tsc = find_tsc().unwrap_or_else(|| {
        panic!(
            "no TypeScript compiler found: looked for `node_modules/.bin/tsc`, `tsc` on PATH, \
             and `npx tsc`. Install the pinned one with `npm ci` at the workspace root."
        )
    });

    let out_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wit_to_ts_suite_guests");
    std::fs::create_dir_all(&out_dir).expect("creating tsc scratch dir");
    let suite_fixtures = suite_fixtures_dir();

    let mut failures = Vec::new();
    for fixture in SUITE_FIXTURES {
        let wit = suite_fixtures.join(fixture.wit);
        let dts = shared::cli_output(&["-d", &wit.to_string_lossy(), "types"]);
        let stem = fixture.wit.trim_end_matches(".wit");
        let dts_path = out_dir.join(format!("{stem}.d.ts"));
        std::fs::write(&dts_path, &dts).expect("writing generated .d.ts for tsc");

        let guests: Vec<PathBuf> = fixture
            .guests
            .iter()
            .map(|guest| suite_fixtures.join(guest))
            .collect();
        let mut files: Vec<&Path> = vec![&dts_path];
        files.extend(guests.iter().map(PathBuf::as_path));
        let output = run_tsc(&tsc, &files, Guests::JavaScript);
        if !output.status.success() {
            failures.push(format!(
                "--- {} ---\n{}{}",
                fixture.wit,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "{} suite world(s) failed `tsc --checkJs`:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// A TypeScript module importing every module `dts` declares, so a specifier
/// that does not resolve is a compile error.
///
/// The specifiers are read back out of the generated text, which keeps the
/// consumer in step with whatever the generator emitted.
fn consumer_for(dts: &str) -> String {
    let mut out = String::from("// Generated by the `wit_to_ts` compile gate.\n");
    for (i, line) in dts
        .lines()
        .filter_map(|line| line.strip_prefix("declare module '"))
        .filter_map(|rest| rest.split_once('\''))
        .map(|(spec, _)| spec)
        .enumerate()
    {
        let _ = writeln!(out, "import * as m{i} from '{line}';");
        let _ = writeln!(out, "void m{i};");
    }
    out
}

/// How long one type-check may run before it is killed.
const TSC_TIME_LIMIT: Duration = Duration::from_secs(120);

/// How long the search for a compiler may spend on one candidate. Short, because reporting a
/// version is all it is being asked to do, and a candidate that cannot manage that promptly is
/// one this test should give up on rather than wait for.
const TSC_PROBE_TIME_LIMIT: Duration = Duration::from_secs(20);

/// Locate a TypeScript compiler: the pinned one under the workspace's
/// `node_modules`, else a bare `tsc` on `PATH`, else `npx tsc`. Returns the argv
/// prefix to invoke it, or `None` if none is available.
fn find_tsc() -> Option<Vec<String>> {
    let local = workspace_root().join("node_modules/.bin/tsc");
    let local = local.to_string_lossy().into_owned();
    for argv in [vec![local.as_str()], vec!["tsc"], vec!["npx", "tsc"]] {
        if reports_typescript_version(&argv) {
            return Some(argv.into_iter().map(str::to_string).collect());
        }
    }
    None
}

/// The repository root, which holds the `package.json` pinning the compiler.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("canonicalizing the workspace root")
}

/// Whether `argv` names the TypeScript compiler, as opposed to being absent or resolving to
/// something else: `tsc --version` prints `Version <number>…`, and the unrelated npm package of
/// the same name prints a banner telling you so.
fn reports_typescript_version(argv: &[&str]) -> bool {
    let (prog, pre) = argv.split_first().expect("non-empty argv");
    let mut cmd = Command::new(prog);
    cmd.args(pre).arg("--version");
    let Some(output) = run_bounded(cmd, TSC_PROBE_TIME_LIMIT) else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.strip_prefix("Version ")
                .is_some_and(|rest| rest.starts_with(|c: char| c.is_ascii_digit()))
        })
}

/// The language of the guest modules [`run_tsc`] checks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guests {
    TypeScript,
    JavaScript,
}

fn run_tsc(tsc: &[String], files: &[&Path], guests: Guests) -> Output {
    let (prog, pre) = tsc.split_first().expect("non-empty tsc argv");
    let mut cmd = Command::new(prog);
    // `esnext` for `Symbol.dispose`, which the resource classes declare, and
    // `dom` for `ReadableStream`, `Request` and `Response`, with
    // `dom.asynciterable` for iterating a `ReadableStream`. `esnext` as the
    // target for `using` in the guest modules, and as the module format for the
    // string export names of versioned interfaces.
    cmd.args(pre).args([
        "--noEmit",
        "--strict",
        "--target",
        "esnext",
        "--module",
        "esnext",
        "--lib",
        "esnext,dom,dom.asynciterable",
    ]);
    if guests == Guests::JavaScript {
        // Every guest file is a module, including one with no imports or
        // exports, and JSON modules are imported with `with { type: "json" }`.
        cmd.args([
            "--allowJs",
            "--checkJs",
            "--noImplicitAny",
            "false",
            "--useUnknownInCatchVariables",
            "false",
            "--moduleDetection",
            "force",
            "--moduleResolution",
            "bundler",
            "--resolveJsonModule",
        ]);
    }
    cmd.args(files);
    run_bounded(cmd, TSC_TIME_LIMIT).unwrap_or_else(|| {
        panic!(
            "`{}` did not finish within {TSC_TIME_LIMIT:?}",
            tsc.join(" ")
        )
    })
}

/// Run `cmd` to completion and collect its output, killing it and returning `None` if it takes
/// longer than `limit` or cannot be spawned at all.
///
/// stdin is `/dev/null` so a child that asks a question reads EOF and gives up rather than
/// waiting for an answer that no one is there to give.
fn run_bounded(mut cmd: Command, limit: Duration) -> Option<Output> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    // The pipes are read while the child runs, so output larger than a pipe's
    // buffer cannot block it.
    let drain = |pipe: Option<Box<dyn std::io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Some(Output {
                    status,
                    stdout: stdout.join().ok()?,
                    stderr: stderr.join().ok()?,
                });
            }
            Ok(None) if start.elapsed() < limit => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
}
