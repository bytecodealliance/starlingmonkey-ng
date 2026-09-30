// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

use {
    crate::Wit,
    anyhow::Context as _,
    clap::Parser as _,
    std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    },
    tokio::runtime::Builder as RuntimeBuilder,
};

/// A utility to convert JavaScript modules into Wasm components
#[derive(clap::Parser, Debug)]
#[command(
    name = "starling-componentize",
    author,
    about,
    disable_version_flag = true
)]
pub struct Options {
    #[command(flatten)]
    pub common: Common,

    /// Print this componentizer's version and the version of the runtime's `init`
    /// interface it calls, and for the runtime in `$STARLING_RUNTIME`, or else the
    /// one it embeds, the SHA-256 digest of its module and the version of that
    /// interface it implements.
    #[arg(short = 'V', long)]
    pub version: bool,

    /// The subcommand to run, required unless `--version` is given.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The fixed CLI world: a world that exports `wasi:cli/run`. The output
/// component exports the runtime's builtin `wasi:cli/run`, which calls the
/// application's `run` export.
///
/// The version must have the major and minor numbers and the pre-release suffix
/// of the one the runtime's builtin exports, because the output component
/// exports that builtin.
const CLI_WORLD_WIT: &str = "\
package starling:cli;

world command {
  export wasi:cli/run@0.3.0;
}

package wasi:cli@0.3.0 {
  interface run {
    run: async func() -> result;
  }
}
";

/// The world name selected from [`CLI_WORLD_WIT`].
const CLI_WORLD_NAME: &str = "command";

/// The fixed serve world: a world exporting `wasi:http/handler`, implemented
/// either by the application or by the runtime's builtin export, which
/// dispatches each request to the `fetch` listeners the application added at
/// Wizer time.
///
/// The interface is empty and names only the version the builtin exports. The
/// runtime's own `wasi:http` package is merged into the world with it.
const SERVE_WORLD_WIT: &str = "\
package starling:serve;

world serve {
  export wasi:http/handler@0.3.0;
}

package wasi:http@0.3.0 {
  interface handler {}
}
";

/// The world name selected from [`SERVE_WORLD_WIT`].
const SERVE_WORLD_NAME: &str = "serve";

#[derive(clap::Args, Clone, Debug)]
pub struct Common {
    /// Files or directories containing WIT document(s).
    ///
    /// A directory brings the packages in its `deps` directory along. This may be
    /// specified more than once, for example: `-d ./wit/deps/<pkg> -d ./wit/app`.
    /// A package several of them define is merged into one with the interfaces
    /// and functions of each. The definitions must agree on the functions they
    /// have in common.
    ///
    /// The targeted world also imports every interface the runtime imports,
    /// with the functions the runtime imports, except those of the WASI APIs
    /// `--disable` names.
    ///
    /// `-d`, `--cli` or `--serve` is required.
    #[arg(short = 'd', long, global = true)]
    pub wit_path: Vec<PathBuf>,

    /// Build a `wasi:cli/run` command component from a JavaScript module that
    /// exports a `run` function.
    ///
    /// The application needs no WIT of its own: its only component export is
    /// `wasi:cli/run`, which forwards to the JS `run` export. With `-d`, the
    /// selected world is merged with a world exporting `wasi:cli/run`. With
    /// `--serve`, the component also exports `wasi:http/handler`, and the module
    /// must serve requests as well.
    #[arg(long, global = true)]
    pub cli: bool,

    /// Build a `wasi:http/handler` serve component from a JavaScript module that
    /// either registers a `fetch` listener with `addEventListener('fetch', …)`
    /// or exports its own implementation of the interface's `handle`.
    ///
    /// The application needs no WIT of its own. With a listener, the runtime's
    /// native `wasi:http/handler` export serves it, dispatching each incoming
    /// request as a `fetch` event. Doing both or neither is an error. With `-d`,
    /// the selected world is merged with a world exporting `wasi:http/handler`.
    /// With `--cli`, the component also exports `wasi:cli/run`, and the module
    /// must export a `run` function as well.
    #[arg(long, global = true)]
    pub serve: bool,

