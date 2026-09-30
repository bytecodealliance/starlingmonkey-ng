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
//! worlds (`tests/fixtures`). The compiler is the one `npm ci` installs at the
//! workspace root, and the tests fail without it.
//!
//! Every declaration is generated once and shared by the tests. The type-checks run
//! [`TSC_JOBS`] compiler processes at a time, one per fixture, since the `declare module`
//! blocks of different fixtures would merge in one program. Every compiler process runs with
//! no stdin and a wall-clock limit, and is killed if it exceeds it.

mod shared;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use componentize::wit_to_ts;
use wit_parser::Resolve;

/// Where a fixture's world comes from.
enum Source {
    /// The world named by the second element in `tests/wit_to_ts/<first>.wit`.
    Wit(&'static str, &'static str),
    /// The world named by the second element in `tests/fixtures/<first>.wit`,
    /// a world from which the componentize suites build components.
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
    // shared-types module, and each importing block re-exports it. An exported
    // interface names an imported resource in its own signature.
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
    // A `stream` and a `future` without a payload type, with a guest module.
    Fixture {
        name: "payloadless",
        source: Source::Wit("payloadless", "payloadless"),
        guests: &["payloadless.guest.ts"],
    },
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
        source: Source::Builtin("--export-cli"),
        guests: &["builtin_cli.guest.ts"],
    },
    Fixture {
        name: "builtin_serve",
        source: Source::Builtin("--export-http"),
        guests: &["builtin_serve.listener.ts", "builtin_serve.handler.ts"],
    },
];

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/wit_to_ts")
}

fn suite_fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The `.d.ts` of each of [`FIXTURES`], in order, generated once.
static GENERATED: LazyLock<Vec<String>> = LazyLock::new(|| FIXTURES.iter().map(generate).collect());

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

/// A world from which the componentize suites build components, and the
/// JavaScript guest modules they componentize against it.
struct SuiteFixture {
    /// The name of the world's `.d.ts` file.
    name: &'static str,
    /// The `starling-componentize` flags that select the world, with paths
    /// relative to `tests/fixtures`.
    world: &'static [&'static str],
    /// The guests, as paths relative to `tests/fixtures`.
    guests: &'static [&'static str],
}

/// Every suite world with its guests.
const SUITE_FIXTURES: &[SuiteFixture] = &[
    SuiteFixture {
        name: "async_export",
        world: &["-w", "async_export.wit"],
        guests: &["async_export.js"],
    },
    SuiteFixture {
        name: "async_import",
        world: &["-w", "async_import.wit"],
        guests: &["async_import.js"],
    },
    SuiteFixture {
        name: "cli_and_serve",
        world: &["--export-cli", "--export-http"],
        guests: &["cli_and_serve.js"],
    },
    SuiteFixture {
        name: "cli_merged",
        world: &[
            "-w",
            "cli/a.wit",
            "-w",
            "cli/b.wit",
            "-n",
            "test:cli-a/one",
            "-n",
            "two",
        ],
        guests: &["cli/app/lib.js", "cli/app/src/main.js"],
    },
    SuiteFixture {
        name: "cli_run",
        world: &["-w", "cli_run.wit"],
        guests: &["cli_run.js", "cli_run_no_run.js"],
    },
    SuiteFixture {
        name: "exported_resource",
        world: &["-w", "exported_resource.wit"],
        guests: &["exported_resource.js"],
    },
    SuiteFixture {
        name: "fetch_streams",
        world: &["-w", "fetch_streams.wit"],
        guests: &["fetch_streams.js"],
    },
    SuiteFixture {
        name: "lowering",
        world: &["-w", "lowering.wit"],
        guests: &["lowering.js"],
    },
    SuiteFixture {
        name: "resources",
        world: &["-w", "resources.wit"],
        guests: &["resources.js"],
    },
    SuiteFixture {
        name: "serve",
        world: &["-w", "serve.wit"],
        guests: &[
            "serve.js",
            "serve_no_listener.js",
            "serve_raw.js",
            "serve_raw_and_listener.js",
            "serve_raw_response.js",
            "serve_wasi_imports.js",
        ],
    },
    SuiteFixture {
        name: "streams",
        world: &["-w", "streams.wit"],
        guests: &["streams.js"],
    },
    SuiteFixture {
        name: "surface",
        world: &["-w", "surface.wit"],
        guests: &["surface.js"],
    },
    SuiteFixture {
        name: "sync_suite",
        world: &["-w", "sync_suite.wit"],
        guests: &["sync_suite.js"],
    },
    SuiteFixture {
        name: "top_level",
        world: &["-w", "top_level.wit"],
        guests: &[
            "top_level/main.js",
            "top_level/entry.js",
            "top_level/lib/back.js",
            "top_level/lib/late.js",
            "top_level/lib/name.js",
        ],
    },
    SuiteFixture {
        name: "values",
        world: &["-w", "values.wit"],
        guests: &["values.js"],
    },
    SuiteFixture {
        name: "wit_dir",
        world: &["-w", "wit_dir"],
        guests: &["wit_dir.js"],
    },
    SuiteFixture {
        name: "world_resource",
        world: &["-w", "world_resource.wit"],
        guests: &["world_resource.js"],
    },
];

