//! The server's request loop, and the hand-over to a newer executable.
//!
//! When the file this server was started from is replaced (`ExeWatch`), the
//! next request starts the new file as a `thoth mcp` child with this server's
//! own arguments, repeats the client's initialize to it, probes its tool list,
//! and from then on copies every client line to it and every line it answers
//! back, byte for byte. One thread per child reads its stdout and writes its
//! notifications and answers to the client at once, in the order the child
//! sent them; the loop only waits for the answer to the request it wrote.
//!
//! Only two things stay here after a hand-over: `thoth_job_status` for a job
//! this process started (only this process can write its `exit.json`), and
//! lines that are not JSON-RPC requests at all. A child that fails to start,
//! to finish the handshake in 30 seconds, or that ends later, sends calls
//! back to this server's old tools with a warning; a request already written
//! to it is never run again here, as the child may have run it.

use super::exe_watch::{ExeWatch, Stamp};
use super::{
    decorate, error, generation, handle, jobs, replaced_line, result, tool_error, tools,
    write_response, Options,
};
use serde_json::{json, Value};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const HANDSHAKE: Duration = Duration::from_secs(30);
const LIST_CHANGED: &[u8] = br#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#;
const INITIALIZED: &[u8] = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
const DROPPED: &str = "thoth mcp: dropped a line from the new executable";

#[derive(Clone, Copy, PartialEq)]
enum Code {
    SpawnFailed,
    HandshakeFailed,
    Exited,
}

impl Code {
    fn as_str(self) -> &'static str {
        match self {
            Code::SpawnFailed => "spawn_failed",
            Code::HandshakeFailed => "handshake_failed",
            Code::Exited => "exited",
        }
    }
}

/// Why calls went back to this server's own tools.
struct Fallback {
    code: Code,
    detail: String,
    /// The file at the path when the child was started.
    stamp: Stamp,
}

enum State {
    /// The executable was not replaced yet (or this server cannot tell).
    Local,
    Fallback(Fallback),
    Forward(Live),
}

/// The id the child's reader thread is waiting to see answered.
struct Pending {
    id: Value,
    call: bool,
    handshake: bool,
}

#[derive(Default)]
struct Shared {
    pending: Option<Pending>,
    /// Put before the first forwarded `tools/call` result with content.
    notice: Option<String>,
    /// Set before the child is killed: nothing it still sends reaches the client.
    closed: bool,
}

enum Msg {
    /// The answer to the registered request, already written to the client
    /// unless it answers the handshake. `eof` when it was the child's last
    /// line, without a `\n`: the end comes in the same message, so the loop
    /// knows of it before it writes anything more.
    Answer { value: Value, eof: bool },
    Eof,
}

/// A running child that calls are handed to.
struct Live {
    child: Child,
    stdin: Option<ChildStdin>,
    rx: Receiver<Msg>,
    shared: Arc<Mutex<Shared>>,
    stamp: Stamp,
    /// The child's stdout ended, as far as the loop knows.
    eof: bool,
    write_failed: Option<String>,
}

enum Sent {
    /// `ended`: the answer was the child's last line.
    Answered { value: Value, ended: bool },
    /// Written, at least in part, and not answered.
    Lost(String),
    /// Not written: the child was already known to be gone.
    Unsent(String),
}

struct Server<'a> {
    options: &'a Options,
    own: &'a Path,
    // Jobs still running at EOF are neither killed nor waited for: dropping
    // `jobs` leaves each child to finish and record its own evidence.
    jobs: jobs::Jobs,
    watch: Option<ExeWatch>,
    args: Vec<OsString>,
    /// The client's last initialize params and the protocol version it got.
    init: Option<(Value, Value)>,
    state: State,
}

