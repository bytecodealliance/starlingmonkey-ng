// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! The `starling-componentize` command-line tool.

use {
    crate::Wit,
    anyhow::Context as _,
    clap::Parser as _,
    std::{
        ffi::OsString,
        fs,
        io::Write,
        path::{Path, PathBuf},
    },
    tokio::runtime::Builder as RuntimeBuilder,
};

/// A utility to convert JavaScript modules into Wasm components.
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

    /// Print the version of this componentizer and of the runtime `init`
    /// interface it calls. For the runtime in `$STARLING_RUNTIME`, or else the
    /// embedded runtime, also print the SHA-256 digest of its module and the
    /// version of the `init` interface it implements.
    #[arg(short = 'V', long)]
    pub version: bool,

    /// The subcommand to run, required unless `--version` is given.
    #[command(subcommand)]
    pub command: Option<Command>,
}

/// The name of the world selected by `--export-cli`, which exports
/// `wasi:cli/run`. The component exports the runtime's implementation of
/// `wasi:cli/run`, which calls the application's `run` function.
const CLI_WORLD_NAME: &str = "command";

/// The name of the world selected by `--export-http`, which exports
/// `wasi:http/handler`. The interface is implemented by the application, or by
/// the runtime, which dispatches each request to the `fetch` listeners that the
/// application added while it was initialized.
const SERVE_WORLD_NAME: &str = "serve";

/// The WIT sources of the worlds selected by `--export-cli` and
/// `--export-http`.
///
/// Each world exports an empty interface under the name of the runtime's export
/// of that interface, such as `wasi:cli/run@0.3.0`. Without a runtime, or if
/// the runtime doesn't export the interface, the name has no version.
struct BuiltinWorlds {
    /// The package `starling:cli` with the world [`CLI_WORLD_NAME`].
    cli: String,
    /// The package `starling:serve` with the world [`SERVE_WORLD_NAME`].
    serve: String,
}

impl BuiltinWorlds {
    fn new(runtime: Option<&crate::Runtime>) -> anyhow::Result<Self> {
        let interface = |prefix: &str| -> anyhow::Result<String> {
            let name = match runtime {
                Some(runtime) => crate::builtin_export_name(runtime, prefix)?,
                None => None,
            };
            Ok(name.unwrap_or_else(|| prefix.to_string()))
        };
        Ok(BuiltinWorlds {
            cli: builtin_world_wit(
                "starling:cli",
                CLI_WORLD_NAME,
                &interface(crate::CLI_RUN_INTERFACE_PREFIX)?,
            ),
            serve: builtin_world_wit(
                "starling:serve",
                SERVE_WORLD_NAME,
                &interface(crate::HTTP_HANDLER_INTERFACE_PREFIX)?,
            ),
        })
    }
}

/// The WIT source of the package `package` with the world `world`, which
/// exports the empty interface `interface`, a name such as
/// `wasi:cli/run@0.3.0` or `wasi:cli/run`.
fn builtin_world_wit(package: &str, world: &str, interface: &str) -> String {
    // `version` is empty or starts with the `@`.
    let (path, version) = interface
        .find('@')
        .map_or((interface, ""), |at| interface.split_at(at));
    let (interface_package, name) = path
        .rsplit_once('/')
        .expect("a builtin interface name has a package");
    format!(
        "package {package};\n\n\
         world {world} {{\n  export {interface};\n}}\n\n\
         package {interface_package}{version} {{\n  interface {name} {{}}\n}}\n"
    )
}

#[derive(clap::Args, Clone, Debug)]
pub struct Common {
    /// Files or directories containing WIT documents.
    ///
    /// The packages in a directory's `deps` directory are loaded as well. This
    /// can be repeated, for example: `-w ./wit/deps/<pkg> -w ./wit/app`. A
    /// package defined in several places is merged into one, with the
    /// interfaces and functions of each definition. The definitions must agree
    /// on the functions they have in common.
    ///
    /// The target world also imports every interface that the runtime imports,
    /// with the functions that the runtime imports, except those of the WASI
    /// APIs named by `--disable`.
    ///
    /// `-w`, `--export-cli` or `--export-http` is required.
    #[arg(short = 'w', long, global = true, value_name = "PATH")]
    pub wit: Vec<PathBuf>,

