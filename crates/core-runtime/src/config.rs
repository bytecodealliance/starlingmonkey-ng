// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! CLI argument parsing for the StarlingMonkey runtime, and the runtime
//! settings derived from it.

use std::{
    cell::Cell,
    env,
    path::{Path, PathBuf},
    time::Duration,
};

use clap::Parser;

/// StarlingMonkey runtime configuration.
#[derive(Parser, Debug, Clone)]
#[command(name = "starling", about = "StarlingMonkey JS runtime")]
pub struct RuntimeConfig {
    /// Path to the content script to execute.
    #[arg(default_value = "./index.js")]
    pub script_path: String,

    /// Evaluate inline script instead of a file.
    #[arg(short = 'e', long = "eval")]
    pub eval_script: Option<String>,

    /// Path to an initialization script (runs in the default global, as a
    /// classic script, before the content script; must complete synchronously).
    #[arg(short = 'i', long = "initializer-script")]
    pub initializer_script_path: Option<String>,

    /// Enable verbose logging.
    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Debug the content script over the Debug Adapter Protocol. The forms are `stdio`,
    /// `listen:[<addr>:]<port>[,wait]` and `connect:<host>:<port>[,timeout=<ms>]`. `stdio` makes
    /// the process a DAP adapter on stdin and stdout. `listen` binds `127.0.0.1` unless an address
    /// is given, and `,wait` holds the content script until a client has attached and sent
    /// `configurationDone`. `connect` dials the client (1000 ms timeout by default). `stdio` and
    /// `connect` hold the content script until the client sends `configurationDone`.
    #[arg(short = 'd', long = "debug", value_name = "TRANSPORT")]
    pub debug: Option<DebugConfig>,

    /// Record the JavaScript stack each promise and async call was created on, and show it as
    /// async parent frames in `Error.stack`.
    ///
    /// Off by default, since it captures a stack for every promise. Always on for globals that have a debugger attached.
    #[arg(long = "async-stacks")]
    pub async_stacks: bool,

    /// Use classic (non-module) script mode.
    #[arg(long = "legacy-script")]
    pub legacy_script: bool,

    /// Enable Node.js compatibility: the `node:` builtin modules, the Node globals (`process`,
    /// `Buffer`, `global`, `setImmediate`, `clearImmediate`), CommonJS modules, and the `node`
    /// condition in package `exports` resolution.
    #[arg(long = "node-compat")]
    pub node_compat: bool,

    /// Accept Node.js runtime flags, such as `--expose-gc` or `--no-warnings`, before the script
    /// path. Options of this runtime keep their meaning, and every other option before the script
    /// path is a Node.js flag, written as `--name` or `--name=value`. The flags appear in
    /// `process.execArgv`. See `docs/node-compat.md` for the flags that take effect, and for those
    /// that are rejected.
    #[arg(long = "allow-node-runtime-flags", requires = "node_compat")]
    pub allow_node_runtime_flags: bool,

    /// The Node.js runtime flags `--allow-node-runtime-flags` accepted, as given.
    #[arg(skip)]
    pub node_runtime_flags: Vec<String>,

    /// Let synchronous APIs block on host I/O, such as reading a file, while serving requests.
    /// Blocking stalls every request the instance serves concurrently, so this is off by default,
    /// and such calls throw instead.
    #[arg(long = "allow-blocking-host-io")]
    pub allow_blocking_host_io: bool,

    /// With `--node-compat`, let `http.Server.listen()` in a script run as a command accept HTTP
    /// connections on a TCP port, through `net.Server`. Without it, `listen()` emits
    /// `ERR_SERVE_MODE_REQUIRED` there. Has no effect with `--serve`.
    #[arg(long = "node-http-listen")]
    pub node_http_listen: bool,

    /// Enable WPT (Web Platform Tests) mode.
    #[arg(long = "wpt-mode")]
    pub wpt_mode: bool,

    /// Enforce the browser-security Fetch constraints (forbidden
    /// request/response headers and methods, no-CORS safelisting, origin/mode
    /// enforcement). Defaults to off except in `--wpt-mode`, which needs the
    /// browser-observable behavior.
    /// Pass `=true`/`=false` to override either default.
    #[arg(
        long = "enforce-fetch-restrictions",
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    pub enforce_fetch_restrictions: Option<bool>,

    /// Override the location URL for initialization.
    #[arg(long = "init-location")]
    pub init_location: Option<String>,

    /// Strip this prefix from script paths.
    #[arg(long = "strip-path-prefix")]
    pub path_prefix: Option<String>,

    /// Serve incoming HTTP requests on this TCP port (native), dispatching each as a `fetch` event
    /// to the handler the script registered with `addEventListener("fetch", …)`. The script runs
    /// once to register handlers; the runtime then stays alive to handle requests.
    #[arg(long = "serve")]
    pub serve: Option<u16>,

    /// Handle each served request in its own global, re-evaluating the content script for it, so
    /// nothing a request leaves behind is visible to the next one.
    ///
    /// Off by default, since the increased isolation comes at a steep cost: besides requiring
    /// reinitialization of the entire global, including evaluating its top-level script again,
    /// it also doesn't support parallelization of requests at all. (Though that part might
    /// change eventually.) Mainly useful to run test suites.
    ///
    /// Native only: on wasm, the host runtime determines concurrency.
    #[cfg(not(target_arch = "wasm32"))]
    #[arg(long = "serve-isolated")]
    pub serve_isolated: bool,

    /// Give up on a served request whose `respondWith` has not settled after this many seconds and
    /// answer it with a 500.
    ///
    /// No limit by default, except in `--wpt-mode`, where it's 120 seconds. `0` means no limit.
    #[arg(long = "dispatch-timeout", value_name = "SECONDS")]
    pub dispatch_timeout: Option<u64>,

