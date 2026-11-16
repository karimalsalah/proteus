//! What programs tell the terminal out of band, read from the byte stream
//! before the parser (which discards all of it): where a prompt, a command
//! and its output begin and end and with what exit status (OSC 133 / 633),
//! the working directory (OSC 7, 9;9), progress (OSC 9;4), and what an agent
//! is doing (OSC 7501, the Program Status Protocol, draft revision 0.4 of
//! 2026-10-10: https://control-codes.page/proposals/osc-7501/).
//!
//! The scanner is a pure state machine: it never changes the bytes, copes
//! with a sequence split across reads, and reports the exact stream offset of
//! everything it finds, so "the output of command N" is a byte range.
//!
//! Anything a program prints can forge a mark. Marks that carry this
//! session's nonce (given to the shell integration through the environment)
//! are `trusted`; the rest are reported but must not be believed.
#![allow(dead_code)] // wired into the session reader after the process-sensing merge

use std::collections::BTreeMap;

/// The longest sequence looked at (the status protocol's own hard cap).
pub const MAX_SEQ: usize = 4096;
/// Records kept per terminal (the protocol's cap).
pub const MAX_RECORDS: usize = 256;

/// The answer to `OSC 7501 ; ? ST`. A program that receives it starts
/// reporting; one that does not, never does.
pub const STATUS_REPLY: &[u8] = b"\x1b]7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth\x1b\\";

