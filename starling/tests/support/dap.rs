// SPDX-License-Identifier: Apache-2.0-WITH-LLVM-exception

//! A DAP client that drives `starlingmonkey --debug`, over stdio or TCP.

use std::collections::VecDeque;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// How long one wait for a message or for the process may take. `debugmozjs` builds and GC zeal
/// runs are slow.
const TIMEOUT: Duration = Duration::from_secs(60);

pub struct Dap {
    /// `None` once [`Dap::into_child`] took it, and for a client of a process the test does not
    /// own.
    child: Option<Child>,
    /// The stream requests are written to. `None` once closed.
    writer: Option<Box<dyn Write + Send>>,
    messages: Receiver<Value>,
    backlog: VecDeque<Value>,
    next_seq: i64,
}

/// What a session left when it ended.
pub struct Finished {
    /// The concatenated `output` of every `output` event.
    pub output: String,
    /// The `exitCode` of the `exited` event.
    pub exit_code: Option<i64>,
    pub status: ExitStatus,
}

impl Dap {
    /// Start `starlingmonkey --debug=stdio <script>`.
    pub fn spawn(script: &Path) -> Self {
        Self::spawn_with_args(&[script.to_str().expect("UTF-8 script path")])
    }

    /// Start `starlingmonkey --debug=stdio <args>`.
    pub fn spawn_with_args(args: &[&str]) -> Self {
        Self::spawn_command(Command::new(env!("CARGO_BIN_EXE_starlingmonkey")), args)
    }

    /// Start `starlingmonkey --debug=stdio <args>` in the working directory `dir`.
    pub fn spawn_in(dir: &Path, args: &[&str]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_starlingmonkey"));
        command.current_dir(dir);
        Self::spawn_command(command, args)
    }

    /// Start `component` under `wasmtime run` with `dir` preopened as `/`, and pass it
    /// `--debug=stdio <args>`.
    pub fn spawn_wasm(component: &Path, dir: &Path, args: &[&str]) -> Self {
        Self::spawn_command(super::wasmtime_run(component, dir), args)
    }

    fn spawn_command(command: Command, args: &[&str]) -> Self {
        let mut child = start_debuggee(command, args, Stdio::inherit());
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Self::from_streams(child, stdout, stdin)
    }

    /// Connect to the listener of `child` at `addr`. `child` is a debuggee started with
    /// `--debug=listen`, or the relay started with `--client`.
    pub fn attach_tcp(child: Child, addr: SocketAddr) -> Self {
        let stream = TcpStream::connect(addr).expect("connecting to the debug listener failed");
        Self::from_tcp(child, stream)
    }

    /// Accept the connection of `child`, started with `--debug=connect`, on `listener`.
    pub fn accept_tcp(child: Child, listener: &TcpListener) -> Self {
        Self::with_tcp(Some(child), Self::accept_stream(listener))
    }

    /// Accept a connection on `listener` from a process the test does not own.
    pub fn accept(listener: &TcpListener) -> Self {
        Self::with_tcp(None, Self::accept_stream(listener))
    }