    /// Give up on a served request's response body after this many seconds, counted from its
    /// headers going out, and truncate it visibly (no terminating chunk natively, error trailers
    /// on wasm) so the client sees an incomplete body rather than a complete-looking one.
    ///
    /// Natively this covers the write to the client, bounding a handler that never finishes its
    /// body and a peer too slow to accept one alike; on wasm it covers handing the body to the
    /// host, which buffers it, so a slow peer is the host's to bound.
    ///
    /// No limit by default, except in `--wpt-mode`, where it's 120 seconds. `0` means no limit.
    #[arg(long = "response-body-timeout", value_name = "SECONDS")]
    pub response_body_timeout: Option<u64>,

    /// Stop waiting for the promises a served request passed to `FetchEvent#waitUntil` after this
    /// many seconds, counted from the response being fully sent.
    ///
    /// No limit by default, except in `--wpt-mode`, where it's 120 seconds. `0` means no limit.
    #[arg(long = "waituntil-timeout", value_name = "SECONDS")]
    pub waituntil_timeout: Option<u64>,

    /// Bound everything a served request runs, dispatch through `waitUntil` work, by this many
    /// seconds of wall clock, however the per-phase timeouts divide it. Each phase runs under the
    /// smaller of its own timeout and what is left of this one, so a phase timeout longer than it
    /// is rejected.
    ///
    /// No limit by default (also in `--wpt-mode`, whose per-phase defaults already bound a run).
    /// `0` means no limit.
    #[arg(long = "end-to-end-timeout", value_name = "SECONDS")]
    pub end_to_end_timeout: Option<u64>,

    /// Limits applied for native builds. On Wasm, `wasi:http` enforces equivalents.
    #[cfg(not(target_arch = "wasm32"))]
    #[command(flatten)]
    pub serve_limits: ServeLimits,

    /// Pre-initialize the runtime for a Wizer snapshot. The runtime then records where each piece
    /// of asynchronous work is created, for reporting work still pending when the snapshot is
    /// taken (see [`EventLoop::ensure_idle_for_snapshot`](crate::event_loop::EventLoop::ensure_idle_for_snapshot)).
    #[arg(skip)]
    pub pre_initialize: bool,
}

impl RuntimeConfig {
    /// Whether to use ES module mode (the default, unless --legacy-script).
    pub fn module_mode(&self) -> bool {
        !self.legacy_script
    }

    /// The effective request-restrictions setting: an explicit
    /// `--enforce-fetch-restrictions[=bool]` wins, otherwise restrictions are
    /// on in WPT mode and off everywhere else.
    pub fn enforce_fetch_restrictions(&self) -> bool {
        self.enforce_fetch_restrictions.unwrap_or(self.wpt_mode)
    }

    /// The effective `--dispatch-timeout`, or `None` for no limit.
    pub fn dispatch_timeout(&self) -> Option<Duration> {
        serve_timeout(self.dispatch_timeout, self.wpt_mode)
    }

    /// The effective `--response-body-timeout`, or `None` for no limit.
    pub fn response_body_timeout(&self) -> Option<Duration> {
        serve_timeout(self.response_body_timeout, self.wpt_mode)
    }

    /// The effective `--waituntil-timeout`, or `None` for no limit.
    pub fn waituntil_timeout(&self) -> Option<Duration> {
        serve_timeout(self.waituntil_timeout, self.wpt_mode)
    }

    /// The effective `--end-to-end-timeout`, or `None` for no limit. Never defaulted by
    /// `--wpt-mode`: the per-phase defaults already bound a WPT run.
    pub fn end_to_end_timeout(&self) -> Option<Duration> {
        serve_timeout(self.end_to_end_timeout, false)
    }

    /// Reject a phase timeout longer than `--end-to-end-timeout`, which could never run its full
    /// window. Only explicit values are compared: the WPT-mode phase defaults are not a user
    /// mistake under a shorter explicit deadline, and `0` on either side means "no bound of its
    /// own", which the deadline caps as a matter of course.
    pub fn validate_serve_timeouts(&self) -> Result<(), String> {
        let Some(end_to_end) = self.end_to_end_timeout.filter(|&s| s != 0) else {
            return Ok(());
        };
        for (flag, seconds) in [
            ("--dispatch-timeout", self.dispatch_timeout),
            ("--response-body-timeout", self.response_body_timeout),
            ("--waituntil-timeout", self.waituntil_timeout),
        ] {
            if seconds.is_some_and(|s| s != 0 && s > end_to_end) {
                return Err(format!(
                    "{flag} {} exceeds --end-to-end-timeout {end_to_end}: a phase cannot run \
                     longer than the whole request",
                    seconds.unwrap(),
                ));
            }
        }
        Ok(())
    }

    /// The base directory for resolving module imports.
    ///
    /// For `--eval` scripts this is the current working directory. Otherwise,
    /// it is the directory containing the content script.
    pub fn base_path(&self) -> PathBuf {
        if self.eval_script.is_some() {
            return env::current_dir().unwrap_or_default();
        }
        let path = Path::new(&self.script_path);
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            env::current_dir().unwrap_or_default().join(path)
        };
        match abs.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
            // A filesystem root has no parent; resolve relative to it directly.
            _ => abs,
        }
    }

    /// Parse from an argument string (e.g., from STARLINGMONKEY_CONFIG env var).
    ///
    /// Splits the string on whitespace, respecting single/double quotes.
    pub fn from_arg_string(args: &str) -> Result<Self, clap::Error> {
        Self::from_args(split_args(args)?)
    }

    /// Parse from WASI CLI arguments (as provided by the host).
    ///
    /// With `--allow-node-runtime-flags`, the options before the script path that aren't options
    /// of this runtime become [`node_runtime_flags`](Self::node_runtime_flags) instead of usage
    /// errors.
    pub fn from_args(args: impl IntoIterator<Item = String>) -> Result<Self, clap::Error> {
        let (args, node_flags) = split_node_runtime_flags(args.into_iter().collect());
        let mut config = Self::try_parse_from(args)?;
        config.node_runtime_flags = node_flags;
        Ok(config)
    }

    /// Parse from the STARLINGMONKEY_CONFIG environment variable.
    pub fn from_env() -> Result<Self, clap::Error> {
        match std::env::var("STARLINGMONKEY_CONFIG") {
            Ok(config) => Self::from_arg_string(&config),
            Err(_) => Self::try_parse_from(["starling"]),
        }
    }

    /// Parse from stdin (for wizer pre-initialization).
    /// Reads a single line of arguments from stdin.
    pub fn from_stdin() -> Result<Self, clap::Error> {
        let mut input = String::new();
        if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
            return Self::try_parse_from(["starling"]);
        }
        let mut config = Self::from_arg_string(input.trim())?;
        config.pre_initialize = true;
        Ok(config)
    }
}