pub fn serve(options: &Options, own: &Path) -> anyhow::Result<i32> {
    let mut server = Server {
        options,
        own,
        jobs: jobs::Jobs::default(),
        watch: std::env::current_exe().ok().and_then(ExeWatch::start),
        args: std::env::args_os().skip(1).collect(),
        init: None,
        state: State::Local,
    };
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = Vec::new();
    loop {
        line.clear();
        if input.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let parsed: Value = match serde_json::from_slice(&line) {
            Ok(v) => v,
            Err(_) => {
                write_value(error(Value::Null, -32700, "Parse error", None))?;
                continue;
            }
        };
        let Some(obj) = parsed.as_object() else {
            write_value(error(Value::Null, -32600, "Invalid Request", None))?;
            continue;
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            write_value(error(id.unwrap_or(Value::Null), -32600, "Invalid Request", None))?;
            continue;
        };
        let Some(id) = id else {
            server.notification(&line);
            continue;
        };
        let params = obj.get("params").cloned().unwrap_or_else(|| json!({}));
        server.request(&line, id, method, &params)?;
    }
    // The child sees EOF too and ends by itself; it is not killed or waited for.
    if let State::Forward(live) = &mut server.state {
        drop(live.stdin.take());
    }
    Ok(0)
}

/// One whole line to the client, under stdout's lock, which the reader
/// threads take too.
fn write_line(line: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(line)?;
    out.write_all(b"\n")?;
    out.flush()
}

fn write_value(v: Value) -> io::Result<()> {
    write_response(&mut io::stdout().lock(), v)
}