    /// Build a `wasi:cli/run` command component from a JavaScript module that
    /// exports a `run` function.
    ///
    /// The application needs no WIT of its own. The component exports
    /// `wasi:cli/run`, which calls the module's `run` export. With `-w`, the
    /// selected world is merged with a world that exports `wasi:cli/run`. With
    /// `--export-http`, the component also exports `wasi:http/handler`, and the
    /// module must also handle requests.
    #[arg(long, global = true)]
    pub export_cli: bool,

    /// Build a `wasi:http/handler` serve component from a JavaScript module that
    /// either registers a `fetch` listener with `addEventListener('fetch', …)`
    /// or exports its own implementation of the interface's `handle`.
    ///
    /// The application needs no WIT of its own. With a listener, the runtime's
    /// `wasi:http/handler` export dispatches each request to it as a `fetch`
    /// event. Componentization fails if the module does both or neither. With
    /// `-w`, the selected world is merged with a world that exports
    /// `wasi:http/handler`. With `--export-cli`, the component also exports
    /// `wasi:cli/run`, and the module must also export a `run` function.
    #[arg(long, global = true)]
    pub export_http: bool,

    /// The world to target: the name of a world in the last package loaded by
    /// `-w`, or a qualified name such as `namespace:package/world@version`.
    /// Defaults to that package's only world.
    ///
    /// Repeat it to target several worlds, which are merged into the world
    /// `starling:componentize/merged`. That world imports and exports
    /// everything that they do. Requires `-w`.
    #[arg(
        short = 'n',
        long,
        global = true,
        requires = "wit",
        value_name = "NAME"
    )]
    pub world_name: Vec<String>,

    /// Disable non-error output, including the output that the application
    /// prints while it is being initialized.
    #[arg(short = 'q', long, global = true)]
    pub quiet: bool,

    /// WIT features to enable. Items annotated with `@unstable(feature = …)`
    /// for an enabled feature are included in the WIT. Repeat, or separate
    /// features with commas or whitespace.
    #[arg(long, global = true)]
    pub unstable_features: Vec<String>,

    /// Enable every WIT feature, which includes every item annotated with
    /// `@unstable` in the WIT.
    #[arg(long, global = true)]
    pub all_unstable_features: bool,

    /// WASI APIs to leave out of the component. Repeat, or separate them with
    /// commas.
    ///
    /// The component imports none of the interfaces of a disabled API. Calling
    /// one of their functions traps, except that a disabled clock returns zero.
    /// An API whose interfaces are used by the interfaces of another API can't
    /// be disabled alone. The error names the API to disable with it.
    #[arg(long, global = true, value_delimiter = ',', value_enum)]
    pub disable: Vec<crate::finalize::WasiApiSet>,
}

#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Generate a component from the specified JavaScript module.
    Componentize(Box<Componentize>),
    /// Generate TypeScript declarations (`.d.ts`) for the JavaScript module that
    /// implements the selected world.
    ///
    /// The output declares the exports that the module must provide, and the
    /// modules that it can import.
    Types(Types),
    /// Print the specifiers of the modules that the JavaScript module can import
    /// for the selected world, one per line.
    ///
    /// A bundler must leave these imports unresolved. There is one module per
    /// imported interface, and `external` for the functions and types imported
    /// directly by the world. An interface is only listed if its module exports
    /// something: a function, a resource class, an error class, or an enum or
    /// flags object.
    Imports(Imports),
}

#[derive(clap::Args, Debug)]
pub struct Componentize {
    /// The filename of a JavaScript module from which to generate a component.
    pub input: PathBuf,

    /// The directory within which the relative imports of the input, and of the
    /// modules it imports, are resolved. Defaults to the input's directory.
    #[arg(short = 'p', long)]
    pub base_directory: Option<PathBuf>,

    /// Output file to which to write the resulting component.
    #[arg(short = 'o', long, default_value = "js.wasm")]
    pub output: PathBuf,

    #[command(flatten)]
    pub runtime: RuntimeArg,

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
    pub runtime: RuntimeArg,
}

#[derive(clap::Args, Debug)]
pub struct Imports {
    #[command(flatten)]
    pub runtime: RuntimeArg,
}

/// The runtime used by a subcommand.
#[derive(clap::Args, Debug)]
pub struct RuntimeArg {
    /// Path to the StarlingMonkey runtime to use. If not provided, the runtime embedded in this
    /// tool is used.
    ///
    /// [env: STARLING_RUNTIME=]
    // `$STARLING_RUNTIME` is read by [`RuntimeArg::load`], which `--version` uses without a
    // subcommand.
    #[arg(long)]
    pub runtime: Option<PathBuf>,
}