    /// A world to target, by name in the last package `-d` loads, or qualified
    /// as `namespace:package/world@version`. Defaults to that package's only
    /// world.
    ///
    /// Repeat it to target several worlds. They are merged into one world,
    /// `starling:componentize/merged`, which imports and exports everything they
    /// do. Requires `-d`.
    #[arg(short = 'w', long, global = true, requires = "wit_path")]
    pub world: Vec<String>,

    /// Disable non-error output, including what the application prints while it
    /// initializes
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// Comma-separated list of unstable features that should be enabled when processing
    /// WIT files.
    ///
    /// This enables using `@unstable` annotations in WIT files.
    #[clap(long, global = true)]
    pub unstable_features: Vec<String>,

    /// Whether to activate all WIT features when processing WIT files.
    ///
    /// This enables using `@unstable` annotations in WIT files.
    #[clap(long, global = true)]
    pub all_unstable_features: bool,

    /// WASI APIs to leave out of the component. Repeat or comma-separate.
    ///
    /// The output then imports none of the feature's interfaces, and a call
    /// reaching one traps, except that a disabled clock reads as zero. A feature
    /// whose interfaces a remaining interface uses cannot be disabled alone. The
    /// error names the other feature to disable with it.
    #[arg(long, global = true, value_delimiter = ',', value_enum)]
    pub disable: Vec<crate::finalize::WASIAPISet>,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Generate a component from the specified JavaScript module.
    Componentize(Box<Componentize>),
    /// Generate a TypeScript declaration (`.d.ts`) describing the guest module a
    /// componentize target implements for the selected world.
    ///
    /// The output declares the named exports the guest must provide and the
    /// modules it may import, with the WIT→JS value mapping this runtime uses.
    Types(Types),
    /// Print the module specifiers the guest may import for the selected world,
    /// one per line.
    ///
    /// These are the names a bundler has to leave unresolved: one per imported
    /// interface, and `external` for top-level imports of functions and types.
    /// An interface is listed only if it contains exports of functions,
    /// resources, errors, or enum or flags types, since only those are reflected
    /// as importable JS values.
    Imports(Imports),
}

#[derive(clap::Args, Debug)]
pub struct Componentize {
    /// The filename of a JavaScript module from which to generate a component.
    pub input: PathBuf,

    /// The directory the input's relative imports, and those of the modules it
    /// imports, resolve within. Defaults to the input's directory.
    #[arg(short = 'p', long)]
    pub base_directory: Option<PathBuf>,

    /// Output file to which to write the resulting component
    #[arg(short = 'o', long, default_value = "js.wasm")]
    pub output: PathBuf,

    #[command(flatten)]
    pub runtime: RuntimeArgs,

    /// The URL `globalThis.location` reflects while the application's top level
    /// runs. If not provided, reading `location` there throws a `TypeError`, which
    /// fails componentization unless the application catches it.
    #[arg(long)]
    pub init_location: Option<url::Url>,

    /// Leave `wasi:random/insecure-seed` out of the component's imports, and
    /// make `get-insecure-seed` always return SEED, split into its high and low
    /// 64 bits. SEED is a 128-bit number, in decimal or with a `0x` prefix in
    /// hex.
    ///
    /// SpiderMonkey seeds its hash tables from this interface. With SEED, an
    /// attacker can craft input that collides in those tables and slows the
    /// application down drastically. Use this only if the component must not
    /// import the interface, and never with untrusted input.
    #[arg(long, value_name = "SEED", value_parser = parse_seed)]
    pub deterministic_insecure_random_seed: Option<u128>,

    /// Keep the compiled component out of wasmtime's global compilation cache.
    #[arg(long)]
    pub no_cache: bool,
}

#[derive(clap::Args, Debug)]
pub struct Types {
    /// Output file for the generated `.d.ts`. Defaults to stdout.
    #[arg(short = 'o', long)]
    pub output: Option<PathBuf>,