    fn accept_stream(listener: &TcpListener) -> TcpStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + TIMEOUT;
        let stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "the debuggee did not connect");
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("accepting the debug connection failed: {error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream
    }

    /// Connect to a debug listener at `addr` that a process the test does not own opens, retrying
    /// until it accepts the connection. Panics if it does not within 60 s.
    pub fn connect_tcp(addr: SocketAddr) -> Self {
        let deadline = Instant::now() + TIMEOUT;
        let stream = loop {
            match TcpStream::connect(addr) {
                Ok(stream) => break stream,
                Err(error) => {
                    assert!(
                        Instant::now() < deadline,
                        "nothing accepted a connection on {addr}: {error}"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        Self::with_tcp(None, stream)
    }

    fn from_tcp(child: Child, stream: TcpStream) -> Self {
        Self::with_tcp(Some(child), stream)
    }

    fn with_tcp(child: Option<Child>, stream: TcpStream) -> Self {
        stream.set_nodelay(true).unwrap();
        let reader = stream.try_clone().unwrap();
        Self::with_streams(child, reader, TcpWriter(stream))
    }

    /// A client for `child` that reads messages from `reader` on a thread and writes requests to
    /// `writer`.
    pub fn from_streams(
        child: Child,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> Self {
        Self::with_streams(Some(child), reader, writer)
    }

    fn with_streams(
        child: Option<Child>,
        reader: impl Read + Send + 'static,
        writer: impl Write + Send + 'static,
    ) -> Self {
        let mut reader = BufReader::new(reader);
        let (tx, messages) = mpsc::channel();
        std::thread::spawn(move || {
            while let Ok(Some(body)) = debugger::transport::read_message(&mut reader) {
                let message: Value =
                    serde_json::from_str(&body).expect("the adapter sent invalid JSON");
                if tx.send(message).is_err() {
                    return;
                }
            }
        });
        Self {
            child,
            writer: Some(Box::new(writer)),
            messages,
            backlog: VecDeque::new(),
            next_seq: 1,
        }
    }

    /// Send a request and return its `seq`.
    pub fn send_request(&mut self, command: &str, arguments: Value) -> i64 {
        self.send_requests(&[(command, arguments)])[0]
    }

    /// Send several requests in one write, and return their `seq`s in order.
    pub fn send_requests(&mut self, requests: &[(&str, Value)]) -> Vec<i64> {
        let mut bytes = Vec::new();
        let mut seqs = Vec::with_capacity(requests.len());
        for (command, arguments) in requests {
            let seq = self.next_seq;
            self.next_seq += 1;
            let body = json!({
                "seq": seq,
                "type": "request",
                "command": command,
                "arguments": arguments,
            })
            .to_string();
            debugger::transport::write_message(&mut bytes, &body).expect("writing to a `Vec`");
            seqs.push(seq);
        }
        let writer = self
            .writer
            .as_mut()
            .expect("the connection is already closed");
        writer.write_all(&bytes).expect("writing requests failed");
        writer.flush().expect("writing requests failed");
        seqs
    }

    /// The response to request `seq`. Messages that arrive before it stay queued.
    pub fn response(&mut self, seq: i64) -> Value {
        self.take(|message| message["type"] == "response" && message["request_seq"] == seq)
    }

    /// Send a request, assert that it succeeded, and return the response body.
    pub fn request(&mut self, command: &str, arguments: Value) -> Value {
        let seq = self.send_request(command, arguments);
        let response = self.response(seq);
        assert_eq!(response["success"], true, "`{command}` failed: {response}");
        response["body"].clone()
    }

    /// The body of the oldest `name` event, waiting for one if none is queued.
    pub fn event(&mut self, name: &str) -> Value {
        self.take(|message| message["type"] == "event" && message["event"] == name)["body"].clone()
    }

    /// Every message in arrival order, up to and including the first `name` event.
    pub fn events_until(&mut self, name: &str) -> Vec<Value> {
        let mut seen = Vec::new();
        loop {
            let message = self.next();
            let done = message["type"] == "event" && message["event"] == name;
            seen.push(message);
            if done {
                return seen;
            }
        }
    }

    /// `initialize`, then `attach` with `arguments`, then the `initialized` event.
    pub fn start(&mut self, arguments: Value) {
        let capabilities = self.request(
            "initialize",
            json!({
                "adapterID": "starling",
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path",
            }),
        );
        assert_eq!(capabilities["supportsConfigurationDoneRequest"], true);
        self.request("attach", arguments);
        self.event("initialized");
    }

    /// Set line breakpoints in `path`, and return the response's `breakpoints`.
    pub fn set_breakpoints(&mut self, path: &Path, lines: &[u32]) -> Vec<Value> {
        let breakpoints: Vec<Value> = lines.iter().map(|line| json!({ "line": line })).collect();
        let body = self.request(
            "setBreakpoints",
            json!({ "source": { "path": path }, "breakpoints": breakpoints }),
        );
        body["breakpoints"]
            .as_array()
            .expect("a breakpoints array")
            .clone()
    }

    /// Evaluate `expression` in the REPL context, and return the response body.
    pub fn evaluate(&mut self, expression: &str) -> Value {
        self.request(
            "evaluate",
            json!({ "expression": expression, "context": "repl" }),
        )
    }

    /// Wait for an `output` event with `output` as its text. Earlier `output` events are dropped.
    pub fn wait_for_output(&mut self, output: &str) {
        while self.event("output")["output"] != output {}
    }

    /// The line of the innermost frame the debuggee is paused in.
    pub fn top_frame_line(&mut self) -> Value {
        let trace = self.request("stackTrace", json!({ "threadId": 1 }));
        trace["stackFrames"][0]["line"].clone()
    }

    /// The name and line of the innermost frame the debuggee is paused in.
    pub fn top_frame(&mut self) -> (String, u64) {
        let trace = self.request("stackTrace", json!({ "threadId": 1, "levels": 1 }));
        let frame = &trace["stackFrames"][0];
        (
            frame["name"].as_str().unwrap().to_string(),
            frame["line"].as_u64().unwrap(),
        )
    }

    /// The messages that arrived and were not taken yet, oldest first. Messages still in transit
    /// from the reader thread are not included.
    pub fn queued(&self) -> impl Iterator<Item = &Value> {
        self.backlog.iter()
    }

    /// Close stdin, or the sending side of a TCP connection, as a client that exits without
    /// sending `disconnect` does.
    pub fn close_stdin(&mut self) {
        drop(self.writer.take());
    }

    /// Close the sending side of the connection, and return the process, which keeps running.
    pub fn into_child(mut self) -> Child {
        self.close_stdin();
        self.take_child()
    }

    /// Return the process and leave the connection open. The process is no longer killed when
    /// this client is dropped.
    pub fn take_child(&mut self) -> Child {
        self.child
            .take()
            .expect("the process was already handed off")
    }

    /// Wait until the process closed the connection. Messages that arrive before stay queued.
    pub fn wait_for_close(&mut self) {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.messages.recv_timeout(remaining) {
                Ok(message) => self.backlog.push_back(message),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("the connection stayed open for {TIMEOUT:?}")
                }
            }
        }
    }

    /// Stop reading the process's stdout. The pipe closes once the next message arrives.
    pub fn close_stdout(&mut self) {
        self.messages = mpsc::channel().1;
    }

    /// The process's exit status, or `None` while it runs.
    pub fn try_exit_status(&mut self) -> Option<ExitStatus> {
        let child = self.child.as_mut().expect("the process was handed off");
        child.try_wait().expect("polling the process failed")
    }

    /// Wait for the process to exit.
    pub fn wait_for_exit(&mut self) -> ExitStatus {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(status) = self.try_exit_status() {
                return status;
            }
            assert!(Instant::now() < deadline, "starlingmonkey did not exit");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Collect `output` events until `terminated`, then wait for the process.
    pub fn run_to_end(mut self) -> Finished {
        let mut output = String::new();
        let mut exit_code = None;
        loop {
            let message = self.next();
            if message["type"] != "event" {
                continue;
            }
            match message["event"].as_str() {
                Some("output") => {
                    output.push_str(message["body"]["output"].as_str().unwrap_or_default())
                }
                Some("exited") => exit_code = message["body"]["exitCode"].as_i64(),
                Some("terminated") => break,
                _ => {}
            }
        }
        self.close_stdin();
        let status = self.wait_for_exit();
        Finished {
            output,
            exit_code,
            status,
        }
    }

    fn take(&mut self, matches: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.backlog.iter().position(&matches) {
            return self
                .backlog
                .remove(index)
                .expect("an index position returned");
        }
        loop {
            let message = self.receive();
            if matches(&message) {
                return message;
            }
            self.backlog.push_back(message);
        }
    }

    fn next(&mut self) -> Value {
        self.backlog.pop_front().unwrap_or_else(|| self.receive())
    }

    fn receive(&mut self) -> Value {
        self.messages.recv_timeout(TIMEOUT).unwrap_or_else(|error| {
            panic!(
                "no DAP message within {TIMEOUT:?} ({error}); queued: {:?}",
                self.backlog
            )
        })
    }
}

impl Drop for Dap {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Start `command --debug=stdio <args>` with stdin and stdout piped and stderr set to `stderr`.
pub fn start_debuggee(mut command: Command, args: &[&str], stderr: Stdio) -> Child {
    command
        .arg("--debug=stdio")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .expect("failed to start the debuggee")
}

/// The `name` → `value` pairs of a `variables` response body.
pub fn values(variables: &Value) -> Vec<(String, String)> {
    variables["variables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["name"].as_str().unwrap().to_string(),
                v["value"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// The sending side of a client's TCP connection. Dropping it shuts down the stream's write
/// direction, so the process reads the end of the stream.
struct TcpWriter(TcpStream);

impl Write for TcpWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl Drop for TcpWriter {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Write);
    }
}

/// Read `stderr` of a process started with `--debug=listen` until the line
/// `debugger listening on 127.0.0.1:<port>`, and return the port.
pub fn listening_port(stderr: impl Read + Send + 'static) -> u16 {
    line_after(stderr, "debugger listening on 127.0.0.1:")
        .parse()
        .expect("a port number")
}

/// Read lines of `stream` on a thread until one starts with `prefix`, and return the rest of that
/// line. Every line is also written to this process's stderr. Panics if no such line arrives
/// within [`TIMEOUT`].
pub fn line_after(stream: impl Read + Send + 'static, prefix: &str) -> String {
    Lines::read(stream).after(prefix)
}

/// The lines of a stream, read on a thread. Every line is also written to this process's stderr,
/// including the lines read after this is dropped.
pub struct Lines {
    lines: Receiver<String>,
    /// Every line taken from `lines` so far.
    read: Vec<String>,
}

impl Lines {
    pub fn read(stream: impl Read + Send + 'static) -> Self {
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stream).lines() {
                let Ok(line) = line else { return };
                eprintln!("{line}");
                let _ = tx.send(line);
            }
        });
        Self {
            lines,
            read: Vec::new(),
        }
    }

    /// Wait for the next line that starts with `prefix`, and return the rest of that line. Panics
    /// if no such line arrives within [`TIMEOUT`].
    pub fn after(&mut self, prefix: &str) -> String {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    let rest = line.strip_prefix(prefix).map(str::to_string);
                    self.read.push(line);
                    if let Some(rest) = rest {
                        return rest;
                    }
                }
                Err(error) => panic!(
                    "no line starting with `{prefix}` ({error}): {:?}",
                    self.read
                ),
            }
        }
    }

    /// Wait until the stream ends, and return every line read from it. Panics if it does not end
    /// within [`TIMEOUT`].
    pub fn until_end(mut self) -> Vec<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(remaining) {
                Ok(line) => self.read.push(line),
                Err(RecvTimeoutError::Disconnected) => return self.read,
                Err(RecvTimeoutError::Timeout) => {
                    panic!("the stream stayed open for {TIMEOUT:?}: {:?}", self.read)
                }
            }
        }
    }
}

/// Write the lines of `stream` to this process's stderr on a thread.
pub fn forward_lines(stream: impl Read + Send + 'static) {
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines() {
            let Ok(line) = line else { return };
            eprintln!("{line}");
        }
    });
}
