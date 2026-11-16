//! Links: what a program declared with OSC 8, and plain text that names
//! something real (a file that exists, at a line; a web address).
//!
//! The window asks `Hover` what is under the pointer whenever the pointer
//! enters another cell; the answer changes only when it enters or leaves a
//! link, and only then is a frame drawn (`renderer/link.rs` underlines the
//! link and names its target in a strip below the grid). Moving over
//! anything else draws nothing.
//!
//! Nothing opens on a plain click. Ctrl+click opens http, https, mailto and
//! files on this computer, nothing else; a file that would run rather than
//! open (a program, a script, a shortcut) is shown in Explorer instead.
//! Plain `path:line[:col]` text becomes a link only if that file exists,
//! looked up from the session's working directory (OSC 7 or 9;9, see
//! marks.rs) or, failing that, the directory the session started in.
//! Network paths are never looked up: a hover must not wait on a server.

use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{viewport_to_point, Term};
use regex::Regex;

use crate::term::Listener;

/// Cells of one screen row: viewport row, first column, last column (inclusive).
pub type Span = (usize, usize, usize);

#[derive(Clone, Debug, PartialEq)]
pub enum Target {
    /// Declared by the program (OSC 8). Cells with the same id and address
    /// are one link, wherever they are on the screen.
    Uri { id: String, uri: String },
    /// Plain text naming a file that exists.
    File { path: PathBuf, line: Option<u32>, col: Option<u32> },
    /// A plain web address in the text.
    Web(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub target: Target,
    pub spans: Vec<Span>,
}

impl Link {
    pub fn contains(&self, row: usize, col: usize) -> bool {
        self.spans.iter().any(|&(r, a, b)| r == row && (a..=b).contains(&col))
    }
}

/// The link under one cell of the screen as it is shown (viewport rows,
/// so a scrolled-back view is hit-tested as seen), or None.
pub fn at(term: &Term<Listener>, row: usize, col: usize, cwd: &Path) -> Option<Link> {
    let (rows, cols) = (term.screen_lines(), term.columns());
    if row >= rows || col >= cols {
        return None;
    }
    let grid = term.grid();
    let off = grid.display_offset();
    let cell = |r: usize, c: usize| -> &Cell { &grid[viewport_to_point(off, Point::new(r, Column(c)))] };
    // The right half of a wide character belongs to its left half.
    let owner = |r: usize, c: usize| if c > 0 && cell(r, c).flags.contains(Flags::WIDE_CHAR_SPACER) { c - 1 } else { c };
    let c0 = owner(row, col);
    if let Some(h) = cell(row, c0).hyperlink() {
        let mut spans: Vec<Span> = Vec::new();
        for r in 0..rows {
            for c in 0..cols {
                if cell(r, owner(r, c)).hyperlink().as_ref() != Some(&h) {
                    continue;
                }
                match spans.last_mut() {
                    Some(s) if s.0 == r && s.2 + 1 == c => s.2 = c,
                    _ => spans.push((r, c, c)),
                }
            }
        }
        return Some(Link { target: Target::Uri { id: h.id().to_string(), uri: h.uri().to_string() }, spans });
    }
    // Plain text: the logical line (rows joined where the program's text
    // wrapped), then whatever on it names something real.
    let (text, map) = logical_line(term, row);
    let at = map.iter().position(|&(r, c, w)| r == row && (c..c + w).contains(&c0))?;
    let (range, target) = find_at(&text, at, cwd)?;
    let mut spans: Vec<Span> = Vec::new();
    for &(r, c, w) in &map[range] {
        match spans.last_mut() {
            Some(s) if s.0 == r && s.2 + 1 == c => s.2 = c + w - 1,
            _ => spans.push((r, c, c + w - 1)),
        }
    }
    Some(Link { target, spans })
}

/// The text of the logical line through a viewport row, one char per
/// character cell, with where each char sits: (row, column, width).
fn logical_line(term: &Term<Listener>, row: usize) -> (String, Vec<(usize, usize, usize)>) {
    let (rows, cols) = (term.screen_lines(), term.columns());
    let grid = term.grid();
    let off = grid.display_offset();
    let cell = |r: usize, c: usize| -> &Cell { &grid[viewport_to_point(off, Point::new(r, Column(c)))] };
    let wraps = |r: usize| cell(r, cols - 1).flags.contains(Flags::WRAPLINE);
    // A few rows either way is plenty for a path or an address.
    let (mut r0, mut r1) = (row, row);
    while r0 > 0 && row - r0 < 8 && wraps(r0 - 1) {
        r0 -= 1;
    }
    while r1 + 1 < rows && r1 - row < 8 && wraps(r1) {
        r1 += 1;
    }
    let mut text = String::with_capacity((r1 - r0 + 1) * cols);
    let mut map = Vec::with_capacity((r1 - r0 + 1) * cols);
    for r in r0..=r1 {
        for c in 0..cols {
            let x = cell(r, c);
            if x.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let ch = match x.c {
                '\0' | '\t' => ' ',
                ch => ch,
            };
            text.push(ch);
            map.push((r, c, if x.flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 }));
        }
    }
    (text, map)
}

fn url_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r#"(?i)\bhttps?://[^\s<>"'`{}|\\^\x00-\x1f]+"#).expect("url pattern"))
}

/// `path:line[:col]`, `path(line[,col])` (MSBuild, tsc) and Python's
/// `File "path", line N`.
fn path_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| {
        Regex::new(
            r#"File "(?P<pq>[^"]+)", line (?P<lq>\d+)|(?P<p>(?:[A-Za-z]:)?[^\s<>"'`|?*:()\[\]{},;=]+)(?::(?P<l1>\d+)(?::(?P<c1>\d+))?|\((?P<l2>\d+)(?:, ?(?P<c2>\d+))?\))"#,
        )
        .expect("path pattern")
    })
}