    #[command(flatten)]
    pub runtime: StaticRuntime,
}

#[derive(clap::Args, Debug)]
pub struct Imports {
    #[command(flatten)]
    pub runtime: StaticRuntime,
}

/// The static runtime build a subcommand reads.
#[derive(clap::Args, Debug)]
pub struct StaticRuntime {
    /// Path to the StarlingMonkey runtime to use. If not provided, the runtime embedded in this
    /// tool is used.
    ///
    /// [env: STARLING_RUNTIME=]
    // `$STARLING_RUNTIME` is read by [`StaticRuntime::load`], not by clap, because
    // clap treats a value taken from the environment as given on the command line
    // and would reject it alongside `--runtime-lib`.
    #[arg(long)]
    pub runtime: Option<PathBuf>,
}

impl StaticRuntime {
    /// Load the runtime at `--runtime`, or else at `$STARLING_RUNTIME`, or else
    /// `embedded`. Returns `None` if none of them is given.
    fn load(&self, embedded: Option<&[u8]>) -> anyhow::Result<Option<crate::Runtime>> {
        let path = self
            .runtime
            .clone()
            .or_else(|| std::env::var_os("STARLING_RUNTIME").map(PathBuf::from));
        match (path, embedded) {
            (Some(path), _) => crate::Runtime::static_from(
                fs::read(&path).with_context(|| format!("unable to read `{}`", path.display()))?,
            )
            .map(Some),
            (None, Some(embedded)) => crate::Runtime::static_from(embedded.to_vec()).map(Some),
            (None, None) => Ok(None),
        }
    }
}

/// The runtime build a componentization links against.
#[derive(clap::Args, Debug)]
pub struct RuntimeArgs {
    #[command(flatten)]
    pub static_runtime: StaticRuntime,

    /// Path to libstarling_rt.so (built by `just build-dylib`). Selects the
    /// dynamic link mode, which links the runtime dylib together with the
    /// wasi-sdk shared libraries from --sysroot-libs.
    #[arg(long, conflicts_with = "runtime")]
    pub runtime_lib: Option<PathBuf>,