#[test]
fn snapshots_match() {
    let bless = std::env::var_os("BLESS").is_some();
    let mut mismatches = Vec::new();

    for (fixture, actual) in FIXTURES.iter().zip(GENERATED.iter()) {
        let stem = fixture.name;
        let snapshot = fixtures_dir().join(format!("{stem}.d.ts.expected"));

        if bless {
            std::fs::write(&snapshot, actual).expect("writing blessed snapshot");
            continue;
        }

        let expected = std::fs::read_to_string(&snapshot)
            .unwrap_or_else(|e| panic!("reading snapshot `{}`: {e}", snapshot.display()));
        if *actual != expected {
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
/// A declaration file parses on its own
/// however its `declare module` blocks are reached, and only an import from
/// another file distinguishes an ambient module declaration, which resolves,
/// from an augmentation of a module that does not exist, which does not.
#[test]
fn generated_dts_is_valid_typescript() {
    let tsc = tsc();

    let out_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wit_to_ts_dts");
    std::fs::create_dir_all(&out_dir).expect("creating tsc scratch dir");

    let mut checks = Vec::new();
    for (fixture, dts) in FIXTURES.iter().zip(GENERATED.iter()) {
        let stem = fixture.name;
        let dts_path = out_dir.join(format!("{stem}.d.ts"));
        std::fs::write(&dts_path, dts).expect("writing generated .d.ts for tsc");
        let consumer_path = out_dir.join(format!("{stem}_consumer.ts"));
        std::fs::write(&consumer_path, consumer_for(dts)).expect("writing consumer for tsc");

        let mut files = vec![dts_path, consumer_path];
        files.extend(
            fixture
                .guests
                .iter()
                .map(|guest| fixtures_dir().join(guest)),
        );
        checks.push((stem.to_string(), files));
    }
    let failures = run_tsc_all(&tsc, checks, Guests::TypeScript);

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
    let tsc = tsc();
    let out_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("wit_to_ts_suite_guests");
    std::fs::create_dir_all(&out_dir).expect("creating tsc scratch dir");
    let suite_fixtures = suite_fixtures_dir();

    let mut checks = Vec::new();
    for fixture in SUITE_FIXTURES {
        let args: Vec<&str> = fixture.world.iter().copied().chain(["types"]).collect();
        let output = shared::run_cli(&args, Some(&suite_fixtures));
        assert!(
            output.status.success(),
            "`starling-componentize {}` failed:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        let dts = String::from_utf8(output.stdout).expect("UTF-8 output");
        let stem = fixture.name;
        let dts_path = out_dir.join(format!("{stem}.d.ts"));
        std::fs::write(&dts_path, &dts).expect("writing generated .d.ts for tsc");

        let mut files = vec![dts_path];
        files.extend(
            fixture
                .guests
                .iter()
                .map(|guest| suite_fixtures.join(guest)),
        );
        checks.push((stem.to_string(), files));
    }
    let failures = run_tsc_all(&tsc, checks, Guests::JavaScript);

    assert!(
        failures.is_empty(),
        "{} suite world(s) failed `tsc --checkJs`:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

/// Every JavaScript module in `tests/fixtures` is a guest of one of
/// [`SUITE_FIXTURES`].
#[test]
fn every_suite_guest_is_type_checked() {
    fn modules(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("reading a fixtures directory") {
            let path = entry.expect("reading a fixtures directory").path();
            if path.is_dir() {
                modules(&path, base, out);
            } else if path.extension().is_some_and(|ext| ext == "js") {
                let relative = path
                    .strip_prefix(base)
                    .expect("a path under `tests/fixtures`");
                out.push(relative.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    let base = suite_fixtures_dir();
    let mut found = Vec::new();
    modules(&base, &base, &mut found);
    let checked: Vec<&str> = SUITE_FIXTURES
        .iter()
        .flat_map(|fixture| fixture.guests.iter().copied())
        .collect();
    let unchecked: Vec<&String> = found
        .iter()
        .filter(|module| !checked.contains(&module.as_str()))
        .collect();
    assert!(
        unchecked.is_empty(),
        "add these guests to `SUITE_FIXTURES`: {unchecked:?}"
    );
}

/// A TypeScript module importing every module `dts` declares, so a specifier
/// that does not resolve is a compile error.
///
/// The specifiers are read back out of the generated text, which keeps the
/// consumer in step with whatever the generator emitted.
fn consumer_for(dts: &str) -> String {
    let mut out = String::from("// Generated by the `wit_to_ts` type-check.\n");
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

/// The TypeScript compiler `npm ci` installs at the workspace root, as the argv
/// prefix to invoke it.
fn tsc() -> Vec<String> {
    let tsc = shared::workspace_root().join("node_modules/.bin/tsc");
    assert!(
        tsc.exists(),
        "no TypeScript compiler at `{}`: install it with `npm ci` at the workspace root",
        tsc.display()
    );
    vec![tsc.to_string_lossy().into_owned()]
}

/// The language of the guest modules [`run_tsc`] checks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Guests {
    TypeScript,
    JavaScript,
}

/// How many type-checks run at a time.
const TSC_JOBS: usize = 4;

/// Type-check each named set of files with [`run_tsc`], [`TSC_JOBS`] at a time.
/// Returns a report for each set that failed, in the order of `checks`.
fn run_tsc_all(tsc: &[String], checks: Vec<(String, Vec<PathBuf>)>, guests: Guests) -> Vec<String> {
    let next = AtomicUsize::new(0);
    let mut results: Vec<(usize, String)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..TSC_JOBS)
            .map(|_| {
                scope.spawn(|| {
                    let mut failures = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some((name, files)) = checks.get(index) else {
                            break;
                        };
                        let files: Vec<&Path> = files.iter().map(PathBuf::as_path).collect();
                        let output = run_tsc(tsc, &files, guests);
                        if !output.status.success() {
                            failures.push((
                                index,
                                format!(
                                    "--- {name} ---\n{}{}",
                                    String::from_utf8_lossy(&output.stdout),
                                    String::from_utf8_lossy(&output.stderr),
                                ),
                            ));
                        }
                    }
                    failures
                })
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|worker| worker.join().expect("a type-check panicked"))
            .collect()
    });
    results.sort_by_key(|(index, _)| *index);
    results.into_iter().map(|(_, report)| report).collect()
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
        // exports. JSON modules are imported with `with { type: "json" }`.
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
/// stdin is `/dev/null`, so a child that reads stdin reads EOF.
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