/// Shell integration for PowerShell (5.1 and 7), loaded into a session's
/// shell after the person's own profile. It wraps whatever prompt function
/// is already there, so prompt themes keep working, and it wraps the line
/// reader, which is how the command text comes from the shell itself and
/// never from keystrokes. Every mark carries the session's nonce.
///
/// What it reports: the previous command's status (133;D), a new prompt
/// (133;A), the working directory (9;9), the end of the prompt (133;B), and,
/// when a line is accepted, the command line (633;E) and the start of its
/// output (133;C). The status is the shell's own account: 0 if the last
/// statement succeeded, else the native exit code if there is one, else 1.
pub const PWSH_INTEGRATION: &str = r#"
if (-not $Global:__proteus) {
  $Global:__proteus = @{ n = $env:PROTEUS_NONCE; prompt = $function:prompt; read = $function:PSConsoleHostReadLine; last = -1; ran = $false }
  function Global:prompt {
    $ok = $?; $code = $global:LASTEXITCODE
    $s = $Global:__proteus; $e = [char]27; $b = [char]7; $o = ''
    $h = Get-History -Count 1
    $id = if ($h) { $h.Id } else { 0 }
    if ($s.ran) {
      if ($id -ne $s.last) {
        $st = if ($ok) { 0 } elseif ($code -is [int] -and $code -ne 0) { $code } else { 1 }
        $o += "$e]133;D;$st;n=$($s.n)$b"
      } else { $o += "$e]133;D;n=$($s.n)$b" }
    }
    $s.last = $id; $s.ran = $false
    $o += "$e]133;A;n=$($s.n)$b$e]9;9;$($PWD.ProviderPath)$b"
    $global:LASTEXITCODE = $code
    $p = if ($s.prompt) { & $s.prompt } else { "PS $PWD> " }
    "$o$p$e]133;B;n=$($s.n)$b"
  }
  if ($Global:__proteus.read) {
    function Global:PSConsoleHostReadLine {
      $s = $Global:__proteus
      $line = & $s.read
      $e = [char]27; $b = [char]7; $t = New-Object System.Text.StringBuilder
      foreach ($c in "$line".ToCharArray()) {
        $v = [int]$c
        if ($c -eq '\') { [void]$t.Append('\\') }
        elseif ($v -lt 32 -or $c -eq ';') { [void]$t.Append(('\x{0:x2}' -f $v)) }
        else { [void]$t.Append($c) }
      }
      $s.ran = $true
      [Console]::Write("$e]633;E;$t;$($s.n)$b$e]133;C;n=$($s.n)$b")
      $line
    }
  }
}
"#;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Blocked,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

/// One program status report. `state` is None for `state=clear`.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Status {
    pub id: String,
    pub state: Option<State>,
    pub kind: Option<Kind>,
    pub progress: Option<u8>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub msg: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mark {
    /// A prompt is being drawn (133;A).
    PromptStart,
    /// The prompt is done; what follows is the person's input (133;B).
    InputStart,
    /// The command has been accepted; what follows is its output (133;C).
    OutputStart,
    /// The command finished, with this status if the shell could say (133;D).
    CommandEnd(Option<i32>),
    /// The command line as the shell itself reports it (633;E). Never keys.
    CommandLine(String),
    Cwd(String),
    /// OSC 9;4: 0 none, 1 normal, 2 error, 3 indeterminate, 4 paused.
    Progress { state: u8, percent: Option<u8> },
    StatusQuery,
    Status(Status),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// Stream offset of the sequence's first byte (the ESC) and one past its last.
    pub start: u64,
    pub end: u64,
    pub mark: Mark,
    /// Carried this session's nonce.
    pub trusted: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum S {
    Ground,
    Esc,
    Osc,
    OscEsc,
}

pub struct Scanner {
    s: S,
    buf: Vec<u8>,
    start: u64,
    pos: u64,
    overflow: bool,
    nonce: Option<String>,
}

impl Scanner {
    pub fn new(nonce: Option<String>) -> Self {
        Self { s: S::Ground, buf: Vec::with_capacity(256), start: 0, pos: 0, overflow: false, nonce: nonce.filter(|n| !n.is_empty()) }
    }

    /// Bytes seen so far.
    pub fn offset(&self) -> u64 {
        self.pos
    }

    /// Look at the next bytes of the stream; anything found is added to `out`.
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Found>) {
        for &b in bytes {
            self.pos += 1;
            match self.s {
                S::Ground => {
                    if b == 0x1b {
                        self.s = S::Esc;
                        self.start = self.pos - 1;
                    }
                }
                S::Esc => match b {
                    b']' => {
                        self.s = S::Osc;
                        self.buf.clear();
                        self.overflow = false;
                    }
                    0x1b => self.start = self.pos - 1,
                    _ => self.s = S::Ground,
                },
                S::Osc => match b {
                    0x07 => self.finish(out),
                    0x1b => self.s = S::OscEsc,
                    // CAN and SUB abort a sequence.
                    0x18 | 0x1a => self.s = S::Ground,
                    _ => {
                        if self.buf.len() < MAX_SEQ {
                            self.buf.push(b);
                        } else {
                            self.overflow = true;
                        }
                    }
                },
                S::OscEsc => match b {
                    b'\\' => self.finish(out),
                    // An escape inside a sequence abandons it and starts anew.
                    b']' => {
                        self.s = S::Osc;
                        self.start = self.pos - 2;
                        self.buf.clear();
                        self.overflow = false;
                    }
                    0x1b => self.start = self.pos - 1,
                    _ => self.s = S::Ground,
                },
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<Found>) {
        self.s = S::Ground;
        if self.overflow {
            return;
        }
        let Ok(text) = std::str::from_utf8(&self.buf) else { return };
        if let Some((mark, trusted)) = parse(text, self.nonce.as_deref()) {
            out.push(Found { start: self.start, end: self.pos, mark, trusted });
        }
    }
}

/// Does any of these options carry the session's nonce (`n=<nonce>`)?
fn has<'a>(nonce: Option<&str>, mut fields: impl Iterator<Item = &'a str>) -> bool {
    nonce.is_some_and(|n| fields.any(|f| f.strip_prefix("n=") == Some(n)))
}

fn parse(text: &str, nonce: Option<&str>) -> Option<(Mark, bool)> {
    let (num, rest) = text.split_once(';').unwrap_or((text, ""));
    match num {
        "133" | "633" => {
            let mut f = rest.split(';');
            let letter = f.next()?;
            let mark = match letter {
                "A" => Mark::PromptStart,
                "B" => Mark::InputStart,
                "C" => Mark::OutputStart,
                "D" => {
                    let rest: Vec<&str> = f.clone().collect();
                    let status = rest.first().and_then(|s| s.trim().parse::<i32>().ok());
                    return Some((Mark::CommandEnd(status), has(nonce, rest.into_iter())));
                }
                // 633;E;<escaped command line>[;<nonce>]
                "E" if num == "633" => {
                    let line = unescape_633(f.next()?);
                    let trusted = nonce.is_some_and(|n| f.next() == Some(n));
                    return Some((Mark::CommandLine(line), trusted));
                }
                // 633;P;Cwd=<path>
                "P" if num == "633" => {
                    let cwd = f.next()?.strip_prefix("Cwd=")?;
                    return Some((Mark::Cwd(unescape_633(cwd)), false));
                }
                _ => return None,
            };
            Some((mark, has(nonce, f)))
        }
        "9" => {
            let (sub, arg) = rest.split_once(';').unwrap_or((rest, ""));
            match sub {
                "9" if !arg.is_empty() => Some((Mark::Cwd(arg.trim_matches('"').to_string()), false)),
                "4" => {
                    let mut f = arg.split(';');
                    let state = f.next()?.trim().parse::<u8>().ok().filter(|s| *s <= 4)?;
                    let percent = f.next().and_then(|p| p.trim().parse::<u8>().ok()).filter(|p| *p <= 100);
                    Some((Mark::Progress { state, percent }, false))
                }
                _ => None,
            }
        }
        "7" => {
            let path = rest.strip_prefix("file://")?;
            let path = &path[path.find('/')?..];
            let mut p = percent_decode(path);
            // file:///C:/x names C:/x, not /C:/x.
            let b = p.as_bytes();
            if b.len() >= 3 && b[0] == b'/' && b[2] == b':' && b[1].is_ascii_alphabetic() {
                p.remove(0);
            }
            Some((Mark::Cwd(p), false))
        }
        "7501" => {
            if rest.trim() == "?" {
                return Some((Mark::StatusQuery, false));
            }
            parse_status(rest).map(|s| (Mark::Status(s), false))
        }
        _ => None,
    }
}

/// VS Code's escaping for 633;E: `\\` is a backslash and `\xHH` a byte.
fn unescape_633(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            if b[i + 1] == b'\\' {
                out.push(b'\\');
                i += 2;
                continue;
            }
            if b[i + 1] == b'x' {
                if let Some(v) = s.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    out.push(v);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            if let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Standard base64, padding optional. None if anything is out of place.
fn base64(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    (s.len() % 4 != 1).then_some(out)
}

/// Decode a label: valid base64 of UTF-8 with no control character at all,
/// within the size limits. None means the whole report is refused.
fn label(value: &str, max_encoded: usize, max_decoded: usize) -> Option<String> {
    if value.len() > max_encoded {
        return None;
    }
    let bytes = base64(value)?;
    if bytes.len() > max_decoded {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    (!text.chars().any(char::is_control)).then_some(text)
}

/// A report by the rules of the protocol: a malformed pair is skipped, an
/// unknown key ignored, a repeated key takes its last value; a report that
/// breaks a size limit or carries a bad label is refused whole.
fn parse_status(rest: &str) -> Option<Status> {
    let mut pairs: BTreeMap<&str, &str> = BTreeMap::new();
    for pair in rest.split(':') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        let key_ok = !k.is_empty() && k.len() <= 16 && k.bytes().all(|c| c.is_ascii_lowercase());
        let value_ok = v.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.,+/=-".contains(&c));
        if key_ok && value_ok {
            pairs.insert(k, v);
        }
    }
    let mut s = Status::default();
    s.state = match *pairs.get("state")? {
        "idle" => Some(State::Idle),
        "working" => Some(State::Working),
        "blocked" => Some(State::Blocked),
        "done" => Some(State::Done),
        "error" => Some(State::Error),
        "clear" => None,
        _ => return None,
    };
    let clear = s.state.is_none();
    if let Some(id) = pairs.get("id") {
        let segs: Vec<&str> = id.split('/').collect();
        if id.len() > 128 || segs.len() > 8 || segs.iter().any(|g| g.is_empty() || g.len() > 32) {
            return None;
        }
        s.id = id.to_string();
    }
    if clear {
        return Some(s);
    }
    if s.state == Some(State::Blocked) {
        s.kind = match pairs.get("kind").copied() {
            Some("permission") => Some(Kind::Permission),
            Some("question") => Some(Kind::Question),
            Some("auth") => Some(Kind::Auth),
            _ => None,
        };
    }
    if matches!(s.state, Some(State::Working | State::Blocked)) {
        s.progress = pairs.get("progress").and_then(|p| p.parse::<u8>().ok()).filter(|p| *p <= 100);
    }
    if let Some(app) = pairs.get("app") {
        if app.len() > 32 || !app.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.+-".contains(&c)) {
            return None;
        }
        s.app = Some(app.to_string());
    }
    if let Some(t) = pairs.get("title") {
        s.title = Some(label(t, 256, 192)?);
    }
    if let Some(m) = pairs.get("msg") {
        s.msg = Some(label(m, 2732, 2048)?);
    }
    Some(s)
}

/// Text from a report, made safe to draw outside the grid: direction
/// overrides and invisible formatting are removed.
pub fn display_safe(s: &str) -> String {
    s.chars().filter(|c| !matches!(*c as u32, 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2069 | 0xFEFF | 0x061C)).collect()
}

/// The status records of one terminal.
#[derive(Default)]
pub struct StatusTable {
    records: BTreeMap<String, (Status, u64)>,
    clock: u64,
}

impl StatusTable {
    /// Apply a report: it replaces its record completely; `clear` removes
    /// the record and everything beneath it (everything, with no id).
    pub fn apply(&mut self, s: Status) {
        self.clock += 1;
        if s.state.is_none() {
            let prefix = format!("{}/", s.id);
            self.records.retain(|id, _| !(s.id.is_empty() || *id == s.id || id.starts_with(&prefix)));
            return;
        }
        if !self.records.contains_key(&s.id) && self.records.len() >= MAX_RECORDS {
            // Full: the record updated longest ago goes.
            if let Some(oldest) = self.records.iter().min_by_key(|(_, (_, t))| *t).map(|(id, _)| id.clone()) {
                self.records.remove(&oldest);
            }
        }
        self.records.insert(s.id.clone(), (s, self.clock));
    }

    /// The program exited or a new prompt began: nothing is working or
    /// waiting any more. What finished or failed stays until seen.
    pub fn drop_active(&mut self) {
        self.records.retain(|_, (s, _)| !matches!(s.state, Some(State::Working | State::Blocked | State::Idle)));
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// The record that most needs a person: blocked, then error, working,
    /// done, idle; the most recent among equals.
    pub fn foremost(&self) -> Option<&Status> {
        let rank = |s: &Status| match s.state {
            Some(State::Blocked) => 4,
            Some(State::Error) => 3,
            Some(State::Working) => 2,
            Some(State::Done) => 1,
            _ => 0,
        };
        self.records.values().max_by_key(|(s, t)| (rank(s), *t)).map(|(s, _)| s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(bytes: &[u8], nonce: Option<&str>) -> Vec<Found> {
        let mut out = Vec::new();
        Scanner::new(nonce.map(String::from)).feed(bytes, &mut out);
        out
    }

    #[test]
    fn marks_are_found_with_exact_offsets_however_the_stream_is_cut() {
        let stream = b"PS> \x1b]133;A\x07\x1b]9;9;C:\\src\x07\x1b]133;B\x07cargo test\r\n\x1b]633;E;cargo test \\x3b echo \\\\;k7\x07\x1b]133;C;n=k7\x1b\\running\r\n\x1b]133;D;101;n=k7\x07";
        let whole = scan(stream, Some("k7"));
        let kinds: Vec<&Mark> = whole.iter().map(|f| &f.mark).collect();
        assert_eq!(
            kinds,
            vec![
                &Mark::PromptStart,
                &Mark::Cwd("C:\\src".into()),
                &Mark::InputStart,
                &Mark::CommandLine("cargo test ; echo \\".into()),
                &Mark::OutputStart,
                &Mark::CommandEnd(Some(101)),
            ]
        );
        for f in &whole {
            assert_eq!(stream[f.start as usize], 0x1b);
            assert!(matches!(stream[f.end as usize - 1], 0x07 | b'\\'));
        }
        // The command's output is exactly the bytes between C's end and D's start.
        assert_eq!(&stream[whole[4].end as usize..whole[5].start as usize], b"running\r\n");
        // Cut anywhere, one byte at a time: the same marks at the same offsets.
        let mut s = Scanner::new(Some("k7".into()));
        let mut bytewise = Vec::new();
        for b in stream {
            s.feed(&[*b], &mut bytewise);
        }
        assert_eq!(bytewise, whole);
        assert_eq!(s.offset(), stream.len() as u64);
    }

    #[test]
    fn only_marks_with_the_nonce_are_trusted() {
        let f = scan(b"\x1b]133;D;0\x07\x1b]133;D;0;n=wrong\x07\x1b]133;D;0;n=k7\x07\x1b]633;E;rm -rf;k7\x07\x1b]633;E;rm -rf;zz\x07", Some("k7"));
        assert_eq!(f.iter().map(|x| x.trusted).collect::<Vec<_>>(), vec![false, false, true, true, false]);
        // With no nonce set nothing is trusted, and printed text cannot guess one.
        assert!(scan(b"\x1b]133;D;0;n=\x07\x1b]133;A;n=k7\x07", None).iter().all(|x| !x.trusted));
        let mut rng = 0x2545_F491u32;
        let mut forged = 0;
        for _ in 0..10_000 {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            let guess = format!("\x1b]133;D;0;n={:x}\x07", rng % 1_000_000);
            forged += scan(guess.as_bytes(), Some("3f9c2a7be1d04586")).iter().filter(|x| x.trusted).count();
        }
        assert_eq!(forged, 0);
    }

    #[test]
    fn broken_and_oversized_sequences_are_dropped_without_losing_the_next() {
        let mut big = b"\x1b]133;A;".to_vec();
        big.extend(std::iter::repeat(b'x').take(MAX_SEQ + 10));
        big.extend(b"\x07\x1b]133;A\x07");
        let f = scan(&big, None);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].start as usize, big.len() - 8);
        // Cancelled, interrupted by another escape, or simply not ours.
        assert_eq!(scan(b"\x1b]133;A\x18\x07\x1b]133;\x1b[0m\x1b]0;title\x07\x1b]133;C\x07", None).len(), 1);
        assert_eq!(scan(b"\x1b]133;B\x1b]133;C\x07", None)[0].mark, Mark::OutputStart);
        assert!(scan(b"\x1b]133;Z\x07\x1b]9;4;9\x07\x1b]7;nope\x07", None).is_empty());
    }

    #[test]
    fn working_directory_and_progress() {
        let f = scan(b"\x1b]7;file://host/C:/Users/a%20b\x07\x1b]7;file:///home/u\x1b\\\x1b]633;P;Cwd=D:\\\\w\x07\x1b]9;4;1;40\x07\x1b]9;4;3\x07", None);
        assert_eq!(
            f.into_iter().map(|x| x.mark).collect::<Vec<_>>(),
            vec![
                Mark::Cwd("C:/Users/a b".into()),
                Mark::Cwd("/home/u".into()),
                Mark::Cwd("D:\\w".into()),
                Mark::Progress { state: 1, percent: Some(40) },
                Mark::Progress { state: 3, percent: None },
            ]
        );
    }

    fn status(s: &str) -> Option<Status> {
        match scan(format!("\x1b]7501;{s}\x1b\\").as_bytes(), None).pop()?.mark {
            Mark::Status(s) => Some(s),
            _ => None,
        }
    }

    /// The three examples in the specification, and its query.
    #[test]
    fn program_status_examples_from_the_specification() {
        assert_eq!(scan(b"\x1b]7501;?\x1b\\", None)[0].mark, Mark::StatusQuery);
        assert_eq!(scan(b"\x1b]7501 ; ? \x07", None).len(), 0, "the number has no spaces; the value may");
        assert_eq!(scan(b"\x1b]7501; ? \x07", None)[0].mark, Mark::StatusQuery);
        let a = status("state=working:app=make:msg=QnVpbGRpbmcgZG9jcw==").unwrap();
        assert_eq!((a.state, a.app.as_deref(), a.msg.as_deref()), (Some(State::Working), Some("make"), Some("Building docs")));
        let b = status(" state=blocked : kind=question : app=make : msg=T3ZlcndyaXRlIGNvbmZpZy50b21sPw== ").unwrap();
        assert_eq!((b.state, b.kind, b.msg.as_deref()), (Some(State::Blocked), Some(Kind::Question), Some("Overwrite config.toml?")));
        let c = status("state=done:app=make:msg=RG9jcyBidWlsdA").unwrap();
        assert_eq!((c.state, c.msg.as_deref()), (Some(State::Done), Some("Docs built")));
        // Our answer is itself a well-formed query reply.
        assert!(STATUS_REPLY.starts_with(b"\x1b]7501;?") && STATUS_REPLY.ends_with(b"\x1b\\") && STATUS_REPLY.len() < MAX_SEQ);
    }

    #[test]
    fn program_status_rules() {
        // Unknown key ignored, malformed pair skipped, repeated key takes the last.
        let s = status("colour=red:nonsense:state=idle:state=working:progress=40").unwrap();
        assert_eq!((s.state, s.progress), (Some(State::Working), Some(40)));
        // kind only with blocked; progress only in range and only while active.
        assert_eq!(status("state=working:kind=auth").unwrap().kind, None);
        assert_eq!(status("state=blocked:kind=bribe").unwrap().kind, None);
        assert_eq!(status("state=working:progress=101").unwrap().progress, None);
        assert_eq!(status("state=done:progress=50").unwrap().progress, None);
        // No state, an unknown state, a bad id or app: no report.
        assert!(status("app=make").is_none());
        assert!(status("state=dancing").is_none());
        assert!(status("state=idle:id=a//b").is_none());
        assert!(status("state=idle:id=1/2/3/4/5/6/7/8/9").is_none());
        assert!(status("state=idle:app=two,words").is_none());
        // A label that is not base64, not UTF-8, too long, or hides a control
        // character refuses the whole report.
        assert!(status("state=done:msg=a.b,").is_none(), "not base64");
        assert!(status("state=done:msg=A").is_none(), "impossible length");
        // Characters outside the value alphabet make the pair malformed: skipped, not fatal.
        assert_eq!(status("state=done:msg=!!!!").unwrap().msg, None);
        assert!(status("state=done:msg=/w==").is_none());
        assert!(status("state=done:title=bGluZQpicmVhaw==").is_none(), "newline");
        assert!(status("state=done:msg=dGFiCWhlcmU=").is_none(), "tab");
        assert!(status(&format!("state=done:title={}", "QUFB".repeat(65))).is_none());
        assert_eq!(status("state=idle:id=build/docs").unwrap().id, "build/docs");
        assert_eq!(display_safe("safe\u{202e}txt\u{200b}"), "safetxt");
    }

    #[test]
    fn status_records_replace_clear_expire_and_stay_bounded() {
        let mut t = StatusTable::default();
        let put = |t: &mut StatusTable, s: &str| t.apply(status(s).unwrap());
        put(&mut t, "state=working:app=make:progress=10");
        put(&mut t, "state=working");
        assert_eq!(t.foremost().unwrap().app, None, "a report replaces its record completely");
        put(&mut t, "state=working:id=a");
        put(&mut t, "state=blocked:kind=permission:id=a/b");
        put(&mut t, "state=error:id=c");
        assert_eq!(t.foremost().unwrap().id, "a/b", "blocked comes first");
        put(&mut t, "state=clear:id=a");
        assert_eq!(t.len(), 2, "clearing a record clears what is beneath it");
        assert_eq!(t.foremost().unwrap().id, "c");
        t.drop_active();
        assert_eq!((t.len(), t.foremost().unwrap().state), (1, Some(State::Error)), "errors stay until seen");
        put(&mut t, "state=clear");
        assert_eq!(t.len(), 0);
        for i in 0..MAX_RECORDS + 40 {
            put(&mut t, &format!("state=done:id=r{i}"));
        }
        assert_eq!(t.len(), MAX_RECORDS);
        assert!(t.records.contains_key(&format!("r{}", MAX_RECORDS + 39)) && !t.records.contains_key("r0"));
    }

    /// A real PowerShell with the integration loaded, on a real
    /// pseudo-console: every command becomes an object with its text, its
    /// exact output range and its exit status, and nothing unmarked is trusted.
    #[cfg(windows)]
    #[test]
    fn powershell_commands_become_objects_with_text_range_and_status() {
        use crate::session::{self, SessionCmd, SessionEvent, SessionSpec};
        let (shell, _) = session::default_shell();
        if !shell.to_ascii_lowercase().contains("pwsh") && !shell.to_ascii_lowercase().contains("powershell") {
            return; // no PowerShell on this machine
        }
        let dir = std::env::temp_dir().join(format!("proteus-marks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if std::env::var_os("PROTEUS_DATA").is_none() {
            std::env::set_var("PROTEUS_DATA", &dir);
        }
        let script = dir.join("integration.ps1");
        std::fs::write(&script, PWSH_INTEGRATION).unwrap();
        let mut env = session::clean_env();
        env.push(("PROTEUS_NONCE".into(), "t0ken9".into()));
        let h = session::spawn_local(SessionSpec {
            exe: shell,
            args: vec!["-NoLogo".into(), "-NoProfile".into(), "-NoExit".into(), "-Command".into(), format!(". '{}'", script.display())],
            cwd: Some(dir.clone()),
            cols: 120,
            rows: 30,
            journal: false,
            env: Some(env),
        })
        .unwrap();
        // The parser side answers the console host's start-up questions.
        let (mut bytes, mut found, mut scanner) = (Vec::new(), Vec::new(), Scanner::new(Some("t0ken9".into())));
        let mut pump = |until: &dyn Fn(&[Found]) -> bool, what: &str, bytes: &mut Vec<u8>, found: &mut Vec<Found>| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
            while !until(found) {
                match h.events.recv_deadline(deadline) {
                    Ok(SessionEvent::Output(b)) => {
                        if b.windows(4).any(|w| w == b"\x1b[6n") {
                            let _ = h.cmds.send(SessionCmd::Input(b"\x1b[1;1R".to_vec()));
                        }
                        if b.windows(3).any(|w| w == b"\x1b[c") {
                            let _ = h.cmds.send(SessionCmd::Input(b"\x1b[?6c".to_vec()));
                        }
                        scanner.feed(&b, found);
                        bytes.extend(b);
                    }
                    // A session also reports the processes it starts; they are not what is waited for.
                    Ok(SessionEvent::Proc(_)) => {}
                    _ => panic!("timed out waiting for {what}; got: {}", String::from_utf8_lossy(bytes).replace('\x1b', "<ESC>").replace('\x07', "<BEL>")),
                }
            }
        };
        let ends = |f: &[Found]| f.iter().filter(|x| matches!(x.mark, Mark::CommandEnd(_))).count();
        let inputs = |f: &[Found]| f.iter().filter(|x| x.mark == Mark::InputStart).count();
        pump(&|f| inputs(f) >= 1, "the first prompt", &mut bytes, &mut found);
        h.cmds.send(SessionCmd::Input(b"cmd /c exit 7\r".to_vec())).unwrap();
        pump(&|f| ends(f) >= 1 && inputs(f) >= 2, "the first command to end", &mut bytes, &mut found);
        h.cmds.send(SessionCmd::Input(b"Write-Output 'hello;marks'\r".to_vec())).unwrap();
        pump(&|f| ends(f) >= 2 && inputs(f) >= 3, "the second command to end", &mut bytes, &mut found);
        // A program printing a mark of its own is seen, and not believed.
        h.cmds.send(SessionCmd::Input(b"[Console]::Write([char]27 + ']133;D;0' + [char]7)\r".to_vec())).unwrap();
        pump(&|f| ends(f) >= 4 && inputs(f) >= 4, "the forged mark", &mut bytes, &mut found);
        let _ = h.cmds.send(SessionCmd::Kill);
        let _ = std::fs::remove_file(&script);

        let lines: Vec<&str> = found.iter().filter_map(|x| if let Mark::CommandLine(l) = &x.mark { Some(l.as_str()) } else { None }).collect();
        assert_eq!(&lines[..2], &["cmd /c exit 7", "Write-Output 'hello;marks'"]);
        let statuses: Vec<(Option<i32>, bool)> = found.iter().filter_map(|x| if let Mark::CommandEnd(s) = x.mark { Some((s, x.trusted)) } else { None }).collect();
        assert_eq!(statuses[0], (Some(7), true), "a failing native command reports its exit code");
        assert_eq!(statuses[1], (Some(0), true));
        assert!(statuses[2..].contains(&(Some(0), false)), "the forged mark is seen but not trusted: {statuses:?}");
        assert!(found.iter().any(|x| matches!(&x.mark, Mark::Cwd(c) if c.to_ascii_lowercase().contains("proteus-marks-test"))));
        // The second command's output is exactly the bytes between its C and its D.
        let c = found.iter().filter(|x| x.mark == Mark::OutputStart && x.trusted).nth(1).unwrap();
        let d = found.iter().filter(|x| matches!(x.mark, Mark::CommandEnd(_)) && x.trusted).nth(1).unwrap();
        let out = String::from_utf8_lossy(&bytes[c.end as usize..d.start as usize]).into_owned();
        assert!(out.contains("hello;marks"), "output range holds: {out:?}");
    }

    /// The day-one question: does the bundled console host deliver these
    /// sequences whole, and where the program put them? A real child prints
    /// text and marks through the real pseudo-console.
    #[cfg(windows)]
    #[test]
    fn the_console_host_delivers_marks_whole_and_in_place() {
        use crate::session::{self, SessionEvent, SessionSpec};
        let dir = std::env::temp_dir().join(format!("proteus-marks-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Tests share one process: never repoint or delete a data folder
        // another test may be using.
        if std::env::var_os("PROTEUS_DATA").is_none() {
            std::env::set_var("PROTEUS_DATA", &dir);
        }
        let file = dir.join("marks.bin");
        std::fs::write(
            &file,
            b"AAA\x1b]133;A\x07BBB\x1b]133;C\x1b\\CCC\x1b]7501;state=working:app=make\x1b\\DDD\x1b]633;E;echo hi\x07EEE\x1b]9;9;C:\\x\x07FFF\x1b]133;D;3\x07GGG\x1b]7501;?\x1b\\HHH\r\n",
        )
        .unwrap();
        let h = session::spawn_local(SessionSpec {
            exe: "cmd.exe".into(),
            args: vec!["/d".into(), "/c".into(), "type".into(), file.to_string_lossy().into_owned()],
            cwd: None,
            cols: 200,
            rows: 30,
            journal: false,
            env: None,
        })
        .unwrap();
        let mut bytes = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            match h.events.recv_deadline(deadline) {
                Ok(SessionEvent::Output(b)) => bytes.extend(b),
                Ok(SessionEvent::Exit(_)) | Err(_) => break,
                Ok(_) => {}
            }
        }
        let _ = std::fs::remove_file(&file);
        let found = scan(&bytes, None);
        let shown = String::from_utf8_lossy(&bytes).replace('\x1b', "<ESC>").replace('\x07', "<BEL>");
        let marks: Vec<&Mark> = found.iter().map(|f| &f.mark).collect();
        assert_eq!(
            marks,
            vec![
                &Mark::PromptStart,
                &Mark::OutputStart,
                &Mark::Status(Status { state: Some(State::Working), app: Some("make".into()), ..Status::default() }),
                &Mark::CommandLine("echo hi".into()),
                &Mark::Cwd("C:\\x".into()),
                &Mark::CommandEnd(Some(3)),
                &Mark::StatusQuery,
            ],
            "what arrived: {shown}"
        );
        // In place: each run of letters lies between the marks it was printed between.
        let at = |needle: &str| bytes.windows(3).position(|w| w == needle.as_bytes()).unwrap_or_else(|| panic!("{needle} missing in {shown}")) as u64;
        let letters = ["AAA", "BBB", "CCC", "DDD", "EEE", "FFF", "GGG", "HHH"];
        for (i, f) in found.iter().enumerate() {
            assert!(at(letters[i]) < f.start && f.end <= at(letters[i + 1]), "mark {i} is out of place in {shown}");
        }
    }
}