    /// Directory containing the wasi-sdk wasm32-wasip2 shared libraries, used
    /// with --runtime-lib.
    ///
    /// Defaults to the sysroot under `$WASI_SDK_PATH`, or under `/opt/wasi-sdk`
    /// when that is unset, matching `scripts/build-dylib.sh`.
    ///
    /// Dynamic linking is wasm32-wasip2 only, so this stays on that sysroot
    /// while the static runtime defaults to wasm32-wasip3.
    #[arg(long, default_value_os_t = default_sysroot_libs(), requires = "runtime_lib")]
    pub sysroot_libs: PathBuf,
}

/// The default `--sysroot-libs`, under `$WASI_SDK_PATH` when it is set.
fn default_sysroot_libs() -> PathBuf {
    let sdk = std::env::var_os("WASI_SDK_PATH").unwrap_or_else(|| "/opt/wasi-sdk".into());
    PathBuf::from(sdk).join("share/wasi-sysroot/lib/wasm32-wasip2")
}

/// Run the command line `args`. `embedded_runtime` is the static runtime embedded
/// in the binary. It's used if neither `--runtime` nor `--runtime-lib` are provided.
pub fn run<T: Into<OsString> + Clone, I: IntoIterator<Item = T>>(
    args: I,
    embedded_runtime: Option<&[u8]>,
) -> anyhow::Result<()> {
    let options = Options::parse_from(args);
    if options.version {
        return print_version(embedded_runtime);
    }
    match options.command {
        Some(Command::Componentize(opts)) => componentize(options.common, *opts, embedded_runtime),
        Some(Command::Types(opts)) => types(options.common, opts, embedded_runtime),
        Some(Command::Imports(opts)) => imports(options.common, opts, embedded_runtime),
        None => <Options as clap::CommandFactory>::command()
            .error(
                clap::error::ErrorKind::MissingSubcommand,
                "a subcommand or `--version` is required",
            )
            .exit(),
    }
}

/// Select the WIT source and the worlds to target from the common flags.
/// [`Common::wit_path`] contains paths to WIT packages the user passed.
/// If `--serve` and/or `--cli` are passed, their built-in worlds are merged with
/// the user-provided ones. Without `-d|--wit-path`, `--cli` and `--serve` target their
/// built-in worlds alone, merged if both are given.
fn select_wit(common: &Common) -> anyhow::Result<(Wit<'_>, crate::WorldSelection<'_>)> {
    let mut builtins = Vec::new();
    if common.cli {
        builtins.push((CLI_WORLD_WIT, CLI_WORLD_NAME));
    }
    if common.serve {
        builtins.push((SERVE_WORLD_WIT, SERVE_WORLD_NAME));
    }
    match common.wit_path.as_slice() {
        [] if builtins.is_empty() => anyhow::bail!(
            "no world to target: pass `-d|--wit-path` with a WIT file or directory, `--cli` for a \
             `wasi:cli/run` command, and/or `--serve` for a `wasi:http/handler` server"
        ),
        [] => {
            let (wit, name) = builtins.remove(0);
            Ok((
                Wit::String(wit),
                crate::WorldSelection {
                    worlds: vec![name],
                    builtins,
                    resolved: Vec::new(),
                },
            ))
        }
        paths => Ok((
            Wit::Paths(paths),
            crate::WorldSelection {
                worlds: common.world.iter().map(String::as_str).collect(),
                builtins,
                resolved: Vec::new(),
            },
        )),
    }
}

/// Build a [`wit_parser::Resolve`] and select the target world from the common
/// flags. Returns the resolve and the selected world's id.
fn load_world(common: &Common) -> anyhow::Result<(wit_parser::Resolve, wit_parser::WorldId)> {
    let (wit, world) = select_wit(common)?;
    crate::load_world(
        wit,
        world,
        &common.unstable_features,
        common.all_unstable_features,
    )
}

/// Add a world to [`crate::WorldSelection`] that contains all pre-existing
/// imports in `runtime`, which are imported due to their use in Rust's
/// standard library or runtime builtins. Without a runtime, nothing is added.
fn add_runtime_imports(
    common: &Common,
    selection: &mut crate::WorldSelection<'_>,
    runtime: Option<&crate::Runtime>,
) -> anyhow::Result<()> {
    if let Some(runtime) = runtime {
        selection
            .resolved
            .push(crate::runtime_imports_world(runtime, &common.disable)?);
    }
    Ok(())
}

fn types(common: Common, opts: Types, embedded_runtime: Option<&[u8]>) -> anyhow::Result<()> {
    let runtime = opts.runtime.load(embedded_runtime)?;
    let (wit, mut world) = select_wit(&common)?;
    add_runtime_imports(&common, &mut world, runtime.as_ref())?;
    let (resolve, world) = crate::load_world_for_types(
        wit,
        world,
        &common.unstable_features,
        common.all_unstable_features,
        runtime.as_ref(),
    )?;
    let dts = crate::wit_to_ts::generate(&resolve, world)?;
    match &opts.output {
        Some(path) => {
            fs::write(path, &dts)
                .with_context(|| format!("unable to write `{}`", path.display()))?;
            if !common.quiet {
                println!("Wrote TypeScript declarations to `{}`", path.display());
            }
        }
        None => print!("{dts}"),
    }
    Ok(())
}

fn imports(common: Common, opts: Imports, embedded_runtime: Option<&[u8]>) -> anyhow::Result<()> {
    let runtime = opts.runtime.load(embedded_runtime)?;
    let (wit, mut world) = select_wit(&common)?;
    add_runtime_imports(&common, &mut world, runtime.as_ref())?;
    let (resolve, world) = crate::load_world_for_types(
        wit,
        world,
        &common.unstable_features,
        common.all_unstable_features,
        runtime.as_ref(),
    )?;
    for module in crate::wit_to_ts::import_modules(&resolve, world) {
        println!("{module}");
    }
    Ok(())
}

/// Print output for `--version` (see [`Options::version`]).
fn print_version(embedded_runtime: Option<&[u8]>) -> anyhow::Result<()> {
    use sha2::Digest as _;
    println!("starling-componentize {}", env!("CARGO_PKG_VERSION"));
    println!("calls {}", crate::init_interface_name());
    let Some(runtime) = StaticRuntime { runtime: None }.load(embedded_runtime)? else {
        println!("runtime none: this componentizer embeds none, and STARLING_RUNTIME is unset");
        return Ok(());
    };
    let digest = sha2::Sha256::digest(runtime.module());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    println!("runtime sha256:{hex}");
    match crate::runtime_init_interface(&runtime)? {
        Some(name) => println!("runtime implements {name}"),
        None => println!("runtime implements no {}", crate::INIT_INTERFACE_PREFIX),
    }
    Ok(())
}

/// The runtime build `args` select: the dynamic library of `--runtime-lib`
/// with the shared libraries of `--sysroot-libs`, or else the static runtime
/// of `--runtime`, of `$STARLING_RUNTIME`, or embedded in this componentizer,
/// the first of them that is given.
fn select_runtime(
    args: &RuntimeArgs,
    embedded_runtime: Option<&[u8]>,
) -> anyhow::Result<crate::Runtime> {
    let read = |path: &Path| {
        fs::read(path).with_context(|| format!("unable to read `{}`", path.display()))
    };
    if let Some(runtime_lib) = &args.runtime_lib {
        let read_lib = |name: &str| {
            let path = args.sysroot_libs.join(name);
            fs::read(&path).with_context(|| {
                if name.starts_with("noeh/") && !path.exists() {
                    format!(
                        "unable to read `{}`. Component-linking needs the shared `noeh/` \
                         libraries, first shipped in wasi-sdk 33. Check --sysroot-libs",
                        path.display()
                    )
                } else {
                    format!("unable to read `{}`", path.display())
                }
            })
        };
        return Ok(crate::Runtime::Dynamic(crate::Libraries {
            runtime: read(runtime_lib)?,
            libc: read_lib("libc.so")?,
            libcxx: read_lib("noeh/libc++.so")?,
            libcxxabi: read_lib("noeh/libc++abi.so")?,
            wasi_emulated_getpid: read_lib("libwasi-emulated-getpid.so")?,
        }));
    }
    args.static_runtime.load(embedded_runtime)?.ok_or_else(|| {
        anyhow::anyhow!(
            "this componentizer embeds no runtime. Pass --runtime with the path of a \
             starling.wasm, which `just build-runtime` builds, or --runtime-lib"
        )
    })
}

/// The directory of the file `input`, the default `--base-directory`.
fn default_base_directory(input: &Path) -> PathBuf {
    match input.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Parse a 128-bit seed, in decimal or with a `0x` prefix in hex.
fn parse_seed(seed: &str) -> Result<u128, std::num::ParseIntError> {
    match seed.strip_prefix("0x") {
        Some(hex) => u128::from_str_radix(hex, 16),
        None => seed.parse(),
    }
}

fn componentize(
    common: Common,
    componentize: Componentize,
    embedded_runtime: Option<&[u8]>,
) -> anyhow::Result<()> {
    let input = fs::read_to_string(&componentize.input)
        .with_context(|| format!("unable to read `{}`", componentize.input.display()))?;

    // Parsed first, so a bad `-d` or world is reported before the runtime build
    // is read. `crate::componentize` parses it again for itself, since it edits
    // the resolve it works from.
    load_world(&common)?;

    let runtime = select_runtime(&componentize.runtime, embedded_runtime)?;

    let (wit, mut world) = select_wit(&common)?;
    add_runtime_imports(&common, &mut world, Some(&runtime))?;
    let base_directory = componentize
        .base_directory
        .clone()
        .unwrap_or_else(|| default_base_directory(&componentize.input));

    let output = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(crate::componentize_with_output(
            wit,
            world,
            &common.unstable_features,
            common.all_unstable_features,
            crate::JsSource {
                name: &componentize.input.display().to_string(),
                text: &input,
            },
            Some(&base_directory),
            &runtime,
            &common.disable,
            componentize.deterministic_insecure_random_seed,
            None,
            componentize.init_location.as_ref().map(url::Url::as_str),
            !componentize.no_cache,
        ))?;

    if !common.quiet {
        print!("{}", output.init_output);
    }
    fs::write(&componentize.output, &output.component)
        .with_context(|| format!("unable to write `{}`", componentize.output.display()))?;

    if !common.quiet {
        println!("Component built successfully");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixed CLI world parses and exports `wasi:cli/run` at the runtime's
    /// version, which is the export `remove_world_export` drops in favour of the
    /// runtime's builtin.
    #[test]
    fn cli_world_exports_cli_run() {
        let mut resolve = wit_parser::Resolve::default();
        let pkg = resolve
            .push_str("cli-world", CLI_WORLD_WIT)
            .expect("the fixed CLI world WIT parses");
        let world = resolve
            .select_world(&[pkg], Some(CLI_WORLD_NAME))
            .expect("the `command` world resolves");
        let exports: Vec<_> = resolve.worlds[world]
            .exports
            .keys()
            .map(|k| resolve.name_world_key(k))
            .collect();
        assert_eq!(exports, vec!["wasi:cli/run@0.3.0".to_string()]);
    }

    /// The fixed serve world parses and exports `wasi:http/handler` at the
    /// runtime's version, which is the export `remove_world_export` drops in
    /// favour of the runtime's builtin, and imports nothing of its own.
    #[test]
    fn serve_world_exports_http_handler() {
        let mut resolve = wit_parser::Resolve::default();
        let pkg = resolve
            .push_str("serve-world", SERVE_WORLD_WIT)
            .expect("the fixed serve world WIT parses");
        let world = resolve
            .select_world(&[pkg], Some(SERVE_WORLD_NAME))
            .expect("the `serve` world resolves");
        let exports: Vec<String> = resolve.worlds[world]
            .exports
            .keys()
            .map(|k| resolve.name_world_key(k))
            .collect();
        assert_eq!(exports, vec!["wasi:http/handler@0.3.0".to_string()]);
        assert!(
            resolve.worlds[world].imports.is_empty(),
            "the serve world declares no imports of its own"
        );
    }

    #[test]
    fn cli_parses() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "-d",
            "wit",
            "-w",
            "hello",
            "componentize",
            "app.js",
            "-o",
            "out.wasm",
        ])
        .unwrap();
        assert_eq!(options.common.world, ["hello"]);
        assert!(!options.common.cli);
        let Some(Command::Componentize(c)) = &options.command else {
            panic!("expected the componentize subcommand");
        };
        assert_eq!(c.input, PathBuf::from("app.js"));
        assert_eq!(c.output, PathBuf::from("out.wasm"));
        // `STARLING_RUNTIME` is read when the runtime is loaded, not parsed into
        // the option.
        assert_eq!(c.runtime.static_runtime.runtime, None);
        assert_eq!(c.runtime.runtime_lib, None);
    }