impl Server<'_> {
    fn request(&mut self, line: &[u8], id: Value, method: &str, params: &Value) -> io::Result<()> {
        let mut list_changed = false;
        if !matches!(self.state, State::Forward(_))
            && method != "initialize"
            && generation(params).is_ok()
            && self.should_try()
        {
            // Nothing is started from a file that cannot be read whole: it
            // counts as unchanged, and no failure is recorded for it.
            if let Some(stamp) = self.watch.as_ref().and_then(ExeWatch::stamp) {
                match self.switch(params, stamp) {
                    Ok((live, changed)) => {
                        self.state = State::Forward(live);
                        list_changed = changed;
                    }
                    Err(f) => self.state = State::Fallback(f),
                }
            }
        }
        let sent = match &mut self.state {
            State::Forward(_) if own_job(&self.jobs, method, params) => None,
            State::Forward(live) => Some(send(live, line, &id, method == "tools/call")),
            _ => None,
        };
        let mut local = sent.is_none();
        match sent {
            None => {}
            Some(Sent::Answered { value, ended }) => {
                if method == "initialize" {
                    if let Some(v) = value["result"]["protocolVersion"].as_str() {
                        self.init = Some((params.clone(), json!(v)));
                    }
                }
                if ended {
                    self.exited("the new executable closed its output after an answer".into());
                    list_changed = true;
                }
            }
            Some(Sent::Lost(detail)) => {
                let answer = may_have_run(id.clone(), method, params, &detail);
                self.exited(detail);
                list_changed = true;
                write_value(answer)?;
            }
            Some(Sent::Unsent(detail)) => {
                self.exited(detail);
                list_changed = true;
                local = true;
            }
        }
        if local {
            // None while forwarding: a job of this process, answered as before.
            let warning = self.warning();
            let response = handle(
                id,
                method,
                params,
                self.options,
                self.own,
                &mut self.jobs,
                warning.as_deref(),
            );
            if method == "initialize" {
                self.init = Some((params.clone(), response["result"]["protocolVersion"].clone()));
            }
            write_value(response)?;
        }
        if list_changed {
            write_line(LIST_CHANGED)?;
        }
        Ok(())
    }

    /// A client notification goes to the child, if there is one.
    fn notification(&mut self, line: &[u8]) {
        let State::Forward(live) = &mut self.state else {
            return;
        };
        live.drain();
        if live.eof || live.write_failed.is_some() {
            return;
        }
        if let Err(e) = live.write(line) {
            live.write_failed = Some(format!("writing to the new executable failed: {e}"));
        }
    }

    /// Whether to start a child for this request.
    fn should_try(&mut self) -> bool {
        let Some(w) = self.watch.as_mut() else {
            return false;
        };
        match &mut self.state {
            State::Local => w.replaced(),
            State::Fallback(f) if f.code == Code::SpawnFailed => true,
            State::Fallback(f) => w.changed_since(&mut f.stamp),
            State::Forward(_) => false,
        }
    }

    fn warning(&self) -> Option<String> {
        match (&self.state, &self.watch) {
            (State::Fallback(f), Some(w)) => Some(replaced_line(
                w.path(),
                f.code.as_str(),
                &f.detail,
                f.code == Code::SpawnFailed,
            )),
            _ => None,
        }
    }

    /// Ends the child (`exited`); later calls are answered here.
    fn exited(&mut self, detail: String) {
        if let State::Forward(mut live) = std::mem::replace(&mut self.state, State::Local) {
            live.end();
            self.state = State::Fallback(Fallback { code: Code::Exited, detail, stamp: live.stamp });
        }
    }

    /// Starts the file at the watched path, stamped just before, and shakes
    /// hands with it. On success also says whether its tool list differs
    /// from this server's.
    fn switch(&mut self, params: &Value, stamp: Stamp) -> Result<(Live, bool), Fallback> {
        let path = self.watch.as_ref().expect("only tried with a watch").path().to_path_buf();
        let mut c = Command::new(&path);
        c.args(&self.args)
            .current_dir(self.own)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        crate::process::quiet(&mut c);
        // As in jobs::start_with: the client's pipes must not reach the
        // child, or the client never sees this server's stdout end. stderr
        // is handed over on purpose.
        #[cfg(windows)]
        let saved = crate::process::windows::clear_std_handle_inheritance();
        let spawned = c.spawn();
        #[cfg(windows)]
        crate::process::windows::restore_std_handle_inheritance(saved);
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => return Err(Fallback { code: Code::SpawnFailed, detail: e.to_string(), stamp }),
        };
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped");
        let shared = Arc::new(Mutex::new(Shared::default()));
        let (tx, rx) = mpsc::channel();
        {
            let shared = shared.clone();
            std::thread::spawn(move || relay(stdout, &shared, &tx));
        }
        let mut live = Live { child, stdin, rx, shared, stamp, eof: false, write_failed: None };
        let meta = match generation(params) {
            Ok(true) => params.get("_meta").cloned(),
            _ => None,
        };
        match handshake(&mut live, self.init.as_ref(), meta) {
            Ok((probed, version)) => {
                let own: Vec<Value> = tools::TOOLS.iter().map(tools::definition).collect();
                let notice = format!(
                    "notice: the thoth executable at {} was replaced; calls now go to the new executable (version {})",
                    path.display(),
                    version.as_deref().unwrap_or("unknown")
                );
                lock(&live.shared).notice = Some(notice);
                Ok((live, probed != own))
            }
            Err(detail) => {
                live.end();
                Err(Fallback { code: Code::HandshakeFailed, detail, stamp: live.stamp })
            }
        }
    }
}