/// A web address ends before the punctuation of the sentence around it.
fn trim_url(s: &str) -> &str {
    let mut s = s;
    loop {
        let Some(last) = s.chars().last() else { return s };
        let unbalanced = |open: char, close: char| last == close && s.matches(open).count() < s.matches(close).count();
        if ".,:;!?*".contains(last) || unbalanced('(', ')') || unbalanced('[', ']') {
            s = &s[..s.len() - last.len_utf8()];
        } else {
            return s;
        }
    }
}

/// What names something real on this line and covers char `at`. Only the
/// candidate under the pointer is looked up on disk.
pub fn find_at(text: &str, at: usize, cwd: &Path) -> Option<(Range<usize>, Target)> {
    let chars: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
    let to_char = |b: usize| chars.partition_point(|&x| x < b);
    let covers = |a: usize, b: usize| (to_char(a)..to_char(b)).contains(&at);
    for m in url_re().find_iter(text) {
        let url = trim_url(m.as_str());
        if covers(m.start(), m.start() + url.len()) {
            return Some((to_char(m.start())..to_char(m.start() + url.len()), Target::Web(url.to_string())));
        }
    }
    for c in path_re().captures_iter(text) {
        let whole = c.get(0)?;
        if !covers(whole.start(), whole.end()) {
            continue;
        }
        let num = |name: &str| c.name(name).and_then(|m| m.as_str().parse::<u32>().ok());
        let (raw, line, col) = match c.name("pq") {
            Some(p) => (p.as_str(), num("lq"), None),
            None => (c.name("p")?.as_str(), num("l1").or(num("l2")), num("c1").or(num("c2"))),
        };
        if let Some(path) = resolve(raw, cwd) {
            return Some((to_char(whole.start())..to_char(whole.end()), Target::File { path, line, col }));
        }
    }
    None
}

/// Is this path on another computer (a share, or a drive mapped to one)?
fn remote(p: &Path) -> bool {
    let s = p.as_os_str().to_string_lossy();
    if s.starts_with("\\\\") || s.starts_with("//") {
        return true;
    }
    let b = s.as_bytes();
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        let root: Vec<u16> = [b[0] as u16, b':' as u16, b'\\' as u16, 0].to_vec();
        // DRIVE_REMOTE
        return unsafe { windows_sys::Win32::Storage::FileSystem::GetDriveTypeW(root.as_ptr()) } == 4;
    }
    false
}

/// Remove `.` and fold `..` without touching the disk.
fn tidy(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// A path as a program printed it, resolved against `cwd`, if it is a file
/// that exists on this computer.
pub fn resolve(raw: &str, cwd: &Path) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() || raw.starts_with("\\\\") || raw.starts_with("//") || raw.contains('\0') {
        return None;
    }
    let b = raw.as_bytes();
    // MSYS and Git Bash spell C:\x as /c/x.
    let msys = (b.len() >= 3 && b[0] == b'/' && b[1].is_ascii_alphabetic() && b[2] == b'/').then(|| format!("{}:/{}", b[1] as char, &raw[3..]));
    let raw = msys.as_deref().unwrap_or(raw);
    let p = match raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) {
        Some(rest) => PathBuf::from(std::env::var_os("USERPROFILE")?).join(rest),
        None => cwd.join(raw),
    };
    if remote(&p) {
        return None;
    }
    let p = tidy(&p);
    p.is_file().then_some(p)
}

/// What Ctrl+click on a link does.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Hand an http, https or mailto address to the system.
    Web(String),
    /// A folder: show it.
    Folder(PathBuf),
    /// A document: open it (in the configured editor at the line, if any).
    Edit { path: PathBuf, line: Option<u32>, col: Option<u32> },
    /// Something that would run if opened: select it in Explorer instead.
    Reveal(PathBuf),
    /// Not opened, and why.
    Refuse(String),
}

/// Extensions that run rather than open, beyond those in PATHEXT.
const RUNS: &[&str] = &[
    "exe", "com", "bat", "cmd", "ps1", "psm1", "psd1", "vbs", "vbe", "js", "jse", "wsf", "wsh", "msc", "msi", "msp", "msix", "msixbundle", "appx",
    "appxbundle", "scr", "pif", "cpl", "hta", "jar", "lnk", "url", "reg", "inf", "application", "appref-ms", "gadget", "scf", "chm", "website",
    "diagcab", "settingcontent-ms", "appinstaller", "xbap", "vsix", "library-ms", "search-ms", "xll", "sys", "dll", "ocx", "py", "pyw",
];

fn runs(p: &Path) -> bool {
    // Windows drops trailing dots and spaces: `go.bat.` and `go.bat ` are go.bat.
    let Some(name) = p.file_name().map(|n| n.to_string_lossy().trim_end_matches(['.', ' ']).to_ascii_lowercase()) else { return false };
    let Some((_, ext)) = name.rsplit_once('.') else { return false };
    let pathext = std::env::var("PATHEXT").unwrap_or_default().to_ascii_lowercase();
    if RUNS.contains(&ext) || pathext.split(';').any(|e| e.trim_start_matches('.') == ext) {
        return true;
    }
    // And whatever the system itself counts as dangerous to open.
    let dotted = wide(&format!(".{ext}"));
    unsafe { windows_sys::Win32::UI::Shell::AssocIsDangerous(dotted.as_ptr()) != 0 }
}

fn local(path: PathBuf, line: Option<u32>, col: Option<u32>) -> Action {
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => Action::Folder(path),
        Ok(_) if runs(&path) => Action::Reveal(path),
        Ok(_) => Action::Edit { path, line, col },
        Err(_) => Action::Refuse(format!("not found: {}", path.display())),
    }
}