/// How a debug session reaches its DAP client, from `--debug`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DebugConfig {
    /// The byte stream the session speaks DAP over.
    pub transport: DebugTransport,
}

/// A byte stream a debug session speaks DAP over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugTransport {
    /// The process's stdin and stdout.
    Stdio,
    /// A TCP listener. With `wait`, the content script is held until a client has attached and
    /// sent `configurationDone`.
    Listen {
        addr: std::net::IpAddr,
        port: u16,
        wait: bool,
    },
    /// A TCP connection the process opens to a waiting client. The script is always held until
    /// the client sends `configurationDone`. `timeout` bounds the connection attempt.
    Connect {
        host: String,
        port: u16,
        timeout: std::time::Duration,
    },
}

/// The `connect` timeout when `--debug` gives none.
const DEFAULT_DEBUG_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

impl std::str::FromStr for DebugConfig {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (transport, options) = match s.split_once(',') {
            Some((transport, options)) => (transport, Some(options)),
            None => (s, None),
        };
        let mut transport = if transport == "stdio" {
            DebugTransport::Stdio
        } else if let Some(rest) = transport.strip_prefix("listen:") {
            let (addr, port) = match rest.rsplit_once(':') {
                Some((addr, port)) => {
                    // An IPv6 address can be written in brackets, as in `[::1]:9229`.
                    let bare = addr
                        .strip_prefix('[')
                        .and_then(|addr| addr.strip_suffix(']'))
                        .unwrap_or(addr);
                    let addr = bare
                        .parse()
                        .map_err(|_| format!("invalid debug listen address `{addr}`"))?;
                    (addr, port)
                }
                None => (std::net::Ipv4Addr::LOCALHOST.into(), rest),
            };
            let port = port
                .parse()
                .map_err(|_| format!("invalid debug listen port `{port}`"))?;
            DebugTransport::Listen {
                addr,
                port,
                wait: false,
            }
        } else if let Some(rest) = transport.strip_prefix("connect:") {
            let (host, port) = rest
                .rsplit_once(':')
                .filter(|(host, _)| !host.is_empty())
                .ok_or_else(|| {
                    format!("expected `connect:<host>:<port>`, found `connect:{rest}`")
                })?;
            let port = port
                .parse()
                .map_err(|_| format!("invalid debug connect port `{port}`"))?;
            DebugTransport::Connect {
                host: host.to_owned(),
                port,
                timeout: DEFAULT_DEBUG_CONNECT_TIMEOUT,
            }
        } else {
            return Err(format!(
                "unknown debug transport `{transport}`, expected `stdio`, \
                 `listen:[<addr>:]<port>` or `connect:<host>:<port>`"
            ));
        };
        for option in options.into_iter().flat_map(|o| o.split(',')) {
            match (&mut transport, option) {
                (DebugTransport::Listen { wait, .. }, "wait") => *wait = true,
                (DebugTransport::Connect { timeout, .. }, _) if option.starts_with("timeout=") => {
                    let ms = &option["timeout=".len()..];
                    *timeout = std::time::Duration::from_millis(
                        ms.parse()
                            .map_err(|_| format!("invalid debug connect timeout `{ms}`"))?,
                    );
                }
                _ => {
                    let name = match transport {
                        DebugTransport::Stdio => "stdio",
                        DebugTransport::Listen { .. } => "listen",
                        DebugTransport::Connect { .. } => "connect",
                    };
                    return Err(format!("unknown debug option `{option}` for `{name}`"));
                }
            }
        }
        Ok(Self { transport })
    }
}

/// A byte count on the command line, written with a unit (`512MiB`, `64KiB`, `1MB`) or as a plain
/// number of bytes.
///
/// Exists so the limits below read as sizes rather than as seven-digit numbers, in `--help` as
/// well as on the command line: clap renders a default with [`Display`](std::fmt::Display) and
/// reads a value with [`FromStr`](std::str::FromStr), and requires the two to round-trip.
/// [`Display`](std::fmt::Display) therefore only uses a unit that divides the count exactly, and
/// [`FromStr`](std::str::FromStr) takes no fractions: every size prints as a value that parses
/// back to the same number of bytes.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteSize(u64);

#[cfg(not(target_arch = "wasm32"))]
impl ByteSize {
    pub const fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn kib(kib: u64) -> Self {
        Self(kib * 1024)
    }

    pub const fn mib(mib: u64) -> Self {
        Self(mib * 1024 * 1024)
    }

    pub const fn gib(gib: u64) -> Self {
        Self(gib * 1024 * 1024 * 1024)
    }

    /// The count itself, for the comparisons the server makes against it.
    pub const fn bytes(self) -> u64 {
        self.0
    }
}