    /// The seed is a 128-bit number in decimal or `0x`-prefixed hex.
    #[test]
    fn deterministic_insecure_random_seed_parses() {
        let seed = |value: &str| {
            let options = Options::try_parse_from([
                "starling-componentize",
                "--cli",
                "componentize",
                "app.js",
                "--deterministic-insecure-random-seed",
                value,
            ])?;
            let Some(Command::Componentize(c)) = options.command else {
                panic!("expected the componentize subcommand");
            };
            Ok::<_, clap::Error>(c.deterministic_insecure_random_seed)
        };
        assert_eq!(seed("42").unwrap(), Some(42));
        assert_eq!(seed("0x2a").unwrap(), Some(42));
        assert_eq!(seed(&u128::MAX.to_string()).unwrap(), Some(u128::MAX));
        for invalid in [
            "-1",
            "0x",
            "seed",
            "340282366920938463463374607431768211456",
        ] {
            assert!(seed(invalid).is_err(), "{invalid}");
        }
    }

    /// `--runtime` takes no value from `$STARLING_RUNTIME` in any subcommand,
    /// so setting it doesn't conflict with `--runtime-lib`.
    #[test]
    fn runtime_flag_reads_no_env() {
        let command = <Options as clap::CommandFactory>::command();
        for name in ["componentize", "types", "imports"] {
            let runtime = command
                .find_subcommand(name)
                .unwrap()
                .get_arguments()
                .find(|arg| arg.get_id() == "runtime")
                .unwrap_or_else(|| panic!("`{name}` takes `--runtime`"));
            assert_eq!(runtime.get_env(), None, "`{name} --runtime`");
        }
    }