/// Names this computer answers to in a file URI's host part.
fn this_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    if h.is_empty() || h == "localhost" {
        return true;
    }
    let mine = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty() && v.eq_ignore_ascii_case(&h));
    mine("COMPUTERNAME") || h.split('.').next().is_some_and(|short| std::env::var("COMPUTERNAME").is_ok_and(|v| v.eq_ignore_ascii_case(short)) && h.ends_with(".local"))
}

fn percent_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let v = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())?;
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok().filter(|t| !t.contains('\0'))
}

/// A file URI to a local path, or why it is not one.
fn file_uri(uri: &str) -> Result<PathBuf, String> {
    let rest = &uri[5..]; // after "file:"
    let (host, path) = match rest.strip_prefix("//") {
        Some(r) => r.split_at(r.find('/').unwrap_or(r.len())),
        None => ("", rest),
    };
    if !this_host(host) {
        return Err(format!("a file on another computer ({host})"));
    }
    // Both slashes are separators to Windows, and two of them at the start
    // name a share (or a device), however they are spelt or escaped.
    let p = percent_decode(path).ok_or("the address is not valid")?.replace('\\', "/");
    // file:///C:/x names C:/x, not /C:/x.
    let p = p.strip_prefix('/').filter(|q| drive(q)).unwrap_or(&p);
    if p.starts_with("//") {
        return Err("a file on another computer".into());
    }
    // Only a full path from a drive letter: a relative or drive-relative
    // one would be found from wherever this program happens to stand.
    if !drive(p) || !p[2..].starts_with('/') {
        return Err("not a full path to a file on this computer".into());
    }
    // A colon after the drive names a stream of a file, not a file.
    if p[2..].contains(':') {
        return Err("not a plain file name".into());
    }
    let p = PathBuf::from(p);
    if remote(&p) {
        return Err("a file on another computer (a network drive)".into());
    }
    Ok(p)
}

/// Starts with a drive letter and its colon.
fn drive(p: &str) -> bool {
    let b = p.as_bytes();
    b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// The policy: what Ctrl+click on this target does.
pub fn action(t: &Target) -> Action {
    match t {
        Target::Web(u) => Action::Web(u.clone()),
        Target::File { path, line, col } => local(path.clone(), *line, *col),
        Target::Uri { uri, .. } => {
            let Some((scheme, _)) = uri.split_once(':') else { return Action::Refuse("it has no scheme".into()) };
            match scheme.to_ascii_lowercase().as_str() {
                "http" | "https" | "mailto" => {
                    if uri.chars().any(|c| c.is_control() || c.is_whitespace()) {
                        Action::Refuse("the address has spaces or control characters in it".into())
                    } else if uri.len() > 8192 {
                        Action::Refuse("the address is too long".into())
                    } else {
                        Action::Web(uri.clone())
                    }
                }
                "file" => match file_uri(uri) {
                    Ok(p) => local(p, None, None),
                    Err(why) => Action::Refuse(why),
                },
                other => Action::Refuse(format!("{}: links are not opened (only http, https, mailto and files on this computer)", crate::marks::display_safe(other))),
            }
        }
    }
}

/// Text for the strip: the target, what Ctrl+click will do, and whether
/// that is a refusal.
pub fn describe(link: &Link) -> (String, String, bool) {
    let clean = |s: &str| crate::marks::display_safe(s).chars().filter(|c| !c.is_control()).collect::<String>();
    let raw = match &link.target {
        Target::Uri { uri, .. } => uri.clone(),
        Target::Web(u) => u.clone(),
        Target::File { path, .. } => path.display().to_string(),
    };
    match action(&link.target) {
        Action::Web(_) => (clean(&raw), "ctrl+click to open".into(), false),
        Action::Folder(p) => (clean(&p.display().to_string()), "ctrl+click to show the folder".into(), false),
        Action::Edit { path, line, col } => {
            let at = match (line, col) {
                (Some(l), Some(c)) => format!(":{l}:{c}"),
                (Some(l), None) => format!(":{l}"),
                _ => String::new(),
            };
            (clean(&format!("{}{at}", path.display())), "ctrl+click to open".into(), false)
        }
        Action::Reveal(p) => (clean(&p.display().to_string()), "ctrl+click shows it in Explorer; links never run programs".into(), false),
        Action::Refuse(why) => (clean(&raw), format!("not opened: {}", clean(&why)), true),
    }
}

/// The pointer and the link it is on.
#[derive(Default)]
pub struct Hover {
    pub link: Option<Link>,
    cell: Option<(usize, usize)>,
}

impl Hover {
    /// The pointer is over viewport cell (row, col), or over no cell. True
    /// when the link under it changed, which is the only time a frame is owed.
    pub fn point(&mut self, term: &Term<Listener>, cell: Option<(usize, usize)>, cwd: &Path) -> bool {
        if cell == self.cell {
            return false;
        }
        self.cell = cell;
        // Still on the same link: nothing to look up.
        if let (Some(l), Some((r, c))) = (&self.link, cell) {
            if l.contains(r, c) {
                return false;
            }
        }
        self.set(cell.and_then(|(r, c)| at(term, r, c, cwd)))
    }

    /// The screen changed under a pointer that has not moved.
    pub fn recheck(&mut self, term: &Term<Listener>, cwd: &Path) -> bool {
        let now = self.cell.and_then(|(r, c)| at(term, r, c, cwd));
        self.set(now)
    }

    /// The pointer left, or what was under it is gone (resize, font change).
    pub fn clear(&mut self) -> bool {
        self.cell = None;
        self.set(None)
    }

    fn set(&mut self, now: Option<Link>) -> bool {
        if now == self.link {
            return false;
        }
        self.link = now;
        true
    }
}

/// The left button and links. A Ctrl+click that opens a link is the
/// link's, press and release both: a program with mouse reporting on must
/// not be sent the release of a press it never saw, and a selection must not
/// end where none began.
#[derive(Default)]
pub struct Click {
    taken: bool,
}

impl Click {
    /// A left press or release. `open` opens the link under the pointer and
    /// says whether there was one; it is asked only on a press with Ctrl.
    /// True when the event is the link's and goes nowhere else.
    pub fn route(&mut self, pressed: bool, ctrl: bool, open: impl FnOnce() -> bool) -> bool {
        if pressed {
            self.taken = ctrl && open();
            self.taken
        } else {
            std::mem::take(&mut self.taken)
        }
    }
}

/// Where plain `path:line` text is looked up until the shell reports a
/// working directory: where the session started (as session.rs chooses it).
pub fn start_dir(journal: Option<&Path>, cwd: Option<&Path>) -> PathBuf {
    journal
        .and_then(journal_cwd)
        .filter(|p| p.is_dir())
        .or_else(|| cwd.filter(|p| p.is_dir()).map(Path::to_path_buf))
        .or_else(|| std::env::var_os("USERPROFILE").map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// The directory a session started in, from its journal's header (a
/// reattached window did not start it, so only the journal knows).
pub fn journal_cwd(path: &Path) -> Option<PathBuf> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut head = [0u8; 12];
    f.read_exact(&mut head).ok()?;
    if &head[..8] != crate::journal::MAGIC {
        return None;
    }
    let n = u32::from_le_bytes(head[8..12].try_into().ok()?) as usize;
    let mut meta = vec![0u8; n.min(1 << 16)];
    f.read_exact(&mut meta).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&meta).ok()?;
    v["cwd"].as_str().filter(|s| !s.is_empty()).map(PathBuf::from)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Carry out an action, off the window's thread (the system may take a
/// while to start a browser). `editor` is the configured command with
/// {path}, {line} and {col} in it, or empty for the system's own choice.
pub fn open(a: Action, editor: String) {
    use windows_sys::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE};
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let _ = std::thread::Builder::new().name("link".into()).spawn(move || {
        let shell = |file: &str, params: Option<&str>| {
            let (f, verb) = (wide(file), wide("open"));
            let p = params.map(wide);
            unsafe {
                CoInitializeEx(std::ptr::null(), (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32);
                ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), f.as_ptr(), p.as_ref().map_or(std::ptr::null(), |v| v.as_ptr()), std::ptr::null(), SW_SHOWNORMAL) as isize > 32
            }
        };
        let (kind, ok) = match a {
            Action::Web(u) => ("address", shell(&u, None)),
            Action::Folder(p) => ("folder", shell(&p.to_string_lossy(), None)),
            Action::Reveal(p) => ("file shown in Explorer", shell("explorer.exe", Some(&format!("/select,\"{}\"", p.display())))),
            Action::Edit { path, line, col } if !editor.trim().is_empty() => {
                let argv = editor_argv(&editor, &path, line, col);
                ("file, in the editor", argv.split_first().is_some_and(|(exe, rest)| std::process::Command::new(exe).args(rest).spawn().is_ok()))
            }
            Action::Edit { path, .. } => ("file", shell(&path.to_string_lossy(), None)),
            Action::Refuse(_) => return,
        };
        crate::paths::log(&format!("link: {} {kind}", if ok { "opened" } else { "could not open" }));
    });
}