/// The units [`Display`](std::fmt::Display) may print, largest first, and the spellings
/// [`FromStr`](std::str::FromStr) accepts for each. A bare `K`/`M`/`G` is binary, as it is in
/// `ls -h` and friends; the decimal spellings mean their decimal multiples.
#[cfg(not(target_arch = "wasm32"))]
const BINARY_UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Display for ByteSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (unit, multiple) in BINARY_UNITS {
            if self.0 >= multiple && self.0.is_multiple_of(multiple) {
                return write!(f, "{}{unit}", self.0 / multiple);
            }
        }
        write!(f, "{}B", self.0)
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::str::FromStr for ByteSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let digits = value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len());
        let (count, unit) = value.split_at(digits);
        // Both failures name the whole value rather than the fragment they tripped on: split
        // apart, `1.5MiB` reports a unit of `.5MiB`, which describes the parse rather than the
        // mistake.
        let malformed = || {
            format!("{value:?} is not a byte size (try 512MiB, 64KiB, 1MB, or a plain byte count)")
        };
        let count: u64 = count.parse().map_err(|_| malformed())?;
        let multiple = match unit.trim().to_ascii_lowercase().as_str() {
            "" | "b" => 1,
            "k" | "kib" => 1 << 10,
            "m" | "mib" => 1 << 20,
            "g" | "gib" => 1 << 30,
            "kb" => 1_000,
            "mb" => 1_000_000,
            "gb" => 1_000_000_000,
            _ => return Err(malformed()),
        };
        count
            .checked_mul(multiple)
            .map(Self)
            .ok_or_else(|| format!("{value:?} is more than {} bytes", u64::MAX))
    }
}

/// The limits the native server enforces when processing incoming requests.
/// Flattened into [`RuntimeConfig`], so each is its own top-level CLI option.
///
/// Native only, since on Wasm the host parses the request and enforces similar bounds.
#[cfg(not(target_arch = "wasm32"))]
#[derive(clap::Args, Debug, Clone, Copy)]
pub struct ServeLimits {
    /// The maximum number of bytes the server will read from the client's connection at once.
    /// Note that this also limits the size of the request head, which needs to be read all at once.
    /// Values below 8KiB are rejected, since that is the smallest read buffer the HTTP/1.1
    /// implementation accepts.
    // Note: the default is the same Hyper uses.
    #[arg(long = "max-connection-buffer-size", value_name = "BYTES", default_value_t = ByteSize::new(8192 + 4096 * 100)
    )]
    pub max_connection_buffer_size: ByteSize,

    /// Cap a served request at this many header fields. More is answered with a `431`.
    #[arg(
        long = "max-request-headers",
        value_name = "COUNT",
        default_value_t = 128
    )]
    pub max_request_headers: usize,

    /// Cap a served request's body at this many bytes. A larger `Content-Length` is refused with a
    /// `413` before the handler runs. A chunked body that grows past this limit fails mid-stream,
    /// since its length is not known ahead of time.
    #[arg(long = "max-request-body-bytes", value_name = "BYTES", default_value_t = ByteSize::mib(512)
    )]
    pub max_request_body_bytes: ByteSize,

    /// If, once a request is fully processed, the request's body hasn't been fully read, it needs
    /// to be drained before the connection can accept the next request. This option sets how many
    /// bytes are read at most before giving up and closing the connection instead.
    #[arg(long = "max-body-drain-bytes", value_name = "BYTES", default_value_t = ByteSize::kib(256)
    )]
    pub max_body_drain_bytes: ByteSize,

    /// Serve at most this many connections at once, keeping additional ones in the accept backlog
    /// until a slot frees up. Ignored when `--serve-isolated` is passed.
    #[arg(long = "max-connections", value_name = "COUNT", default_value_t = 1024)]
    pub max_connections: usize,

    /// Give up after this many seconds on a client that has stopped sending: on the request head,
    /// which closes the connection, and on each subsequent read of the body off the connection.
    /// Waiting for the handler to consume what has already been read does not count against it,
    /// but waiting for a handler that never reads its body does — past it the rest of the body is
    /// drained rather than delivered.
    ///
    /// `0` means no limit.
    #[arg(
        long = "request-read-timeout",
        value_name = "SECONDS",
        default_value_t = 30
    )]
    pub request_read_timeout: u64,

    /// Close a kept-alive connection that has sat idle for this many seconds without the next
    /// request's first byte. Expiring closes it quietly rather than answering a 408: between
    /// requests, silence is a client that is done with the connection.
    ///
    /// `0` means no limit.
    #[arg(
        long = "keepalive-timeout",
        value_name = "SECONDS",
        default_value_t = 30
    )]
    pub keepalive_timeout: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl ServeLimits {
    /// The effective `--request-read-timeout`, or `None` for no limit.
    pub fn request_read_timeout(&self) -> Option<Duration> {
        let seconds = self.request_read_timeout;
        (seconds != 0).then(|| Duration::from_secs(seconds))
    }

    /// The effective `--keepalive-timeout`, or `None` for no limit.
    pub fn keepalive_timeout(&self) -> Option<Duration> {
        let seconds = self.keepalive_timeout;
        (seconds != 0).then(|| Duration::from_secs(seconds))
    }

    /// Reject a size or count limit of zero for all limits except `--max-body-drain-bytes`, and a
    /// request headers cap below the smallest read buffer the HTTP/1.1 implementation accepts.
    pub fn validate(&self) -> Result<(), String> {
        for (flag, value) in [
            (
                "--max-connection-buffer-size",
                self.max_connection_buffer_size.bytes(),
            ),
            ("--max-request-headers", self.max_request_headers as u64),
            (
                "--max-request-body-bytes",
                self.max_request_body_bytes.bytes(),
            ),
            ("--max-connections", self.max_connections as u64),
        ] {
            if value == 0 {
                return Err(format!("Invalid value `0` for {flag}"));
            }
        }
        if self.max_connection_buffer_size.bytes() < MIN_REQUEST_HEAD_BYTES {
            return Err(format!(
                "Invalid value `{}` for --max-connection-buffer-size: must be at least \
                 {MIN_REQUEST_HEAD_BYTES} bytes",
                self.max_connection_buffer_size,
            ));
        }
        Ok(())
    }
}