    #[test]
    fn types_and_imports_take_runtime() {
        for name in ["types", "imports"] {
            let options = Options::try_parse_from([
                "starling-componentize",
                "--cli",
                name,
                "--runtime",
                "a.wasm",
            ])
            .unwrap();
            let runtime = match &options.command {
                Some(Command::Types(t)) => &t.runtime,
                Some(Command::Imports(i)) => &i.runtime,
                _ => panic!("expected the `{name}` subcommand"),
            };
            assert_eq!(runtime.runtime, Some(PathBuf::from("a.wasm")));
        }
    }

    /// `--runtime` and `--runtime-lib` select different link modes, so naming
    /// both is rejected.
    #[test]
    fn runtime_flags_are_exclusive() {
        let error = Options::try_parse_from([
            "starling-componentize",
            "--cli",
            "componentize",
            "app.js",
            "--runtime",
            "a.wasm",
            "--runtime-lib",
            "b.so",
        ])
        .expect_err("both runtime flags at once should be rejected");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn types_subcommand_parses() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "-d",
            "wit",
            "-w",
            "hello",
            "types",
            "-o",
            "world.d.ts",
        ])
        .unwrap();
        assert_eq!(options.common.world, ["hello"]);
        let Some(Command::Types(t)) = &options.command else {
            panic!("expected the types subcommand");
        };
        assert_eq!(t.output, Some(PathBuf::from("world.d.ts")));
    }

    #[test]
    fn types_works_with_cli_world() {
        let options = Options::try_parse_from(["starling-componentize", "--cli", "types"]).unwrap();
        assert!(options.common.cli);
        assert!(matches!(options.command, Some(Command::Types(_))));
    }

    #[test]
    fn cli_flag_parses_without_wit() {
        let options =
            Options::try_parse_from(["starling-componentize", "--cli", "componentize", "app.js"])
                .unwrap();
        assert!(options.common.cli);
        assert!(options.common.wit_path.is_empty());
        assert!(options.common.world.is_empty());
    }

    /// `--cli` with `-d` targets the selected world merged with the built-in
    /// CLI world.
    #[test]
    fn cli_flag_with_wit_path_adds_the_builtin_world() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--cli",
            "-d",
            "wit",
            "componentize",
            "app.js",
        ])
        .unwrap();
        let (wit, selection) = select_wit(&options.common).unwrap();
        assert!(matches!(wit, Wit::Paths([path]) if path == Path::new("wit")));
        assert!(selection.worlds.is_empty());
        assert_eq!(selection.builtins, [(CLI_WORLD_WIT, CLI_WORLD_NAME)]);
    }

    #[test]
    fn serve_flag_parses_without_wit() {
        let options =
            Options::try_parse_from(["starling-componentize", "--serve", "componentize", "app.js"])
                .unwrap();
        assert!(options.common.serve);
        assert!(!options.common.cli);
        assert!(options.common.wit_path.is_empty());
        assert!(options.common.world.is_empty());
    }

    /// `-w` may repeat, and `--serve` with `-d` adds the built-in serve world
    /// to the ones it names.
    #[test]
    fn several_worlds_and_the_serve_world() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--serve",
            "-d",
            "a",
            "-d",
            "b",
            "-w",
            "x:y/one",
            "-w",
            "two",
            "componentize",
            "app.js",
        ])
        .unwrap();
        let (wit, selection) = select_wit(&options.common).unwrap();
        assert!(matches!(wit, Wit::Paths(paths) if paths.len() == 2));
        assert_eq!(selection.worlds, ["x:y/one", "two"]);
        assert_eq!(selection.builtins, [(SERVE_WORLD_WIT, SERVE_WORLD_NAME)]);
    }

    /// `-w` names a world in the WIT `-d` loads, so it requires `-d`.
    #[test]
    fn world_requires_wit_path() {
        let err = Options::try_parse_from([
            "starling-componentize",
            "--serve",
            "-w",
            "app",
            "componentize",
            "app.js",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn imports_subcommand_parses() {
        let options =
            Options::try_parse_from(["starling-componentize", "-d", "wit", "imports"]).unwrap();
        assert!(matches!(options.command, Some(Command::Imports(_))));
    }

    /// `--version` prints without a runtime, too.
    #[test]
    fn version_prints_without_a_runtime() {
        print_version(None).unwrap();
    }

    /// `-V` and `--version` need no subcommand.
    #[test]
    fn version_flag_parses_without_a_subcommand() {
        for flag in ["-V", "--version"] {
            let options = Options::try_parse_from(["starling-componentize", flag]).unwrap();
            assert!(options.version && options.command.is_none(), "{flag}");
        }
    }

    /// `--cli` and `--serve` combine. Without `-d`, they target the two built-in
    /// worlds, merged into one exporting both interfaces.
    #[test]
    fn cli_and_serve_combine() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--serve",
            "--cli",
            "componentize",
            "app.js",
        ])
        .unwrap();
        let (wit, selection) = select_wit(&options.common).unwrap();
        assert!(matches!(wit, Wit::String(CLI_WORLD_WIT)));
        assert_eq!(selection.worlds, [CLI_WORLD_NAME]);
        assert_eq!(selection.builtins, [(SERVE_WORLD_WIT, SERVE_WORLD_NAME)]);

        let (resolve, world) = crate::load_world(wit, selection, &[], false).unwrap();
        let exports: Vec<String> = resolve.worlds[world]
            .exports
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect();
        assert_eq!(exports, ["wasi:cli/run@0.3.0", "wasi:http/handler@0.3.0"]);
    }

    /// With `-d`, `--cli` and `--serve` both add their built-in world.
    #[test]
    fn cli_and_serve_combine_with_wit_path() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--cli",
            "--serve",
            "-d",
            "wit",
            "componentize",
            "app.js",
        ])
        .unwrap();
        let (_, selection) = select_wit(&options.common).unwrap();
        assert!(selection.worlds.is_empty());
        assert_eq!(
            selection.builtins,
            [
                (CLI_WORLD_WIT, CLI_WORLD_NAME),
                (SERVE_WORLD_WIT, SERVE_WORLD_NAME)
            ]
        );
    }

    #[test]
    fn wit_path_required_unless_cli_or_serve() {
        let options =
            Options::try_parse_from(["starling-componentize", "componentize", "app.js"]).unwrap();
        let error = select_wit(&options.common)
            .err()
            .expect("no world is selected");
        assert!(
            error.to_string().starts_with("no world to target"),
            "{error}"
        );
    }

    /// The common flags may follow the subcommand.
    #[test]
    fn common_flags_follow_the_subcommand() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "componentize",
            "--cli",
            "app.js",
            "-q",
        ])
        .unwrap();
        assert!(options.common.cli && options.common.quiet);
    }

    /// The base directory defaults to the input's directory.
    #[test]
    fn base_directory_defaults_to_the_input_directory() {
        assert_eq!(
            default_base_directory(Path::new("src/app.js")),
            PathBuf::from("src")
        );
        assert_eq!(
            default_base_directory(Path::new("app.js")),
            PathBuf::from(".")
        );
    }
}