impl RuntimeArg {
    /// Load the runtime at `--runtime`, or else at `$STARLING_RUNTIME`, or else
    /// `embedded`. Returns `None` if none of them is given.
    fn load(&self, embedded: Option<&[u8]>) -> anyhow::Result<Option<crate::Runtime>> {
        let path = self
            .runtime
            .clone()
            .or_else(|| std::env::var_os("STARLING_RUNTIME").map(PathBuf::from));
        match (path, embedded) {
            (Some(path), _) => crate::Runtime::new(
                fs::read(&path).with_context(|| format!("unable to read `{}`", path.display()))?,
            )
            .map(Some),
            (None, Some(embedded)) => crate::Runtime::new(embedded.to_vec()).map(Some),
            (None, None) => Ok(None),
        }
    }
}

/// Run the command line `args`. `embedded_runtime` is the runtime embedded in the
/// binary, which is used if neither `--runtime` nor `$STARLING_RUNTIME` is given.
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

/// Select the WIT source and the target worlds from `common`. The built-in
/// worlds of `--export-cli` and `--export-http`, whose sources are in `worlds`,
/// are merged with the worlds selected with `-w`. Without `-w`, only the
/// built-in worlds are selected.
fn select_wit<'a>(
    common: &'a Common,
    worlds: &'a BuiltinWorlds,
) -> anyhow::Result<(Wit<'a>, crate::WorldSelection<'a>)> {
    let mut builtins = Vec::new();
    if common.export_cli {
        builtins.push((worlds.cli.as_str(), CLI_WORLD_NAME));
    }
    if common.export_http {
        builtins.push((worlds.serve.as_str(), SERVE_WORLD_NAME));
    }
    match common.wit.as_slice() {
        [] if builtins.is_empty() => anyhow::bail!(
            "no world to target: pass `-w|--wit` with a WIT file or directory, `--export-cli` \
             for a `wasi:cli/run` command, and/or `--export-http` for a `wasi:http/handler` \
             server"
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
                worlds: common.world_name.iter().map(String::as_str).collect(),
                builtins,
                resolved: Vec::new(),
            },
        )),
    }
}

/// Load the world described by the `types` and `imports` subcommands, from
/// `common` and the runtime selected by `runtime`, if any.
fn load_types_world(
    common: &Common,
    runtime: &RuntimeArg,
    embedded_runtime: Option<&[u8]>,
) -> anyhow::Result<(wit_parser::Resolve, wit_parser::WorldId)> {
    let runtime = runtime.load(embedded_runtime)?;
    let builtin_worlds = BuiltinWorlds::new(runtime.as_ref())?;
    let (wit, mut world) = select_wit(common, &builtin_worlds)?;
    if let Some(runtime) = &runtime {
        world
            .resolved
            .push(crate::runtime_imports_world(runtime, &common.disable)?);
    }
    crate::load_world_for_types(
        wit,
        world,
        &common.unstable_features,
        common.all_unstable_features,
        runtime.as_ref(),
    )
}

fn types(common: Common, opts: Types, embedded_runtime: Option<&[u8]>) -> anyhow::Result<()> {
    let (resolve, world) = load_types_world(&common, &opts.runtime, embedded_runtime)?;
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
    let (resolve, world) = load_types_world(&common, &opts.runtime, embedded_runtime)?;
    for module in crate::wit_to_ts::import_modules(&resolve, world) {
        println!("{module}");
    }
    Ok(())
}

/// Print output for `--version` (see [`Options::version`]).
fn print_version(embedded_runtime: Option<&[u8]>) -> anyhow::Result<()> {
    let runtime = RuntimeArg { runtime: None }.load(embedded_runtime)?;
    write_version(&mut std::io::stdout().lock(), runtime.as_ref())
}

/// Write the output of `--version` for `runtime` to `out`.
fn write_version(out: &mut impl Write, runtime: Option<&crate::Runtime>) -> anyhow::Result<()> {
    use sha2::Digest as _;
    writeln!(out, "starling-componentize {}", env!("CARGO_PKG_VERSION"))?;
    writeln!(out, "calls {}", crate::init_interface_name())?;
    let Some(runtime) = runtime else {
        writeln!(
            out,
            "runtime none: this componentizer embeds none, and STARLING_RUNTIME is unset"
        )?;
        return Ok(());
    };
    writeln!(
        out,
        "runtime sha256:{}",
        hex::encode(sha2::Sha256::digest(runtime.module()))
    )?;
    match crate::runtime_init_interface(runtime)? {
        Some(name) => writeln!(out, "runtime implements {name}")?,
        None => writeln!(
            out,
            "runtime implements no {}",
            crate::INIT_INTERFACE_PREFIX
        )?,
    }
    Ok(())
}