/// The smallest `--max-connection-buffer-size` the server accepts, matching the minimum read buffer
/// its HTTP/1.1 implementation allows.
#[cfg(not(target_arch = "wasm32"))]
pub const MIN_REQUEST_HEAD_BYTES: u64 = 8 * 1024;

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self::try_parse_from(["starling"]).unwrap()
    }
}

// ---------------------------------------------------------------------------
// Request restrictions — runtime state
// ---------------------------------------------------------------------------
//
// Several Fetch constraints are browser-security policies (forbidden request
// and response headers, forbidden methods, no-CORS header/method safelisting,
// origin/mode enforcement) rather than HTTP correctness rules. In non-browser
// contexts these policies are usually unwanted, so they're disabled by default.

js::instance_local! {
    static ENFORCE_REQUEST_RESTRICTIONS: Cell<bool> = const { Cell::new(false) };
}

/// Enable or disable enforcement of the browser-security Fetch constraints.
/// `Runtime::init` sets this from the [`RuntimeConfig`], embedders can override
/// it at any point after.
pub fn set_enforce_fetch_restrictions(enabled: bool) {
    ENFORCE_REQUEST_RESTRICTIONS.with(|cell| cell.set(enabled));
}

/// Whether the browser-security Fetch constraints are currently enforced.
#[inline]
pub fn enforce_fetch_restrictions() -> bool {
    ENFORCE_REQUEST_RESTRICTIONS.with(|cell| cell.get())
}

/// How long a serve-mode timeout given as `--…-timeout SECONDS` runs for: the value if it was
/// given, otherwise [`WPT_SERVE_TIMEOUT`] in WPT mode and no limit everywhere else. An explicit
/// `0` is how a WPT run asks for no limit.
fn serve_timeout(seconds: Option<u64>, wpt_mode: bool) -> Option<Duration> {
    match seconds {
        Some(0) => None,
        Some(seconds) => Some(Duration::from_secs(seconds)),
        None if wpt_mode => Some(WPT_SERVE_TIMEOUT),
        None => None,
    }
}

/// The serve-mode timeout WPT runs use. Well above the harness's own per-test limit, so a test that
/// is merely slow still reports a result and only a genuinely stuck one is cut off.
const WPT_SERVE_TIMEOUT: Duration = Duration::from_secs(120);

/// Split the Node.js runtime flags out of `args`, the program name followed by the command line,
/// if `--allow-node-runtime-flags` comes before the script path. Returns the remaining arguments
/// and the Node.js flags in their order. Without `--allow-node-runtime-flags`, returns `args`
/// unchanged and no flags.
///
/// Only the arguments before the first positional argument or `--` are considered. Of those, an
/// option of [`RuntimeConfig`] stays, together with the next argument if the option requires a
/// value and wasn't given one with `=`, and every other argument that starts with `-` is a Node.js
/// flag. A Node.js flag's value therefore has to be given with `=`.
fn split_node_runtime_flags(args: Vec<String>) -> (Vec<String>, Vec<String>) {
    let mut command = <RuntimeConfig as clap::CommandFactory>::command();
    command.build();
    let lookup = |arg: &str| -> Option<bool> {
        let (long, short) = if let Some(long) = arg.strip_prefix("--") {
            (
                Some(long.split_once('=').map_or(long, |(name, _)| name)),
                None,
            )
        } else {
            let mut chars = arg.chars().skip(1);
            let short = chars.next()?;
            // A short option written together with its value, such as `-e1`, has it already.
            (None, Some((short, chars.next().is_some())))
        };
        let option = command.get_arguments().find(|option| match (long, short) {
            (Some(long), _) => option.get_long() == Some(long),
            (_, Some((short, _))) => option.get_short() == Some(short),
            _ => false,
        })?;
        let requires_value = option
            .get_num_args()
            .is_some_and(|range| range.min_values() > 0);
        let has_value = long.is_some_and(|_| arg.contains('=')) || short.is_some_and(|(_, v)| v);
        Some(requires_value && !has_value)
    };

    let mut remaining = Vec::with_capacity(args.len());
    let mut node_flags = Vec::new();
    let mut allowed = false;
    let mut iter = args.iter().cloned();
    remaining.extend(iter.next());
    while let Some(arg) = iter.next() {
        if arg == "--" || arg == "-" || !arg.starts_with('-') {
            remaining.push(arg);
            break;
        }
        match lookup(&arg) {
            Some(takes_next) => {
                allowed |= arg == "--allow-node-runtime-flags";
                remaining.push(arg);
                if takes_next {
                    remaining.extend(iter.next());
                }
            }
            None => node_flags.push(arg),
        }
    }
    if !allowed {
        return (args, Vec::new());
    }
    remaining.extend(iter);
    (remaining, node_flags)
}

/// Split an argument string into individual arguments, respecting quotes.
fn split_args(s: &str) -> Result<Vec<String>, clap::Error> {
    let mut args = vec!["starling".into()];
    args.extend(shlex::split(s).ok_or(clap::Error::raw(
        clap::error::ErrorKind::InvalidValue,
        "Failed to parse arguments",
    ))?);
    Ok(args)
}