fn lock(m: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// `thoth_job_status` for a job this process started.
fn own_job(jobs: &jobs::Jobs, method: &str, params: &Value) -> bool {
    method == "tools/call"
        && params.get("name").and_then(Value::as_str) == Some("thoth_job_status")
        && params
            .get("arguments")
            .and_then(|a| a.get("job_id"))
            .and_then(Value::as_str)
            .is_some_and(|j| jobs.owns(j))
}

/// The answer to a request written to a child that then gave none.
fn may_have_run(id: Value, method: &str, params: &Value, detail: &str) -> Value {
    let message = format!(
        "{detail}; the new thoth executable may already have run this call. \
         Check with thoth status (or thoth_status) before you retry it."
    );
    if method == "tools/call" {
        let mut v = tool_error(message, None, None, None);
        decorate(&mut v, matches!(generation(params), Ok(true)));
        result(id, v)
    } else {
        error(id, -32603, &message, None)
    }
}

/// Repeats the client's initialize (when it sent one) and probes the tool
/// list. Returns the tools and the child's version, if it said.
fn handshake(
    live: &mut Live,
    init: Option<&(Value, Value)>,
    meta: Option<Value>,
) -> Result<(Vec<Value>, Option<String>), String> {
    let deadline = Instant::now() + HANDSHAKE;
    let mut version = None;
    if let Some((params, expected)) = init {
        let v = live.ask("thoth-handshake-initialize", "initialize", params.clone(), deadline)?;
        let got = &v["result"]["protocolVersion"];
        if got != expected {
            return Err(format!(
                "initialize returned protocol version {got}, this client uses {expected}"
            ));
        }
        if live.eof {
            return Err("the new executable closed its output after it answered initialize".into());
        }
        version = v["result"]["serverInfo"]["version"].as_str().map(str::to_owned);
        live.write(INITIALIZED)
            .map_err(|e| format!("writing to the new executable failed: {e}"))?;
    }
    let probe = match meta {
        Some(m) => json!({"_meta": m}),
        None => json!({}),
    };
    let v = live.ask("thoth-handshake-tools-list", "tools/list", probe, deadline)?;
    let Some(tools) = v["result"]["tools"].as_array() else {
        return Err("tools/list returned no tools".into());
    };
    if version.is_none() {
        version = v["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["version"]
            .as_str()
            .map(str::to_owned);
    }
    Ok((tools.clone(), version))
}

impl Live {
    fn register(&self, pending: Option<Pending>) {
        lock(&self.shared).pending = pending;
    }

    fn write(&mut self, line: &[u8]) -> io::Result<()> {
        let w = self
            .stdin
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "input closed"))?;
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line);
        bytes.push(b'\n');
        w.write_all(&bytes)?;
        w.flush()
    }

    /// Takes whatever already arrived; only an end matters between requests.
    fn drain(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(Msg::Answer { eof, .. }) => self.eof |= eof,
                Ok(Msg::Eof) => self.eof = true,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.eof = true;
                    break;
                }
            }
        }
    }

    /// One handshake request; its answer never reaches the client.
    fn ask(&mut self, id: &str, method: &str, params: Value, deadline: Instant) -> Result<Value, String> {
        let id = json!(id);
        self.register(Some(Pending { id: id.clone(), call: false, handshake: true }));
        let request = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if let Err(e) = self.write(request.to_string().as_bytes()) {
            self.register(None);
            return Err(format!("writing {method} to the new executable failed: {e}"));
        }
        match self.rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::Answer { value, eof }) => {
                self.eof |= eof;
                match value.get("error") {
                    Some(e) => Err(format!("{method} returned an error: {e}")),
                    None => Ok(value),
                }
            }
            Ok(Msg::Eof) | Err(RecvTimeoutError::Disconnected) => {
                self.register(None);
                self.eof = true;
                Err(format!("the new executable closed its output before it answered {method}"))
            }
            Err(RecvTimeoutError::Timeout) => {
                self.register(None);
                Err(format!(
                    "the new executable did not finish the handshake within {} seconds",
                    HANDSHAKE.as_secs()
                ))
            }
        }
    }

    /// Kills and reaps the child; its reader thread writes nothing more.
    fn end(&mut self) {
        lock(&self.shared).closed = true;
        drop(self.stdin.take());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Writes one client request to the child and waits, without a time limit,
/// for the reader thread to say its answer went out.
fn send(live: &mut Live, line: &[u8], id: &Value, call: bool) -> Sent {
    live.drain();
    if live.eof {
        return Sent::Unsent("the new executable closed its output".into());
    }
    if let Some(e) = &live.write_failed {
        return Sent::Unsent(e.clone());
    }
    live.register(Some(Pending { id: id.clone(), call, handshake: false }));
    if let Err(e) = live.write(line) {
        live.register(None);
        return Sent::Lost(format!("writing to the new executable failed: {e}"));
    }
    match live.rx.recv() {
        Ok(Msg::Answer { value, eof }) => {
            live.eof |= eof;
            Sent::Answered { value, ended: eof }
        }
        Ok(Msg::Eof) | Err(_) => {
            live.register(None);
            live.eof = true;
            Sent::Lost("the new executable closed its output before it answered".into())
        }
    }
}

/// The reader thread of one child: every line in order, then one `Eof`,
/// unless the last line was an answer, which carries the end itself. The
/// end is handed to the loop in one message with whatever came with it, so
/// the loop never writes a request to a child it could already know is gone.
fn relay(stdout: ChildStdout, shared: &Mutex<Shared>, tx: &Sender<Msg>) {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let last = line.last() != Some(&b'\n');
        if !last {
            line.pop();
        }
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        let answer = relay_line(&line, shared);
        if !last {
            if let Some(value) = answer {
                let _ = tx.send(Msg::Answer { value, eof: false });
            }
            continue;
        }
        // The output ended with this line: one message tells the loop both.
        #[cfg(debug_assertions)]
        test_eof_delay();
        let _ = tx.send(match answer {
            Some(value) => Msg::Answer { value, eof: true },
            None => Msg::Eof,
        });
        return;
    }
    #[cfg(debug_assertions)]
    test_eof_delay();
    let _ = tx.send(Msg::Eof);
}

