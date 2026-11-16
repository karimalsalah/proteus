//! `proteus mcp` end to end: the built executable, real pipes, a daemon of
//! its own in a temporary folder, real programs on real pseudo-terminals.
//! Nothing here touches the sessions of whoever is using this machine.
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

/// A data folder, a socket and a private copy of the executable. The copy is
/// what runs, so the build output is never held open.
struct Home {
    dir: PathBuf,
    exe: PathBuf,
}

static HOMES: AtomicU64 = AtomicU64::new(0);

impl Home {
    fn new(tag: &str) -> Self {
        let n = HOMES.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("proteus-mcp-test-{}-{tag}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let exe = dir.join("proteus-under-test.exe");
        std::fs::copy(env!("CARGO_BIN_EXE_proteus"), &exe).unwrap();
        Self { dir, exe }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.exe);
        c.args(args)
            .env("PROTEUS_DATA", self.dir.join("data"))
            .env("PROTEUS_SOCKET", self.dir.join("d.sock"))
            .env("PROTEUS_SOUND", "off")
            .env_remove("PROTEUS_SHELL");
        c
    }

    fn sessions(&self) -> Value {
        let out = self.command(&["sessions"]).stdin(Stdio::null()).output().unwrap();
        serde_json::from_slice(&out.stdout).unwrap_or(Value::Null)
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        // The test's own daemon leaves when asked, once nothing is running in
        // it; the sessions of a server that has just gone take a moment to
        // end. Ask until it has left. One that will not leave (a test failed
        // with a session still running) is ended by its process id: it is
        // this test's own, the only one listening on this socket. A daemon
        // left behind holds this run's output pipe open, so whoever waits for
        // the run to end (the gate) would wait for as long as it lives; and
        // its running image keeps the folder from being removed.
        let sock = self.dir.join("d.sock");
        let pid = self.sessions()["pid"].as_u64();
        let deadline = Instant::now() + Duration::from_secs(6);
        let mut gone = false;
        while !gone && Instant::now() < deadline {
            match uds_windows::UnixStream::connect(&sock) {
                Ok(s) => {
                    let _ = BufWriter::new(&s).write_all(&[7, 0, 0, 0, 0]);
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => gone = true,
            }
        }
        if let (false, Some(pid)) = (gone, pid) {
            let _ = Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
        }
        for _ in 0..30 {
            if std::fs::remove_dir_all(&self.dir).is_ok() || !self.dir.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

/// An MCP client: one server process and its two pipes.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next: u64,
    /// Messages read while waiting for another one.
    held: Vec<Value>,
}

impl Client {
    fn start(home: &Home, flags: &[&str]) -> Self {
        let mut args = vec!["mcp"];
        args.extend_from_slice(flags);
        let mut child = home.command(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self { stdin: child.stdin.take(), child, lines: rx, next: 0, held: Vec::new() }
    }

    fn write(&mut self, msg: &Value) {
        let line = format!("{msg}\n");
        let stdin = self.stdin.as_mut().unwrap();
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    /// The answer to request `id`. Every line the server prints must be one JSON message.
    fn answer(&mut self, id: &Value, within: Duration) -> Option<Value> {
        if let Some(i) = self.held.iter().position(|m| &m["id"] == id) {
            return Some(self.held.remove(i));
        }
        let deadline = Instant::now() + within;
        while let Ok(line) = self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            let msg: Value = serde_json::from_str(&line).unwrap_or_else(|e| panic!("not JSON on stdout ({e}): {line}"));
            assert_eq!(msg["jsonrpc"], "2.0", "{line}");
            if &msg["id"] == id {
                return Some(msg);
            }
            self.held.push(msg);
        }
        None
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let id = json!(self.next);
        self.write(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.answer(&id, Duration::from_secs(90)).unwrap_or_else(|| panic!("no answer to {method}"))
    }

    fn call_raw(&mut self, tool: &str, args: Value) -> Value {
        let r = self.request("tools/call", json!({ "name": tool, "arguments": args }));
        assert!(r["error"].is_null(), "{tool}: {r}");
        r["result"].clone()
    }

    /// A tool call that must succeed: its facts (the first block, one line of
    /// JSON) and the text that follows, if any.
    fn call(&mut self, tool: &str, args: Value) -> (Value, String) {
        let r = self.call_raw(tool, args.clone());
        assert_eq!(r["isError"], false, "{tool} {args}: {r}");
        let facts: Value = serde_json::from_str(r["content"][0]["text"].as_str().unwrap()).unwrap();
        let body = r["content"][1]["text"].as_str().unwrap_or("").to_string();
        (facts, body)
    }

    /// A tool call that must be refused, and why.
    fn refused(&mut self, tool: &str, args: Value) -> String {
        let r = self.call_raw(tool, args);
        assert_eq!(r["isError"], true, "{tool}: {r}");
        r["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn handshake(&mut self) {
        let r = self.request("initialize", json!({ "protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": { "name": "proteus-test", "version": "0" } }));
        assert_eq!(r["result"]["protocolVersion"], "2025-11-25", "{r}");
        self.write(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
    }

    /// Close the pipe and wait for the server to leave by itself.
    fn close(mut self) -> bool {
        self.leave()
    }

    fn leave(&mut self) -> bool {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        false
    }

    /// As `close`, and then: does the server's output read end-of-file? It
    /// does once nothing holds the other end of that pipe any more.
    fn close_to_the_end(mut self) -> (bool, bool) {
        let left = self.leave();
        loop {
            match self.lines.recv_timeout(Duration::from_secs(3)) {
                Ok(_) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return (left, true),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return (left, false),
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.kill(); // our own child, by handle
        let _ = self.child.wait();
    }
}

fn decode_png(b64: &str) -> (u32, u32, Vec<u8>) {
    let mut bytes = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    for c in b64.bytes().filter(|c| *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => panic!("not base64"),
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            bytes.push((acc >> bits) as u8);
        }
    }
    let mut reader = png::Decoder::new(std::io::Cursor::new(bytes)).read_info().expect("a PNG");
    let mut buf = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut buf).unwrap();
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// The path the brief names: handshake, a shell, a command, wait, read, see, exit.
#[test]
fn an_agent_drives_a_shell_from_start_to_exit() {
    let home = Home::new("drive");
    let mut c = Client::start(&home, &[]);
    c.handshake();
    assert_eq!(c.request("ping", json!({}))["result"], json!({}));
    let tools = c.request("tools/list", json!({}));
    let names: Vec<&str> = tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["sessions", "spawn", "send", "wait", "read", "screen", "shot", "kill"]);

    let (spawned, _) = c.call("spawn", json!({ "command": "cmd.exe /d", "cols": 100, "rows": 30 }));
    let id = spawned["session"].as_u64().unwrap();
    assert_eq!(spawned["watch"], format!("proteus --attach {id}"));
    let (listed, _) = c.call("sessions", json!({}));
    let mine = &listed["sessions"][0];
    assert_eq!((mine["session"].as_u64(), mine["spawned_here"].as_bool(), mine["cols"].as_u64(), mine["rows"].as_u64()), (Some(id), Some(true), Some(100), Some(30)));
    assert!(listed["daemon"]["caps"].as_array().unwrap().contains(&json!("sync")));

    // The prompt arrives; until then the session is not quiet.
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": r">\s*$" }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "regex", "{w}");
    println!("spawn to prompt: {} ms", w["elapsed_ms"]);
    c.call("wait", json!({ "session": id, "until": { "quiet_ms": 300 }, "timeout_ms": 20000 }));
    let (first, banner) = c.call("read", json!({ "session": id }));
    assert!(banner.contains("Microsoft Windows"), "{banner}");
    assert!(banner.trim_end().ends_with('>'), "{banner}");
    assert_eq!(first["exited"], false);

    // Type a command. Its echo contains "hello" too, so the pattern is
    // anchored to a line of output.
    let (sent, _) = c.call("send", json!({ "session": id, "text": "echo hello & exit 3", "enter": true }));
    assert_eq!(sent["sent_bytes"], 20);
    assert_eq!(sent["cursor"], first["cursor"], "nothing was printed in between");
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": r"^hello\b" }, "timeout_ms": 20000 }));
    assert_eq!((w["reason"].as_str(), w["match"].as_str(), w["line"].as_str().map(str::trim_end)), (Some("regex"), Some("hello"), Some("hello")), "{w}");
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "exit": true }, "timeout_ms": 20000 }));
    assert_eq!((w["reason"].as_str(), w["exit_code"].as_u64()), (Some("exit"), Some(3)), "{w}");

    // Lean: the command as it was run and what it printed, nothing else.
    let (facts, body) = c.call("read", json!({ "session": id }));
    let lines: Vec<&str> = body.lines().collect();
    assert!(lines[0].ends_with(">echo hello & exit 3"), "{body}");
    assert_eq!(lines[1], "hello");
    assert_eq!(lines.len(), 2, "{body}");
    assert_eq!((facts["exited"].as_bool(), facts["exit_code"].as_u64(), facts["more_bytes"].as_u64()), (Some(true), Some(3), Some(0)));
    let (raw_bytes, lean_bytes) = (facts["raw_bytes"].as_u64().unwrap(), facts["lean_bytes"].as_u64().unwrap());
    assert_eq!(lean_bytes as usize, body.len());
    assert!(raw_bytes > lean_bytes, "{facts}");
    // And always a way to the raw text: the same span with its escape codes.
    let (rawf, raw) = c.call("read", json!({ "session": id, "cursor": sent["cursor"], "mode": "raw" }));
    assert_eq!(rawf["raw_bytes"].as_u64(), Some(raw_bytes));
    assert!(raw.contains('\u{1b}') && raw.contains("hello"), "{raw:?}");
    // Nothing is left: a read with nothing new says so and returns no text.
    let (again, nothing) = c.call("read", json!({ "session": id }));
    assert_eq!((again["raw_bytes"].as_u64(), nothing.as_str()), (Some(0), ""));

    // The screen as a person would see it.
    let (scr, text) = c.call("screen", json!({ "session": id }));
    assert_eq!((scr["cols"].as_u64(), scr["rows"].as_u64()), (Some(100), Some(30)));
    assert!(scr["title"].as_str().unwrap().contains("cmd.exe"), "{scr}");
    let rows: Vec<&str> = text.lines().collect();
    assert!(rows[0].starts_with("Microsoft Windows"), "{text}");
    let hello = rows.iter().position(|r| *r == "hello").expect("hello on screen");
    assert!(rows[hello - 1].ends_with(">echo hello & exit 3"));
    assert_eq!(scr["cursor_row"].as_u64(), Some(hello as u64 + 2), "the cursor sits on the line after the output");
    // A fiftieth of a second in, the shell had printed nothing yet: time travel through the journal.
    let (then, blank) = c.call("screen", json!({ "session": id, "at": 0.02 }));
    assert_eq!((blank.trim(), then["cursor_row"].as_u64()), ("", Some(1)), "{then}");

    // The same as a picture.
    let shot = c.call_raw("shot", json!({ "session": id }));
    assert_eq!(shot["isError"], false, "{shot}");
    let facts: Value = serde_json::from_str(shot["content"][0]["text"].as_str().unwrap()).unwrap();
    let image = &shot["content"][1];
    assert_eq!((image["type"].as_str(), image["mimeType"].as_str()), (Some("image"), Some("image/png")));
    let (w, h, px) = decode_png(image["data"].as_str().unwrap());
    assert_eq!((Some(w as u64), Some(h as u64)), (facts["width"].as_u64(), facts["height"].as_u64()));
    assert!(w > 600 && h > 300, "{w}x{h}");
    assert_eq!(px.len(), (w * h * 3) as usize, "three channels");
    let bg = px[..3].to_vec();
    let inked = px.chunks(3).filter(|p| p.iter().zip(&bg).any(|(a, b)| a.abs_diff(*b) > 40)).count();
    assert!(inked > 500, "the picture is blank ({inked} pixels differ from the background)");
    let exact = c.call_raw("shot", json!({ "session": id, "exact": true }));
    let exact: Value = serde_json::from_str(exact["content"][0]["text"].as_str().unwrap()).unwrap();
    println!("shot of a screen with five lines on it: {w}x{h}, {} bytes; with exact pixels {} bytes", facts["png_bytes"], exact["png_bytes"]);
    assert!(exact["png_bytes"].as_u64() > facts["png_bytes"].as_u64());
    // A blank moment renders blank: the ink in the first picture is the text.
    let early = c.call_raw("shot", json!({ "session": id, "at": 0.02 }));
    let (_, _, px0) = decode_png(early["content"][1]["data"].as_str().unwrap());
    let inked0 = px0.chunks(3).filter(|p| p.iter().zip(&bg).any(|(a, b)| a.abs_diff(*b) > 40)).count();
    assert!(inked0 * 4 < inked, "{inked0} against {inked}");

    // A picture that would be too large is refused; the server carries on.
    let (wide, _) = c.call("spawn", json!({ "command": "cmd.exe /d /c echo wide", "cols": 500, "rows": 40 }));
    c.call("wait", json!({ "session": wide["session"], "until": { "exit": true }, "timeout_ms": 20000 }));
    assert!(c.refused("shot", json!({ "session": wide["session"], "scale": 3 })).contains("the limit is 8192"));
    let (_, small) = c.call("screen", json!({ "session": wide["session"] }));
    assert_eq!(small, "wide");

    // It has ended: it cannot be typed into, but it is still listed and readable.
    assert!(c.refused("send", json!({ "session": id, "text": "x" })).contains("has ended (exit code 3)"));
    let (listed, _) = c.call("sessions", json!({}));
    assert!(listed["sessions"].as_array().unwrap().is_empty());
    assert_eq!((listed["ended"][0]["session"].as_u64(), listed["ended"][0]["exit_code"].as_u64()), (Some(id), Some(3)));
    assert_eq!(listed["ended"].as_array().map(Vec::len), Some(2));
    assert!(c.refused("read", json!({ "session": 4242 })).contains("no session 4242"));
    assert!(c.close(), "the server leaves when its input closes");
}

/// The server may look at any session but drives only its own.
#[test]
fn another_servers_session_can_be_read_but_not_driven() {
    let home = Home::new("safety");
    let mut owner = Client::start(&home, &[]);
    owner.handshake();
    let (spawned, _) = owner.call("spawn", json!({ "command": "cmd.exe", "args": ["/d"] }));
    let id = spawned["session"].as_u64().unwrap();
    owner.call("wait", json!({ "session": id, "until": { "regex": r">\s*$" }, "timeout_ms": 20000 }));

    let mut guest = Client::start(&home, &[]);
    guest.handshake();
    let (listed, _) = guest.call("sessions", json!({}));
    let theirs = &listed["sessions"][0];
    assert_eq!((theirs["session"].as_u64(), theirs["spawned_here"].as_bool(), theirs["can_drive"].as_bool()), (Some(id), Some(false), Some(false)));
    for (tool, args) in [("send", json!({ "session": id, "text": "echo intruder", "enter": true })), ("kill", json!({ "session": id }))] {
        let why = guest.refused(tool, args);
        assert!(why.contains("not started by this server") && why.contains("--drive-all"), "{why}");
    }
    // Looking is allowed, from the journal, without becoming one of its windows.
    let (_, seen) = guest.call("screen", json!({ "session": id }));
    assert!(seen.contains("Microsoft Windows"), "{seen}");
    // A wait with no cursor looks at what comes from now on: the banner printed
    // before the guest first looked is on its screen but is not news.
    let (w, _) = guest.call("wait", json!({ "session": id, "until": { "regex": "Microsoft Windows" }, "timeout_ms": 700 }));
    assert_eq!(w["reason"], "timeout", "{w}");
    let (w, _) = guest.call("wait", json!({ "session": id, "until": { "regex": "Microsoft Windows" }, "timeout_ms": 700, "cursor": 0 }));
    assert_eq!(w["reason"], "regex", "from the top it is there: {w}");
    owner.call("send", json!({ "session": id, "text": "echo for-all-to-see", "enter": true }));
    let (w, _) = guest.call("wait", json!({ "session": id, "until": { "regex": "^for-all-to-see" }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "regex", "{w}");
    let (_, read) = guest.call("read", json!({ "session": id }));
    assert!(read.contains("for-all-to-see") && !read.contains("intruder"), "{read}");
    let now = home.sessions();
    assert_eq!(now["sessions"][0]["windows"], 1, "the guest never attached: {now}");
    assert_eq!((now["sessions"][0]["cols"].as_u64(), now["sessions"][0]["rows"].as_u64()), (Some(120), Some(32)), "and nobody resized it");

    // With --drive-all the same request is honoured, and the owner sees it.
    let mut driver = Client::start(&home, &["--drive-all"]);
    driver.handshake();
    let (sent, _) = driver.call("send", json!({ "session": id, "text": "echo driven-from-outside", "enter": true }));
    assert!(sent["cursor"].as_u64().unwrap() > 0, "it has the whole history: {sent}");
    let (w, _) = owner.call("wait", json!({ "session": id, "until": { "regex": "^driven-from-outside" }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "regex", "{w}");
    // The guest's refused input never reached the shell.
    let (_, all) = owner.call("read", json!({ "session": id, "cursor": 0, "mode": "plain" }));
    assert!(all.contains("driven-from-outside") && !all.contains("intruder"), "{all}");
    let (killed, _) = driver.call("kill", json!({ "session": id }));
    assert_eq!(killed["ended"], true, "{killed}");
    let (w, _) = owner.call("wait", json!({ "session": id, "until": { "exit": true }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "exit");
    // The only size the journal ever recorded is the one it was spawned with.
    let journal = std::fs::read(spawned["journal"].as_str().unwrap()).unwrap();
    assert_eq!(resize_records(&journal), [(120, 32)]);
}

/// Every resize a journal holds, in order.
fn resize_records(j: &[u8]) -> Vec<(u16, u16)> {
    let mut out = Vec::new();
    let mut i = 12 + u32::from_le_bytes(j[8..12].try_into().unwrap()) as usize;
    while i + 13 <= j.len() {
        let len = u32::from_le_bytes(j[i + 9..i + 13].try_into().unwrap()) as usize;
        if j[i] == 2 && len >= 4 {
            out.push((u16::from_le_bytes([j[i + 13], j[i + 14]]), u16::from_le_bytes([j[i + 15], j[i + 16]])));
        }
        i += 13 + len;
    }
    out
}

/// Three thousand lines: lean cuts them to a few, loses no error, and every
/// line is still there to be had, whole and once, a page at a time.
#[test]
fn long_output_is_cut_down_and_can_still_be_had_in_full() {
    let home = Home::new("long");
    let mut c = Client::start(&home, &[]);
    c.handshake();
    let script = "for /L %i in (1,1,3000) do @(echo line %i of the build log& if %i==1777 echo error: disk full at step %i)";
    let (spawned, _) = c.call("spawn", json!({ "command": "cmd.exe", "args": ["/d", "/c", script] }));
    let id = spawned["session"].as_u64().unwrap();
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "exit": true }, "timeout_ms": 60000 }));
    assert_eq!((w["reason"].as_str(), w["exit_code"].as_u64()), (Some("exit"), Some(0)), "{w}");

    let (facts, body) = c.call("read", json!({ "session": id }));
    println!("3001 lines: {facts}");
    assert_eq!((facts["lines"].as_u64(), facts["errors"].as_u64(), facts["more_bytes"].as_u64()), (Some(3001), Some(1), Some(0)), "{facts}");
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines, [
        "line 1 of the build log",
        "[... 1775 similar lines ...]",
        "line 1777 of the build log",
        "error: disk full at step 1777",
        "line 1778 of the build log",
        "[... 1221 similar lines ...]",
        "line 3000 of the build log",
    ]);
    assert!(facts["raw_bytes"].as_u64().unwrap() > 70_000 && facts["lean_bytes"].as_u64().unwrap() < 250, "{facts}");
    assert!(facts["saved_pct"].as_f64().unwrap() > 99.0);

    // The lines a count stands for, by number.
    let (win, part) = c.call("read", json!({ "session": id, "cursor": 0, "mode": "plain", "lines": [1500, 1502] }));
    assert_eq!(part, "line 1500 of the build log\nline 1501 of the build log\nline 1502 of the build log", "{win}");

    // Everything, in pages of about 4,000 bytes: no line twice, none cut in half, none missing.
    let mut cursor = 0;
    let mut all: Vec<String> = Vec::new();
    let mut pages = 0;
    loop {
        let (f, page) = c.call("read", json!({ "session": id, "cursor": cursor, "mode": "plain", "max_bytes": 4000 }));
        all.extend(page.lines().map(String::from));
        pages += 1;
        cursor = f["cursor"].as_u64().unwrap();
        assert!(page.len() < 6000, "a page stays near its size: {}", page.len());
        if f["more_bytes"] == 0 {
            break;
        }
        assert!(pages < 200);
    }
    assert!(pages > 10, "{pages} pages");
    let mut want: Vec<String> = Vec::new();
    for i in 1..=3000 {
        want.push(format!("line {i} of the build log"));
        if i == 1777 {
            want.push("error: disk full at step 1777".into());
        }
    }
    assert_eq!(all.len(), want.len());
    assert_eq!(all, want);

    // Raw, in pages too: the bytes add up to exactly what the session printed.
    let (mut cursor, mut total) = (0, 0);
    loop {
        let (f, _) = c.call("read", json!({ "session": id, "cursor": cursor, "mode": "raw", "max_bytes": 50000 }));
        total += f["raw_bytes"].as_u64().unwrap();
        cursor = f["cursor"].as_u64().unwrap();
        if f["more_bytes"] == 0 {
            break;
        }
    }
    assert_eq!(total, facts["raw_bytes"].as_u64().unwrap());
}

/// The stateless revision of the protocol, and the two revisions side by side.
#[test]
fn the_2026_revision_is_served_without_a_handshake() {
    let home = Home::new("modern");
    let mut c = Client::start(&home, &[]);
    let meta = |v: &str| json!({ "io.modelcontextprotocol/protocolVersion": v, "io.modelcontextprotocol/clientCapabilities": {}, "io.modelcontextprotocol/clientInfo": { "name": "proteus-test", "version": "0" } });
    let d = c.request("server/discover", json!({ "_meta": meta("2026-07-28") }));
    assert_eq!(d["result"]["resultType"], "complete", "{d}");
    assert_eq!(d["result"]["supportedVersions"][0], "2026-07-28");
    assert_eq!(d["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "proteus");
    let old = c.request("tools/list", json!({ "_meta": meta("2030-01-01") }));
    assert_eq!(old["error"]["code"], -32022, "{old}");
    let r = c.request("tools/call", json!({ "name": "spawn", "arguments": { "command": "cmd.exe /d /c echo stateless-ok" }, "_meta": meta("2026-07-28") }));
    assert_eq!((r["result"]["resultType"].as_str(), r["result"]["isError"].as_bool()), (Some("complete"), Some(false)), "{r}");
    let id = serde_json::from_str::<Value>(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap()["session"].as_u64().unwrap();
    let r = c.request("tools/call", json!({ "name": "wait", "arguments": { "session": id, "until": { "exit": true }, "timeout_ms": 20000 }, "_meta": meta("2026-07-28") }));
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("\"reason\":\"exit\""), "{r}");
    let r = c.request("tools/call", json!({ "name": "read", "arguments": { "session": id }, "_meta": meta("2026-07-28") }));
    assert_eq!(r["result"]["content"][1]["text"], "stateless-ok", "{r}");
    assert_eq!(r["result"]["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "proteus");
    // A malformed line is answered as such and does not end the conversation.
    c.stdin.as_mut().unwrap().write_all(b"this is not json\n").unwrap();
    c.stdin.as_mut().unwrap().flush().unwrap();
    let bad = c.answer(&Value::Null, Duration::from_secs(10)).expect("a parse error");
    assert_eq!(bad["error"]["code"], -32700);
    assert_eq!(c.request("ping", json!({}))["result"], json!({}));
}

/// A wait can be called off, costs nothing while it sleeps, and a server
/// that goes away takes its sessions with it.
#[test]
fn a_wait_is_cancelled_and_sessions_end_with_their_server() {
    let home = Home::new("cancel");
    let mut c = Client::start(&home, &[]);
    c.handshake();
    let (spawned, _) = c.call("spawn", json!({ "command": "cmd.exe /d" }));
    let id = spawned["session"].as_u64().unwrap();
    c.call("wait", json!({ "session": id, "until": { "regex": r">\s*$" }, "timeout_ms": 20000 }));
    c.call("wait", json!({ "session": id, "until": { "quiet_ms": 400 }, "timeout_ms": 20000 }));

    // Silence is measured, not assumed: quiet for 300 ms takes at least 300 ms.
    let t = Instant::now();
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "quiet_ms": 300 }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "quiet");
    assert!(t.elapsed() >= Duration::from_millis(300) && t.elapsed() < Duration::from_millis(1500), "{:?}", t.elapsed());
    // A wait for something that never comes ends at its timeout.
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": "never printed" }, "timeout_ms": 400 }));
    assert_eq!(w["reason"], "timeout");
    assert!((400..2000).contains(&w["elapsed_ms"].as_u64().unwrap()), "{w}");

    // Start one that would last a minute, then call it off.
    c.write(&json!({ "jsonrpc": "2.0", "id": "long", "method": "tools/call", "params": { "name": "wait", "arguments": { "session": id, "until": { "regex": "never printed" }, "timeout_ms": 60000 } } }));
    std::thread::sleep(Duration::from_millis(300));
    // Other requests are served while it is blocked.
    assert_eq!(c.request("ping", json!({}))["result"], json!({}));
    c.write(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": "long", "reason": "test" } }));
    assert!(c.answer(&json!("long"), Duration::from_millis(1500)).is_none(), "a cancelled request is not answered");
    assert_eq!(c.request("ping", json!({}))["result"], json!({}));

    // The client goes: the server leaves, and the daemon ends the session nobody can reach any more.
    assert_eq!(home.sessions()["sessions"].as_array().map(Vec::len), Some(1));
    assert!(c.close());
    let deadline = Instant::now() + Duration::from_secs(10);
    while home.sessions()["sessions"].as_array().is_some_and(|s| !s.is_empty()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(home.sessions()["sessions"].as_array().map(Vec::len), Some(0), "the session outlived its server");
}

/// What arrives on standard input may be anything. Bytes that are not a
/// message are answered as such; the server, and the sessions it holds, stay.
#[test]
fn bytes_that_are_not_a_message_do_not_end_the_server() {
    let home = Home::new("bytes");
    let mut c = Client::start(&home, &[]);
    c.handshake();
    let (spawned, _) = c.call("spawn", json!({ "command": "cmd.exe /d" }));
    let id = spawned["session"].as_u64().unwrap();
    // Not UTF-8 inside a string; not UTF-8 at all; a megabyte of nothing.
    let noise = vec![0xC3u8; 1 << 20];
    for bad in [&b"{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"ping\",\"params\":{\"x\":\"\xff\xfe\"}}"[..], &b"\xff\xfe\x00\x80"[..], &noise[..]] {
        let stdin = c.stdin.as_mut().unwrap();
        stdin.write_all(bad).unwrap();
        stdin.write_all(b"\n").unwrap();
        stdin.flush().unwrap();
        let e = c.answer(&Value::Null, Duration::from_secs(10)).expect("a parse error, not silence");
        assert_eq!(e["error"]["code"], -32700, "{e}");
        assert!(e.to_string().len() < 400, "the answer does not echo the noise");
    }
    assert_eq!(c.request("ping", json!({}))["result"], json!({}));
    // The session spawned before is still there and still this server's to drive.
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": r">\s*$" }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "regex", "{w}");
    c.call("send", json!({ "session": id, "text": "echo still-here", "enter": true }));
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": "^still-here" }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "regex", "{w}");

    // An argument of the wrong kind is refused by name, not replaced by a default.
    let t = Instant::now();
    let why = c.refused("wait", json!({ "session": id, "until": { "quiet_ms": 100 }, "timeout_ms": "5" }));
    assert!(why.contains("timeout_ms must be a whole number") && t.elapsed() < Duration::from_secs(2), "{why}");
    assert!(c.refused("screen", json!({ "session": id, "at": "3" })).contains("at must be a number"));
    assert!(c.refused("wait", json!({ "session": id, "until": { "quiet_ms": 100 }, "timeout": 5 })).contains("unknown argument timeout"));
    assert!(c.refused("spawn", json!({ "command": 5 })).contains("command must be text"));
    assert!(c.refused("send", json!({ "session": id, "keys": [""] })).contains("unknown key"));
    // A whole number written as a float, and one key as a string, are taken.
    let (w, _) = c.call("wait", json!({ "session": id as f64, "until": { "quiet_ms": 50.0 }, "timeout_ms": 5000 }));
    assert_eq!(w["reason"], "quiet", "{w}");
    c.call("send", json!({ "session": id, "keys": "Escape" }));
    // A command line Windows could not start is refused at once, and briefly.
    let t = Instant::now();
    let why = c.refused("spawn", json!({ "command": "x".repeat(200_000) }));
    assert!(why.contains("Windows starts at most 32767") && why.len() < 200 && t.elapsed() < Duration::from_secs(2), "{} bytes after {:?}", why.len(), t.elapsed());
    assert_eq!(home.sessions()["sessions"].as_array().map(Vec::len), Some(1));
}

/// A valid pattern can be slow beyond use: nested counted repeats compile to
/// megabytes and take minutes per megabyte of text. The wait still ends at
/// its timeout, says how far it got, and stops working when called off.
#[test]
fn a_slow_pattern_cannot_hold_a_wait_past_its_timeout() {
    let home = Home::new("slow");
    let mut c = Client::start(&home, &[]);
    c.handshake();
    // 300 KB of the letter the pattern counts.
    std::fs::write(home.dir.join("chew.txt"), format!("{} \r\n", "a".repeat(90)).repeat(3300)).unwrap();
    let (s, _) = c.call("spawn", json!({ "command": "cmd.exe /d /c type chew.txt", "cwd": home.dir.to_string_lossy() }));
    let id = s["session"].as_u64().unwrap();
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "exit": true }, "timeout_ms": 60000 }));
    assert_eq!(w["reason"], "exit", "{w}");
    let end = w["cursor"].as_u64().unwrap();
    assert!(end > 300_000, "{w}");
    let per_ms = cycles_per_ms();

    let slow = "(?:(?:(?:a{1,30}){1,30}){1,30})Q";
    let wait = |id_: &str, timeout: u64| json!({ "jsonrpc": "2.0", "id": id_, "method": "tools/call", "params": { "name": "wait", "arguments": { "session": id, "until": { "regex": slow }, "timeout_ms": timeout, "cursor": 0 } } });
    let t = Instant::now();
    c.write(&wait("slow", 300));
    let r = c.answer(&json!("slow"), Duration::from_secs(10)).expect("a wait answers when its timeout has passed, whatever the pattern");
    let took = t.elapsed();
    let facts: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    println!("slow pattern, timeout 300 ms: answered after {} ms, {} of {end} bytes searched", took.as_millis(), facts["cursor"]);
    assert_eq!(facts["reason"], "timeout", "{facts}");
    let got_to = facts["cursor"].as_u64().unwrap();
    assert!(got_to > 0 && got_to < end, "the cursor says how far the search got: {facts}");
    assert!(took < Duration::from_millis(2500), "{took:?}");

    // The same pattern over output that turns slow only after a megabyte it
    // rejects at a glance. How fast the text before was searched says nothing
    // about the text to come: a search paced by it answered here after 51 s.
    let quick = format!("{} \r\n", "x".repeat(90)).repeat(11_300);
    std::fs::write(home.dir.join("turn.txt"), format!("{quick}{}", format!("{} \r\n", "a".repeat(90)).repeat(3300))).unwrap();
    let (s, _) = c.call("spawn", json!({ "command": "cmd.exe /d /c type turn.txt", "cwd": home.dir.to_string_lossy() }));
    let turn = s["session"].as_u64().unwrap();
    let (w, _) = c.call("wait", json!({ "session": turn, "until": { "exit": true }, "timeout_ms": 60000 }));
    let turn_end = w["cursor"].as_u64().unwrap();
    assert!(w["reason"] == "exit" && turn_end > 1_300_000, "{w}");
    let t = Instant::now();
    c.write(&json!({ "jsonrpc": "2.0", "id": "turn", "method": "tools/call", "params": { "name": "wait", "arguments": { "session": turn, "until": { "regex": slow }, "timeout_ms": 300, "cursor": 0 } } }));
    let r = c.answer(&json!("turn"), Duration::from_secs(10)).expect("a wait answers when its timeout has passed, whatever the text");
    let took = t.elapsed();
    let facts: Value = serde_json::from_str(r["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    println!("slow pattern, quick text then slow text, timeout 300 ms: answered after {} ms, {} of {turn_end} bytes searched", took.as_millis(), facts["cursor"]);
    assert_eq!(facts["reason"], "timeout", "{facts}");
    assert!(took < Duration::from_secs(5), "{took:?}");
    // A pattern this heavy sees less far back than an ordinary one, and the answer says so.
    assert_eq!(facts["pattern_reach"], 256, "{facts}");
    assert!(facts["pattern_note"].as_str().is_some_and(|n| n.contains("more than 1024 KB")), "{facts}");

    // Called off, it stops: half a second later the server is using no processor.
    c.write(&wait("slow-2", 120_000));
    std::thread::sleep(Duration::from_millis(500));
    c.write(&json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": "slow-2" } }));
    std::thread::sleep(Duration::from_millis(700));
    let (before, t0) = (cpu(c.child.id()).1, Instant::now());
    std::thread::sleep(Duration::from_millis(500));
    let busy_ms = (cpu(c.child.id()).1 - before) as f64 / per_ms;
    println!("after the cancel: {busy_ms:.1} ms of CPU in {} ms", t0.elapsed().as_millis());
    assert!(busy_ms < 100.0, "a cancelled wait kept searching: {busy_ms:.0} ms of CPU in half a second");
    assert!(c.answer(&json!("slow-2"), Duration::from_millis(200)).is_none(), "a cancelled request is not answered");

    // An ordinary pattern over the same output is found as ever.
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": "^a+ $" }, "timeout_ms": 5000, "cursor": 0 }));
    assert_eq!((w["reason"].as_str(), w["match"].as_str().map(str::len)), (Some("regex"), Some(91)), "{w}");
    assert!(w.get("pattern_reach").is_none(), "an ordinary pattern has the full reach and no note: {w}");
}

fn frame(ty: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![ty];
    f.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    f.extend_from_slice(payload);
    f
}

/// The case the safety rule exists for: a session a person's window started
/// (no agent mark; the daemon keeps it whatever comes and goes). A server
/// may look at it. It may not type into it, end it, resize it, or take it
/// down with itself, and with --drive-all it may type but still never
/// resizes it and never ends it by leaving.
#[test]
fn a_persons_own_session_is_looked_at_and_left_alone() {
    use std::io::Read;
    let home = Home::new("person");
    let mut agent = Client::start(&home, &[]);
    agent.handshake();
    // The first spawn brings this test's daemon up.
    let (up, _) = agent.call("spawn", json!({ "command": "cmd.exe /d /c echo up" }));
    agent.call("wait", json!({ "session": up["session"], "until": { "exit": true }, "timeout_ms": 20000 }));

    // The window: it speaks the daemon's protocol as a window does (spawn = 1, input = 3, kill = 5).
    let mut window = uds_windows::UnixStream::connect(home.dir.join("d.sock")).unwrap();
    let spec = json!({ "exe": "cmd.exe", "args": ["/d"], "cols": 90, "rows": 25 });
    window.write_all(&frame(1, spec.to_string().as_bytes())).unwrap();
    let mut seen = window.try_clone().unwrap();
    let (tx, rx) = channel::<(u8, Vec<u8>)>();
    std::thread::spawn(move || loop {
        let mut head = [0u8; 5];
        if seen.read_exact(&mut head).is_err() {
            break;
        }
        let mut p = vec![0u8; u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize];
        if seen.read_exact(&mut p).is_err() || tx.send((head[0], p)).is_err() {
            break;
        }
    });
    // Everything the window is sent, until `want` shows up in the output (frames of type 10).
    let window_sees = |want: &str| {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut out = String::new();
        while let Ok((ty, p)) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            if ty == 10 {
                out.push_str(&String::from_utf8_lossy(&p));
                if out.contains(want) {
                    return true;
                }
            }
        }
        false
    };
    let (ty, info) = rx.recv_timeout(Duration::from_secs(20)).expect("the daemon answers a spawn");
    assert_eq!(ty, 12, "{}", String::from_utf8_lossy(&info));
    let id = serde_json::from_slice::<Value>(&info).unwrap()["id"].as_u64().unwrap();
    assert!(window_sees(">"), "the person's shell printed its prompt");
    let listed = home.sessions();
    let mine = listed["sessions"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
    assert_eq!((mine["agent"].as_bool(), mine["windows"].as_u64()), (Some(false), Some(1)), "{mine}");
    let journal = mine["journal"].as_str().unwrap().to_string();

    // Without --drive-all: looked at with every tool, driven with none.
    let (l, _) = agent.call("sessions", json!({}));
    let theirs = l["sessions"].as_array().unwrap().iter().find(|s| s["session"] == id).unwrap();
    assert_eq!((theirs["spawned_here"].as_bool(), theirs["can_drive"].as_bool()), (Some(false), Some(false)), "{theirs}");
    for (tool, args) in [
        ("send", json!({ "session": id, "text": "echo intruder", "enter": true })),
        ("send", json!({ "session": id, "keys": ["Ctrl+C"] })),
        ("kill", json!({ "session": id })),
    ] {
        assert!(agent.refused(tool, args).contains("not started by this server"));
    }
    let (_, screen) = agent.call("screen", json!({ "session": id }));
    assert!(screen.contains("Microsoft Windows"), "{screen}");
    let shot = agent.call_raw("shot", json!({ "session": id }));
    assert_eq!(shot["isError"], false);
    agent.call("read", json!({ "session": id }));
    // The person types; the agent, only watching, sees it.
    window.write_all(&frame(3, b"echo typed-by-the-person\r")).unwrap();
    let (w, _) = agent.call("wait", json!({ "session": id, "until": { "regex": "^typed-by-the-person" }, "timeout_ms": 20000 }));
    assert_eq!(w["reason"], "regex", "{w}");
    let still = |what: &str| {
        let now = home.sessions();
        let s = now["sessions"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap_or_else(|| panic!("{what}: the person's session is gone: {now}")).clone();
        assert_eq!((s["windows"].as_u64(), s["cols"].as_u64(), s["rows"].as_u64()), (Some(1), Some(90), Some(25)), "{what}: {s}");
    };
    still("after an agent looked at it with every tool");
    // The watching server goes away: nothing happens to the session.
    assert!(agent.close());
    std::thread::sleep(Duration::from_millis(400));
    still("after the watching server left");

    // With --drive-all it may type. The person's window sees what was typed and its output.
    let mut driver = Client::start(&home, &["--drive-all"]);
    driver.handshake();
    driver.call("send", json!({ "session": id, "text": "echo driven-by-an-agent", "enter": true }));
    assert!(window_sees("driven-by-an-agent\r\n"), "the window was sent the output");
    let now = home.sessions();
    let s = now["sessions"].as_array().unwrap().iter().find(|s| s["id"] == id).unwrap().clone();
    assert_eq!((s["windows"].as_u64(), s["cols"].as_u64(), s["rows"].as_u64()), (Some(2), Some(90), Some(25)), "attached, and the size is the person's: {s}");
    driver.call("screen", json!({ "session": id }));
    driver.call("read", json!({ "session": id }));
    // That server goes too, without a kill: the person's session stays, at the person's size.
    assert!(driver.close());
    std::thread::sleep(Duration::from_millis(400));
    still("after a --drive-all server left");
    window.write_all(&frame(3, b"echo the-person-carries-on\r")).unwrap();
    assert!(window_sees("the-person-carries-on\r\n"), "the session is alive and still the window's");
    assert_eq!(resize_records(&std::fs::read(&journal).unwrap()), [(90, 25)], "the only size it ever had is the window's");
    assert!(!String::from_utf8_lossy(&std::fs::read(&journal).unwrap()).contains("intruder"));

    // The window closes its session, as a window does.
    window.write_all(&frame(5, &[])).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while home.sessions()["sessions"].as_array().is_some_and(|s| !s.is_empty()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(home.sessions()["sessions"].as_array().map(Vec::len), Some(0));
}

/// With --linger a session outlives the server that spawned it. It is still
/// an agent's: a plain window launch, which picks up the newest session
/// nobody is attached to, must not be handed it.
#[test]
fn a_lingering_session_outlives_its_server_and_no_window_adopts_it() {
    use std::io::Read;
    let home = Home::new("linger");
    let mut c = Client::start(&home, &["--linger"]);
    c.handshake();
    let (s, _) = c.call("spawn", json!({ "command": "cmd.exe /d" }));
    let id = s["session"].as_u64().unwrap();
    c.call("wait", json!({ "session": id, "until": { "regex": r">\s*$" }, "timeout_ms": 20000 }));
    // The server leaves. The daemon it started lives on, with the session:
    // it must not have been handed the client's pipes when it was started,
    // or the client's end of the server's output never reads end-of-file.
    let (left, at_its_end) = c.close_to_the_end();
    assert!(left, "the server leaves when its input closes");
    assert!(at_its_end, "the server has gone and its output still does not end: the daemon it started holds the other end of the client's pipe");
    std::thread::sleep(Duration::from_millis(1500));
    let left = home.sessions();
    let mine = left["sessions"].as_array().unwrap().iter().find(|e| e["id"] == id).unwrap_or_else(|| panic!("the lingering session ended with its server: {left}"));
    assert_eq!((mine["windows"].as_u64(), mine["agent"].as_bool()), (Some(0), Some(true)), "{mine}");

    let reply = |payload: &[u8]| {
        let mut sock = uds_windows::UnixStream::connect(home.dir.join("d.sock")).unwrap();
        sock.write_all(&frame(2, payload)).unwrap();
        let mut head = [0u8; 5];
        sock.read_exact(&mut head).unwrap();
        let mut p = vec![0u8; u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize];
        sock.read_exact(&mut p).unwrap();
        (head[0], String::from_utf8_lossy(&p).into_owned(), sock)
    };
    // What a plain launch asks: attach to session 0, "the newest one without a window".
    let (ty, why, _) = reply(&0u64.to_le_bytes());
    assert_eq!((ty, why.as_str()), (16, "no session to attach"), "an agent's session was offered to a window");
    // A new server sees it as someone else's: it may look, not drive.
    let mut later = Client::start(&home, &[]);
    later.handshake();
    assert!(later.refused("kill", json!({ "session": id })).contains("--drive-all"));
    let (_, screen) = later.call("screen", json!({ "session": id }));
    assert!(screen.contains("Microsoft Windows"), "{screen}");
    // By number it can be attached to (that is what `proteus --attach N` does), and ended.
    let (ty, _, mut sock) = reply(&id.to_le_bytes());
    assert_eq!(ty, 12);
    sock.write_all(&frame(5, &[])).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while home.sessions()["sessions"].as_array().is_some_and(|s| !s.is_empty()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(home.sessions()["sessions"].as_array().map(Vec::len), Some(0));
}

// ---------------------------------------------------------------------------
// Measurements. Not run by `cargo test`; each prints lines starting MEASURE.
//   cargo test --release --test mcp measure -- --ignored --nocapture --test-threads=1

extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> isize;
    fn GetProcessTimes(h: isize, created: *mut u64, exited: *mut u64, kernel: *mut u64, user: *mut u64) -> i32;
    fn QueryProcessCycleTime(h: isize, cycles: *mut u64) -> i32;
    fn CloseHandle(h: isize) -> i32;
}

/// CPU a process has used so far: milliseconds (counted in 15.6 ms steps) and processor cycles (exact).
fn cpu(pid: u32) -> (f64, u64) {
    unsafe {
        let h = OpenProcess(0x1000, 0, pid); // PROCESS_QUERY_LIMITED_INFORMATION
        assert!(h != 0, "cannot open process {pid}");
        let (mut c, mut e, mut k, mut u, mut cycles) = (0u64, 0u64, 0u64, 0u64, 0u64);
        GetProcessTimes(h, &mut c, &mut e, &mut k, &mut u);
        QueryProcessCycleTime(h, &mut cycles);
        CloseHandle(h);
        ((k + u) as f64 / 10_000.0, cycles)
    }
}

/// Processor cycles in one millisecond of a busy core, found by spinning for 200 ms.
fn cycles_per_ms() -> f64 {
    let me = std::process::id();
    let (c0, t0) = (cpu(me).1, Instant::now());
    let mut x = 0u64;
    while t0.elapsed() < Duration::from_millis(200) {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
    }
    std::hint::black_box(x);
    (cpu(me).1 - c0) as f64 / t0.elapsed().as_secs_f64() / 1000.0
}

fn summary(samples: &mut [f64]) -> Value {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let at = |p: f64| samples[((samples.len() - 1) as f64 * p).round() as usize];
    let r = |x: f64| (x * 100.0).round() / 100.0;
    json!({ "n": samples.len(), "min": r(samples[0]), "p50": r(at(0.5)), "p95": r(at(0.95)), "max": r(samples[samples.len() - 1]) })
}

fn timed<T>(f: impl FnOnce() -> T) -> (T, f64) {
    let t = Instant::now();
    let v = f();
    (v, t.elapsed().as_secs_f64() * 1000.0)
}

/// Read everything pending in one mode, a page at a time, and add it up.
fn drain(c: &mut Client, id: u64, mode: &str, from: Option<u64>, max_bytes: u64) -> Value {
    let (mut raw, mut out, mut calls, mut lines, mut shown, mut errors, mut warnings) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
    let mut cursor = from;
    let t = Instant::now();
    loop {
        let mut args = json!({ "session": id, "mode": mode, "max_bytes": max_bytes });
        if let Some(cur) = cursor {
            args["cursor"] = json!(cur);
        }
        let (f, _) = c.call("read", args);
        raw += f["raw_bytes"].as_u64().unwrap();
        out += f["lean_bytes"].as_u64().unwrap();
        lines += f["lines"].as_u64().unwrap_or(0);
        shown += f["lines_shown"].as_u64().unwrap_or(0);
        errors += f["errors"].as_u64().unwrap_or(0);
        warnings += f["warnings"].as_u64().unwrap_or(0);
        calls += 1;
        cursor = f["cursor"].as_u64();
        if f["more_bytes"] == 0 {
            break;
        }
    }
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    json!({ "raw_bytes": raw, "bytes": out, "saved_pct": ((1.0 - out as f64 / raw.max(1) as f64) * 1000.0).round() / 10.0, "calls": calls, "lines": lines, "lines_shown": shown, "errors": errors, "warnings": warnings, "ms": ms.round() })
}

fn run_to_exit(c: &mut Client, command: &str, args: &[&str], cwd: &std::path::Path) -> u64 {
    let (s, _) = c.call("spawn", json!({ "command": command, "args": args, "cwd": cwd.to_string_lossy() }));
    let id = s["session"].as_u64().unwrap();
    let (w, _) = c.call("wait", json!({ "session": id, "until": { "exit": true }, "timeout_ms": 300000 }));
    assert_eq!(w["reason"], "exit", "{w}");
    id
}

/// Lean against raw on three real captures made here, now.
#[test]
#[ignore]
fn measure_lean_against_raw() {
    let home = Home::new("m-lean");
    let mut c = Client::start(&home, &[]);
    c.handshake();
    let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    // A small crate with 25 warnings and one error.
    let krate = home.dir.join("leanfix");
    std::fs::create_dir_all(krate.join("src")).unwrap();
    std::fs::write(krate.join("Cargo.toml"), "[package]\nname = \"leanfix\"\nversion = \"0.1.0\"\nedition = \"2021\"\n").unwrap();
    let mut src = String::from("use std::collections::HashMap;\n\n");
    for i in 0..24 {
        src.push_str(&format!("fn helper_{i}(x: i32) -> i32 {{\n    let scratch = x;\n    x + {i}\n}}\n\n"));
        if i == 11 {
            src.push_str("fn broken() -> u32 {\n    let total: u32 = \"seven\";\n    total\n}\n\n");
        }
    }
    src.push_str("fn main() {\n    println!(\"{}\", broken());\n}\n");
    std::fs::write(krate.join("src").join("main.rs"), src).unwrap();
    let pwsh_bar = "1..200 | % { [Console]::Write(\"`rDownloading crate $_ of 200 [\" + ('#' * [int]($_/5)).PadRight(40) + \"] $([int]($_/2))%\"); Start-Sleep -Milliseconds 8 }; [Console]::WriteLine(); 'done: 200 crates'";
    let cases: [(&str, &str, Vec<&str>, &std::path::Path); 3] = [
        ("cargo build --color always (25 warnings, 1 error)", "cargo", vec!["build", "--color", "always"], &krate),
        ("git --no-pager log -p --color=always (this repository)", "git", vec!["--no-pager", "log", "-p", "--color=always"], &repo),
        ("pwsh: a progress bar redrawn 200 times", "pwsh.exe", vec!["-NoLogo", "-NoProfile", "-Command", pwsh_bar], &repo),
    ];
    for (name, command, args, cwd) in cases {
        let id = run_to_exit(&mut c, command, &args, cwd);
        let lean = drain(&mut c, id, "lean", None, 24_000);
        let plain = drain(&mut c, id, "plain", Some(0), 1 << 20);
        println!("MEASURE {}", json!({ "what": "lean", "capture": name, "lean": lean, "plain": plain }));
    }
}

/// From a line being printed to `wait` returning it, and what a wait costs while nothing happens.
#[test]
#[ignore]
fn measure_wait() {
    let home = Home::new("m-wait");
    let mut owner = Client::start(&home, &[]);
    owner.handshake();
    let per_ms = cycles_per_ms();

    // A program that prints the time it is, to a tenth of a microsecond, four times a second.
    let ticker = "1..41 | % { Start-Sleep -Milliseconds 250; [Console]::Out.WriteLine('TICK ' + [DateTime]::UtcNow.Ticks) }";
    for who in ["spawned here (read from its connection)", "another server's (read from its journal)"] {
        let (s, _) = owner.call("spawn", json!({ "command": "pwsh.exe", "args": ["-NoLogo", "-NoProfile", "-Command", ticker] }));
        let id = s["session"].as_u64().unwrap();
        let mut guest = Client::start(&home, &[]);
        guest.handshake();
        let c = if who.starts_with("spawned") { &mut owner } else { &mut guest };
        let mut ms = Vec::new();
        let mut cursor = 0u64;
        loop {
            let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": r"^TICK (\d+)" }, "timeout_ms": 30000, "cursor": cursor }));
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as f64 / 100.0;
            if w["reason"] != "regex" {
                break;
            }
            cursor = w["cursor"].as_u64().unwrap();
            let printed = w["groups"][0].as_str().unwrap().parse::<f64>().unwrap() - 621_355_968_000_000_000.0;
            ms.push((now - printed) / 10_000.0);
        }
        // The first is taken while the shell is still starting; leave it out.
        let mut steady = ms[1..].to_vec();
        println!("MEASURE {}", json!({ "what": "wait wake-up: ms from the program printing a line to wait returning it", "session": who, "ms": summary(&mut steady) }));
    }

    // Thirty seconds waiting on a silent shell.
    let (s, _) = owner.call("spawn", json!({ "command": "cmd.exe /d" }));
    let id = s["session"].as_u64().unwrap();
    owner.call("wait", json!({ "session": id, "until": { "regex": r">\s*$" }, "timeout_ms": 20000 }));
    owner.call("wait", json!({ "session": id, "until": { "quiet_ms": 1000 }, "timeout_ms": 20000 }));
    let mut guest = Client::start(&home, &[]);
    guest.handshake();
    let daemon = home.sessions()["pid"].as_u64().unwrap() as u32;
    // What the daemon and an idle server cost with nobody waiting, for comparison.
    let (s0, d0, t0) = (cpu(owner.child.id()), cpu(daemon), Instant::now());
    std::thread::sleep(Duration::from_secs(30));
    let secs = t0.elapsed().as_secs_f64();
    let idle = |a: (f64, u64), b: (f64, u64)| json!({ "cpu_ms_by_cycles": (((b.1 - a.1) as f64 / per_ms) * 100.0).round() / 100.0 });
    println!("MEASURE {}", json!({ "what": "cpu with no wait in progress (baseline)", "seconds": (secs * 10.0).round() / 10.0, "server": idle(s0, cpu(owner.child.id())), "daemon": idle(d0, cpu(daemon)) }));
    for who in ["spawned here (read from its connection)", "another server's (read from its journal)"] {
        let c = if who.starts_with("spawned") { &mut owner } else { &mut guest };
        let server = c.child.id();
        let (s0, d0, t0) = (cpu(server), cpu(daemon), Instant::now());
        let (w, _) = c.call("wait", json!({ "session": id, "until": { "regex": "never printed" }, "timeout_ms": 30000, "cursor": 0 }));
        assert_eq!(w["reason"], "timeout");
        let secs = t0.elapsed().as_secs_f64();
        let (s1, d1) = (cpu(server), cpu(daemon));
        let share = |a: (f64, u64), b: (f64, u64)| {
            let busy_ms = (b.1 - a.1) as f64 / per_ms;
            json!({ "cpu_ms_by_the_clock": b.0 - a.0, "cpu_ms_by_cycles": (busy_ms * 100.0).round() / 100.0, "pct_of_one_core": (busy_ms / (secs * 1000.0) * 100_000.0).round() / 1000.0 })
        };
        println!("MEASURE {}", json!({ "what": "cpu while a wait sleeps on a silent session", "session": who, "seconds": (secs * 10.0).round() / 10.0, "server": share(s0, s1), "daemon": share(d0, d1) }));
    }
}

/// A person's session that has lost its window is picked up by their next
/// plain launch ("attach to the newest session without a window"). What
/// happens to that while a --drive-all server is attached to it? Prints what
/// the daemon answered; asserts nothing about the answer in the middle.
#[test]
#[ignore]
fn measure_a_windowless_session_held_by_a_driving_server() {
    use std::io::Read;
    let home = Home::new("m-held");
    let mut driver = Client::start(&home, &["--drive-all"]);
    driver.handshake();
    let (up, _) = driver.call("spawn", json!({ "command": "cmd.exe /d /c echo up" }));
    driver.call("wait", json!({ "session": up["session"], "until": { "exit": true }, "timeout_ms": 20000 }));
    // One frame to the daemon and its first frame back, on a connection of its own.
    let ask = |ty: u8, payload: &[u8]| {
        let mut sock = uds_windows::UnixStream::connect(home.dir.join("d.sock")).unwrap();
        sock.write_all(&frame(ty, payload)).unwrap();
        let mut head = [0u8; 5];
        sock.read_exact(&mut head).unwrap();
        let mut p = vec![0u8; u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize];
        sock.read_exact(&mut p).unwrap();
        (head[0], String::from_utf8_lossy(&p).into_owned(), sock)
    };
    let windows = |id: u64| home.sessions()["sessions"].as_array().unwrap().iter().find(|s| s["id"] == id).map(|s| s["windows"].as_u64().unwrap());
    // A window starts a shell (spawn = 1) and goes away without ending it, as a window that is detached does.
    let (ty, info, window) = ask(1, json!({ "exe": "cmd.exe", "args": ["/d"], "cols": 90, "rows": 25 }).to_string().as_bytes());
    assert_eq!(ty, 12, "{info}");
    let id = serde_json::from_str::<Value>(&info).unwrap()["id"].as_u64().unwrap();
    std::thread::sleep(Duration::from_millis(3500));
    drop(window);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(windows(id), Some(0), "the session stays, with no window");
    // What a plain launch asks (attach = 2, session 0).
    let offered = |what: &str| {
        let (ty, said, sock) = ask(2, &0u64.to_le_bytes());
        drop(sock);
        std::thread::sleep(Duration::from_millis(300));
        println!("{what}: {}", if ty == 12 { "offered to a plain launch".to_string() } else { format!("not offered ({said})") });
        ty == 12
    };
    let before = offered("no server attached");
    driver.call("send", json!({ "session": id, "keys": ["Space", "Backspace"] }));
    let held = windows(id);
    let during = offered("a --drive-all server attached (it typed once)");
    assert!(driver.close());
    std::thread::sleep(Duration::from_millis(500));
    let after = offered("after that server left");
    println!("MEASURE {}", json!({ "what": "a person's windowless session and a plain launch", "offered_before": before, "windows_while_a_driving_server_holds_it": held,
        "offered_while_held": during, "offered_after_it_left": after }));
    let (ty, _, mut sock) = ask(2, &id.to_le_bytes());
    assert_eq!(ty, 12);
    sock.write_all(&frame(5, &[])).unwrap();
    std::thread::sleep(Duration::from_millis(500));
}

/// `screen`, `read`, `shot` and typing against 1 MB and 20 MB of history.
#[test]
#[ignore]
fn measure_big_sessions() {
    let home = Home::new("m-big");
    let mut owner = Client::start(&home, &[]);
    owner.handshake();
    for mb in [1usize, 20] {
        // Coloured text like a long build log.
        let file = home.dir.join(format!("big-{mb}.txt"));
        let mut text = String::with_capacity((mb << 20) + 200);
        let mut i = 0;
        while text.len() < mb << 20 {
            text.push_str(&format!("{i:07} \x1b[3{}mthe quick brown fox jumps over the lazy dog\x1b[0m 0123456789 abcdefghijklmnopqrstuvwxyz\r\n", 1 + i % 6));
            i += 1;
        }
        std::fs::write(&file, &text).unwrap();
        let (s, _) = owner.call("spawn", json!({ "command": "cmd.exe /d", "cwd": home.dir.to_string_lossy() }));
        let id = s["session"].as_u64().unwrap();
        owner.call("wait", json!({ "session": id, "until": { "regex": r">\s*$" }, "timeout_ms": 20000 }));
        let (_, flood_ms) = timed(|| {
            owner.call("send", json!({ "session": id, "text": format!("type big-{mb}.txt & echo FLOOD-OVER"), "enter": true }));
            let (w, _) = owner.call("wait", json!({ "session": id, "until": { "regex": "^FLOOD-OVER" }, "timeout_ms": 300000 }));
            assert_eq!(w["reason"], "regex", "{w}");
        });
        owner.call("wait", json!({ "session": id, "until": { "quiet_ms": 500 }, "timeout_ms": 20000 }));
        let (listed, _) = owner.call("sessions", json!({}));
        let entry = listed["sessions"].as_array().unwrap().iter().find(|e| e["session"] == id).unwrap().clone();
        let journal_bytes = entry["journal_bytes"].as_u64().unwrap();
        let r = |x: f64| (x * 100.0).round() / 100.0;
        let mut out = json!({ "what": "big session", "journal_bytes": journal_bytes, "flood_mb_per_s": r((journal_bytes as f64 / 1e6) / (flood_ms / 1000.0)) });

        // The server that spawned it: the bytes are in its memory.
        let ((f, _), cold) = timed(|| owner.call("screen", json!({ "session": id })));
        assert!(f["cursor"].as_u64().unwrap() > (mb as u64) << 20);
        let mut warm: Vec<f64> = (0..20).map(|_| timed(|| owner.call("screen", json!({ "session": id }))).1).collect();
        out["owner_screen_ms"] = json!({ "first": r(cold), "again": summary(&mut warm) });
        let age = entry["age_s"].as_f64().unwrap();
        let _ = age;
        // A moment after the end: the whole journal is replayed from disk.
        let (_, back) = timed(|| owner.call("screen", json!({ "session": id, "at": 1.0e6 })));
        out["owner_screen_at_ms"] = json!(r(back));
        let (_, lean_ms) = timed(|| owner.call("read", json!({ "session": id, "cursor": 0 })));
        out["owner_read_lean_whole_ms"] = json!(r(lean_ms));
        let (shot, shot_cold) = timed(|| owner.call_raw("shot", json!({ "session": id })));
        let mut shot_warm: Vec<f64> = (0..10).map(|_| timed(|| owner.call_raw("shot", json!({ "session": id }))).1).collect();
        let facts: Value = serde_json::from_str(shot["content"][0]["text"].as_str().unwrap()).unwrap();
        let (exact, exact_ms) = timed(|| owner.call_raw("shot", json!({ "session": id, "exact": true })));
        let exact: Value = serde_json::from_str(exact["content"][0]["text"].as_str().unwrap()).unwrap();
        out["owner_shot_ms"] = json!({ "first": r(shot_cold), "again": summary(&mut shot_warm), "png_bytes": facts["png_bytes"], "size": format!("{}x{}", facts["width"], facts["height"]), "exact_png_bytes": exact["png_bytes"], "exact_ms": r(exact_ms) });
        if mb == 1 {
            let image = owner.call_raw("shot", json!({ "session": id }));
            let (w, h, px) = decode_png(image["content"][1]["data"].as_str().unwrap());
            let out_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("forge").join("out");
            std::fs::create_dir_all(&out_dir).unwrap();
            let file = std::io::BufWriter::new(std::fs::File::create(out_dir.join("organs-shot.png")).unwrap());
            let mut enc = png::Encoder::new(file, w, h);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&px).unwrap();
        }
        let mut typing: Vec<f64> = (0..50).map(|_| timed(|| owner.call("send", json!({ "session": id, "keys": ["Space", "Backspace"] }))).1).collect();
        out["owner_send_ms"] = summary(&mut typing);

        // A server that did not: the journal on disk.
        let mut guest = Client::start(&home, &[]);
        guest.handshake();
        let (_, cold) = timed(|| guest.call("screen", json!({ "session": id })));
        let mut warm: Vec<f64> = (0..20).map(|_| timed(|| guest.call("screen", json!({ "session": id }))).1).collect();
        out["guest_screen_ms"] = json!({ "first": r(cold), "again": summary(&mut warm) });
        let (_, lean_ms) = timed(|| guest.call("read", json!({ "session": id })));
        out["guest_read_lean_whole_ms"] = json!(r(lean_ms));
        drop(guest);

        // Typing from outside. The command line tool attaches, and so replays, every time...
        let mut cli: Vec<f64> = (0..3)
            .map(|_| timed(|| assert!(home.command(&["send", &id.to_string(), " "]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success())).1)
            .collect();
        out["cli_send_ms"] = summary(&mut cli);
        // ...a server allowed to drive attaches once and keeps the connection.
        let mut driver = Client::start(&home, &["--drive-all"]);
        driver.handshake();
        let (_, first) = timed(|| driver.call("send", json!({ "session": id, "keys": ["Space"] })));
        let mut later: Vec<f64> = (0..50).map(|_| timed(|| driver.call("send", json!({ "session": id, "keys": ["Backspace", "Space"] }))).1).collect();
        out["drive_all_send_ms"] = json!({ "first": r(first), "again": summary(&mut later) });
        drop(driver);
        let mut list: Vec<f64> = (0..50).map(|_| timed(|| owner.call("sessions", json!({}))).1).collect();
        out["sessions_ms"] = summary(&mut list);
        println!("MEASURE {out}");
        owner.call("kill", json!({ "session": id }));
    }
}