// Nothing platform-specific in these tests, so skip them on wasm32.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = RuntimeConfig::default();
        assert_eq!(config.script_path, "./index.js");
        assert!(config.module_mode());
        assert!(!config.verbose);
        assert!(config.debug.is_none());
        assert!(!config.async_stacks);
        assert!(!config.wpt_mode);
    }

    #[test]
    fn debug_flag_selects_the_stdio_transport() {
        let config = RuntimeConfig::from_arg_string("--debug=stdio main.js").unwrap();
        assert_eq!(
            config.debug,
            Some(DebugConfig {
                transport: DebugTransport::Stdio
            })
        );
        let short = RuntimeConfig::from_arg_string("-d stdio main.js").unwrap();
        assert_eq!(short.debug, config.debug);
        assert_eq!(short.script_path, "main.js");
    }

    fn debug(s: &str) -> Result<DebugConfig, String> {
        s.parse::<DebugConfig>()
    }

    #[test]
    fn debug_listen_forms() {
        use std::net::{IpAddr, Ipv4Addr};
        let local = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert_eq!(
            debug("listen:9229").unwrap().transport,
            DebugTransport::Listen {
                addr: local,
                port: 9229,
                wait: false
            }
        );
        assert_eq!(
            debug("listen:0.0.0.0:9229,wait").unwrap().transport,
            DebugTransport::Listen {
                addr: "0.0.0.0".parse().unwrap(),
                port: 9229,
                wait: true
            }
        );
        for form in ["listen:[::1]:9229", "listen:::1:9229"] {
            assert_eq!(
                debug(form).unwrap().transport,
                DebugTransport::Listen {
                    addr: "::1".parse().unwrap(),
                    port: 9229,
                    wait: false
                },
                "{form}"
            );
        }
        assert!(debug("listen:9229,timeout=5")
            .unwrap_err()
            .contains("timeout"));
        assert!(debug("listen:notaport").unwrap_err().contains("notaport"));
    }

    #[test]
    fn debug_listen_accepts_port_zero() {
        assert!(matches!(
            debug("listen:0").unwrap().transport,
            DebugTransport::Listen { port: 0, .. }
        ));
    }

    #[test]
    fn debug_connect_forms() {
        assert_eq!(
            debug("connect:localhost:9230").unwrap().transport,
            DebugTransport::Connect {
                host: "localhost".into(),
                port: 9230,
                timeout: std::time::Duration::from_millis(1000),
            }
        );
        assert_eq!(
            debug("connect:10.0.0.2:9230,timeout=250")
                .unwrap()
                .transport,
            DebugTransport::Connect {
                host: "10.0.0.2".into(),
                port: 9230,
                timeout: std::time::Duration::from_millis(250),
            }
        );
        assert!(debug("connect:9230").unwrap_err().contains("host"));
        let err = debug("connect:host").unwrap_err();
        assert!(
            err.contains("<port>") && err.contains("connect:host"),
            "{err}"
        );
        assert!(debug("connect:h:1,wait").unwrap_err().contains("wait"));
    }

    #[test]
    fn debug_stdio_takes_no_options() {
        assert!(debug("stdio,wait").unwrap_err().contains("wait"));
    }

    #[test]
    fn debug_flag_rejects_unknown_transports() {
        let err = RuntimeConfig::from_arg_string("--debug=tcp main.js").unwrap_err();
        assert!(
            err.to_string().contains("unknown debug transport `tcp`"),
            "{err}"
        );
    }

    #[test]
    fn serve_timeouts_default_to_no_limit_outside_wpt_mode() {
        let config = RuntimeConfig::default();
        assert_eq!(config.dispatch_timeout(), None);
        assert_eq!(config.waituntil_timeout(), None);

        // WPT runs want a stuck test cut off rather than stalling the run.
        let wpt = RuntimeConfig::from_arg_string("--wpt-mode").unwrap();
        assert_eq!(wpt.dispatch_timeout(), Some(WPT_SERVE_TIMEOUT));
        assert_eq!(wpt.waituntil_timeout(), Some(WPT_SERVE_TIMEOUT));

        // An explicit value wins over either default, and `0` is how to ask for no limit at all.
        let explicit =
            RuntimeConfig::from_arg_string("--wpt-mode --dispatch-timeout 5 --waituntil-timeout 0")
                .unwrap();
        assert_eq!(explicit.dispatch_timeout(), Some(Duration::from_secs(5)));
        assert_eq!(explicit.waituntil_timeout(), None);
    }

    #[test]
    fn response_body_timeout_defaults_like_the_other_phase_timeouts() {
        // No limit by default; WPT mode wants a stuck body cut off like a stuck dispatch.
        assert_eq!(RuntimeConfig::default().response_body_timeout(), None);
        let wpt = RuntimeConfig::from_arg_string("--wpt-mode").unwrap();
        assert_eq!(wpt.response_body_timeout(), Some(WPT_SERVE_TIMEOUT));
        let explicit =
            RuntimeConfig::from_arg_string("--wpt-mode --response-body-timeout 0").unwrap();
        assert_eq!(explicit.response_body_timeout(), None);
    }

    #[test]
    fn end_to_end_timeout_has_no_wpt_default() {
        // The per-phase timeouts already bound a WPT run; a wall clock over their sum is
        // opt-in everywhere.
        assert_eq!(RuntimeConfig::default().end_to_end_timeout(), None);
        let wpt = RuntimeConfig::from_arg_string("--wpt-mode").unwrap();
        assert_eq!(wpt.end_to_end_timeout(), None);
        let explicit = RuntimeConfig::from_arg_string("--end-to-end-timeout 30").unwrap();
        assert_eq!(explicit.end_to_end_timeout(), Some(Duration::from_secs(30)));
    }

    #[test]
    fn a_phase_timeout_longer_than_end_to_end_is_a_config_error() {
        // A phase that could never use its full window is a contradiction worth catching.
        for phase in ["dispatch", "response-body", "waituntil"] {
            let config = RuntimeConfig::from_arg_string(&format!(
                "--end-to-end-timeout 10 --{phase}-timeout 11"
            ))
            .unwrap();
            let error = config.validate_serve_timeouts().unwrap_err();
            assert!(
                error.contains(&format!("--{phase}-timeout")) && error.contains("10"),
                "error should name the offending flag and the limit, got: {error}"
            );
        }
    }

    #[test]
    fn timeouts_within_end_to_end_validate() {
        // Equal is fine, shorter is fine, and unlimited phases under a finite end_to_end are
        // fine — that is "no own bound, capped end-to-end", not a contradiction.
        for args in [
            "--end-to-end-timeout 10 --dispatch-timeout 10",
            "--end-to-end-timeout 10 --dispatch-timeout 2 --response-body-timeout 5",
            "--end-to-end-timeout 10",
            "--end-to-end-timeout 10 --waituntil-timeout 0",
            "--dispatch-timeout 500",
        ] {
            let config = RuntimeConfig::from_arg_string(args).unwrap();
            assert_eq!(config.validate_serve_timeouts(), Ok(()), "for: {args}");
        }
    }

    #[test]
    fn wpt_defaults_do_not_trip_end_to_end_validation() {
        // WPT mode defaults the phase timeouts to 120s; an explicit shorter end_to_end must not
        // be rejected on account of defaults the user never set — the deadline simply cuts
        // earlier.
        let config = RuntimeConfig::from_arg_string("--wpt-mode --end-to-end-timeout 30").unwrap();
        assert_eq!(config.validate_serve_timeouts(), Ok(()));
    }

    #[test]
    fn a_byte_size_renders_the_largest_unit_that_divides_it_exactly() {
        assert_eq!(ByteSize::gib(2).to_string(), "2GiB");
        assert_eq!(ByteSize::mib(512).to_string(), "512MiB");
        assert_eq!(ByteSize::kib(64).to_string(), "64KiB");
        assert_eq!(ByteSize::new(0).to_string(), "0B");
        // A size no unit divides stays in bytes rather than rounding: what `--help` prints has to
        // parse back to the number it printed.
        assert_eq!(ByteSize::new(1000).to_string(), "1000B");
        assert_eq!(ByteSize::new(1025).to_string(), "1025B");
    }

    #[test]
    fn a_byte_size_round_trips_through_display_and_parse() {
        // clap renders a `default_value_t` with `Display` and reads user input with `FromStr`, so
        // a default that did not survive the round trip would advertise a value it then rejects.
        for bytes in [
            0,
            1,
            1000,
            8 * 1024,
            64 * 1024,
            256 * 1024,
            512 * 1024 * 1024,
            u64::MAX,
        ] {
            let size = ByteSize::new(bytes);
            assert_eq!(size.to_string().parse(), Ok(size), "{size}");
        }
    }

    #[test]
    fn a_byte_size_reads_binary_and_decimal_units() {
        // The prefixes mean what they say, so `MB` is not a synonym for `MiB`.
        assert_eq!("512MiB".parse(), Ok(ByteSize::mib(512)));
        assert_eq!("512 MiB".parse(), Ok(ByteSize::mib(512)));
        assert_eq!("512mib".parse(), Ok(ByteSize::mib(512)));
        assert_eq!("512MB".parse(), Ok(ByteSize::new(512_000_000)));
        assert_eq!("64KiB".parse(), Ok(ByteSize::kib(64)));
        assert_eq!("64KB".parse(), Ok(ByteSize::new(64_000)));
        // A bare `K`/`M`/`G` is binary, as it is in `ls -h` and friends.
        assert_eq!("8K".parse(), Ok(ByteSize::kib(8)));
        // A bare number is bytes, which is all these flags took before they took units.
        assert_eq!("65536".parse(), Ok(ByteSize::kib(64)));
        assert_eq!("0".parse(), Ok(ByteSize::new(0)));
    }

    #[test]
    fn a_byte_size_refuses_what_it_cannot_represent_exactly() {
        for bad in [
            "",
            "MiB",
            "12MiBs",
            "-1",
            // Fractions would not round-trip through a byte count.
            "1.5MiB",
            "abc",
            // Past `u64`, in the number itself and in the multiplication.
            "99999999999999999999",
            "17179869184GiB",
        ] {
            assert!(bad.parse::<ByteSize>().is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn serve_limit_timeouts_take_zero_as_no_limit() {
        // The convention the phase timeouts already set: `0` asks for no bound at all.
        let unlimited =
            RuntimeConfig::from_arg_string("--request-read-timeout 0 --keepalive-timeout 0")
                .unwrap();
        assert_eq!(unlimited.serve_limits.request_read_timeout(), None);
        assert_eq!(unlimited.serve_limits.keepalive_timeout(), None);

        let explicit =
            RuntimeConfig::from_arg_string("--request-read-timeout 5 --keepalive-timeout 90")
                .unwrap();
        assert_eq!(
            explicit.serve_limits.request_read_timeout(),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            explicit.serve_limits.keepalive_timeout(),
            Some(Duration::from_secs(90))
        );
    }

    #[test]
    fn a_zero_size_or_count_limit_is_a_config_error() {
        // Unlike a timeout, a cap of zero is not "no limit" but a server that can accept
        // nothing at all, which is never what was meant.
        for flag in [
            "--max-connection-buffer-size",
            "--max-request-headers",
            "--max-request-body-bytes",
            "--max-connections",
        ] {
            let config = RuntimeConfig::from_arg_string(&format!("{flag} 0")).unwrap();
            let error = config.serve_limits.validate().unwrap_err();
            assert!(
                error.contains(flag),
                "the error should name the offending flag, got: {error}"
            );
        }
    }

    #[test]
    fn a_zero_drain_budget_is_a_setting_rather_than_a_mistake() {
        // Spending nothing on reaching the next request's framing boundary is a coherent
        // policy: a connection whose request body went unread closes instead of being reused.
        let config = RuntimeConfig::from_arg_string("--max-body-drain-bytes 0").unwrap();
        assert_eq!(config.serve_limits.validate(), Ok(()));
        assert_eq!(config.serve_limits.max_body_drain_bytes, ByteSize::new(0));
    }

    #[test]
    fn a_request_head_cap_below_the_read_buffer_minimum_is_a_config_error() {
        let config = RuntimeConfig::from_arg_string("--max-connection-buffer-size 512").unwrap();
        let error = config.serve_limits.validate().unwrap_err();
        assert!(
            error.contains("--max-connection-buffer-size"),
            "the error should name the offending flag, got: {error}"
        );

        let at_minimum =
            RuntimeConfig::from_arg_string("--max-connection-buffer-size 8KiB").unwrap();
        assert_eq!(at_minimum.serve_limits.validate(), Ok(()));
    }

    #[test]
    fn serve_limits_parse_from_the_command_line() {
        let config = RuntimeConfig::from_arg_string(
            "--max-connection-buffer-size 16KiB --max-request-headers 16 \
             --max-request-body-bytes 1MiB --max-body-drain-bytes 2048 --max-connections 8",
        )
        .unwrap();
        assert_eq!(
            config.serve_limits.max_connection_buffer_size,
            ByteSize::kib(16)
        );
        assert_eq!(config.serve_limits.max_request_headers, 16);
        assert_eq!(config.serve_limits.max_request_body_bytes, ByteSize::mib(1));
        assert_eq!(
            config.serve_limits.max_body_drain_bytes,
            ByteSize::new(2048)
        );
        assert_eq!(config.serve_limits.max_connections, 8);
        assert_eq!(config.serve_limits.validate(), Ok(()));
    }

    #[test]
    fn test_parse_args() {
        let config = RuntimeConfig::from_arg_string("-v --legacy-script app.js").unwrap();
        assert_eq!(config.script_path, "app.js");
        assert!(config.verbose);
        assert!(!config.module_mode());
    }

    #[test]
    fn node_runtime_flags_are_split_out_before_the_script_path() {
        let config = RuntimeConfig::from_arg_string(
            "--node-compat --expose-gc --allow-node-runtime-flags -v --title=foo \
             --init-location http://example.com/ --expose_internals app.js",
        )
        .unwrap();
        assert_eq!(
            config.node_runtime_flags,
            ["--expose-gc", "--title=foo", "--expose_internals"]
        );
        assert_eq!(config.script_path, "app.js");
        assert!(config.verbose);
        assert_eq!(config.init_location.as_deref(), Some("http://example.com/"));
    }

    #[test]
    fn node_runtime_flags_are_split_out_around_an_eval_script() {
        let config = RuntimeConfig::from_arg_string(
            "--node-compat --allow-node-runtime-flags -e 1+1 --pending-deprecation",
        )
        .unwrap();
        assert_eq!(config.eval_script.as_deref(), Some("1+1"));
        assert_eq!(config.node_runtime_flags, ["--pending-deprecation"]);
    }

    #[test]
    fn node_runtime_flags_end_at_the_script_path_or_a_double_dash() {
        let config =
            RuntimeConfig::from_arg_string("--node-compat --allow-node-runtime-flags -- --x")
                .unwrap();
        assert_eq!(config.script_path, "--x");
        assert!(config.node_runtime_flags.is_empty());
        let error = RuntimeConfig::from_arg_string(
            "--node-compat --allow-node-runtime-flags app.js --no-warnings",
        )
        .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn node_runtime_flags_are_unknown_options_without_the_option() {
        let error = RuntimeConfig::from_arg_string("--node-compat --expose-gc app.js").unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
        let config = RuntimeConfig::from_arg_string("--node-compat app.js").unwrap();
        assert!(config.node_runtime_flags.is_empty());
    }

    #[test]
    fn node_runtime_flags_require_node_compat() {
        let error = RuntimeConfig::from_arg_string("--allow-node-runtime-flags --expose-gc app.js")
            .unwrap_err();
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn help_is_a_runtime_option_with_node_runtime_flags() {
        let error =
            RuntimeConfig::from_arg_string("--node-compat --allow-node-runtime-flags --help")
                .unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
    }

    #[test]
    fn enforce_fetch_restrictions_defaults_off_wpt_on_flag_wins() {
        assert!(!RuntimeConfig::default().enforce_fetch_restrictions());
        assert!(RuntimeConfig::from_arg_string("--wpt-mode")
            .unwrap()
            .enforce_fetch_restrictions());
        assert!(
            !RuntimeConfig::from_arg_string("--wpt-mode --enforce-fetch-restrictions=false")
                .unwrap()
                .enforce_fetch_restrictions()
        );
        assert!(
            RuntimeConfig::from_arg_string("--enforce-fetch-restrictions")
                .unwrap()
                .enforce_fetch_restrictions()
        );
    }

    #[test]
    fn base_path_for_eval_is_cwd() {
        let config = RuntimeConfig::from_arg_string("-e 42").unwrap();
        assert_eq!(config.base_path(), env::current_dir().unwrap());
    }

    #[test]
    fn base_path_for_bare_filename_is_cwd() {
        let config = RuntimeConfig::from_arg_string("app.js").unwrap();
        assert_eq!(config.base_path(), env::current_dir().unwrap());
    }

    #[test]
    fn base_path_for_absolute_script_is_its_directory() {
        let config = RuntimeConfig::from_arg_string("/srv/js/app.js").unwrap();
        assert_eq!(config.base_path(), Path::new("/srv/js"));
    }

    #[test]
    fn base_path_for_relative_script_is_absolute() {
        let config = RuntimeConfig::from_arg_string("sub/dir/app.js").unwrap();
        assert_eq!(
            config.base_path(),
            env::current_dir().unwrap().join("sub/dir")
        );
    }
}