/// Test seam, for tests/mcp_replaced.rs only; debug builds only, so a
/// release build has no such pause. `THOTH_TEST_MCP_EOF_DELAY_MS` holds the
/// reader thread for that long after it has read the child's last line and
/// the end of its output, before it tells the loop. The pause widens the
/// window a reader that reported an answer and the end as two messages
/// would open, in which the loop writes the next request to a child that
/// has already closed its output; with one message there is no window.
#[cfg(debug_assertions)]
fn test_eof_delay() {
    let ms = std::env::var("THOTH_TEST_MCP_EOF_DELAY_MS").ok().and_then(|v| v.parse::<u64>().ok());
    if let Some(ms) = ms {
        std::thread::sleep(Duration::from_millis(ms));
    }
}

fn dropped(why: &str) -> Option<Value> {
    crate::output::try_eprintln_masked(&format!("{DROPPED}: {why}"));
    None
}

/// Writes a notification or the awaited answer to the client, and returns
/// that answer for the loop; an answer to the handshake only goes to the loop.
fn relay_line(line: &[u8], shared: &Mutex<Shared>) -> Option<Value> {
    let Ok(mut v) = serde_json::from_slice::<Value>(line) else {
        return dropped("not JSON");
    };
    let Some(obj) = v.as_object() else {
        return dropped("not a JSON object");
    };
    let id = obj.get("id").cloned();
    if obj.contains_key("method") {
        if id.is_some() {
            return dropped("a request to the client");
        }
        // Held while writing, so a child being ended writes nothing after.
        let s = lock(shared);
        if !s.closed {
            let _ = write_line(line);
        }
        return None;
    }
    let mut s = lock(shared);
    if s.closed {
        return None;
    }
    let matched = id.is_some() && s.pending.as_ref().map(|p| &p.id) == id.as_ref();
    if !matched {
        drop(s);
        return dropped("an answer to no request waiting for one");
    }
    let p = s.pending.take().expect("matched");
    if p.handshake {
        return Some(v);
    }
    let content = v
        .get_mut("result")
        .and_then(|r| r.get_mut("content"))
        .and_then(Value::as_array_mut);
    match (p.call, content, s.notice.as_ref()) {
        (true, Some(content), Some(_)) => {
            let notice = s.notice.take().expect("checked");
            content.insert(0, json!({"type":"text","text":notice}));
            let _ = write_line(v.to_string().as_bytes());
        }
        _ => {
            let _ = write_line(line);
        }
    }
    Some(v)
}