/// Return `runtime`, the runtime selected on the command line, or an error if
/// none was selected.
fn select_runtime(runtime: Option<crate::Runtime>) -> anyhow::Result<crate::Runtime> {
    runtime.ok_or_else(|| {
        anyhow::anyhow!(
            "this componentizer embeds no runtime. Pass --runtime with the path of a \
             starling.wasm, which `just build-runtime` builds"
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

    // The WIT is parsed first, so that an invalid `-w` or world is reported
    // before the runtime is read. `crate::componentize` parses it again, because
    // it modifies the `Resolve`. The built-in worlds name their interfaces
    // without a version here, because the runtime isn't read yet.
    let unversioned_worlds = BuiltinWorlds::new(None)?;
    let (wit, world) = select_wit(&common, &unversioned_worlds)?;
    crate::load_world(
        wit,
        world,
        &common.unstable_features,
        common.all_unstable_features,
    )?;

    let runtime = select_runtime(componentize.runtime.load(embedded_runtime)?)?;

    let builtin_worlds = BuiltinWorlds::new(Some(&runtime))?;
    let (wit, mut world) = select_wit(&common, &builtin_worlds)?;
    world
        .resolved
        .push(crate::runtime_imports_world(&runtime, &common.disable)?);
    let base_directory = componentize
        .base_directory
        .clone()
        .unwrap_or_else(|| default_base_directory(&componentize.input));

    let output = RuntimeBuilder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(crate::componentize(
            wit,
            world,
            crate::JsSource {
                name: &componentize.input.display().to_string(),
                text: &input,
            },
            &runtime,
            &crate::ComponentizeOptions {
                features: &common.unstable_features,
                all_features: common.all_unstable_features,
                base_directory: Some(&base_directory),
                disabled: &common.disable,
                insecure_seed: componentize.deterministic_insecure_random_seed,
                init_location: componentize.init_location.as_ref().map(url::Url::as_str),
                cache: !componentize.no_cache,
            },
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

    #[test]
    fn cli_parses() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--wit",
            "wit",
            "--world-name",
            "hello",
            "componentize",
            "app.js",
            "-o",
            "out.wasm",
        ])
        .unwrap();
        assert_eq!(options.common.wit, [PathBuf::from("wit")]);
        assert_eq!(options.common.world_name, ["hello"]);
        assert!(!options.common.export_cli);
        let Some(Command::Componentize(c)) = &options.command else {
            panic!("expected the componentize subcommand");
        };
        assert_eq!(c.input, PathBuf::from("app.js"));
        assert_eq!(c.output, PathBuf::from("out.wasm"));
        // `STARLING_RUNTIME` is read when the runtime is loaded, not parsed into
        // the option.
        assert_eq!(c.runtime.runtime, None);
    }

    /// The seed is a 128-bit number in decimal or `0x`-prefixed hex.
    #[test]
    fn deterministic_insecure_random_seed_parses() {
        let seed = |value: &str| {
            let options = Options::try_parse_from([
                "starling-componentize",
                "--export-cli",
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

    #[test]
    fn types_subcommand_parses() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "-w",
            "wit",
            "-n",
            "hello",
            "types",
            "-o",
            "world.d.ts",
        ])
        .unwrap();
        assert_eq!(options.common.world_name, ["hello"]);
        let Some(Command::Types(t)) = &options.command else {
            panic!("expected the types subcommand");
        };
        assert_eq!(t.output, Some(PathBuf::from("world.d.ts")));
    }

    /// `-n` may repeat, and `--export-http` with `-w` adds the built-in serve
    /// world to the worlds it names.
    #[test]
    fn several_worlds_and_the_serve_world() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--export-http",
            "-w",
            "a",
            "-w",
            "b",
            "-n",
            "x:y/one",
            "-n",
            "two",
            "componentize",
            "app.js",
        ])
        .unwrap();
        let worlds = BuiltinWorlds::new(None).unwrap();
        let (wit, selection) = select_wit(&options.common, &worlds).unwrap();
        assert!(matches!(wit, Wit::Paths(paths) if paths.len() == 2));
        assert_eq!(selection.worlds, ["x:y/one", "two"]);
        assert_eq!(
            selection.builtins,
            [(worlds.serve.as_str(), SERVE_WORLD_NAME)]
        );
    }

    /// `-n` names a world in the WIT `-w` loads, so it requires `-w`.
    #[test]
    fn world_name_requires_wit() {
        let err = Options::try_parse_from([
            "starling-componentize",
            "--export-http",
            "-n",
            "app",
            "componentize",
            "app.js",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    /// `--version` without a runtime prints the componentizer's version, the
    /// interface it calls, and a `runtime none` line.
    #[test]
    fn version_without_a_runtime_says_so() {
        let mut out = Vec::new();
        write_version(&mut out, None).unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!(
                "starling-componentize {}\ncalls {}\nruntime none: this componentizer embeds \
                 none, and STARLING_RUNTIME is unset\n",
                env!("CARGO_PKG_VERSION"),
                crate::init_interface_name()
            )
        );
    }

    /// Componentizing without a runtime fails, naming the flag to pass.
    #[test]
    fn componentizing_without_a_runtime_names_the_flag() {
        let error = select_runtime(None).err().unwrap().to_string();
        assert!(
            error.starts_with("this componentizer embeds no runtime. Pass --runtime"),
            "{error}"
        );
    }

    /// `-V` and `--version` need no subcommand.
    #[test]
    fn version_flag_parses_without_a_subcommand() {
        for flag in ["-V", "--version"] {
            let options = Options::try_parse_from(["starling-componentize", flag]).unwrap();
            assert!(options.version && options.command.is_none(), "{flag}");
        }
    }

    /// `--export-cli` and `--export-http` combine. Without `-w`, they target the
    /// two built-in worlds, merged into one exporting both interfaces. Without a
    /// runtime, the interfaces have no version.
    #[test]
    fn export_cli_and_export_http_combine() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--export-http",
            "--export-cli",
            "componentize",
            "app.js",
        ])
        .unwrap();
        let worlds = BuiltinWorlds::new(None).unwrap();
        let (wit, selection) = select_wit(&options.common, &worlds).unwrap();
        assert!(matches!(wit, Wit::String(wit) if wit == worlds.cli));
        assert_eq!(selection.worlds, [CLI_WORLD_NAME]);
        assert_eq!(
            selection.builtins,
            [(worlds.serve.as_str(), SERVE_WORLD_NAME)]
        );

        let (resolve, world) = crate::load_world(wit, selection, &[], false).unwrap();
        let exports: Vec<String> = resolve.worlds[world]
            .exports
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect();
        assert_eq!(exports, ["wasi:cli/run", "wasi:http/handler"]);
    }

    /// A built-in world exports its interface under the name it is given,
    /// version included.
    #[test]
    fn builtin_world_exports_the_named_version() {
        let wit = builtin_world_wit("starling:cli", CLI_WORLD_NAME, "wasi:cli/run@0.3.0-rc.1");
        let (resolve, world) = crate::load_world(
            Wit::<PathBuf>::String(&wit),
            Some(CLI_WORLD_NAME),
            &[],
            false,
        )
        .unwrap();
        let exports: Vec<String> = resolve.worlds[world]
            .exports
            .keys()
            .map(|key| resolve.name_world_key(key))
            .collect();
        assert_eq!(exports, ["wasi:cli/run@0.3.0-rc.1"]);
    }

    /// With `-w`, `--export-cli` and `--export-http` both add their built-in world.
    #[test]
    fn export_cli_and_export_http_combine_with_wit() {
        let options = Options::try_parse_from([
            "starling-componentize",
            "--export-cli",
            "--export-http",
            "-w",
            "wit",
            "componentize",
            "app.js",
        ])
        .unwrap();
        let worlds = BuiltinWorlds::new(None).unwrap();
        let (_, selection) = select_wit(&options.common, &worlds).unwrap();
        assert!(selection.worlds.is_empty());
        assert_eq!(
            selection.builtins,
            [
                (worlds.cli.as_str(), CLI_WORLD_NAME),
                (worlds.serve.as_str(), SERVE_WORLD_NAME)
            ]
        );
    }

    #[test]
    fn wit_required_unless_export_cli_or_export_http() {
        let options =
            Options::try_parse_from(["starling-componentize", "componentize", "app.js"]).unwrap();
        let worlds = BuiltinWorlds::new(None).unwrap();
        let error = select_wit(&options.common, &worlds)
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
            "--export-cli",
            "app.js",
            "-q",
        ])
        .unwrap();
        assert!(options.common.export_cli && options.common.quiet);
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