/// The editor command with the target filled in. Words are split on spaces
/// outside double quotes; the path is always one word of its own, so
/// nothing in it can become an option or a second command.
pub fn editor_argv(template: &str, path: &Path, line: Option<u32>, col: Option<u32>) -> Vec<String> {
    let mut words: Vec<String> = Vec::new();
    let (mut cur, mut quoted, mut any) = (String::new(), false, false);
    for ch in template.chars() {
        match ch {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if c.is_whitespace() && !quoted => {
                if any || !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
                any = false;
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        words.push(cur);
    }
    let p = path.display().to_string();
    let (l, c) = (line.unwrap_or(1).to_string(), col.unwrap_or(1).to_string());
    words.into_iter().map(|w| w.replace("{line}", &l).replace("{col}", &c).replace("{path}", &p)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// What a cell of the sample is expected to be: written down by the
    /// generator as it lays the sample out, never by the hit-test.
    #[derive(Clone, Debug, PartialEq)]
    enum Want {
        Uri(&'static str),
        File(&'static str, u32, Option<u32>),
        Web(&'static str),
    }

    /// Lays text out the way an 80-column terminal does (autowrap at the
    /// right edge, wide characters in two cells) and remembers what each
    /// printed cell should hit-test to.
    struct Gen {
        bytes: Vec<u8>,
        cols: usize,
        row: usize,
        col: usize,
        cur: Option<Want>,
        want: HashMap<(usize, usize), Want>,
    }

    impl Gen {
        fn new(cols: usize) -> Self {
            Self { bytes: Vec::new(), cols, row: 0, col: 0, cur: None, want: HashMap::new() }
        }
        fn put(&mut self, s: &str, width: usize) {
            for ch in s.chars() {
                if self.col + width > self.cols {
                    self.row += 1;
                    self.col = 0;
                }
                if let Some(w) = &self.cur {
                    for k in 0..width {
                        self.want.insert((self.row, self.col + k), w.clone());
                    }
                }
                let mut b = [0u8; 4];
                self.bytes.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
                self.col += width;
            }
        }
        fn text(&mut self, s: &str) {
            self.put(s, 1);
        }
        fn raw(&mut self, s: &str) {
            self.bytes.extend_from_slice(s.as_bytes());
        }
        fn nl(&mut self) {
            self.raw("\r\n");
            self.row += 1;
            self.col = 0;
        }
        fn osc8(&mut self, params: &str, uri: &'static str, label: &str) {
            self.raw(&format!("\x1b]8;{params};{uri}\x1b\\"));
            self.cur = Some(Want::Uri(uri));
            self.text(label);
            self.cur = None;
            self.raw("\x1b]8;;\x1b\\");
        }
        fn plain(&mut self, s: &str, w: Want) {
            self.cur = Some(w);
            self.text(s);
            self.cur = None;
        }
    }

    const LONG: &str = "https://example.com/a/long/address/that/does/not/fit/on/one/row/of/the/screen/and/so/wraps/onto/the/next/row";

    /// The gate sample: three shapes of link (declared, a file named in
    /// plain text, a plain web address), and the awkward cases around them.
    fn sample() -> Gen {
        let mut g = Gen::new(80);
        g.raw("\x1b[1m");
        g.text("PROTEUS links");
        g.raw("\x1b[0m");
        g.text("  hover names the target; ctrl+click opens it");
        g.nl();
        g.nl();
        g.text("1. Declared by the program (OSC 8), one id in two places:");
        g.nl();
        g.text("   see ");
        g.osc8("id=docs", "https://example.com/docs", "the manual");
        g.text(" for details, or go straight to ");
        g.osc8("id=docs", "https://example.com/docs", "its index");
        g.text(".");
        g.nl();
        g.text("   no id: ");
        g.raw("\x1b[36m");
        g.osc8("", "mailto:team@example.com", "write to us");
        g.raw("\x1b[0m");
        g.text(" or read ");
        g.osc8("", "file://localhost/C:/Windows/win.ini", "win.ini");
        g.nl();
        g.text("   side by side: ");
        g.osc8("", "https://example.com/one", "one");
        g.osc8("", "https://example.com/two", "two");
        g.text("   refused: ");
        g.raw("\x1b[31m");
        g.osc8("", "javascript:alert(1)", "run this");
        g.raw("\x1b[0m");
        g.nl();
        g.text("   wide: ");
        g.put("\u{7d42}\u{7aef}", 2);
        g.text(" ");
        g.raw("\x1b]8;;https://example.com/wide\x1b\\");
        g.cur = Some(Want::Uri("https://example.com/wide"));
        g.put("\u{30ea}\u{30f3}\u{30af}", 2);
        g.cur = None;
        g.raw("\x1b]8;;\x1b\\");
        g.nl();
        g.nl();
        g.text("2. Plain text that names a real file, found in the working directory:");
        g.nl();
        g.raw("\x1b[1;31m");
        g.text("error[E0308]");
        g.raw("\x1b[0;1m");
        g.text(": mismatched types");
        g.raw("\x1b[0m");
        g.nl();
        g.raw("\x1b[34m");
        g.text("  --> ");
        g.raw("\x1b[0m");
        g.plain("src/app.rs:42:7", Want::File("src/app.rs", 42, Some(7)));
        g.nl();
        g.text("   MSVC style: ");
        g.plain("src\\links.rs(12,5)", Want::File("src/links.rs", 12, Some(5)));
        g.text(": note: here");
        g.nl();
        g.text("   not a link, no such file: src/nothere.rs:3 ; a time: 12:30:15");
        g.nl();
        g.nl();
        g.text("3. A long declared link wraps; a plain address ends before the full stop:");
        g.nl();
        g.text("   ");
        g.osc8("", LONG, LONG);
        g.nl();
        g.text("   see ");
        g.plain("https://example.com/plain?q=1", Want::Web("https://example.com/plain?q=1"));
        g.text(".");
        g.nl();
        g
    }

    fn manifest() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn term_with(bytes: &[u8], cols: usize, rows: usize) -> crate::term::Terminal {
        let t = crate::term::Terminal::new(cols, rows, crate::theme::Theme::void_light(), std::sync::Arc::new(|_| {}), None);
        t.feed(bytes);
        t
    }

    fn matches(got: &Option<Link>, want: Option<&Want>, cwd: &Path) -> bool {
        match (got.as_ref().map(|l| &l.target), want) {
            (None, None) => true,
            (Some(Target::Uri { uri, .. }), Some(Want::Uri(w))) => uri == w,
            (Some(Target::Web(u)), Some(Want::Web(w))) => u == w,
            (Some(Target::File { path, line, col }), Some(Want::File(f, l, c))) => *path == tidy(&cwd.join(f)) && *line == Some(*l) && col == c,
            _ => false,
        }
    }

    /// Every cell of the screen, link or not, against the generator's own
    /// record. Returns (cells checked, cells right, first few mistakes).
    fn score(term: &Term<Listener>, want: &HashMap<(usize, usize), Want>, cwd: &Path) -> (usize, usize, Vec<String>) {
        let (mut n, mut ok, mut bad) = (0, 0, Vec::new());
        for row in 0..term.screen_lines() {
            for col in 0..term.columns() {
                let got = at(term, row, col, cwd);
                let w = want.get(&(row, col));
                n += 1;
                // A hit must also cover the cell it was found from.
                if matches(&got, w, cwd) && got.as_ref().is_none_or(|l| l.contains(row, col)) {
                    ok += 1;
                } else if bad.len() < 6 {
                    bad.push(format!("({row},{col}) want {w:?} got {:?}", got.map(|l| l.target)));
                }
            }
        }
        (n, ok, bad)
    }

    #[test]
    #[ignore]
    fn write_the_sample() {
        std::fs::write(manifest().join("forge/samples/links.vt"), sample().bytes).unwrap();
    }

    #[test]
    fn the_checked_in_sample_is_the_generated_one() {
        let file = std::fs::read(manifest().join("forge/samples/links.vt")).expect("forge/samples/links.vt");
        assert!(file == sample().bytes, "forge/samples/links.vt differs from the generator: run the ignored test write_the_sample");
    }

    /// Hit-testing on the sample: every one of the 80x24 cells returns the
    /// right target or none, on the live screen and on a scrolled-back view.
    #[test]
    fn hit_testing_is_right_for_every_cell_of_the_sample() {
        let g = sample();
        let cwd = manifest();
        let t = term_with(&g.bytes, 80, 24);
        let (n, ok, bad) = score(&t.term.lock().unwrap(), &g.want, &cwd);
        println!("links: {ok} of {n} cells hit-test right ({} link cells)", g.want.len());
        assert_eq!(ok, n, "{bad:#?}");
        assert!(g.want.len() > 150, "the sample has too few link cells to mean much");

        // Push the sample up by 9 rows (the cursor is on row 18: five line
        // feeds reach the bottom, nine more scroll), then scroll the view
        // back: the same screen, drawn from history, must hit-test the same.
        t.feed("\r\n".repeat(5 + 9).as_bytes());
        let mut term = t.term.lock().unwrap();
        let moved: HashMap<(usize, usize), Want> = g.want.iter().filter(|((r, _), _)| *r >= 9).map(|((r, c), w)| ((r - 9, *c), w.clone())).collect();
        let (n1, ok1, bad1) = score(&term, &moved, &cwd);
        assert_eq!(ok1, n1, "pushed up: {bad1:#?}");
        term.scroll_display(alacritty_terminal::grid::Scroll::Delta(9));
        let (n2, ok2, bad2) = score(&term, &g.want, &cwd);
        assert_eq!(ok2, n2, "scrolled back: {bad2:#?}");
    }

    /// A declared id groups its cells wherever they are; the wide link's
    /// right halves belong to it; the wrapped link is one link on two rows.
    #[test]
    fn spans_group_by_id_and_cover_wide_and_wrapped_text() {
        let g = sample();
        let t = term_with(&g.bytes, 80, 24);
        let term = t.term.lock().unwrap();
        let cwd = manifest();
        let docs = g.want.iter().find(|(_, w)| **w == Want::Uri("https://example.com/docs")).map(|(k, _)| *k).unwrap();
        let l = at(&term, docs.0, docs.1, &cwd).unwrap();
        assert_eq!(l.spans.len(), 2, "one id in two places is one link: {:?}", l.spans);
        let cells: usize = l.spans.iter().map(|s| s.2 - s.1 + 1).sum();
        assert_eq!(cells, "the manual".len() + "its index".len());
        let wide = g.want.iter().filter(|(_, w)| **w == Want::Uri("https://example.com/wide")).count();
        let first = g.want.iter().find(|(_, w)| **w == Want::Uri("https://example.com/wide")).map(|(k, _)| *k).unwrap();
        let w = at(&term, first.0, first.1, &cwd).unwrap();
        assert_eq!(w.spans.iter().map(|s| s.2 - s.1 + 1).sum::<usize>(), wide, "{:?}", w.spans);
        let long = g.want.iter().find(|(_, w)| **w == Want::Uri(LONG)).map(|(k, _)| *k).unwrap();
        assert_eq!(at(&term, long.0, long.1, &cwd).unwrap().spans.len(), 2, "the long link wraps onto a second row");
    }

    /// The window draws a frame only when `point` says so. Walking the
    /// pointer over every cell of the sample, row by row: entering or
    /// leaving a link owes a frame, and nothing else does.
    #[test]
    fn moving_over_cells_that_are_not_links_owes_no_frames() {
        let g = sample();
        let t = term_with(&g.bytes, 80, 24);
        let term = t.term.lock().unwrap();
        let cwd = manifest();
        let mut h = Hover::default();
        let (mut moves, mut frames, mut plain_moves, mut plain_frames, mut entries) = (0, 0, 0, 0, 0);
        let mut prev: Option<Want> = None;
        for row in 0..24 {
            for col in 0..80 {
                let w = g.want.get(&(row, col)).cloned();
                let before = h.link.clone();
                let drew = h.point(&term, Some((row, col)), &cwd);
                assert_eq!(drew, before != h.link, "({row},{col}): a frame must mean the link under the pointer changed");
                moves += 1;
                frames += drew as u32;
                if w.is_none() && prev.is_none() {
                    plain_moves += 1;
                    plain_frames += drew as u32;
                }
                entries += (w.is_some() && w != prev) as u32;
                prev = w;
            }
        }
        h.clear();
        println!("pointer walk: {moves} moves, {frames} frames for {entries} link entries; {plain_moves} moves between non-link cells drew {plain_frames}");
        assert_eq!(plain_frames, 0);
        // Each entry draws once and each exit (to plain text) once at most.
        assert!(frames <= 2 * entries, "{frames} frames for {entries} links entered");
        // Moving back and forth inside one link owes nothing.
        let docs = g.want.iter().find(|(_, w)| **w == Want::Uri("https://example.com/docs")).map(|(k, _)| *k).unwrap();
        h.point(&term, Some(docs), &cwd);
        assert!(!h.point(&term, Some((docs.0, docs.1 + 1)), &cwd) && !h.point(&term, Some(docs), &cwd));
    }

    #[test]
    fn plain_text_finds_files_that_exist_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("proteus-links-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src").join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
        let find = |s: &str| {
            let at = s.find('@').unwrap();
            let text = s.replace('@', "");
            find_at(&text, text[..at].chars().count(), &dir).map(|(r, t)| (text.chars().skip(r.start).take(r.len()).collect::<String>(), t))
        };
        let file = |line, col| Target::File { path: dir.join("src").join("main.rs"), line: Some(line), col };
        assert_eq!(find("  --> src/m@ain.rs:12:5"), Some(("src/main.rs:12:5".into(), file(12, Some(5)))));
        assert_eq!(find("src\\main.rs(3,9)@: error TS2304"), None, "the colon after the location is not part of it");
        assert_eq!(find("src\\main.r@s(3,9): error TS2304"), Some(("src\\main.rs(3,9)".into(), file(3, Some(9)))));
        assert_eq!(find("  File \"src/main.rs\", li@ne 7, in <module>"), Some(("File \"src/main.rs\", line 7".into(), file(7, None))));
        assert_eq!(find("Makef@ile:2: *** missing separator"), Some(("Makefile:2".into(), Target::File { path: dir.join("Makefile"), line: Some(2), col: None })));
        assert_eq!(find(&format!("{}\\src\\ma@in.rs:1", dir.display())), Some((format!("{}\\src\\main.rs:1", dir.display()), file(1, None))));
        assert_eq!(find("./src/../src/main@.rs:4:1:"), Some(("./src/../src/main.rs:4:1".into(), file(4, Some(1)))));
        for none in ["src/gone@.rs:1", "at 12:3@0:15", "local@host:8080", "error@: 3", "\\\\server\\share\\x@.rs:1", "//server/share/x@.rs:1", "src/main.rs@ without a line"] {
            assert_eq!(find(none), None, "{none}");
        }
        assert_eq!(find("see https://example.com/a_(b)@x)."), Some(("https://example.com/a_(b)x".into(), Target::Web("https://example.com/a_(b)x".into()))));
        assert_eq!(find("<https://exa@mple.com/q?x=1>,"), Some(("https://example.com/q?x=1".into(), Target::Web("https://example.com/q?x=1".into()))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_web_mail_and_local_files_open_and_programs_never_run() {
        let dir = std::env::temp_dir().join(format!("proteus-links-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["notes.txt", "setup.exe", "go.bat", "run.ps1", "app.lnk", "a b.md"] {
            std::fs::write(dir.join(f), "x").unwrap();
        }
        let uri = |u: &str| action(&Target::Uri { id: "x".into(), uri: u.into() });
        let fileuri = |name: &str| format!("file:///{}", dir.join(name).display().to_string().replace('\\', "/").replace(' ', "%20"));
        assert_eq!(uri("https://example.com/x"), Action::Web("https://example.com/x".into()));
        assert_eq!(uri("HTTP://example.com"), Action::Web("HTTP://example.com".into()));
        assert_eq!(uri("mailto:a@example.com"), Action::Web("mailto:a@example.com".into()));
        assert!(matches!(uri("javascript:alert(1)"), Action::Refuse(w) if w.contains("javascript")));
        for bad in ["ms-settings:privacy", "vbscript:x", "data:text/html,x", "ftp://example.com", "\\\\server\\share", "search-ms:query=x", "noscheme"] {
            assert!(matches!(uri(bad), Action::Refuse(_)), "{bad} was not refused");
        }
        assert!(matches!(uri("https://example.com/a b"), Action::Refuse(_)));
        assert!(matches!(uri("https://example.com/\x07"), Action::Refuse(_)));
        assert_eq!(uri(&fileuri("notes.txt")), Action::Edit { path: dir.join("notes.txt"), line: None, col: None });
        assert_eq!(uri(&fileuri("a b.md")), Action::Edit { path: dir.join("a b.md"), line: None, col: None });
        for runs in ["setup.exe", "go.bat", "run.ps1", "app.lnk"] {
            assert_eq!(uri(&fileuri(runs)), Action::Reveal(dir.join(runs)), "{runs} must be shown, not run");
        }
        assert!(matches!(uri(&fileuri("missing.txt")), Action::Refuse(w) if w.starts_with("not found")));
        let local = format!("file://localhost/{}", dir.join("notes.txt").display().to_string().replace('\\', "/"));
        assert_eq!(uri(&local), Action::Edit { path: dir.join("notes.txt"), line: None, col: None });
        assert!(matches!(uri("file://fileserver.example.com/share/notes.txt"), Action::Refuse(w) if w.contains("another computer")));
        assert!(matches!(uri("file:////server/share/x.txt"), Action::Refuse(_)));
        assert_eq!(uri(&format!("file:///{}", dir.display().to_string().replace('\\', "/"))), Action::Folder(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// File URIs that spell another computer some other way, or no full path
    /// at all, are refused from the text of the address alone: the refusal
    /// must come before anything asks the disk (or the network) about them,
    /// because merely pointing at a link runs this policy for the strip.
    #[test]
    fn file_uris_naming_a_share_are_refused_without_looking() {
        let uri = |u: &str| action(&Target::Uri { id: "x".into(), uri: u.into() });
        for bad in [
            "file:///\\\\host\\share\\x.txt",
            "file:/\\\\host/share/x.txt",
            "file:///%5C%5Chost%5Cshare%5Cx.txt",
            "file:///%2F%2Fhost/share/x.txt",
            "file://localhost//host/share/x.txt",
            "file://localhost/\\\\host\\share\\x.txt",
            "file:///\\/host/share/x.txt",
            "file://127.0.0.1/C:/Windows/win.ini",
            "file:rel.txt",
            "file:///Windows/win.ini",
            "file:///C:/Windows/win.ini::$DATA",
            "file:///C:/Windows/win.ini:stream",
            "file:///C:/../../x.txt%00",
        ] {
            match uri(bad) {
                Action::Refuse(why) => assert!(!why.starts_with("not found"), "{bad}: refused only after looking ({why})"),
                other => panic!("{bad} was not refused: {other:?}"),
            }
        }
        // The ordinary spellings of a local file still open.
        assert!(matches!(uri("file:///C:/Windows/win.ini"), Action::Edit { .. }));
        assert!(matches!(uri("file:///c|/Windows/win.ini"), Action::Edit { .. } | Action::Refuse(_)));
    }

    /// Windows drops trailing dots and spaces from a name, so `go.bat.` and
    /// `go.bat ` are go.bat: they must be shown in Explorer, never opened.
    /// Types the system itself calls dangerous are caught without a list.
    #[test]
    fn a_program_is_not_opened_under_another_spelling() {
        let dir = std::env::temp_dir().join(format!("proteus-links-spell-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("go.bat"), "@echo hi\r\n").unwrap();
        std::fs::write(dir.join("x.appinstaller"), "x").unwrap();
        std::fs::write(dir.join("x.shb"), "x").unwrap();
        let uri = |u: &str| action(&Target::Uri { id: "x".into(), uri: u.into() });
        let base = format!("file:///{}", dir.display().to_string().replace('\\', "/"));
        for spelt in ["go.bat.", "go.bat%20", "go.bat.%20.", "GO.BAT"] {
            assert!(matches!(uri(&format!("{base}/{spelt}")), Action::Reveal(_)), "{spelt} was not kept from running");
        }
        // The same through plain text: `go.bat.:1` resolves to go.bat.
        let found = find_at("go.bat.:1", 2, &dir).map(|(_, t)| action(&t));
        assert!(matches!(found, None | Some(Action::Reveal(_))), "{found:?}");
        // .shb is on no list here: the system calls it dangerous (AssocIsDangerous).
        assert!(matches!(uri(&format!("{base}/x.shb")), Action::Reveal(_)), "a type Windows calls dangerous was opened");
        // .appinstaller installs packages from the web; Windows does not call it dangerous, the list does.
        assert!(matches!(uri(&format!("{base}/x.appinstaller")), Action::Reveal(_)), "an installer was opened");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ctrl+click on a link takes the press and its release, even when Ctrl
    /// is let go first; every other click reaches the program untouched.
    #[test]
    fn a_link_click_keeps_its_release_from_the_program() {
        let mut c = Click::default();
        let mut opened = 0;
        // Ctrl+click on a link: press and release are both the link's.
        assert!(c.route(true, true, || { opened += 1; true }));
        assert!(c.route(false, false, || unreachable!()), "the release went on to the program");
        // The next plain click is the program's, press and release.
        assert!(!c.route(true, false, || unreachable!()));
        assert!(!c.route(false, false, || unreachable!()));
        // Ctrl+click on plain text: the program's (it may want Ctrl+click itself).
        assert!(!c.route(true, true, || false));
        assert!(!c.route(false, true, || unreachable!()));
        // A release with no press seen (the press went to another window).
        assert!(!c.route(false, true, || unreachable!()));
        assert_eq!(opened, 1);
    }

    /// Through the real console host: a program's OSC 7 reaches the window
    /// as the working directory, and its OSC 8 link reaches the grid's cells.
    /// Everything here depends on ConPTY passing both on.
    #[test]
    fn the_console_host_passes_on_links_and_the_working_directory() {
        use crate::session::{self, SessionSpec};
        let dir = std::env::temp_dir().join(format!("proteus-links-conpty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if std::env::var_os("PROTEUS_DATA").is_none() {
            std::env::set_var("PROTEUS_DATA", &dir);
        }
        let file = dir.join("links.bin");
        std::fs::write(&file, b"\x1b]7;file://localhost/C:/Windows\x1b\\before \x1b]8;id=k;https://example.com/conpty\x1b\\a link\x1b]8;;\x1b\\ after\r\n").unwrap();
        let h = session::spawn_local(SessionSpec { exe: "cmd.exe".into(), args: vec!["/d".into(), "/c".into(), "type".into(), file.to_string_lossy().into_owned()], cwd: None, cols: 100, rows: 10, journal: false, env: None }).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let notify: crate::term::Notify = std::sync::Arc::new(move |e| {
            if matches!(e, crate::term::UiEvent::Exit(_)) {
                let _ = tx.send(());
            }
        });
        let t = crate::term::Terminal::new(100, 10, crate::theme::Theme::void_light(), notify, Some(h.cmds.clone()));
        t.attach(h.events);
        let _ = rx.recv_timeout(std::time::Duration::from_secs(20));
        let _ = std::fs::remove_file(&file);
        assert_eq!(t.cwd.lock().unwrap().as_deref(), Some("C:/Windows"), "OSC 7 did not arrive as the working directory");
        let term = t.term.lock().unwrap();
        let mut hit = None;
        'find: for row in 0..10 {
            for col in 0..100 {
                if let Some(l) = at(&term, row, col, Path::new("C:\\")) {
                    hit = Some(l);
                    break 'find;
                }
            }
        }
        let l = hit.expect("no link cell reached the grid through the console host");
        // The console host may renumber the id; the address must be intact.
        assert!(matches!(&l.target, Target::Uri { uri, .. } if uri == "https://example.com/conpty"), "{:?}", l.target);
        println!("through ConPTY: {:?}", l.target);
        assert_eq!(l.spans.iter().map(|s| s.2 - s.1 + 1).sum::<usize>(), "a link".len());
    }

    #[test]
    fn the_editor_command_keeps_the_path_one_word() {
        let p = Path::new("C:\\dir with space\\a.rs");
        assert_eq!(editor_argv("code --goto {path}:{line}:{col}", p, Some(3), Some(9)), ["code", "--goto", "C:\\dir with space\\a.rs:3:9"]);
        assert_eq!(editor_argv("\"C:\\Program Files\\ed\\ed.exe\" +{line} {path}", p, None, None), ["C:\\Program Files\\ed\\ed.exe", "+1", "C:\\dir with space\\a.rs"]);
    }
}
