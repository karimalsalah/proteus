//! Chronicle: undo for the shell.
//!
//! A content-addressed history of a working tree, so that any command by any
//! program can be undone and files rewound to any recorded moment.
//!
//!   proteus chronicle track [--max-files N] [--max-mb M]
//!   proteus chronicle status | snap [-m LABEL] [--rehash] | log [-n N]
//!   proteus chronicle diff A [B] | restore TREE [--apply] | undo [--step N] [--apply] [--force]
//!   proteus chronicle watch [DIR] [--debounce MS] [--buffer-kb N]
//!   proteus run [-m LABEL] [--rehash] -- CMD ARGS...     (exits with the command's exit code)
//! Every command takes --root DIR (default: the nearest folder above the
//! current one that holds .git, else the current folder) and --json. The
//! commands that name no tree also take the folder as a bare word.
//!
//! On disk, under `<data>/chronicle/<name>-<id>/`:
//!   root.json       which folder this is, and its budget
//!   pack-NNNN.bin   file contents and folder listings, each stored once, named
//!                   by the SHA-256 of what they hold (deflated when that helps)
//!   objects.idx     where each object is; can be rebuilt from the packs
//!   log.jsonl       append-only: snapshots {tree, times, time, label, parent} and
//!                   steps {command, exit code, duration, tree before, tree after}
//!   cache.bin       for each file last read: its size, when it was written,
//!                   when anything about it changed and which file it is, so
//!                   an unchanged file is not read again
//!   lock            held by whichever process is writing
//!
//! A TREE ID is the SHA-256 of the listing of the root folder. A listing
//! names each file by the hash of its content and each folder by the hash of
//! its own listing, so one id fixes the content of every file below it, and
//! a change to one file stores one new listing per folder on its path.
//!
//! What is not in a tree, and so is never restored and never deleted:
//! anything the ignore rules exclude (.gitignore, .git/info/exclude, the
//! global git ignore file, .proteusignore), version-control folders (.git,
//! .hg, .svn, .jj), and links (symbolic links and junctions are reported and
//! never followed). Modified times are not part of a tree either: they are
//! recorded beside it with every snapshot, and a restored file is written
//! now, as `git checkout` does, so build tools rebuild it.
//!
//! The rule restore and undo keep: no file content on disk is overwritten or
//! deleted unless that exact content is already in the store. Each file is
//! read again, with writers shut out, at the moment it is replaced or deleted.
//! Content means the file's main stream. A file that is replaced keeps its
//! permissions, named streams and creation time (they are carried over to
//! the new content); a file that is deleted loses them with it, and if a
//! later restore brings it back it has the permissions of its folder.
//!
//! A dry run looks at the disk for everything `--apply` would refuse (links
//! on the way, things outside the tree in the way, files held open for
//! writing, content the store no longer has, folders that cannot be emptied
//! or that a program is using) and lists it, so what it prints is what
//! `--apply` does. Every file is written under a temporary name
//! (`.proteus-restore-<pid>-<n>.tmp`) beside its path and moved in whole; a
//! restore that is killed leaves such files, and the next snapshot or
//! `--apply` removes them.
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{FileExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use windows_sys::Win32::Foundation::{CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_FILES, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FileBasicInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, GetFileInformationByHandle, GetFileInformationByHandleEx,
    GetVolumeInformationByHandleW, MoveFileExW, ReplaceFileW, SetFileAttributesW, BY_HANDLE_FILE_INFORMATION, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_FLAG_SEQUENTIAL_SCAN, FILE_ID_BOTH_DIR_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, OPEN_EXISTING,
};

use crate::watch;

pub type Hash = [u8; 32];
type R<T> = Result<T, String>;

const A_READONLY: u32 = 0x1;
const A_HIDDEN: u32 = 0x2;
const A_SYSTEM: u32 = 0x4;
const A_DIRECTORY: u32 = 0x10;
const A_REPARSE: u32 = 0x400;
const A_OFFLINE: u32 = 0x1000;
const A_RECALL: u32 = 0x40_0000 | 0x4_0000;
/// The attributes a tree records and restore puts back. (ARCHIVE is set by
/// Windows on every write, so it says nothing.)
const KEEP_ATTRS: u32 = A_READONLY | A_HIDDEN | A_SYSTEM;

/// Set in the tag of a link that stands for another path (symbolic link, junction).
const TAG_NAME_SURROGATE: u32 = 0x2000_0000;
const SHARE_ALL: u32 = 7;
const SHARE_READ: u32 = 1;
const SHARE_DELETE: u32 = 4;

/// Files up to this size are read whole; larger ones are streamed.
const SMALL: u64 = 8 << 20;
/// A pack file is closed at this size and a new one begun.
const PACK_ROLL: u64 = if cfg!(test) { 256 << 10 } else { 1 << 30 };
/// A file modified this recently (100 ns units) is read again next time even
/// if its size and time look the same: a second write inside one clock tick
/// (or inside FAT's two-second stamps) would otherwise go unseen.
const RACY: u64 = 2 * 10_000_000;

pub const DEFAULT_MAX_FILES: u64 = 200_000;
pub const DEFAULT_MAX_MB: u64 = 8192;

fn hex(h: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(h.len() * 2);
    for b in h {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> Option<Hash> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut h = [0u8; 32];
    for (i, b) in h.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(h)
}

fn short(h: &Hash) -> String {
    hex(&h[..6])
}

fn sha(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}

/// Now, as Windows stamps files: 100 ns units since 1601.
fn now_ft() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| (d.as_nanos() / 100) as u64).unwrap_or(0) + 116_444_736_000_000_000
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// "2026-11-16 03:12:01Z" from milliseconds since 1970.
fn stamp(ms: u64) -> String {
    let s = ms / 1000;
    let (days, rem) = ((s / 86400) as i64, s % 86400);
    // Days to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (d, m) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A path as a person writes it: without the `\\?\` that Windows adds.
fn show(p: &Path) -> String {
    let s = p.display().to_string();
    match s.strip_prefix(r"\\?\UNC\") {
        Some(rest) => format!(r"\\{rest}"),
        None => s.strip_prefix(r"\\?\").unwrap_or(&s).to_string(),
    }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------- paths

/// One name inside a tree. Anything Windows would read as something else is
/// refused: `..`, separators, drive letters and alternate data streams (both
/// need `:`), device names, and names Windows silently trims.
pub fn valid_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("an empty name");
    }
    if name == "." || name == ".." {
        return Err("a dot name");
    }
    if name.encode_utf16().count() > 255 {
        return Err("a name longer than 255 characters");
    }
    if name.chars().any(|c| (c as u32) < 0x20 || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')) {
        return Err("a character Windows does not allow in a name");
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("a trailing dot or space, which Windows drops");
    }
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ').to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || (stem.chars().count() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.chars().nth(3).is_some_and(|c| c.is_ascii_digit() || matches!(c, '\u{b9}' | '\u{b2}' | '\u{b3}')));
    if device {
        return Err("a Windows device name");
    }
    Ok(())
}

/// A path inside a tree: names joined by `/`, relative, nothing that climbs out.
pub fn valid_rel(path: &str) -> R<()> {
    if path.is_empty() {
        return Err("an empty path".into());
    }
    for part in path.split('/') {
        valid_name(part).map_err(|why| format!("{path:?} has {why}"))?;
    }
    Ok(())
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
    }
    p
}

fn parent_rel(rel: &str) -> &str {
    rel.rfind('/').map(|i| &rel[..i]).unwrap_or("")
}

/// `--root`, or the nearest folder at or above `cwd` that holds `.git`, or `cwd`.
pub fn find_root(given: Option<&str>, cwd: &Path) -> PathBuf {
    if let Some(g) = given {
        return PathBuf::from(g);
    }
    cwd.ancestors().find(|a| a.join(".git").exists()).unwrap_or(cwd).to_path_buf()
}

fn canon(p: &Path) -> R<PathBuf> {
    fs::canonicalize(p).map_err(|e| format!("{}: {e}", show(p)))
}

/// Folders that are never tracked: a whole drive, the user profile or
/// anything above it, and PROTEUS's own data.
pub fn check_root(root: &Path, data: &Path, profile: Option<&Path>) -> R<()> {
    if !root.is_dir() {
        return Err(format!("{} is not a folder", show(root)));
    }
    if root.parent().is_none() {
        return Err(format!("refusing to track {}: it is a whole drive or share", show(root)));
    }
    if let Some(profile) = profile.and_then(|p| fs::canonicalize(p).ok()) {
        if profile.starts_with(root) {
            return Err(format!("refusing to track {}: it is the user profile or holds it. Track one project folder.", show(root)));
        }
    }
    if let Ok(data) = fs::canonicalize(data) {
        if root.starts_with(&data) {
            return Err(format!("refusing to track {}: it is inside PROTEUS's own data folder", show(root)));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- what the file system says

/// What tells one state of a file from another without reading it: when it
/// was last written; when anything about it last changed (Windows keeps
/// that separately, and the call programs use to put a modified time back,
/// SetFileTime, does not touch it); and which file on the volume it is
/// (another file moved onto the same path is a different one).
///
/// Two kinds of write get past all three, measured on NTFS: one made
/// through a memory-mapped view (Windows changes neither the size nor any
/// time, not even after the file is closed), and one by a program that
/// puts the change time back as well (it can, through FileBasicInfo). Only
/// reading the file sees those: `snap --rehash`, `run --rehash`. Restore
/// does read: it never replaces a file on the word of these stamps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stamp {
    pub mtime: u64,
    pub ctime: u64,
    pub id: u64,
}

impl Stamp {
    /// Do two readings describe the same state? A file system that keeps no
    /// change time or file ids reports zero there, and zero matches anything.
    fn agrees(&self, o: &Stamp) -> bool {
        self.mtime == o.mtime && (self.ctime == 0 || o.ctime == 0 || self.ctime == o.ctime) && (self.id == 0 || o.id == 0 || self.id == o.id)
    }
}

struct Stat {
    attrs: u32,
    size: u64,
    stamp: Stamp,
    /// How many names (hard links) the file has.
    links: u32,
}

fn stat_handle(h: HANDLE) -> io::Result<Stat> {
    let mut basic: FILE_BASIC_INFO = unsafe { std::mem::zeroed() };
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetFileInformationByHandleEx(h, FileBasicInfo, (&mut basic as *mut FILE_BASIC_INFO).cast(), std::mem::size_of::<FILE_BASIC_INFO>() as u32) != 0
            && GetFileInformationByHandle(h, &mut info) != 0
    };
    if !ok {
        return Err(io::Error::last_os_error());
    }
    Ok(Stat {
        attrs: info.dwFileAttributes,
        size: (info.nFileSizeHigh as u64) << 32 | info.nFileSizeLow as u64,
        stamp: Stamp { mtime: basic.LastWriteTime as u64, ctime: basic.ChangeTime as u64, id: (info.nFileIndexHigh as u64) << 32 | info.nFileIndexLow as u64 },
        links: info.nNumberOfLinks,
    })
}

/// What is at `p` itself: a link is reported as a link, never as its target.
fn stat_path(p: &Path) -> io::Result<Stat> {
    let w = wide(p);
    let h = unsafe {
        CreateFileW(w.as_ptr(), FILE_READ_ATTRIBUTES, SHARE_ALL, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let r = stat_handle(h);
    unsafe { CloseHandle(h) };
    r
}

struct Listed {
    /// Err: the name is not valid Unicode (shown as closely as it can be).
    name: Result<String, String>,
    attrs: u32,
    size: u64,
    stamp: Stamp,
    /// For a link, what kind it is.
    tag: u32,
}

/// The entries of one folder, with everything Windows keeps in the listing
/// itself: no file is opened. If `abs` is a link, the link is listed (it is
/// empty), not what it points at.
fn list_dir(abs: &Path, buf: &mut Vec<u64>) -> io::Result<Vec<Listed>> {
    let w = wide(abs);
    let h = unsafe {
        CreateFileW(w.as_ptr(), FILE_LIST_DIRECTORY, SHARE_ALL, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut())
    };
    if h == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // 64 KB, aligned for the 64-bit fields of the records; the caller keeps
    // it from one folder to the next.
    buf.resize(8192, 0);
    let bytes = buf.len() * 8;
    let name_at = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
    let mut out = Vec::new();
    let mut class = FileIdBothDirectoryRestartInfo;
    let result = loop {
        if unsafe { GetFileInformationByHandleEx(h, class, buf.as_mut_ptr().cast(), bytes as u32) } == 0 {
            let e = io::Error::last_os_error();
            let done = [ERROR_NO_MORE_FILES as i32, ERROR_FILE_NOT_FOUND as i32].contains(&e.raw_os_error().unwrap_or(0));
            break if done { Ok(()) } else { Err(e) };
        }
        class = FileIdBothDirectoryInfo;
        let base = buf.as_ptr() as *const u8;
        let mut at = 0usize;
        while at + name_at <= bytes {
            let rec = unsafe { &*(base.add(at) as *const FILE_ID_BOTH_DIR_INFO) };
            let len = (rec.FileNameLength as usize).min(bytes - at - name_at);
            let name = unsafe { std::slice::from_raw_parts(base.add(at + name_at) as *const u16, len / 2) };
            if name != [46] && name != [46, 46] {
                out.push(Listed {
                    name: String::from_utf16(name).map_err(|_| String::from_utf16_lossy(name)),
                    attrs: rec.FileAttributes,
                    size: rec.EndOfFile as u64,
                    stamp: Stamp { mtime: rec.LastWriteTime as u64, ctime: rec.ChangeTime as u64, id: rec.FileId as u64 },
                    tag: if rec.FileAttributes & A_REPARSE != 0 { rec.EaSize } else { 0 },
                });
            }
            if rec.NextEntryOffset == 0 {
                break;
            }
            at += rec.NextEntryOffset as usize;
        }
    };
    unsafe { CloseHandle(h) };
    result.map(|_| out)
}

// ---------------------------------------------------------------- ignore rules

/// One ignore file, compiled. Letter case is ignored, as git does on Windows.
fn rule_file(dir: &Path, file: &Path, failures: &mut Vec<Failure>) -> Option<Gitignore> {
    let mut b = GitignoreBuilder::new(dir);
    let _ = b.case_insensitive(true);
    if let Some(e) = b.add(file) {
        failures.push(Failure { path: show(file), what: format!("ignore rules: {e}") });
    }
    b.build().ok().filter(|g| !g.is_empty())
}

/// Where git keeps a repository's private ignore file, for the `.git` in
/// `dir`: a folder, or (in a worktree) a file naming the real one.
fn exclude_file(dir: &Path) -> Option<PathBuf> {
    let dot = dir.join(".git");
    let git_dir = if dot.is_dir() {
        dot
    } else {
        let text = fs::read_to_string(&dot).ok()?;
        dir.join(text.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim())
    };
    let common = fs::read_to_string(git_dir.join("commondir")).ok().map(|c| git_dir.join(c.trim())).unwrap_or(git_dir);
    let f = common.join("info").join("exclude");
    f.is_file().then_some(f)
}

/// The ignore rules in force inside one folder: what its own files say,
/// then what its parent's say. Shared by every folder below that adds none.
struct Rules {
    /// .proteusignore here.
    own: Option<Gitignore>,
    /// .gitignore here.
    git: Option<Gitignore>,
    /// .git/info/exclude, when a repository starts here.
    exclude: Option<Gitignore>,
    /// A git repository starts here, and git's rules from above stop here.
    repo: bool,
    parent: Option<Arc<Rules>>,
}

impl Rules {
    /// The rules for `dir`, given which of (.gitignore, .proteusignore, .git)
    /// it holds. A folder that adds nothing shares its parent's.
    fn load(dir: &Path, has: (bool, bool, bool), bounded: bool, parent: Option<Arc<Rules>>, failures: &mut Vec<Failure>) -> Option<Arc<Rules>> {
        let mut file = |yes: bool, name: &str| {
            let f = dir.join(name);
            if yes && f.is_file() {
                rule_file(dir, &f, failures)
            } else {
                None
            }
        };
        let (git, own) = (file(has.0, ".gitignore"), file(has.1, ".proteusignore"));
        let dot_git = has.2 && dir.join(".git").exists();
        let exclude = if dot_git { exclude_file(dir).and_then(|f| rule_file(dir, &f, failures)) } else { None };
        let repo = dot_git && bounded;
        if git.is_none() && own.is_none() && exclude.is_none() && !repo {
            return parent;
        }
        Some(Arc::new(Rules { own, git, exclude, repo, parent }))
    }
}

/// What holds for a whole walk.
struct RuleBase {
    /// From folders above the root, when the root is inside a git repository.
    above: Option<Arc<Rules>>,
    /// The user's global git ignore file.
    global: Option<Gitignore>,
    /// Inside a git repository: a nested repository starts its own rules.
    bounded: bool,
    hermetic: bool,
}

impl RuleBase {
    /// In a git repository: what git would ignore, up to the top of that
    /// repository, and the user's global file. Outside one: the ignore
    /// files in the tree, and the global file. `hermetic`: the tree's own
    /// files and nothing else.
    fn new(root: &Path, hermetic: bool, failures: &mut Vec<Failure>) -> RuleBase {
        if hermetic {
            return RuleBase { above: None, global: None, bounded: false, hermetic };
        }
        let top = root.ancestors().find(|a| a.join(".git").exists());
        let mut above = None;
        if let Some(top) = top {
            let mut chain: Vec<&Path> = root.ancestors().skip(1).take_while(|a| a.starts_with(top)).collect();
            chain.reverse();
            for dir in chain {
                above = Rules::load(dir, (true, true, dir == top), true, above, failures);
            }
        }
        let mut b = GitignoreBuilder::new(root);
        let _ = b.case_insensitive(true);
        let (global, _) = b.build_global();
        RuleBase { above, global: Some(global).filter(|g| !g.is_empty()), bounded: top.is_some(), hermetic }
    }

    /// The rules inside `dir`, whose listing showed these three files.
    fn enter(&self, dir: &Path, has: (bool, bool, bool), parent: Option<Arc<Rules>>, failures: &mut Vec<Failure>) -> Option<Arc<Rules>> {
        if !(has.0 || has.1 || has.2 && !self.hermetic) {
            return parent;
        }
        Rules::load(dir, (has.0, has.1, has.2 && !self.hermetic), self.bounded, parent, failures)
    }

    /// Is the entry at `abs` ignored, by the rules of the folder it is in?
    /// Nearest file first; .proteusignore before .gitignore before git's
    /// private file before the global one; a `!` rule lets a path back in.
    fn ignored(&self, rules: Option<&Rules>, abs: &Path, is_dir: bool) -> bool {
        use ignore::Match;
        let (mut own, mut git, mut exclude): (Match<()>, Match<()>, Match<()>) = (Match::None, Match::None, Match::None);
        let mut git_applies = true;
        let mut node = rules;
        while let Some(n) = node {
            if own.is_none() {
                if let Some(g) = &n.own {
                    own = g.matched(abs, is_dir).map(|_| ());
                }
            }
            if git_applies {
                if git.is_none() {
                    if let Some(g) = &n.git {
                        git = g.matched(abs, is_dir).map(|_| ());
                    }
                }
                if exclude.is_none() {
                    if let Some(g) = &n.exclude {
                        exclude = g.matched(abs, is_dir).map(|_| ());
                    }
                }
                git_applies = !n.repo;
            }
            node = n.parent.as_deref();
        }
        let global = self.global.as_ref().map(|g| g.matched(abs, is_dir).map(|_| ())).unwrap_or(Match::None);
        own.or(git).or(exclude).or(global).is_ignore()
    }
}

// ---------------------------------------------------------------- trees

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    /// It was there, but its content could not be read (locked, access
    /// denied, not downloaded). Recorded so that no tree pretends it was absent.
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub kind: Kind,
    /// Read-only, hidden and system bits.
    pub attrs: u32,
    pub size: u64,
    pub hash: Hash,
    /// As it stood on disk when captured. Not part of the tree's identity:
    /// it is how restore tells that the file is still the one that was read.
    pub stamp: Stamp,
}

/// Every path in a tree (`/` between names), sorted.
pub type Manifest = BTreeMap<String, Entry>;

/// The same thing in two trees. An unreadable entry is the same as nothing.
fn same(a: Option<&Entry>, b: Option<&Entry>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.kind == b.kind && a.kind != Kind::Unknown && a.attrs == b.attrs && a.size == b.size && a.hash == b.hash,
        _ => false,
    }
}

const TREE_HEAD: &str = "proteus-tree 1\n";

/// One listing per folder of `m`, deepest first, and the id of the root's.
/// `line` writes one entry; for a folder it is given the id of that folder's
/// own listing, which is how a listing fixes everything below it.
fn listings(m: &Manifest, head: &str, line: impl Fn(&str, &Entry, Option<&Hash>) -> String) -> (Hash, Vec<(Hash, Vec<u8>)>) {
    let mut children: BTreeMap<&str, Vec<(&str, &Entry)>> = BTreeMap::new();
    children.insert("", Vec::new());
    for (path, e) in m {
        let parent = parent_rel(path);
        let name = &path[if parent.is_empty() { 0 } else { parent.len() + 1 }..];
        children.entry(parent).or_default().push((name, e));
        if e.kind == Kind::Dir {
            children.entry(path).or_default();
        }
    }
    let mut dirs: Vec<&str> = children.keys().copied().collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(if d.is_empty() { 0 } else { d.matches('/').count() + 1 }));
    let mut ids: HashMap<&str, Hash> = HashMap::new();
    let mut out = Vec::with_capacity(dirs.len());
    for dir in dirs {
        let mut text = String::from(head);
        for (name, e) in &children[dir] {
            let below = (e.kind == Kind::Dir).then(|| if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") });
            text.push_str(&line(name, e, below.map(|b| &ids[b.as_str()])));
        }
        let bytes = text.into_bytes();
        let id = sha(&bytes);
        ids.insert(dir, id);
        out.push((id, bytes));
    }
    (ids[""], out)
}

/// The tree: what is where, by content.
///   f ATTRS SIZE HASH NAME    a file
///   d ATTRS TREE NAME         a folder, by the id of its own listing
///   x ATTRS SIZE NAME         something that could not be read
fn build_trees(m: &Manifest) -> (Hash, Vec<(Hash, Vec<u8>)>) {
    listings(m, TREE_HEAD, |name, e, below| match (e.kind, below) {
        (Kind::Dir, Some(id)) => format!("d {:x} {} {name}\n", e.attrs, hex(id)),
        (Kind::File, _) => format!("f {:x} {} {} {name}\n", e.attrs, e.size, hex(&e.hash)),
        _ => format!("x {:x} {} {name}\n", e.attrs, e.size),
    })
}

const TIMES_HEAD: &str = "proteus-times 1\n";

/// When each file was last written, as a snapshot found it. Kept beside the
/// tree, not in it: a tree names content, and touching a file must not make
/// a new one. Same shape as the tree, so folders nothing was written in
/// cost nothing from one snapshot to the next.
///   f MODIFIED NAME    (100 ns units since 1601, as Windows stamps files)
///   d TIMES NAME
fn build_times(m: &Manifest) -> (Hash, Vec<(Hash, Vec<u8>)>) {
    listings(m, TIMES_HEAD, |name, e, below| match below {
        Some(id) => format!("d {} {name}\n", hex(id)),
        None => format!("f {} {name}\n", e.stamp.mtime),
    })
}

/// Read the modified times recorded with a snapshot: path -> time.
fn load_times(objects: &Objects, id: &Hash, prefix: &str, depth: usize, out: &mut BTreeMap<String, u64>) -> R<()> {
    if depth > 200 {
        return Err("the times are nested more than 200 folders deep".into());
    }
    let bytes = objects.get(id).map_err(|e| format!("times {}: {e}", short(id)))?;
    let text = std::str::from_utf8(&bytes).ok().and_then(|t| t.strip_prefix(TIMES_HEAD)).ok_or_else(|| format!("{} is not a list of times", short(id)))?;
    for line in text.split_terminator('\n') {
        let mut parts = line.splitn(3, ' ');
        let (kind, value, name) = (parts.next(), parts.next(), parts.next().filter(|n| valid_name(n).is_ok()));
        let path = |name: &str| if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
        match (kind, value, name) {
            (Some("f"), Some(v), Some(name)) => {
                out.insert(path(name), v.parse().map_err(|_| format!("times {} has a line that cannot be read", short(id)))?);
            }
            (Some("d"), Some(v), Some(name)) => {
                let below = unhex(v).ok_or_else(|| format!("times {} has a line that cannot be read", short(id)))?;
                load_times(objects, &below, &path(name), depth + 1, out)?;
            }
            _ => return Err(format!("times {} has a line that cannot be read", short(id))),
        }
    }
    Ok(())
}

/// A Windows file time as milliseconds since 1970.
fn ft_to_unix_ms(ft: u64) -> u64 {
    ft.saturating_sub(116_444_736_000_000_000) / 10_000
}

/// Read one folder listing into `out`, and the folders below it. Every name
/// is checked: a listing is data from disk and is not trusted to stay inside
/// the root.
fn load_tree(objects: &Objects, id: &Hash, prefix: &str, depth: usize, out: &mut Manifest) -> R<()> {
    if depth > 200 {
        return Err("the tree is nested more than 200 folders deep".into());
    }
    let bytes = objects.get(id).map_err(|e| format!("tree {}: {e}", short(id)))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| format!("tree {} is not text", short(id)))?;
    let body = text.strip_prefix(TREE_HEAD).ok_or_else(|| format!("{} is not a tree listing", short(id)))?;
    let bad = |line: &str| format!("tree {} has a line that cannot be read: {line:?}", short(id));
    let mut last = "";
    for line in body.split_terminator('\n') {
        let mut parts = line.splitn(if line.starts_with('f') { 5 } else { 4 }, ' ');
        let kind = parts.next().ok_or_else(|| bad(line))?;
        let attrs = parts.next().and_then(|a| u32::from_str_radix(a, 16).ok()).ok_or_else(|| bad(line))?;
        if attrs & !KEEP_ATTRS != 0 {
            return Err(bad(line));
        }
        let (kind, size, hash) = match kind {
            "f" => {
                let size = parts.next().and_then(|s| s.parse::<u64>().ok()).ok_or_else(|| bad(line))?;
                (Kind::File, size, parts.next().and_then(unhex).ok_or_else(|| bad(line))?)
            }
            "d" => (Kind::Dir, 0, parts.next().and_then(unhex).ok_or_else(|| bad(line))?),
            "x" => (Kind::Unknown, parts.next().and_then(|s| s.parse::<u64>().ok()).ok_or_else(|| bad(line))?, [0u8; 32]),
            _ => return Err(bad(line)),
        };
        let name = parts.next().ok_or_else(|| bad(line))?;
        valid_name(name).map_err(|why| format!("tree {} names {name:?}: {why}. Refused.", short(id)))?;
        if name <= last {
            return Err(format!("tree {} is not in order at {name:?}", short(id)));
        }
        last = name;
        let path = if prefix.is_empty() { name.to_string() } else { format!("{prefix}/{name}") };
        if kind == Kind::Dir {
            out.insert(path.clone(), Entry { kind, attrs, size: 0, hash: [0u8; 32], stamp: Stamp::default() });
            load_tree(objects, &hash, &path, depth + 1, out)?;
        } else {
            out.insert(path, Entry { kind, attrs, size, hash, stamp: Stamp::default() });
        }
    }
    Ok(())
}

pub struct Change<'a> {
    pub path: &'a str,
    pub before: Option<&'a Entry>,
    pub after: Option<&'a Entry>,
}

impl Change<'_> {
    pub fn what(&self) -> &'static str {
        match (self.before, self.after) {
            (None, _) => "added",
            (_, None) => "removed",
            (Some(a), Some(b)) if a.kind != b.kind => "type",
            (Some(a), Some(b)) if a.kind == Kind::Unknown || b.kind == Kind::Unknown => "unreadable",
            (Some(a), Some(b)) if a.hash != b.hash || a.size != b.size => "modified",
            _ => "attributes",
        }
    }

    fn json(&self) -> Value {
        let size = |e: Option<&Entry>| e.filter(|e| e.kind != Kind::Dir).map(|e| e.size);
        let dir = self.after.or(self.before).is_some_and(|e| e.kind == Kind::Dir);
        json!({ "path": self.path, "change": self.what(), "folder": dir, "size_before": size(self.before), "size_after": size(self.after) })
    }
}

/// Every path that differs between two trees.
pub fn diff<'a>(a: &'a Manifest, b: &'a Manifest) -> Vec<Change<'a>> {
    let mut out = Vec::new();
    let (mut ia, mut ib) = (a.iter().peekable(), b.iter().peekable());
    loop {
        let order = match (ia.peek(), ib.peek()) {
            (None, None) => break,
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (Some((pa, _)), Some((pb, _))) => pa.cmp(pb),
        };
        match order {
            std::cmp::Ordering::Less => {
                let (p, e) = ia.next().unwrap();
                out.push(Change { path: p, before: Some(e), after: None });
            }
            std::cmp::Ordering::Greater => {
                let (p, e) = ib.next().unwrap();
                out.push(Change { path: p, before: None, after: Some(e) });
            }
            std::cmp::Ordering::Equal => {
                let ((p, ea), (_, eb)) = (ia.next().unwrap(), ib.next().unwrap());
                if !same(Some(ea), Some(eb)) {
                    out.push(Change { path: p, before: Some(ea), after: Some(eb) });
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------- objects

const REC_MAGIC: &[u8; 4] = b"PRTO";
/// magic 4 | flags 1 | zero 3 | raw length 8 | stored length 8 | hash 32
const REC_HEAD: u64 = 56;
/// hash 32 | pack 4 | flags 4 | data offset 8 | stored length 8 | raw length 8
const IDX_REC: usize = 64;
const F_DEFLATE: u8 = 1;

#[derive(Clone, Copy)]
struct Loc {
    pack: u32,
    off: u64,
    stored: u64,
    raw: u64,
    flags: u8,
}

fn rec_head(hash: &Hash, flags: u8, raw: u64, stored: u64) -> [u8; REC_HEAD as usize] {
    let mut h = [0u8; REC_HEAD as usize];
    h[..4].copy_from_slice(REC_MAGIC);
    h[4] = flags;
    h[8..16].copy_from_slice(&raw.to_le_bytes());
    h[16..24].copy_from_slice(&stored.to_le_bytes());
    h[24..56].copy_from_slice(hash);
    h
}

fn idx_rec(hash: &Hash, l: &Loc) -> [u8; IDX_REC] {
    let mut r = [0u8; IDX_REC];
    r[..32].copy_from_slice(hash);
    r[32..36].copy_from_slice(&l.pack.to_le_bytes());
    r[36] = l.flags;
    r[40..48].copy_from_slice(&l.off.to_le_bytes());
    r[48..56].copy_from_slice(&l.stored.to_le_bytes());
    r[56..64].copy_from_slice(&l.raw.to_le_bytes());
    r
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}

fn deflate(raw: &[u8]) -> Option<Vec<u8>> {
    let mut e = flate2::write::DeflateEncoder::new(Vec::with_capacity(raw.len() / 3 + 64), flate2::Compression::fast());
    e.write_all(raw).ok()?;
    e.finish().ok()
}

/// What is judged first, so that data which is already compressed costs
/// one small attempt and no more.
const SAMPLE: usize = 64 << 10;

/// Worth deflating? Judged on the first 64 KB.
fn shrinks(sample: &[u8]) -> bool {
    let sample = &sample[..sample.len().min(SAMPLE)];
    sample.len() >= 256 && deflate(sample).is_some_and(|z| z.len() * 10 <= sample.len() * 9)
}

/// How an object will sit in a pack: deflated if that saves a tenth or more.
fn encode(raw: &[u8]) -> (u8, Cow<'_, [u8]>) {
    if raw.len() >= 256 && (raw.len() <= SAMPLE || shrinks(raw)) {
        if let Some(z) = deflate(raw) {
            if z.len() * 10 <= raw.len() * 9 {
                return (F_DEFLATE, Cow::Owned(z));
            }
        }
    }
    (0, Cow::Borrowed(raw))
}

/// Reads `left` bytes of a pack from `pos`, wherever the file's own cursor is.
struct Span<'a> {
    file: &'a File,
    pos: u64,
    left: u64,
}

impl Read for Span<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let want = (buf.len() as u64).min(self.left) as usize;
        if want == 0 {
            return Ok(0);
        }
        let n = self.file.seek_read(&mut buf[..want], self.pos)?;
        self.pos += n as u64;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Read the object that lies at `l` in the pack `file` through to `out`,
/// checking on the way that it is what its name says: `hash`, and as long as
/// it was recorded to be.
fn read_out(file: &File, l: &Loc, hash: &Hash, out: &mut dyn Write) -> io::Result<u64> {
    let span = Span { file, pos: l.off, left: l.stored };
    let mut src: Box<dyn Read + '_> = if l.flags & F_DEFLATE != 0 { Box::new(flate2::read::DeflateDecoder::new(span)) } else { Box::new(span) };
    let damaged = || io::Error::new(io::ErrorKind::InvalidData, "its content is damaged in the store (it no longer matches its hash)");
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; (l.raw.clamp(1, 1 << 20)) as usize];
    let mut total = 0u64;
    loop {
        let n = src.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        // Longer than it was recorded to be: stop here, whatever a
        // deflate stream that someone tampered with would expand to.
        if total > l.raw {
            return Err(damaged());
        }
        hasher.update(&chunk[..n]);
        out.write_all(&chunk[..n])?;
    }
    let got: Hash = hasher.finalize().into();
    if &got != hash || total != l.raw {
        return Err(damaged());
    }
    Ok(total)
}

/// Fill `buf` unless the file ends first.
fn read_full(f: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match f.read(&mut buf[n..])? {
            0 => break,
            k => n += k,
        }
    }
    Ok(n)
}

/// Everything ever captured, each distinct content once.
pub struct Objects {
    dir: PathBuf,
    map: HashMap<Hash, Loc>,
    loaded: bool,
    /// The pack being appended to: its number, file and length (written or not).
    pack: Option<(u32, File, u64)>,
    /// Appended objects not yet written to the pack file.
    buf: Vec<u8>,
    idx_pending: Vec<u8>,
    pub added: u64,
    pub added_bytes: u64,
}

impl Objects {
    fn new(dir: &Path) -> Objects {
        Objects { dir: dir.to_path_buf(), map: HashMap::new(), loaded: false, pack: None, buf: Vec::new(), idx_pending: Vec::new(), added: 0, added_bytes: 0 }
    }

    fn pack_path(&self, id: u32) -> PathBuf {
        self.dir.join(format!("pack-{id:04}.bin"))
    }

    fn packs(&self) -> BTreeMap<u32, u64> {
        let mut out = BTreeMap::new();
        for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let id = name.strip_prefix("pack-").and_then(|s| s.strip_suffix(".bin")).and_then(|s| s.parse::<u32>().ok());
            if let (Some(id), Ok(md)) = (id, e.metadata()) {
                out.insert(id, md.len());
            }
        }
        out
    }

    /// Read the index. Objects that reached a pack but not the index (the
    /// process died in between) are found by reading the pack from where the
    /// index stops. With `repair` (the caller holds the store's lock) each
    /// of those is read through and checked against its hash before it is
    /// believed, the index is brought up to date and a torn tail is cut off.
    fn load(&mut self, repair: bool) -> io::Result<()> {
        if self.loaded {
            return Ok(());
        }
        // The index first, then the packs' sizes: a writer fills the pack
        // before it names anything in the index, so read in this order the
        // packs always hold what the index names.
        let idx_path = self.dir.join("objects.idx");
        let bytes = match fs::read(&idx_path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        let packs = self.packs();
        let mut ends: BTreeMap<u32, u64> = packs.keys().map(|k| (*k, 0)).collect();
        let mut good = 0usize;
        for r in bytes.chunks_exact(IDX_REC) {
            let hash: Hash = r[..32].try_into().unwrap();
            let l = Loc { pack: u32::from_le_bytes(r[32..36].try_into().unwrap()), flags: r[36], off: u64_at(r, 40), stored: u64_at(r, 48), raw: u64_at(r, 56) };
            let end = l.off.saturating_add(l.stored);
            if l.off < REC_HEAD || packs.get(&l.pack).is_none_or(|len| end > *len) {
                break;
            }
            self.map.insert(hash, l);
            let e = ends.entry(l.pack).or_insert(0);
            *e = (*e).max(end);
            good += IDX_REC;
        }
        let mut adopted = Vec::new();
        for (&id, &len) in &packs {
            let mut at = ends[&id];
            if at >= len {
                continue;
            }
            let f = File::open(self.pack_path(id))?;
            let mut head = [0u8; REC_HEAD as usize];
            // Where the last record that can be relied on ends.
            let mut sound = at;
            while at + REC_HEAD <= len && f.seek_read(&mut head, at)? == head.len() && &head[..4] == REC_MAGIC {
                let l = Loc { pack: id, off: at + REC_HEAD, stored: u64_at(&head, 16), raw: u64_at(&head, 8), flags: head[4] };
                if l.off.saturating_add(l.stored) > len {
                    break;
                }
                let hash: Hash = head[24..56].try_into().unwrap();
                at = l.off + l.stored;
                // A record the index never named was not yet known to be on
                // disk when its writer stopped. After a power cut it can have
                // its header and its length and not its content. A writer is
                // about to rely on what it adopts (the same content will not
                // be stored a second time), so it reads each one through
                // first. One that is not what its header says is passed over:
                // it stays unknown, and is cut off below if nothing sound
                // follows it.
                if repair && read_out(&f, &l, &hash, &mut io::sink()).is_err() {
                    continue;
                }
                adopted.extend_from_slice(&idx_rec(&hash, &l));
                self.map.insert(hash, l);
                sound = at;
            }
            ends.insert(id, sound);
        }
        if repair {
            if good != bytes.len() {
                let mut whole = bytes[..good].to_vec();
                whole.extend_from_slice(&adopted);
                let tmp = self.dir.join("objects.idx.tmp");
                fs::write(&tmp, &whole)?;
                fs::rename(&tmp, &idx_path)?;
            } else if !adopted.is_empty() {
                let mut f = OpenOptions::new().create(true).append(true).open(&idx_path)?;
                f.write_all(&adopted)?;
                f.sync_data()?;
            }
            for (&id, &len) in &packs {
                if ends[&id] < len {
                    OpenOptions::new().write(true).open(self.pack_path(id))?.set_len(ends[&id])?;
                }
            }
        }
        self.loaded = true;
        Ok(())
    }

    fn has(&self, hash: &Hash) -> bool {
        self.map.contains_key(hash)
    }

    /// The pack to append to, opened on first use.
    fn writer(&mut self) -> io::Result<()> {
        let roll = matches!(&self.pack, Some((_, _, len)) if *len >= PACK_ROLL);
        if roll {
            self.flush()?;
            if let Some((_, f, _)) = &self.pack {
                f.sync_data()?;
            }
        }
        if self.pack.is_none() || roll {
            let id = match &self.pack {
                Some((id, _, _)) => id + 1,
                None => match self.packs().iter().next_back() {
                    Some((id, len)) if *len >= PACK_ROLL => id + 1,
                    Some((id, _)) => *id,
                    None => 0,
                },
            };
            let f = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(self.pack_path(id))?;
            let len = f.metadata()?.len();
            self.pack = Some((id, f, len));
        }
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some((_, f, len)) = &mut self.pack {
            if !self.buf.is_empty() {
                f.seek(SeekFrom::Start(*len - self.buf.len() as u64))?;
                f.write_all(&self.buf)?;
                self.buf.clear();
            }
        }
        Ok(())
    }

    fn note(&mut self, hash: Hash, l: Loc) {
        self.idx_pending.extend_from_slice(&idx_rec(&hash, &l));
        self.map.insert(hash, l);
        self.added += 1;
        self.added_bytes += REC_HEAD + l.stored;
    }

    /// Store `data` (already encoded as `flags` says) under `hash`, unless it is there.
    fn put(&mut self, hash: &Hash, raw_len: u64, flags: u8, data: &[u8]) -> io::Result<bool> {
        if self.has(hash) {
            return Ok(false);
        }
        self.writer()?;
        let Some((id, _, len)) = &mut self.pack else { return Err(io::Error::other("no pack")) };
        let l = Loc { pack: *id, off: *len + REC_HEAD, stored: data.len() as u64, raw: raw_len, flags };
        *len += REC_HEAD + data.len() as u64;
        self.buf.extend_from_slice(&rec_head(hash, flags, raw_len, data.len() as u64));
        self.buf.extend_from_slice(data);
        self.note(*hash, l);
        if self.buf.len() >= 4 << 20 {
            self.flush()?;
        }
        Ok(true)
    }

    #[cfg(test)]
    fn put_bytes(&mut self, raw: &[u8]) -> io::Result<Hash> {
        let hash = sha(raw);
        if !self.has(&hash) {
            let (flags, data) = encode(raw);
            self.put(&hash, raw.len() as u64, flags, &data)?;
        }
        Ok(hash)
    }

    /// A file too large to hold in memory, copied into the pack while it is
    /// hashed. Its record gets its real header only once it is complete, so
    /// a copy cut short is never mistaken for an object.
    fn put_stream(&mut self, src: &mut File) -> io::Result<(Hash, u64)> {
        self.writer()?;
        self.flush()?;
        let Some((id, f, len)) = &mut self.pack else { return Err(io::Error::other("no pack")) };
        let start = *len;
        f.seek(SeekFrom::Start(start))?;
        f.write_all(&[0u8; REC_HEAD as usize])?;
        let mut chunk = vec![0u8; 1 << 20];
        let mut n = read_full(src, &mut chunk)?;
        let flags = if shrinks(&chunk[..n]) { F_DEFLATE } else { 0 };
        let mut hasher = Sha256::new();
        let mut raw = 0u64;
        let mut copy = |out: &mut dyn Write| -> io::Result<()> {
            while n > 0 {
                hasher.update(&chunk[..n]);
                raw += n as u64;
                out.write_all(&chunk[..n])?;
                n = read_full(src, &mut chunk)?;
            }
            Ok(())
        };
        let copied = if flags == F_DEFLATE {
            let mut enc = flate2::write::DeflateEncoder::new(&mut *f, flate2::Compression::fast());
            copy(&mut enc).and_then(|_| enc.finish().map(|_| ()))
        } else {
            copy(&mut *f)
        };
        let end = match copied.and_then(|_| f.stream_position()) {
            Ok(end) => end,
            Err(e) => {
                let _ = f.set_len(start);
                return Err(e);
            }
        };
        let hash: Hash = hasher.finalize().into();
        if self.map.contains_key(&hash) {
            f.set_len(start)?;
            return Ok((hash, raw));
        }
        let l = Loc { pack: *id, off: start + REC_HEAD, stored: end - start - REC_HEAD, raw, flags };
        f.seek(SeekFrom::Start(start))?;
        f.write_all(&rec_head(&hash, flags, raw, l.stored))?;
        *len = end;
        self.note(hash, l);
        Ok((hash, raw))
    }

    /// Make everything put so far durable: pack first, then the index.
    fn commit(&mut self) -> io::Result<()> {
        self.flush()?;
        if self.idx_pending.is_empty() {
            return Ok(());
        }
        if let Some((_, f, _)) = &self.pack {
            f.sync_data()?;
        }
        let mut idx = OpenOptions::new().create(true).append(true).open(self.dir.join("objects.idx"))?;
        idx.write_all(&self.idx_pending)?;
        idx.sync_data()?;
        self.idx_pending.clear();
        Ok(())
    }

    /// Copy an object out, checking on the way that it still hashes to its
    /// name. `packs` holds this caller's open pack files.
    fn copy_out(&self, hash: &Hash, packs: &mut HashMap<u32, File>, out: &mut dyn Write) -> io::Result<u64> {
        let l = self.map.get(hash).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "its content is missing from the store"))?;
        if !packs.contains_key(&l.pack) {
            packs.insert(l.pack, File::open(self.pack_path(l.pack))?);
        }
        read_out(&packs[&l.pack], l, hash, out)
    }

    fn get(&self, hash: &Hash) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        self.copy_out(hash, &mut HashMap::new(), &mut v)?;
        Ok(v)
    }

    fn stats(&self) -> (usize, u64) {
        (self.map.len(), self.packs().values().sum())
    }
}

// ---------------------------------------------------------------- store

#[derive(Clone, Debug)]
pub struct Failure {
    pub path: String,
    pub what: String,
}

fn failures_json(f: &[Failure]) -> Value {
    Value::Array(f.iter().map(|f| json!({ "path": f.path, "what": f.what })).collect())
}

struct Seen {
    rel: String,
    dir: bool,
    size: u64,
    stamp: Stamp,
    attrs: u32,
}

#[derive(Default)]
struct Walked {
    items: Vec<Seen>,
    links: Vec<String>,
    failures: Vec<Failure>,
    unlistable: HashSet<String>,
    /// Folders that were gone by the time they were to be listed.
    vanished: HashSet<String>,
    /// Temporary files of a restore that never finished (see `sweep`).
    leftovers: Vec<String>,
    files: u64,
    bytes: u64,
}

struct Hashed {
    hash: Hash,
    size: u64,
    stamp: Stamp,
    racy: bool,
    /// The file has other names too (hard links).
    shared: bool,
}

#[derive(Clone, Copy)]
struct Cached {
    size: u64,
    stamp: Stamp,
    hash: Hash,
}

const CACHE_MAGIC: &[u8; 8] = b"PRTC0002";

fn cache_load(path: &Path) -> HashMap<String, Cached> {
    let mut out = HashMap::new();
    let Ok(b) = fs::read(path) else { return out };
    if b.len() < 8 || &b[..8] != CACHE_MAGIC {
        return out;
    }
    let mut at = 8;
    while at + 4 <= b.len() {
        let n = u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        if at + n + 64 > b.len() {
            break;
        }
        let Ok(path) = std::str::from_utf8(&b[at..at + n]) else { break };
        at += n;
        let stamp = Stamp { mtime: u64_at(&b, at + 8), ctime: u64_at(&b, at + 16), id: u64_at(&b, at + 24) };
        out.insert(path.to_string(), Cached { size: u64_at(&b, at), stamp, hash: b[at + 32..at + 64].try_into().unwrap() });
        at += 64;
    }
    out
}

fn cache_save(path: &Path, cache: &BTreeMap<&str, Cached>) -> io::Result<()> {
    let mut b = Vec::with_capacity(8 + cache.len() * 96);
    b.extend_from_slice(CACHE_MAGIC);
    for (p, c) in cache {
        b.extend_from_slice(&(p.len() as u32).to_le_bytes());
        b.extend_from_slice(p.as_bytes());
        for n in [c.size, c.stamp.mtime, c.stamp.ctime, c.stamp.id] {
            b.extend_from_slice(&n.to_le_bytes());
        }
        b.extend_from_slice(&c.hash);
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, &b)?;
    fs::rename(&tmp, path)
}

/// Read one file and, when `objects` is given, store its content.
fn capture_file(abs: &Path, objects: Option<&Mutex<&mut Objects>>) -> io::Result<Hashed> {
    let read_at = now_ft();
    // Share with every other program, and open a link as itself, never its target.
    let mut f = OpenOptions::new().read(true).share_mode(SHARE_ALL).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN).open(abs)?;
    let stat = |f: &File| stat_handle(f.as_raw_handle() as HANDLE);
    let mut before = stat(&f)?;
    if before.attrs & (A_DIRECTORY | A_REPARSE) != 0 {
        return Err(io::Error::other("it stopped being an ordinary file while it was being captured"));
    }
    let mut stable = false;
    let mut small: Option<Vec<u8>> = None;
    for _ in 0..3 {
        if before.size > SMALL {
            break;
        }
        let mut buf = Vec::with_capacity(before.size as usize + 1);
        f.seek(SeekFrom::Start(0))?;
        (&mut f).take(SMALL + 1).read_to_end(&mut buf)?;
        let after = stat(&f)?;
        stable = after.size == before.size && after.stamp == before.stamp && buf.len() as u64 == after.size;
        before = after;
        small = (buf.len() as u64 <= SMALL).then_some(buf);
        if stable || small.is_none() {
            break;
        }
    }
    let (hash, size) = match small {
        Some(buf) => {
            let hash = sha(&buf);
            if let Some(objects) = objects {
                if !objects.lock().unwrap().has(&hash) {
                    let (flags, data) = encode(&buf);
                    objects.lock().unwrap().put(&hash, buf.len() as u64, flags, &data)?;
                }
            }
            (hash, buf.len() as u64)
        }
        None => {
            f.seek(SeekFrom::Start(0))?;
            let (hash, size) = match objects {
                Some(objects) => objects.lock().unwrap().put_stream(&mut f)?,
                None => {
                    let mut hasher = Sha256::new();
                    let mut chunk = vec![0u8; 1 << 20];
                    let mut size = 0u64;
                    loop {
                        let n = read_full(&mut f, &mut chunk)?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&chunk[..n]);
                        size += n as u64;
                    }
                    (hasher.finalize().into(), size)
                }
            };
            let after = stat(&f)?;
            stable = after.size == before.size && after.stamp == before.stamp && size == after.size;
            before = after;
            (hash, size)
        }
    };
    // Unsettled, or written so recently that another write could carry the
    // same time: read it again next time rather than trust the cache. The
    // same for a file with more than one name: a write through one name
    // leaves the size and times in the listing of the others as they were
    // (Windows updates only the name the file was opened by), so for such a
    // file the listing proves nothing.
    let racy = !stable || before.links > 1 || before.stamp.mtime.max(before.stamp.ctime).saturating_add(RACY) >= read_at;
    Ok(Hashed { hash, size, stamp: before.stamp, racy, shared: before.links > 1 })
}

fn workers(jobs: usize) -> usize {
    if jobs < 16 {
        return 1;
    }
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8)
}

/// Run `job` over `0..n` on a few threads; results come back in order.
fn parallel<T: Send>(n: usize, job: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<T>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|s| {
        for _ in 0..workers(n) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    break;
                }
                let r = job(i);
                out.lock().unwrap()[i] = Some(r);
            });
        }
    });
    out.into_inner().unwrap().into_iter().map(|r| r.expect("every job ran")).collect()
}

pub struct SnapOpts<'a> {
    pub label: Option<&'a str>,
    /// Why it was taken: track, snap, watch, run.before, run.after, ...
    pub reason: &'a str,
    /// false: work out the tree and store nothing.
    pub record: bool,
    /// Read every file again, whatever the cache says.
    pub rehash: bool,
}

pub struct Snap {
    pub tree: Hash,
    /// Its number in the log, when this snapshot added a line there.
    pub seq: Option<u64>,
    pub manifest: Manifest,
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    pub read_files: u64,
    pub read_bytes: u64,
    pub new_objects: u64,
    pub stored_bytes: u64,
    pub failures: Vec<Failure>,
    pub links: Vec<String>,
    /// Temporary files of a restore that never finished, still in the folder,
    /// and how many of them this snapshot removed.
    pub leftovers: Vec<String>,
    pub swept: u64,
    pub ms: f64,
    /// Of that: listing the folders, and reading, hashing and storing files.
    pub walk_ms: f64,
    pub read_ms: f64,
    /// Different from the snapshot before it.
    pub changed: bool,
}

impl Snap {
    fn json(&self) -> Value {
        json!({
            "tree": hex(&self.tree), "seq": self.seq, "changed": self.changed,
            "files": self.files, "folders": self.dirs, "bytes": self.bytes,
            "read_files": self.read_files, "read_bytes": self.read_bytes,
            "new_objects": self.new_objects, "stored_bytes": self.stored_bytes,
            "failures": failures_json(&self.failures), "skipped_links": self.links, "ms": (self.ms * 10.0).round() / 10.0,
            "restore_leftovers": self.leftovers, "swept": self.swept,
            "walk_ms": (self.walk_ms * 10.0).round() / 10.0, "read_ms": (self.read_ms * 10.0).round() / 10.0,
        })
    }
}

pub struct Store {
    /// The tracked folder, as Windows resolves it.
    pub root: PathBuf,
    /// Where its history is kept.
    pub dir: PathBuf,
    pub max_files: u64,
    pub max_bytes: u64,
    /// Ignore rules from this tree only: nothing from folders above it or
    /// from the user's global git settings. (Tests use this.)
    pub hermetic: bool,
    pub objects: Objects,
    log: Vec<Value>,
    /// Folders the walk never enters: the chronicle's own.
    skip: Vec<PathBuf>,
}

/// Held while a store is being written; released when dropped.
pub struct Guard(#[allow(dead_code)] File);

impl Store {
    pub fn dir_for(data: &Path, root: &Path) -> PathBuf {
        let shown = show(root);
        let name: String = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' })
            .take(40)
            .collect();
        data.join("chronicle").join(format!("{name}-{}", hex(&sha(shown.to_lowercase().as_bytes())[..6])))
    }

    fn at(data: &Path, root: PathBuf) -> Store {
        let dir = Store::dir_for(data, &root);
        let skip = fs::canonicalize(data.join("chronicle")).into_iter().collect();
        Store { objects: Objects::new(&dir), root, dir, max_files: DEFAULT_MAX_FILES, max_bytes: DEFAULT_MAX_MB << 20, hermetic: false, log: Vec::new(), skip }
    }

    /// Open the history of a folder that is already tracked.
    pub fn open(data: &Path, root: &Path) -> R<Store> {
        let mut s = Store::at(data, canon(root)?);
        let meta = fs::read(s.dir.join("root.json")).map_err(|_| format!("{} is not tracked. Start with: proteus chronicle track --root \"{}\"", show(&s.root), show(&s.root)))?;
        let meta: Value = serde_json::from_slice(&meta).map_err(|e| format!("{}: {e}", show(&s.dir.join("root.json"))))?;
        s.max_files = meta["max_files"].as_u64().unwrap_or(DEFAULT_MAX_FILES);
        s.max_bytes = meta["max_mb"].as_u64().unwrap_or(DEFAULT_MAX_MB) << 20;
        s.log = log_read(&s.dir);
        Ok(s)
    }

    /// Begin tracking a folder: check it may be tracked and fits the budget,
    /// then take the first snapshot. Tracking it again changes the budget.
    pub fn track(data: &Path, root: &Path, max_files: Option<u64>, max_mb: Option<u64>, profile: Option<&Path>, hermetic: bool) -> R<(Store, Snap)> {
        let root = canon(root)?;
        check_root(&root, data, profile)?;
        let mut s = match Store::open(data, &root) {
            Ok(s) => s,
            Err(_) => Store::at(data, root),
        };
        s.hermetic = hermetic;
        s.max_files = max_files.unwrap_or(s.max_files);
        s.max_bytes = max_mb.map(|m| m << 20).unwrap_or(s.max_bytes);
        // Counted before anything is created or read.
        let t0 = Instant::now();
        let w = s.walk()?;
        fs::create_dir_all(&s.dir).map_err(|e| format!("{}: {e}", show(&s.dir)))?;
        s.skip = fs::canonicalize(data.join("chronicle")).into_iter().collect();
        let meta = json!({ "version": 1, "root": show(&s.root), "max_files": s.max_files, "max_mb": s.max_bytes >> 20, "tracked_ms": unix_ms() });
        fs::write(s.dir.join("root.json"), serde_json::to_vec_pretty(&meta).unwrap_or_default()).map_err(|e| e.to_string())?;
        let _guard = s.begin()?;
        let first = s.log.is_empty();
        let snap = s.snapshot_of(w, &SnapOpts { label: first.then_some("tracked"), reason: "track", record: true, rehash: false }, t0)?;
        Ok((s, snap))
    }

    /// Take the store's lock and read what other processes have written
    /// since. Everything that writes to a store holds this.
    pub fn begin(&mut self) -> R<Guard> {
        let t = Instant::now();
        let lock = loop {
            match OpenOptions::new().write(true).create(true).truncate(false).share_mode(0).open(self.dir.join("lock")) {
                Ok(f) => break f,
                Err(e) if e.raw_os_error() == Some(32) && t.elapsed() < Duration::from_secs(60) => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(format!("the history of {} is busy or cannot be locked: {e}", show(&self.root))),
            }
        };
        self.log = log_read(&self.dir);
        self.objects = Objects::new(&self.dir);
        self.objects.load(true).map_err(|e| format!("reading the object index: {e}"))?;
        Ok(Guard(lock))
    }

    /// For reading only (no lock): load the index as it is on disk.
    fn load(&mut self) -> R<()> {
        self.objects.load(false).map_err(|e| format!("reading the object index: {e}"))
    }

    /// List one folder for the walk: what it holds goes into `w`, and the
    /// folders to list next are returned with the rules that hold in them.
    fn walk_dir(&self, dir: &str, rules: Option<Arc<Rules>>, base: &RuleBase, w: &mut Walked, buf: &mut Vec<u64>) -> Vec<(String, Option<Arc<Rules>>)> {
        let abs = if dir.is_empty() { self.root.clone() } else { join_rel(&self.root, dir) };
        let listed = match list_dir(&abs, buf) {
            Ok(l) => l,
            // Removed since the folder above it was listed. It is not there,
            // and a tree must not say that it was there and unreadable.
            Err(e) if e.kind() == io::ErrorKind::NotFound && !dir.is_empty() => {
                w.vanished.insert(dir.to_string());
                return Vec::new();
            }
            Err(e) => {
                w.unlistable.insert(dir.to_string());
                w.failures.push(Failure { path: dir.to_string(), what: format!("cannot be listed: {e}") });
                return Vec::new();
            }
        };
        let holds = |name: &str| listed.iter().any(|l| l.name.as_deref() == Ok(name));
        let rules = base.enter(&abs, (holds(".gitignore"), holds(".proteusignore"), holds(".git")), rules, &mut w.failures);
        let judged = rules.is_some() || base.global.is_some();
        let mut below = Vec::new();
        for l in listed {
            let name = match l.name {
                Ok(n) => n,
                Err(lossy) => {
                    w.failures.push(Failure { path: format!("{dir}/{lossy}"), what: "not captured: its name is not valid Unicode".into() });
                    continue;
                }
            };
            if VCS.contains(&name.as_str()) {
                continue;
            }
            let is_dir = l.attrs & A_DIRECTORY != 0;
            if is_dir && self.skip.iter().any(|s| *s == abs.join(&name)) {
                continue;
            }
            let rel = if dir.is_empty() { name.clone() } else { format!("{dir}/{name}") };
            // Restore's own temporary file: never part of a tree. One that is
            // still here was left by a restore that did not finish.
            if !is_dir && l.attrs & A_REPARSE == 0 && is_restore_tmp(&name) {
                w.leftovers.push(rel);
                continue;
            }
            // A link is reported and never entered, whatever it points at.
            if l.attrs & A_REPARSE != 0 && (is_dir || l.tag & TAG_NAME_SURROGATE != 0) {
                w.links.push(rel);
                continue;
            }
            if judged && base.ignored(rules.as_deref(), &abs.join(&name), is_dir) {
                continue;
            }
            if is_dir {
                below.push((rel.clone(), rules.clone()));
            } else {
                w.files += 1;
                w.bytes += l.size;
            }
            w.items.push(Seen { rel, dir: is_dir, size: l.size, stamp: l.stamp, attrs: l.attrs });
        }
        below
    }

    /// Everything in the tree that the ignore rules let through, from
    /// directory listings alone (no file is opened). Folders are listed on
    /// several threads; the order of the result means nothing.
    fn walk(&self) -> R<Walked> {
        type Task = (String, Option<Arc<Rules>>);
        struct Shared {
            todo: Vec<Task>,
            idle: usize,
        }
        let mut total = Walked::default();
        let base = RuleBase::new(&self.root, self.hermetic, &mut total.failures);
        // Measured on 5,017 folders: 165 ms on one thread, 62 on four, 45 on
        // eight, 42 on twelve. Past eight the file system is the limit.
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
        let shared = Mutex::new(Shared { todo: vec![(String::new(), base.above.clone())], idle: 0 });
        let wake = Condvar::new();
        // How many threads are waiting for work: read without the lock, as a hint.
        let hungry = AtomicUsize::new(0);
        let parts: Vec<Walked> = std::thread::scope(|s| {
            let work = || {
                let mut w = Walked::default();
                let mut mine: Vec<Task> = Vec::new();
                let mut buf = Vec::new();
                loop {
                    let task = match mine.pop() {
                        Some(t) => t,
                        None => {
                            let mut q = shared.lock().unwrap();
                            let got = loop {
                                if let Some(t) = q.todo.pop() {
                                    break Some(t);
                                }
                                q.idle += 1;
                                if q.idle == threads {
                                    // Nobody has anything left: the walk is over.
                                    wake.notify_all();
                                    break None;
                                }
                                hungry.store(q.idle, Ordering::Relaxed);
                                q = wake.wait(q).unwrap();
                                if q.idle == threads {
                                    break None;
                                }
                                q.idle -= 1;
                            };
                            hungry.store(if got.is_some() { q.idle } else { 0 }, Ordering::Relaxed);
                            match got {
                                Some(t) => t,
                                None => return w,
                            }
                        }
                    };
                    mine.extend(self.walk_dir(&task.0, task.1, &base, &mut w, &mut buf));
                    if mine.len() > 1 && hungry.load(Ordering::Relaxed) > 0 {
                        let mut q = shared.lock().unwrap();
                        let give = mine.len() / 2;
                        q.todo.extend(mine.drain(..give));
                        drop(q);
                        wake.notify_all();
                    }
                }
            };
            let running: Vec<_> = (0..threads).map(|_| s.spawn(work)).collect();
            running.into_iter().map(|t| t.join().expect("a walk thread panicked")).collect()
        });
        for mut part in parts {
            total.items.append(&mut part.items);
            total.links.append(&mut part.links);
            total.failures.append(&mut part.failures);
            total.unlistable.extend(part.unlistable);
            total.vanished.extend(part.vanished);
            total.leftovers.append(&mut part.leftovers);
            total.files += part.files;
            total.bytes += part.bytes;
        }
        let mut w = total;
        if w.unlistable.contains("") {
            return Err(format!("cannot list {}: {}", show(&self.root), w.failures.last().map(|f| f.what.as_str()).unwrap_or("")));
        }
        w.links.sort();
        w.leftovers.sort();
        if w.files > self.max_files || w.bytes > self.max_bytes {
            return Err(format!(
                "{} holds {} files and {:.1} MB after the ignore rules; the budget is {} files and {} MB. Nothing was recorded. Raise it (proteus chronicle track --max-files N --max-mb M) or ignore more (.gitignore, .proteusignore).",
                show(&self.root),
                w.files,
                w.bytes as f64 / 1048576.0,
                self.max_files,
                self.max_bytes >> 20
            ));
        }
        Ok(w)
    }

    fn head(&self) -> Option<Hash> {
        self.log.iter().rev().find(|r| r["t"] == "snap").and_then(|r| r["tree"].as_str()).and_then(unhex)
    }

    /// Work out the tree as it is now. With `record`, also store every new
    /// content and add a line to the log when the tree changed or has a
    /// label; the caller must hold `begin()`'s guard.
    pub fn snapshot(&mut self, o: &SnapOpts) -> R<Snap> {
        let t0 = Instant::now();
        let w = self.walk()?;
        self.snapshot_of(w, o, t0)
    }

    fn snapshot_of(&mut self, w: Walked, o: &SnapOpts, t0: Instant) -> R<Snap> {
        let walk_ms = t0.elapsed().as_secs_f64() * 1e3;
        let cache_path = self.dir.join("cache.bin");
        let cache = cache_load(&cache_path);
        if o.record && !self.objects.loaded {
            return Err("snapshot: the store is not open for writing".into());
        }
        let (added0, bytes0) = (self.objects.added, self.objects.added_bytes);
        let mut failures = w.failures;
        let mut m = Manifest::new();
        let mut todo: Vec<&Seen> = Vec::new();
        let (mut files, mut dirs, mut bytes) = (0u64, 0u64, 0u64);
        for s in &w.items {
            if let Err(why) = valid_rel(&s.rel) {
                failures.push(Failure { path: s.rel.clone(), what: format!("not captured: {why}") });
                continue;
            }
            let attrs = s.attrs & KEEP_ATTRS;
            if s.dir {
                if w.vanished.contains(&s.rel) {
                    continue;
                }
                dirs += 1;
                let kind = if w.unlistable.contains(&s.rel) { Kind::Unknown } else { Kind::Dir };
                m.insert(s.rel.clone(), Entry { kind, attrs, size: 0, hash: [0u8; 32], stamp: s.stamp });
                continue;
            }
            files += 1;
            bytes += s.size;
            if s.attrs & (A_OFFLINE | A_RECALL) != 0 {
                failures.push(Failure { path: s.rel.clone(), what: "not captured: its content is not on this disk (cloud placeholder)".into() });
                m.insert(s.rel.clone(), Entry { kind: Kind::Unknown, attrs, size: s.size, hash: [0u8; 32], stamp: s.stamp });
                continue;
            }
            match cache.get(&s.rel) {
                Some(c) if !o.rehash && c.size == s.size && c.stamp.agrees(&s.stamp) && (!o.record || self.objects.has(&c.hash)) => {
                    m.insert(s.rel.clone(), Entry { kind: Kind::File, attrs, size: s.size, hash: c.hash, stamp: c.stamp });
                }
                _ => todo.push(s),
            }
        }

        let t_read = Instant::now();
        let root = &self.root;
        let shared = o.record.then(|| Mutex::new(&mut self.objects));
        let results = parallel(todo.len(), |i| capture_file(&join_rel(root, &todo[i].rel), shared.as_ref()));
        drop(shared);
        let mut fresh: HashMap<&str, Cached> = HashMap::new();
        // Files just read that have other names: which file, and what it holds.
        let mut shared: HashMap<u64, (Hash, u64)> = HashMap::new();
        let (mut read_files, mut read_bytes) = (0u64, 0u64);
        for (s, r) in todo.iter().zip(results) {
            let attrs = s.attrs & KEEP_ATTRS;
            match r {
                Ok(h) => {
                    read_files += 1;
                    read_bytes += h.size;
                    bytes = bytes - s.size + h.size;
                    if !h.racy {
                        fresh.insert(&s.rel, Cached { size: h.size, stamp: h.stamp, hash: h.hash });
                    }
                    if h.shared && h.stamp.id != 0 {
                        shared.insert(h.stamp.id, (h.hash, h.size));
                    }
                    m.insert(s.rel.clone(), Entry { kind: Kind::File, attrs, size: h.size, hash: h.hash, stamp: h.stamp });
                }
                // Deleted between being listed and being read (a build's
                // temporary files do this all day): it is not there.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    files -= 1;
                    bytes -= s.size;
                }
                Err(e) => {
                    failures.push(Failure { path: s.rel.clone(), what: format!("not captured: {e}") });
                    m.insert(s.rel.clone(), Entry { kind: Kind::Unknown, attrs, size: s.size, hash: [0u8; 32], stamp: s.stamp });
                }
            }
        }
        // Another name of a file that was just read is that same file, so it
        // holds the same content, whatever the cache made of a listing that
        // Windows does not keep up to date for it. (No longer matching the
        // cache, it is dropped from it below and read next time.)
        if !shared.is_empty() {
            for e in m.values_mut().filter(|e| e.kind == Kind::File) {
                if let Some((hash, size)) = shared.get(&e.stamp.id).filter(|now| (now.0, now.1) != (e.hash, e.size)) {
                    bytes = bytes - e.size + size;
                    (e.hash, e.size) = (*hash, *size);
                }
            }
        }

        let read_ms = t_read.elapsed().as_secs_f64() * 1e3;
        let (tree, mut listings) = build_trees(&m);
        let head = self.head();
        let changed = head != Some(tree);
        let logged = o.record && (changed || o.label.is_some());
        // Every snapshot that reaches the log also records when each file was written.
        let times = logged.then(|| {
            let (id, mut lists) = build_times(&m);
            listings.append(&mut lists);
            id
        });
        let mut seq = None;
        if o.record {
            for (id, bytes) in &listings {
                if !self.objects.has(id) {
                    let (flags, data) = encode(bytes);
                    self.objects.put(id, bytes.len() as u64, flags, &data).map_err(|e| format!("writing to the store: {e}"))?;
                }
            }
            self.objects.commit().map_err(|e| format!("writing to the store: {e}"))?;
            // The cache keeps what is still true: entries used as they were, and files read just now.
            let mut keep: BTreeMap<&str, Cached> = BTreeMap::new();
            let mut dirty = false;
            for (path, e) in &m {
                if e.kind != Kind::File {
                    continue;
                }
                if let Some(c) = fresh.get(path.as_str()) {
                    keep.insert(path.as_str(), *c);
                    dirty = true;
                } else if let Some(c) = cache.get(path).filter(|c| c.hash == e.hash && c.size == e.size && c.stamp == e.stamp) {
                    keep.insert(path.as_str(), *c);
                }
            }
            if dirty || keep.len() != cache.len() {
                cache_save(&cache_path, &keep).map_err(|e| format!("writing the cache: {e}"))?;
            }
            if logged {
                let mut rec = json!({
                    "t": "snap", "tree": hex(&tree), "times": times.map(|t| hex(&t)), "parent": head.map(|h| hex(&h)), "time_ms": unix_ms(),
                    "label": o.label, "reason": o.reason, "files": files, "folders": dirs, "bytes": bytes,
                    "failures": failures.len(), "skipped_links": w.links.len(),
                });
                seq = Some(self.log_append(&mut rec)?);
            }
        }
        // A recording snapshot holds the store's lock, so no restore of this
        // tree is running: any temporary file of one is a leftover.
        let (mut leftovers, mut swept) = (w.leftovers, 0u64);
        if o.record {
            leftovers.retain(|rel| {
                let gone = self.sweep(rel);
                swept += gone as u64;
                !gone
            });
        }
        Ok(Snap {
            tree,
            seq,
            manifest: m,
            files,
            dirs,
            bytes,
            read_files,
            read_bytes,
            new_objects: self.objects.added - added0,
            stored_bytes: self.objects.added_bytes - bytes0,
            failures,
            links: w.links,
            leftovers,
            swept,
            ms: t0.elapsed().as_secs_f64() * 1e3,
            walk_ms,
            read_ms,
            changed,
        })
    }

    fn log_append(&mut self, rec: &mut Value) -> R<u64> {
        let seq = self.log.last().and_then(|r| r["seq"].as_u64()).unwrap_or(0) + 1;
        rec["seq"] = json!(seq);
        let path = self.dir.join("log.jsonl");
        let mut f = OpenOptions::new().create(true).append(true).read(true).open(&path).map_err(|e| format!("{}: {e}", show(&path)))?;
        let mut line = Vec::new();
        // A line cut short by a crash must not swallow the next one.
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        let mut last = [b'\n'];
        if len > 0 {
            let _ = f.seek_read(&mut last, len - 1);
        }
        if last != [b'\n'] {
            line.push(b'\n');
        }
        line.extend_from_slice(rec.to_string().as_bytes());
        line.push(b'\n');
        f.write_all(&line).and_then(|_| f.sync_data()).map_err(|e| format!("{}: {e}", show(&path)))?;
        self.log.push(rec.clone());
        Ok(seq)
    }

    pub fn tree(&self, id: &Hash) -> R<Manifest> {
        let mut m = Manifest::new();
        load_tree(&self.objects, id, "", 0, &mut m)?;
        Ok(m)
    }

    /// When each file of a recorded tree was last written (milliseconds
    /// since 1970), from the latest snapshot that found that tree. Empty if
    /// no snapshot recorded times for it.
    pub fn written(&self, tree: &Hash) -> BTreeMap<String, u64> {
        let name = hex(tree);
        let mut out = BTreeMap::new();
        let found = self.log.iter().rev().find(|r| r["t"] == "snap" && r["tree"] == name.as_str()).and_then(|r| r["times"].as_str()).and_then(unhex);
        if let Some(id) = found {
            if load_times(&self.objects, &id, "", 0, &mut out).is_err() {
                out.clear();
            }
        }
        out.values_mut().for_each(|t| *t = ft_to_unix_ms(*t));
        out
    }

    /// A tree named on the command line: `last`, a snapshot's number from
    /// the log, or the first characters (8 or more) of a tree id. Only trees
    /// the log knows can be named.
    pub fn resolve(&self, name: &str) -> R<Hash> {
        if name.eq_ignore_ascii_case("last") || name.eq_ignore_ascii_case("head") {
            return self.head().ok_or_else(|| "there are no snapshots yet".to_string());
        }
        if name.len() < 8 {
            if let Ok(n) = name.parse::<u64>() {
                let rec = self.log.iter().find(|r| r["seq"].as_u64() == Some(n)).ok_or_else(|| format!("the log has no entry {n}"))?;
                let field = if rec["t"] == "snap" { "tree" } else { "after" };
                return rec[field].as_str().and_then(unhex).ok_or_else(|| format!("log entry {n} names no tree"));
            }
            return Err(format!("{name:?}: give a log number, `last`, or at least 8 characters of a tree id"));
        }
        let name = name.to_ascii_lowercase();
        let mut found: Option<Hash> = None;
        for rec in &self.log {
            for field in ["tree", "before", "after"] {
                if let Some(h) = rec[field].as_str().filter(|h| h.starts_with(&name)).and_then(unhex) {
                    if found.is_some_and(|f| f != h) {
                        return Err(format!("{name} matches more than one tree; give more characters"));
                    }
                    found = Some(h);
                }
            }
        }
        found.ok_or_else(|| format!("no recorded tree starts with {name}"))
    }

    /// The step to undo: the one numbered `seq`, or the last one that
    /// changed anything. A command that left the tree as it found it (a test
    /// run, a listing) is a step too, but there is nothing in it to undo;
    /// the numbers of those passed over come back as well.
    pub fn step(&self, seq: Option<u64>) -> R<(&Value, Vec<u64>)> {
        let mut steps = self.log.iter().rev().filter(|r| r["t"] == "step");
        if let Some(n) = seq {
            return steps.find(|r| r["seq"].as_u64() == Some(n)).map(|s| (s, Vec::new())).ok_or_else(|| format!("log entry {n} is not a step"));
        }
        let mut passed = Vec::new();
        for s in steps {
            if s["before"] != s["after"] {
                return Ok((s, passed));
            }
            passed.extend(s["seq"].as_u64());
        }
        Err(match passed.len() {
            0 => "no step has been recorded yet (steps come from `proteus run`, undo and restore)".to_string(),
            n => format!("none of the {n} recorded steps changed anything in the tree, so there is nothing to undo"),
        })
    }
}

fn log_read(dir: &Path) -> Vec<Value> {
    let Ok(text) = fs::read_to_string(dir.join("log.jsonl")) else { return Vec::new() };
    text.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|v| v["seq"].is_u64()).collect()
}

// ---------------------------------------------------------------- restore

/// Files being written by restore carry this prefix until they are complete.
const TMP_PREFIX: &str = ".proteus-restore-";

/// `.proteus-restore-<process id>-<number>.tmp`: the name restore gives a file
/// while it is being written, and no other name.
fn is_restore_tmp(name: &str) -> bool {
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    name.strip_prefix(TMP_PREFIX).and_then(|rest| rest.strip_suffix(".tmp")).and_then(|ids| ids.split_once('-')).is_some_and(|(pid, n)| digits(pid) && digits(n))
}

/// Volume flags (GetVolumeInformation): permissions are kept per file; files can have named streams.
const VOL_PERSISTENT_ACLS: u32 = 0x8;
const VOL_NAMED_STREAMS: u32 = 0x4_0000;

/// Does the volume this open file is on keep anything for a file besides
/// its content and attributes? Not knowing counts as yes.
fn volume_keeps_more(f: &File) -> bool {
    let mut flags = 0u32;
    let h = f.as_raw_handle() as HANDLE;
    let ok = unsafe { GetVolumeInformationByHandleW(h, std::ptr::null_mut(), 0, std::ptr::null_mut(), std::ptr::null_mut(), &mut flags, std::ptr::null_mut(), 0) } != 0;
    !ok || flags & (VOL_PERSISTENT_ACLS | VOL_NAMED_STREAMS) != 0
}

/// Give `from` the name `to`, which nothing may have yet: a file, a folder
/// or a link there (a link is not followed) makes this fail.
fn move_to_free_name(from: &Path, to: &Path) -> io::Result<()> {
    let (f, t) = (wide(from), wide(to));
    if unsafe { MoveFileExW(f.as_ptr(), t.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Put the finished file `tmp` in the place of the file at `abs`. The file
/// keeps what it had that is not content and that the store never held:
/// who may read and write it, its named streams (the mark Windows puts on a
/// download is one), when it was created.
fn replace_keeping_the_rest(tmp: &Path, abs: &Path) -> io::Result<()> {
    let (t, a) = (wide(tmp), wide(abs));
    if unsafe { ReplaceFileW(a.as_ptr(), t.as_ptr(), std::ptr::null(), 0, std::ptr::null(), std::ptr::null()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Version-control folders: never captured, never restored into.
const VCS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];

#[derive(Clone, Debug)]
pub enum Op {
    Delete(String),
    RmDir(String),
    MkDir(String, u32),
    Write(String, Entry),
    Attrs(String, u32),
    /// The folder at the first path is the one wanted at the second: the
    /// same folder, its name differing in letter case only. It is renamed,
    /// with everything in it. Made by `preflight`, which has seen the disk,
    /// in place of removing a folder that cannot be emptied and making it again.
    Rename(String, String, u32),
}

impl Op {
    /// The path this acts on.
    fn path(&self) -> &String {
        let (Op::Delete(p) | Op::RmDir(p) | Op::MkDir(p, _) | Op::Write(p, _) | Op::Attrs(p, _) | Op::Rename(_, p, _)) = self;
        p
    }

    fn json(&self) -> Value {
        match self {
            Op::Delete(p) => json!({ "do": "delete", "path": p }),
            Op::RmDir(p) => json!({ "do": "remove-folder", "path": p }),
            Op::Rename(from, to, _) => json!({ "do": "rename-folder", "path": to, "from": from }),
            Op::MkDir(p, _) => json!({ "do": "make-folder", "path": p }),
            Op::Write(p, e) => json!({ "do": "write", "path": p, "size": e.size, "hash": hex(&e.hash) }),
            Op::Attrs(p, a) => json!({ "do": "attributes", "path": p, "readonly": a & A_READONLY != 0, "hidden": a & A_HIDDEN != 0, "system": a & A_SYSTEM != 0 }),
        }
    }
}

#[derive(Default)]
pub struct Plan {
    pub ops: Vec<Op>,
    /// Changed since the step being undone ended: left alone unless forced.
    pub conflicts: Vec<Failure>,
    /// Cannot be done without risking content that is not in the store. Never forced.
    pub blocked: Vec<Failure>,
    /// Folders that should go but hold something that is not in the tree
    /// (ignored files, a link), or that a program is using: they stay, with
    /// what they hold.
    pub kept: Vec<Failure>,
}

impl Plan {
    fn json(&self) -> Value {
        let count = |f: fn(&Op) -> bool| self.ops.iter().filter(|o| f(o)).count();
        json!({
            "write": count(|o| matches!(o, Op::Write(..))), "delete": count(|o| matches!(o, Op::Delete(..))),
            "make_folders": count(|o| matches!(o, Op::MkDir(..))), "remove_folders": count(|o| matches!(o, Op::RmDir(..))),
            "rename_folders": count(|o| matches!(o, Op::Rename(..))), "attributes": count(|o| matches!(o, Op::Attrs(..))),
            "bytes": self.ops.iter().map(|o| if let Op::Write(_, e) = o { e.size } else { 0 }).sum::<u64>(),
            "ops": self.ops.iter().map(Op::json).collect::<Vec<_>>(),
            "conflicts": failures_json(&self.conflicts), "blocked": failures_json(&self.blocked), "kept": failures_json(&self.kept),
        })
    }

    /// Nothing was held back: carried out, this plan reaches what was asked for.
    fn whole(&self) -> bool {
        self.conflicts.is_empty() && self.blocked.is_empty() && self.kept.is_empty()
    }
}

/// What must happen on disk for each path in `want` to become what it says
/// (`None`: absent), given the tree as it is now.
pub fn plan(cur: &Manifest, want: &BTreeMap<String, Option<Entry>>) -> Plan {
    let mut p = Plan::default();
    for (path, want) in want {
        let have = cur.get(path);
        if same(have, want.as_ref()) {
            continue;
        }
        if let Err(why) = valid_rel(path) {
            p.blocked.push(Failure { path: path.clone(), what: format!("refused: {why}") });
            continue;
        }
        if want.as_ref().is_some_and(|w| w.kind == Kind::Unknown) {
            p.blocked.push(Failure { path: path.clone(), what: "its content was never captured in that tree (it could not be read then)".into() });
            continue;
        }
        if have.is_some_and(|h| h.kind == Kind::Unknown) {
            p.blocked.push(Failure { path: path.clone(), what: "what is there now cannot be read, so it cannot be saved first".into() });
            continue;
        }
        match (have.map(|h| h.kind), want) {
            (Some(Kind::File), Some(w)) if w.kind == Kind::File => {
                if have.is_some_and(|h| h.hash == w.hash && h.size == w.size) {
                    p.ops.push(Op::Attrs(path.clone(), w.attrs));
                } else {
                    p.ops.push(Op::Write(path.clone(), w.clone()));
                }
            }
            (Some(Kind::Dir), Some(w)) if w.kind == Kind::Dir => p.ops.push(Op::Attrs(path.clone(), w.attrs)),
            (have, want) => {
                match have {
                    Some(Kind::File) => p.ops.push(Op::Delete(path.clone())),
                    Some(Kind::Dir) => p.ops.push(Op::RmDir(path.clone())),
                    _ => {}
                }
                match want {
                    Some(w) if w.kind == Kind::Dir => p.ops.push(Op::MkDir(path.clone(), w.attrs)),
                    Some(w) => p.ops.push(Op::Write(path.clone(), w.clone())),
                    None => {}
                }
            }
        }
    }
    p
}

/// The tree a plan leads to if every part of it is carried out.
fn expect(cur: &Manifest, p: &Plan) -> Hash {
    let mut m = cur.clone();
    for op in &p.ops {
        match op {
            Op::Delete(path) | Op::RmDir(path) => {
                m.remove(path);
            }
            Op::MkDir(path, attrs) => {
                m.insert(path.clone(), Entry { kind: Kind::Dir, attrs: *attrs, size: 0, hash: [0u8; 32], stamp: Stamp::default() });
            }
            // What the tree knows inside it is taken out and written back by
            // other parts of the plan, under the new name.
            Op::Rename(from, to, attrs) => {
                m.remove(from);
                m.insert(to.clone(), Entry { kind: Kind::Dir, attrs: *attrs, size: 0, hash: [0u8; 32], stamp: Stamp::default() });
            }
            Op::Write(path, e) => {
                m.insert(path.clone(), e.clone());
            }
            Op::Attrs(path, attrs) => {
                if let Some(e) = m.get_mut(path) {
                    e.attrs = *attrs;
                }
            }
        }
    }
    build_trees(&m).0
}

#[derive(Default)]
pub struct Applied {
    pub written: u64,
    pub bytes: u64,
    pub deleted: u64,
    pub folders_made: u64,
    pub folders_removed: u64,
    pub folders_renamed: u64,
    pub attributes: u64,
    /// Refused at the last moment: what is on disk is not what was captured.
    pub blocked: Vec<Failure>,
    pub failed: Vec<Failure>,
    /// Folders that should have gone and could not be removed after all
    /// (something turned up in them, or a program took hold of them).
    pub kept_folders: Vec<String>,
}

impl Applied {
    pub fn complete(&self) -> bool {
        self.blocked.is_empty() && self.failed.is_empty() && self.kept_folders.is_empty()
    }

    fn json(&self) -> Value {
        json!({
            "written": self.written, "bytes": self.bytes, "deleted": self.deleted, "folders_made": self.folders_made,
            "folders_removed": self.folders_removed, "folders_renamed": self.folders_renamed, "attributes": self.attributes, "blocked": failures_json(&self.blocked),
            "failed": failures_json(&self.failed), "kept_folders": self.kept_folders,
        })
    }
}

fn attrs_of(p: &Path) -> io::Result<u32> {
    stat_path(p).map(|s| s.attrs)
}

/// Set the read-only, hidden and system bits of `p` to exactly `keep`.
fn set_attrs(p: &Path, keep: u32) -> io::Result<()> {
    let now = attrs_of(p)?;
    let want = (now & !KEEP_ATTRS) | (keep & KEEP_ATTRS);
    if want != now {
        let w = wide(p);
        if unsafe { SetFileAttributesW(w.as_ptr(), if want == 0 { 0x80 } else { want }) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

impl Store {
    /// Every folder on the way to `rel` must be a real folder inside the
    /// root: never a link, which would carry the write somewhere else.
    /// With `make`, missing ones are made. `known` remembers folders already checked.
    fn parents_ok(&self, rel: &str, make: bool, known: &Mutex<HashSet<String>>) -> R<()> {
        valid_rel(rel)?;
        let mut at = 0;
        while let Some(i) = rel[at..].find('/') {
            let dir = &rel[..at + i];
            at += i + 1;
            if known.lock().unwrap().contains(dir) {
                continue;
            }
            let abs = join_rel(&self.root, dir);
            match attrs_of(&abs) {
                Ok(a) if a & A_REPARSE != 0 => return Err(format!("{dir} is a link; restore never writes or deletes through one")),
                Ok(a) if a & A_DIRECTORY == 0 => return Err(format!("{dir} is not a folder")),
                Ok(_) => {}
                Err(e) if make && e.kind() == io::ErrorKind::NotFound => match fs::create_dir(&abs) {
                    Ok(()) => {}
                    // Files are written on several threads, and another one made this folder first.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists && attrs_of(&abs).is_ok_and(|a| a & A_DIRECTORY != 0 && a & A_REPARSE == 0) => {}
                    Err(e) => return Err(format!("making {dir}: {e}")),
                },
                Err(e) => return Err(format!("{dir}: {e}")),
            }
            known.lock().unwrap().insert(dir.to_string());
        }
        Ok(())
    }

    /// Take hold of the file at `rel` in order to replace or delete it.
    /// It is opened so that no program can be writing to it (if one is, this
    /// fails), and read through, to see that it is byte for byte what the
    /// snapshot captured: sizes and times are not taken on trust here,
    /// because Windows does not update them while a writer keeps the file
    /// open. The handle is returned and kept until the change is made, so
    /// that no writer can arrive in between. Also returns the file's
    /// read-only, hidden and system bits as they are now.
    fn claim(&self, rel: &str, cur: &Manifest) -> R<(File, u32)> {
        const CHANGED: &str = "it changed after the snapshot taken first; nothing was done to it. Run the command again.";
        let Some(e) = cur.get(rel).filter(|e| e.kind == Kind::File) else { return Err("it is not in the snapshot taken first".into()) };
        // Others may read it or rename it while it is held, but not write to it.
        let mut f = OpenOptions::new()
            .read(true)
            .share_mode(SHARE_READ | SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_SEQUENTIAL_SCAN)
            .open(join_rel(&self.root, rel))
            .map_err(|err| match err.raw_os_error() {
                Some(32) => "another program has it open for writing; nothing was done to it".to_string(),
                _ => err.to_string(),
            })?;
        let now = stat_handle(f.as_raw_handle() as HANDLE).map_err(|e| e.to_string())?;
        if now.attrs & (A_REPARSE | A_DIRECTORY) != 0 || now.size != e.size {
            return Err(CHANGED.into());
        }
        let mut hasher = Sha256::new();
        let mut chunk = vec![0u8; e.size.clamp(1, 1 << 20) as usize];
        loop {
            match read_full(&mut f, &mut chunk).map_err(|e| e.to_string())? {
                0 => break,
                n => hasher.update(&chunk[..n]),
            }
        }
        let on_disk: Hash = hasher.finalize().into();
        if on_disk != e.hash {
            return Err(CHANGED.into());
        }
        Ok((f, now.attrs & KEEP_ATTRS))
    }

    /// Put one file of the plan on disk. Err((true, why)): refused, because
    /// what is there is not what the snapshot captured. Err((false, why)): failed.
    fn write_one(&self, rel: &str, e: &Entry, cur: &Manifest, known: &Mutex<HashSet<String>>, packs: &mut HashMap<u32, File>, n: u64) -> Result<u64, (bool, String)> {
        const IN_THE_WAY: &str = "something is there that is not in the snapshot taken first (ignored, a link, or just created); it was left alone";
        let block = |why: String| (true, why);
        let fail = |why: String| (false, why);
        self.parents_ok(rel, true, known).map_err(block)?;
        let abs = join_rel(&self.root, rel);
        // Replacing a file: it is checked and held first. A new file: nothing
        // may be at its path (a link is something, and is never followed).
        let held = if cur.get(rel).is_some_and(|c| c.kind == Kind::File) {
            Some(self.claim(rel, cur).map_err(block)?)
        } else if stat_path(&abs).is_ok() {
            return Err(block(IN_THE_WAY.into()));
        } else {
            None
        };
        // Either way the content is written beside the path under a
        // temporary name and moved in whole, so the path itself never holds
        // half a file, not even if this process is killed. Nothing else can
        // remove the temporary file while it is being written.
        let tmp = abs.with_file_name(format!("{TMP_PREFIX}{}-{n}.tmp", std::process::id()));
        let made = (|| -> io::Result<u64> {
            let mut f = OpenOptions::new().write(true).create_new(true).share_mode(SHARE_READ).open(&tmp)?;
            let bytes = self.objects.copy_out(&e.hash, packs, &mut f)?;
            f.flush()?;
            Ok(bytes)
        })();
        let bytes = match made {
            Ok(b) => b,
            Err(err) => {
                let _ = fs::remove_file(&tmp);
                return Err(fail(err.to_string()));
            }
        };
        let Some((held, had)) = held else {
            // The move itself refuses whatever turned up at the path meanwhile.
            if let Err(err) = move_to_free_name(&tmp, &abs) {
                let _ = fs::remove_file(&tmp);
                return Err(if stat_path(&abs).is_ok() { block(IN_THE_WAY.into()) } else { fail(err.to_string()) });
            }
            if e.attrs != 0 {
                set_attrs(&abs, e.attrs).map_err(|err| fail(err.to_string()))?;
            }
            return Ok(bytes);
        };
        let moved = (|| -> io::Result<()> {
            if had != 0 {
                set_attrs(&abs, 0)?;
            }
            // Swapped in while the old file is still held, in the way that
            // carries over its permissions and named streams. Where that
            // cannot be done to a file that is open, it is let go first and
            // the swap follows at once. A plain rename, which would drop
            // them, is used only where the volume keeps neither.
            if let Err(first) = replace_keeping_the_rest(&tmp, &abs) {
                if volume_keeps_more(&held) {
                    drop(held);
                    replace_keeping_the_rest(&tmp, &abs).map_err(|_| io::Error::other(format!("it could not be replaced in a way that keeps its permissions and named streams ({first}); left as it is")))?;
                } else if fs::rename(&tmp, &abs).is_err() {
                    drop(held);
                    fs::rename(&tmp, &abs)?;
                }
            }
            if e.attrs != 0 {
                set_attrs(&abs, e.attrs)?;
            }
            Ok(())
        })();
        if let Err(err) = moved {
            let _ = fs::remove_file(&tmp);
            if had != 0 {
                let _ = set_attrs(&abs, had);
            }
            return Err(fail(err.to_string()));
        }
        Ok(bytes)
    }

    /// Remove one temporary file that a restore which did not finish left in
    /// the tree. The caller holds the store's lock, so no restore of this
    /// tree is running; one being written for a tree nested in this one
    /// cannot be removed (see `write_one`) and is left.
    fn sweep(&self, rel: &str) -> bool {
        let abs = join_rel(&self.root, rel);
        rel.rsplit('/').next().is_some_and(is_restore_tmp)
            && self.parents_ok(rel, false, &Mutex::new(HashSet::new())).is_ok()
            && stat_path(&abs).is_ok_and(|s| s.attrs & (A_DIRECTORY | A_REPARSE) == 0)
            && fs::remove_file(&abs).is_ok()
    }

    /// Carry out a plan. `cur` is the snapshot taken just before: a file is
    /// replaced or deleted only if it is still exactly what that snapshot
    /// captured, so nothing is lost that the store does not hold.
    pub fn apply(&self, plan: &Plan, cur: &Manifest) -> Applied {
        let mut a = Applied::default();
        let known = Mutex::new(HashSet::new());
        let depth = |p: &str| p.matches('/').count();

        for op in &plan.ops {
            let Op::Delete(rel) = op else { continue };
            let done = self.parents_ok(rel, false, &known).and_then(|_| self.claim(rel, cur)).map_err(|why| (true, why)).and_then(|(held, had)| {
                let abs = join_rel(&self.root, rel);
                // A read-only file cannot be deleted until it is not.
                let cleared = if had != 0 { set_attrs(&abs, 0) } else { Ok(()) };
                let gone = cleared.and_then(|_| fs::remove_file(&abs)).map_err(|e| (false, e.to_string()));
                // Still there after all: it keeps the bits it had.
                if gone.is_err() && had != 0 {
                    let _ = set_attrs(&abs, had);
                }
                drop(held);
                gone
            });
            match done {
                Ok(()) => a.deleted += 1,
                Err((true, what)) => a.blocked.push(Failure { path: rel.clone(), what }),
                Err((false, what)) => a.failed.push(Failure { path: rel.clone(), what }),
            }
        }

        let mut gone: Vec<&String> = plan.ops.iter().filter_map(|o| if let Op::RmDir(p) = o { Some(p) } else { None }).collect();
        gone.sort_by_key(|p| std::cmp::Reverse(depth(p)));
        for rel in gone {
            let abs = join_rel(&self.root, rel);
            let done = self.parents_ok(rel, false, &known).and_then(|_| match attrs_of(&abs) {
                Ok(at) if at & A_REPARSE != 0 || at & A_DIRECTORY == 0 => Err("it is no longer an ordinary folder; left alone".to_string()),
                Ok(_) => Ok(()),
                Err(e) => Err(e.to_string()),
            });
            match done {
                Err(what) => a.blocked.push(Failure { path: rel.clone(), what }),
                // Only ever an empty folder: whatever the tree does not know stays.
                Ok(()) => {
                    let had = attrs_of(&abs).unwrap_or(0) & KEEP_ATTRS;
                    let _ = set_attrs(&abs, 0);
                    match fs::remove_dir(&abs) {
                        Ok(()) => a.folders_removed += 1,
                        Err(_) => {
                            let _ = set_attrs(&abs, had);
                            a.kept_folders.push(rel.clone());
                        }
                    }
                }
            }
        }

        // Folders that only change the letter case of their name, outermost
        // first (the old path of an inner one still finds it: Windows does
        // not tell letter case apart when it looks a path up).
        let mut renamed: Vec<(&String, &String, u32)> = plan.ops.iter().filter_map(|o| if let Op::Rename(from, to, at) = o { Some((from, to, *at)) } else { None }).collect();
        renamed.sort_by_key(|(_, to, _)| depth(to));
        for (from, to, attrs) in renamed {
            let (src, dst) = (join_rel(&self.root, from), join_rel(&self.root, to));
            let name = |p: &str| p.rsplit('/').next().unwrap_or("").to_string();
            // Both paths must be one and the same real folder: then all that
            // can change is how its name is written.
            let done = self.parents_ok(to, false, &known).and_then(|_| match (stat_path(&src), stat_path(&dst)) {
                (Ok(s), Ok(d)) if s.stamp.id != 0 && s.stamp.id == d.stamp.id && s.attrs & A_DIRECTORY != 0 && s.attrs & A_REPARSE == 0 => Ok(()),
                _ => Err(format!("{from} is no longer this folder under another name; left alone")),
            });
            let done = done.and_then(|_| if name(from) == name(to) { Ok(()) } else { move_to_free_name(&src, &dst).map_err(|e| format!("renaming {from}: {e}")) });
            match done.and_then(|_| set_attrs(&dst, attrs).map_err(|e| e.to_string())) {
                Ok(()) => a.folders_renamed += 1,
                Err(what) => a.blocked.push(Failure { path: to.clone(), what }),
            }
        }

        let mut made: Vec<(&String, u32)> = plan.ops.iter().filter_map(|o| if let Op::MkDir(p, at) = o { Some((p, *at)) } else { None }).collect();
        made.sort_by_key(|(p, _)| depth(p));
        for (rel, attrs) in made {
            let abs = join_rel(&self.root, rel);
            // Making it is the test: that never follows a link and never replaces anything.
            let done = self.parents_ok(rel, true, &known).and_then(|_| match fs::create_dir(&abs) {
                Ok(()) => Ok(true),
                Err(made) => match attrs_of(&abs) {
                    Ok(at) if at & A_REPARSE != 0 || at & A_DIRECTORY == 0 => Err("something else is there (a link, or a file that is not in the snapshot taken first); left alone".to_string()),
                    Ok(_) => Ok(false),
                    Err(_) => Err(made.to_string()),
                },
            });
            let done = done.and_then(|new| {
                if !new || attrs != 0 {
                    set_attrs(&abs, attrs).map_err(|e| e.to_string())?;
                }
                Ok(new)
            });
            match done {
                Ok(true) => {
                    known.lock().unwrap().insert(rel.clone());
                    a.folders_made += 1;
                }
                Ok(false) => {}
                Err(what) => a.blocked.push(Failure { path: rel.clone(), what }),
            }
        }

        let writes: Vec<(&String, &Entry)> = plan.ops.iter().filter_map(|o| if let Op::Write(p, e) = o { Some((p, e)) } else { None }).collect();
        let counter = AtomicU64::new(0);
        let results = {
            let next = AtomicUsize::new(0);
            let out: Mutex<Vec<Option<Result<u64, (bool, String)>>>> = Mutex::new((0..writes.len()).map(|_| None).collect());
            std::thread::scope(|s| {
                for _ in 0..workers(writes.len()) {
                    s.spawn(|| {
                        let mut packs = HashMap::<u32, File>::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= writes.len() {
                                break;
                            }
                            let r = self.write_one(writes[i].0, writes[i].1, cur, &known, &mut packs, counter.fetch_add(1, Ordering::Relaxed));
                            out.lock().unwrap()[i] = Some(r);
                        }
                    });
                }
            });
            out.into_inner().unwrap()
        };
        for ((rel, _), r) in writes.iter().zip(results) {
            match r.unwrap_or_else(|| Err((false, "not attempted".into()))) {
                Ok(bytes) => {
                    a.written += 1;
                    a.bytes += bytes;
                }
                Err((true, what)) => a.blocked.push(Failure { path: (*rel).clone(), what }),
                Err((false, what)) => a.failed.push(Failure { path: (*rel).clone(), what }),
            }
        }

        for op in &plan.ops {
            let Op::Attrs(rel, attrs) = op else { continue };
            let done = self.parents_ok(rel, false, &known).and_then(|_| match attrs_of(&join_rel(&self.root, rel)) {
                Ok(at) if at & A_REPARSE != 0 => Err("it is a link now; left alone".to_string()),
                Ok(_) => set_attrs(&join_rel(&self.root, rel), *attrs).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            });
            match done {
                Ok(()) => a.attributes += 1,
                Err(what) => a.failed.push(Failure { path: rel.clone(), what }),
            }
        }
        a
    }

    /// Can the file at `rel` be taken hold of as `claim` will, and then be
    /// replaced or deleted? Opened the same way, with the right to delete it
    /// asked for as well, and let go at once; nothing is read.
    fn probe(&self, rel: &str) -> R<()> {
        const READ_AND_DELETE: u32 = 0x12_0089 | 0x1_0000; // FILE_GENERIC_READ | DELETE
        let f = OpenOptions::new()
            .access_mode(READ_AND_DELETE)
            .share_mode(SHARE_READ | SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .open(join_rel(&self.root, rel))
            .map_err(|err| match err.raw_os_error() {
                Some(32) => "another program has it open for writing, or in a way that lets nobody replace it; it will be left alone".to_string(),
                _ => err.to_string(),
            })?;
        match stat_handle(f.as_raw_handle() as HANDLE) {
            Ok(now) if now.attrs & (A_REPARSE | A_DIRECTORY) != 0 => Err("it is no longer an ordinary file; it will be left alone".into()),
            _ => Ok(()),
        }
    }

    /// Could the folder at `rel` be removed once it is empty? Opened the way
    /// removing it opens it (asking for the right to delete) and let go at
    /// once. A folder that a program holds open cannot be: a shell whose
    /// current folder it is, for one.
    fn probe_dir(&self, rel: &str) -> R<()> {
        const DELETE: u32 = 0x1_0000;
        let w = wide(&join_rel(&self.root, rel));
        let h = unsafe { CreateFileW(w.as_ptr(), DELETE, SHARE_ALL, std::ptr::null(), OPEN_EXISTING, FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT, std::ptr::null_mut()) };
        if h == INVALID_HANDLE_VALUE {
            let e = io::Error::last_os_error();
            return Err(match e.raw_os_error() {
                Some(32) => "another program is using it (a shell whose current folder it is, for one); it stays".to_string(),
                _ => format!("it cannot be removed ({e}); it stays"),
            });
        }
        unsafe { CloseHandle(h) };
        Ok(())
    }

    /// Look at the folder as it is, changing nothing, and take out of the
    /// plan whatever `apply` would refuse or could not do: a link on the
    /// way to a path, something outside the tree sitting where a file or
    /// folder should go, a file another program holds open for writing, a
    /// file whose content the store no longer has, a folder that cannot be
    /// removed because it holds something outside the tree or because a
    /// program is using it. Each goes to `blocked` or `kept` with its reason.
    ///
    /// Both the dry run and the real run pass their plan through here, so
    /// what a dry run prints is what `--apply` then does. `apply` still
    /// checks everything again at the last moment: that is what protects
    /// the files, this is what keeps the report true.
    pub fn preflight(&self, p: &mut Plan, cur: &Manifest) {
        #[derive(Clone)]
        enum Dir {
            There,
            /// Not there, or about to be replaced by a folder: nothing can be in it.
            Absent,
            Unusable(String),
        }
        let abs = |rel: &str| join_rel(&self.root, rel);
        let is_file = |rel: &str| cur.get(rel).is_some_and(|c| c.kind == Kind::File);
        let mut why: Vec<Option<String>> = vec![None; p.ops.len()];
        let mut keep: Vec<Option<String>> = vec![None; p.ops.len()];

        // 1. Files to be replaced or deleted: can they be taken hold of?
        let held: Vec<usize> = (0..p.ops.len()).filter(|&i| matches!(&p.ops[i], Op::Delete(_)) || matches!(&p.ops[i], Op::Write(rel, _) if is_file(rel))).collect();
        let probed = parallel(held.len(), |k| match &p.ops[held[k]] {
            Op::Delete(rel) | Op::Write(rel, _) => self.probe(rel).err(),
            _ => None,
        });
        for (i, r) in held.iter().zip(probed) {
            why[*i] = r;
        }
        //    And files to be written: is their content in the store at all?
        //    (Lost with a pack file, say. Whether it is also undamaged is
        //    only known once it is read, when it is written.)
        for (i, op) in p.ops.iter().enumerate() {
            if let (Op::Write(_, e), None) = (op, &why[i]) {
                if !self.objects.has(&e.hash) {
                    why[i] = Some("its content is missing from the store, so it cannot be put back; it will be left as it is".into());
                }
            }
        }

        // What the plan takes away, by name (letter case folded, as Windows
        // compares names) and by which file on the volume it is. Something
        // found at a path is in the way unless it is one of these.
        let goes = |why: &[Option<String>], keep: &[Option<String>]| -> (HashSet<String>, HashSet<u64>) {
            let (mut names, mut ids) = (HashSet::new(), HashSet::new());
            for (i, op) in p.ops.iter().enumerate() {
                if let (Op::Delete(rel) | Op::RmDir(rel), None, None) = (op, &why[i], &keep[i]) {
                    names.insert(rel.to_lowercase());
                    ids.extend(cur.get(rel).map(|e| e.stamp.id).filter(|id| *id != 0));
                }
            }
            (names, ids)
        };
        let in_the_way = |rel: &str, at: &Stat, gone: &(HashSet<String>, HashSet<u64>)| !gone.0.contains(&rel.to_lowercase()) && !gone.1.contains(&at.stamp.id);

        // 2. Every folder on the way to each path: there, absent, or unusable.
        let gone = goes(&why, &keep);
        let mut dirs: HashMap<String, Dir> = HashMap::new();
        fn state(dir: &str, dirs: &mut HashMap<String, Dir>, look: &dyn Fn(&str) -> Dir) -> Dir {
            if dir.is_empty() {
                return Dir::There;
            }
            if let Some(known) = dirs.get(dir) {
                return known.clone();
            }
            let s = match state(parent_rel(dir), dirs, look) {
                Dir::There => look(dir),
                other => other,
            };
            dirs.insert(dir.to_string(), s.clone());
            s
        }
        let look = |dir: &str| match stat_path(&abs(dir)) {
            Ok(at) if at.attrs & A_REPARSE != 0 => Dir::Unusable(format!("{dir} is a link; restore never writes or deletes through one")),
            Ok(at) if at.attrs & A_DIRECTORY != 0 => Dir::There,
            Ok(at) if in_the_way(dir, &at, &gone) => Dir::Unusable(format!("{dir} is not a folder")),
            _ => Dir::Absent,
        };
        let above: Vec<Dir> = p.ops.iter().map(|op| state(parent_rel(op.path()), &mut dirs, &look)).collect();
        for (i, a) in above.iter().enumerate() {
            if let (Dir::Unusable(reason), None) = (a, &why[i]) {
                why[i] = Some(reason.clone());
            }
        }

        // 3. Folders to remove, deepest first: will each be empty by then?
        //    First, which folder on the volume each folder to be made already
        //    is, if it is one (it can only be one under a name that differs
        //    in letter case: a folder with exactly that name would be in the tree).
        let made: HashMap<u64, usize> = p
            .ops
            .iter()
            .enumerate()
            .filter(|(i, op)| matches!(op, Op::MkDir(..)) && why[*i].is_none() && matches!(above[*i], Dir::There))
            .filter_map(|(i, op)| stat_path(&abs(op.path())).ok().map(|at| (at, i)))
            .filter(|(at, _)| at.attrs & A_DIRECTORY != 0 && at.attrs & A_REPARSE == 0 && at.stamp.id != 0)
            .map(|(at, i)| (at.stamp.id, i))
            .collect();
        let index = |want: fn(&Op) -> Option<&String>| -> HashMap<&str, usize> { p.ops.iter().enumerate().filter_map(|(i, op)| want(op).map(|rel| (rel.as_str(), i))).collect() };
        let deletes = index(|op| if let Op::Delete(rel) = op { Some(rel) } else { None });
        let removes = index(|op| if let Op::RmDir(rel) = op { Some(rel) } else { None });
        let mut deepest: Vec<(&str, usize)> = removes.iter().map(|(rel, i)| (*rel, *i)).collect();
        deepest.sort_by_key(|(rel, _)| std::cmp::Reverse(rel.matches('/').count()));
        // Folders that cannot be emptied but are wanted again under a name
        // that differs only in letter case: renamed instead (which removal,
        // by which making).
        let mut renamed: HashMap<usize, usize> = HashMap::new();
        let mut buf = Vec::new();
        for (rel, i) in deepest {
            if why[i].is_some() {
                continue;
            }
            let Ok(listed) = list_dir(&abs(rel), &mut buf) else { continue };
            let stays = listed.iter().find_map(|l| {
                let Ok(name) = &l.name else { return Some("a name that is not valid Unicode".to_string()) };
                let child = format!("{rel}/{name}");
                let leaves = if l.attrs & A_REPARSE != 0 {
                    false
                } else if l.attrs & A_DIRECTORY != 0 {
                    removes.get(child.as_str()).is_some_and(|j| why[*j].is_none() && keep[*j].is_none() && !renamed.contains_key(j))
                } else {
                    // A leftover temporary file is cleared away before anything is applied.
                    is_restore_tmp(name) || deletes.get(child.as_str()).is_some_and(|j| why[*j].is_none())
                };
                (!leaves).then(|| name.clone())
            });
            let Some(name) = stays else {
                // Empty by then. A program that holds it open still keeps it there.
                keep[i] = self.probe_dir(rel).err();
                continue;
            };
            match cur.get(rel).and_then(|e| made.get(&e.stamp.id)) {
                Some(j) => drop(renamed.insert(i, *j)),
                None => keep[i] = Some(format!("it still holds {name}, which this would not remove (ignored, a link, or not in the tree); it stays, with what it holds")),
            }
        }

        // 4. New files and folders: is something outside the tree in their place?
        let gone = goes(&why, &keep);
        for (i, op) in p.ops.iter().enumerate() {
            if why[i].is_some() || !matches!(above[i], Dir::There) {
                continue;
            }
            match op {
                Op::Write(rel, _) if !is_file(rel) => {
                    if stat_path(&abs(rel)).is_ok_and(|at| in_the_way(rel, &at, &gone)) {
                        why[i] = Some("something is there that is not in the tree as it stands (ignored, a link, a folder that stays, or just created); it will be left alone".into());
                    }
                }
                Op::MkDir(rel, _) => {
                    if stat_path(&abs(rel)).is_ok_and(|at| at.attrs & A_REPARSE != 0 || (at.attrs & A_DIRECTORY == 0 && in_the_way(rel, &at, &gone))) {
                        why[i] = Some("something else is there (a link, or a file that is not in the tree as it stands); it will be left alone".into());
                    }
                }
                Op::Attrs(rel, _) => {
                    if stat_path(&abs(rel)).is_ok_and(|at| at.attrs & A_REPARSE != 0) {
                        why[i] = Some("it is a link now; it will be left alone".into());
                    }
                }
                _ => {}
            }
        }

        let ops = std::mem::take(&mut p.ops);
        let partners: HashSet<usize> = renamed.values().copied().collect();
        for (i, op) in ops.iter().enumerate() {
            // A removal and a making that are one rename become one line of the plan.
            let op = match (renamed.get(&i), op) {
                (Some(j), Op::RmDir(from)) => match (&why[*j], &ops[*j]) {
                    (None, Op::MkDir(to, attrs)) => Op::Rename(from.clone(), to.clone(), *attrs),
                    _ => op.clone(),
                },
                _ if partners.contains(&i) && why[i].is_none() => continue,
                _ => op.clone(),
            };
            let path = op.path().clone();
            match (why[i].take(), keep[i].take()) {
                (Some(what), _) => p.blocked.push(Failure { path, what }),
                (None, Some(what)) => p.kept.push(Failure { path, what }),
                (None, None) => p.ops.push(op),
            }
        }
    }

    /// What `restore` would do: make the whole tree equal `target`.
    pub fn plan_restore(&self, cur: &Manifest, target: &Manifest) -> Plan {
        let mut want: BTreeMap<String, Option<Entry>> = BTreeMap::new();
        for c in diff(cur, target) {
            want.insert(c.path.to_string(), c.after.cloned());
        }
        // Unreadable entries never compare equal, so they are in `want` and
        // the plan reports each one instead of passing over it in silence.
        plan(cur, &want)
    }

    /// What `undo` would do: put back what one step changed, where the tree
    /// still is as that step left it.
    pub fn plan_undo(&self, step: &Value, cur: &Manifest, force: bool) -> R<Plan> {
        let id = |field: &str| step[field].as_str().and_then(unhex).ok_or_else(|| format!("step {} names no `{field}` tree", step["seq"]));
        let (before, after) = (self.tree(&id("before")?)?, self.tree(&id("after")?)?);
        let mut want: BTreeMap<String, Option<Entry>> = BTreeMap::new();
        let mut conflicts = Vec::new();
        for c in diff(&after, &before) {
            // Here `before`/`after` are the diff's sides: c.before is the step's result, c.after what it replaced.
            let now = cur.get(c.path);
            if same(now, c.after) {
                continue;
            }
            if same(now, c.before) || force {
                want.insert(c.path.to_string(), c.after.cloned());
            } else {
                conflicts.push(Failure { path: c.path.to_string(), what: "changed after the step ended; left as it is (--force puts back the earlier content anyway)".into() });
            }
        }
        let mut p = plan(cur, &want);
        p.conflicts = conflicts;
        Ok(p)
    }

    /// Dry run or real: snapshot first, plan, and with `apply` carry it out,
    /// snapshot again and record the whole thing as one step.
    fn change(&mut self, kind: &str, apply: bool, detail: Value, make: impl Fn(&Store, &Manifest) -> R<Plan>) -> R<Value> {
        if !apply {
            self.load()?;
            let now = self.snapshot(&SnapOpts { label: None, reason: "dry-run", record: false, rehash: false })?;
            let mut p = make(self, &now.manifest)?;
            self.preflight(&mut p, &now.manifest);
            return Ok(json!({
                "applied": false, "kind": kind, "detail": detail, "now": hex(&now.tree), "plan": p.json(), "would_be_complete": p.whole(),
                "failures": failures_json(&now.failures), "restore_leftovers": now.leftovers,
            }));
        }
        let _guard = self.begin()?;
        let reason = format!("{kind}.before");
        let before = self.snapshot(&SnapOpts { label: None, reason: &reason, record: true, rehash: false })?;
        let mut p = make(self, &before.manifest)?;
        self.preflight(&mut p, &before.manifest);
        if p.ops.is_empty() {
            // Nothing to change: no step is recorded, so the step before this
            // stays the one that undo would take next.
            return Ok(json!({
                "applied": true, "nothing_to_do": true, "kind": kind, "detail": detail, "step": null, "before": hex(&before.tree), "after": hex(&before.tree),
                "plan": p.json(), "result": Applied::default().json(), "apply_ms": 0.0,
                "complete": p.whole(), "verified": true, "failures": failures_json(&before.failures), "restore_leftovers": before.leftovers,
            }));
        }
        let expected = expect(&before.manifest, &p);
        let t = Instant::now();
        let done = self.apply(&p, &before.manifest);
        let apply_ms = t.elapsed().as_secs_f64() * 1e3;
        let reason = format!("{kind}.after");
        let after = self.snapshot(&SnapOpts { label: None, reason: &reason, record: true, rehash: false })?;
        let mut step = json!({
            "t": "step", "kind": kind, "detail": detail, "before": hex(&before.tree), "after": hex(&after.tree), "time_ms": unix_ms(),
            "ms": apply_ms.round(), "written": done.written, "deleted": done.deleted,
            "conflicts": p.conflicts.len(), "blocked": p.blocked.len() + done.blocked.len(), "failed": done.failed.len(),
            "kept": p.kept.len() + done.kept_folders.len(),
        });
        let seq = self.log_append(&mut step)?;
        Ok(json!({
            "applied": true, "kind": kind, "detail": detail, "step": seq, "before": hex(&before.tree), "after": hex(&after.tree),
            "plan": p.json(), "result": done.json(), "apply_ms": (apply_ms * 10.0).round() / 10.0,
            // Complete: everything asked for was done. A path left alone, or
            // a folder that stays because of what it holds, makes it not so.
            "complete": done.complete() && p.whole(),
            // Every file the plan touched was read back from disk for the snapshot after.
            "verified": after.tree == expected,
            "failures": failures_json(&after.failures), "restore_leftovers": after.leftovers,
        }))
    }

    pub fn restore(&mut self, target: &Hash, apply: bool) -> R<Value> {
        self.load()?;
        let want = self.tree(target)?;
        let mut v = self.change("restore", apply, json!({ "target": hex(target) }), |s, cur| Ok(s.plan_restore(cur, &want)))?;
        if apply {
            v["reached_target"] = json!(v["after"] == hex(target));
        }
        Ok(v)
    }

    pub fn undo(&mut self, seq: Option<u64>, apply: bool, force: bool) -> R<Value> {
        self.load()?;
        let (step, passed) = self.step(seq)?;
        let step = step.clone();
        let detail = json!({ "undoes": step["seq"], "of": step["kind"], "cmd": step["detail"]["cmd"], "force": force, "passed_over": passed });
        let mut v = self.change("undo", apply, detail, |s, cur| s.plan_undo(&step, cur, force))?;
        if apply {
            v["back_to_before"] = json!(v["after"] == step["before"]);
        }
        Ok(v)
    }

    /// `run_step_with`, going by sizes and times: what `proteus run` does unless told otherwise.
    #[cfg(test)]
    pub fn run_step(&mut self, cmd: &[String], cwd: &Path, label: Option<&str>) -> Result<Value, RunFailed> {
        self.run_step_with(cmd, cwd, label, false)
    }

    /// Snapshot, run a command, snapshot again; the pair is one undoable step.
    /// The lock is not held while the command runs. If the state before
    /// cannot be recorded the command is not run at all.
    ///
    /// `rehash`: read every file before and after the command instead of
    /// going by sizes and times. That is what it takes to see a write made
    /// through a memory-mapped view (a database, a numeric array on disk):
    /// Windows changes neither the size nor any time of such a file.
    pub fn run_step_with(&mut self, cmd: &[String], cwd: &Path, label: Option<&str>, rehash: bool) -> Result<Value, RunFailed> {
        let not_run = |why: String| RunFailed { exit: None, why };
        let before = {
            let _guard = self.begin().map_err(not_run)?;
            self.snapshot(&SnapOpts { label: None, reason: "run.before", record: true, rehash }).map_err(not_run)?
        };
        let started = unix_ms();
        let t = Instant::now();
        let status = spawn(cmd, cwd).and_then(|mut child| child.wait()).map_err(|e| not_run(format!("could not run {:?}: {e}", cmd[0])))?;
        let ms = t.elapsed().as_secs_f64() * 1e3;
        let ran = |why: String| RunFailed { exit: Some(status.code().unwrap_or(1)), why };
        let _guard = self.begin().map_err(ran)?;
        let after = self.snapshot(&SnapOpts { label: None, reason: "run.after", record: true, rehash }).map_err(ran)?;
        let changes = diff(&before.manifest, &after.manifest);
        let count = |what: &str| changes.iter().filter(|c| c.what() == what).count();
        let mut step = json!({
            "t": "step", "kind": "run", "label": label, "before": hex(&before.tree), "after": hex(&after.tree), "time_ms": started,
            "ms": ms.round(), "exit": status.code(), "detail": { "cmd": cmd, "cwd": show(cwd) }, "rehash": rehash,
            "added": count("added"), "removed": count("removed"), "modified": changes.len() - count("added") - count("removed"),
            "failures": before.failures.len() + after.failures.len(),
        });
        let seq = self.log_append(&mut step).map_err(ran)?;
        step["seq"] = json!(seq);
        step["snapshot_ms"] = json!([(before.ms * 10.0).round() / 10.0, (after.ms * 10.0).round() / 10.0]);
        step["not_captured"] = failures_json(&[before.failures, after.failures].concat());
        Ok(step)
    }
}

/// `proteus run` could not record its step.
#[derive(Debug)]
pub struct RunFailed {
    /// The command's exit code if it ran; `None` if it was never started.
    pub exit: Option<i32>,
    pub why: String,
}

/// Start `cmd` with this process's console and standard streams. A name
/// with no extension that is not an .exe is looked up as the shell would
/// (PATHEXT), so `npm` finds `npm.cmd`.
fn spawn(cmd: &[String], cwd: &Path) -> io::Result<std::process::Child> {
    let start = |program: &std::ffi::OsStr| std::process::Command::new(program).args(&cmd[1..]).current_dir(cwd).spawn();
    match start(cmd[0].as_ref()) {
        Err(e) if e.kind() == io::ErrorKind::NotFound && Path::new(&cmd[0]).extension().is_none() && !cmd[0].contains(['\\', '/']) => {
            let exts = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
            let dirs = std::iter::once(cwd.to_path_buf()).chain(std::env::var_os("PATH").iter().flat_map(std::env::split_paths).collect::<Vec<_>>());
            for dir in dirs {
                for ext in exts.split(';').filter(|x| !x.is_empty()) {
                    let candidate = dir.join(format!("{}{ext}", cmd[0]));
                    if candidate.is_file() {
                        return start(candidate.as_os_str());
                    }
                }
            }
            Err(e)
        }
        other => other,
    }
}

// ---------------------------------------------------------------- watch

pub struct Capture {
    pub snap: Snap,
    /// Change events that led to this capture (0 for the one taken at start).
    pub events: u64,
    pub rescans: u64,
    /// From reading the first of those events to the snapshot being on disk.
    pub latency_ms: f64,
}

/// Decides whether a change event can matter to the tree, by the same rules
/// as the walk, and without touching the disk for the common case (a build
/// writing into an ignored folder).
struct Relevant {
    root: PathBuf,
    skip: Vec<PathBuf>,
    hermetic: bool,
    base: Option<RuleBase>,
    /// Folders already judged, by path under the root: `None` is ignored,
    /// otherwise the rules that hold inside.
    dirs: HashMap<PathBuf, Option<Option<Arc<Rules>>>>,
}

impl Relevant {
    fn new(store: &Store) -> Relevant {
        Relevant { root: store.root.clone(), skip: store.skip.clone(), hermetic: store.hermetic, base: None, dirs: HashMap::new() }
    }

    /// Forget every compiled rule: an ignore file changed, or events were lost.
    fn forget(&mut self) {
        self.base = None;
        self.dirs.clear();
    }

    /// The rules inside the folder at `rel`, or `None` if it is ignored.
    fn inside(&mut self, rel: &Path) -> Option<Option<Arc<Rules>>> {
        if let Some(known) = self.dirs.get(rel) {
            return known.clone();
        }
        let mut sink = Vec::new();
        let base = self.base.get_or_insert_with(|| RuleBase::new(&self.root, self.hermetic, &mut sink));
        let above = match rel.parent() {
            None => Some(base.above.clone()),
            Some(parent) => {
                let parent = parent.to_path_buf();
                self.inside(&parent)
            }
        };
        let base = self.base.as_ref().expect("made above");
        let abs = self.root.join(rel);
        let judged = above.and_then(|rules| {
            if rel.as_os_str().is_empty() || !base.ignored(rules.as_deref(), &abs, true) {
                Some(base.enter(&abs, (true, true, true), rules, &mut sink))
            } else {
                None
            }
        });
        self.dirs.insert(rel.to_path_buf(), judged.clone());
        judged
    }

    fn check(&mut self, ev: &watch::Event) -> bool {
        if ev.change == watch::Change::Rescan {
            self.forget();
            return true;
        }
        let name = ev.path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if ev.path.components().any(|c| VCS.iter().any(|v| c.as_os_str() == *v)) {
            // Git's own exclude file lives in there and changes the rules.
            if name == "exclude" {
                self.forget();
            }
            return false;
        }
        let abs = self.root.join(&ev.path);
        if self.skip.iter().any(|s| abs.starts_with(s)) || is_restore_tmp(name) {
            return false;
        }
        if name == ".gitignore" || name == ".proteusignore" {
            self.forget();
            return true;
        }
        // A folder that was renamed or removed may be remembered under its old name.
        if !matches!(ev.change, watch::Change::Create | watch::Change::Modify) {
            self.dirs.retain(|dir, _| !dir.starts_with(&ev.path));
            if let watch::Change::Rename { from } = &ev.change {
                self.dirs.retain(|dir, _| !dir.starts_with(from));
            }
        }
        let Some(rules) = self.inside(ev.path.parent().unwrap_or(Path::new(""))) else { return false };
        let base = self.base.as_ref().expect("made by inside");
        if base.ignored(rules.as_deref(), &abs, false) {
            return false;
        }
        // Not ignored as a file; a folder may still be (a rule like `target/`).
        !(abs.is_dir() && base.ignored(rules.as_deref(), &abs, true))
    }
}

/// Watch the tree and snapshot it whenever something that is not ignored
/// has changed and then been quiet for `debounce`. `each` is called after
/// every capture (the first is taken at once, before any event) and with
/// `Err` when a capture fails; return false to stop. Blocks in the kernel
/// while nothing changes.
/// `limits`: the OS buffer in bytes and the event queue length, if not the defaults.
pub fn watch(store: &mut Store, debounce: Duration, limits: Option<(usize, usize)>, mut each: impl FnMut(Result<&Capture, &str>) -> bool) -> R<()> {
    let w = match limits {
        Some((buffer, queue)) => watch::Watcher::with_limits(&store.root, buffer, queue),
        None => watch::Watcher::start(&store.root),
    }
    .map_err(|e| format!("cannot watch {}: {e}", show(&store.root)))?;
    let mut relevant = Relevant::new(store);
    let capture = |store: &mut Store, events: u64, rescans: u64, first: Instant| -> R<Capture> {
        let _guard = store.begin()?;
        let snap = store.snapshot(&SnapOpts { label: None, reason: "watch", record: true, rehash: false })?;
        Ok(Capture { snap, events, rescans, latency_ms: first.elapsed().as_secs_f64() * 1e3 })
    };
    let mut report = |r: R<Capture>| match r {
        Ok(c) => each(Ok(&c)),
        Err(e) => each(Err(&e)),
    };
    if !report(capture(store, 0, 0, Instant::now())) {
        return Ok(());
    }
    loop {
        let Ok(ev) = w.events.recv() else { break };
        if !relevant.check(&ev) {
            continue;
        }
        let first = ev.at;
        let (mut events, mut rescans) = (1u64, (ev.change == watch::Change::Rescan) as u64);
        // Wait for quiet, but not for ever: a tree that never rests is
        // captured every two seconds anyway.
        while first.elapsed() < Duration::from_secs(2) {
            match w.events.recv_timeout(debounce) {
                Ok(e) => {
                    if relevant.check(&e) {
                        events += 1;
                        rescans += (e.change == watch::Change::Rescan) as u64;
                    }
                }
                Err(_) => break,
            }
        }
        if !report(capture(store, events, rescans, first)) {
            return Ok(());
        }
    }
    let code = w.counters.ended_with.load(Ordering::Relaxed);
    Err(format!("the watch on {} ended (Windows error {code}): was the folder removed?", show(&store.root)))
}

// ---------------------------------------------------------------- command line

#[derive(Default)]
struct Args {
    root: Option<String>,
    json: bool,
    apply: bool,
    force: bool,
    rehash: bool,
    label: Option<String>,
    n: Option<u64>,
    step: Option<u64>,
    max_files: Option<u64>,
    max_mb: Option<u64>,
    debounce: Option<u64>,
    buffer_kb: Option<u64>,
    rest: Vec<String>,
}

/// Options this command accepts; anything else is an error, never ignored.
fn parse(args: &[String], accepts: &[&str]) -> R<Args> {
    let mut a = Args::default();
    let mut i = 0;
    let number = |i: &mut usize, name: &str| -> R<u64> {
        *i += 1;
        args.get(*i).and_then(|v| v.parse::<u64>().ok()).ok_or_else(|| format!("{name} needs a number"))
    };
    while i < args.len() {
        let arg = args[i].as_str();
        if arg.starts_with('-') && arg.len() > 1 && !accepts.contains(&arg) && arg != "--root" && arg != "--json" {
            return Err(format!("unknown option {arg}"));
        }
        match arg {
            "--root" => {
                i += 1;
                a.root = Some(args.get(i).ok_or("--root needs a folder")?.clone());
            }
            "--json" => a.json = true,
            "--apply" => a.apply = true,
            "--force" => a.force = true,
            "--rehash" => a.rehash = true,
            "-m" | "--label" => {
                i += 1;
                a.label = Some(args.get(i).ok_or("-m needs a label")?.clone());
            }
            "-n" => a.n = Some(number(&mut i, "-n")?),
            "--step" => a.step = Some(number(&mut i, "--step")?),
            "--max-files" => a.max_files = Some(number(&mut i, "--max-files")?),
            "--max-mb" => a.max_mb = Some(number(&mut i, "--max-mb")?),
            "--debounce" => a.debounce = Some(number(&mut i, "--debounce")?),
            "--buffer-kb" => a.buffer_kb = Some(number(&mut i, "--buffer-kb")?),
            _ => a.rest.push(args[i].clone()),
        }
        i += 1;
    }
    Ok(a)
}

fn here() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn emit(json: bool, v: &Value, human: impl FnOnce(&Value) -> String) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_default());
    } else {
        println!("{}", human(v).trim_end());
    }
}

/// The first 12 characters of an id held in JSON.
fn id12(v: &Value) -> String {
    v.as_str().unwrap_or("").chars().take(12).collect()
}

/// A size in bytes held in JSON, as a person reads it.
fn mb(v: &Value) -> String {
    match v.as_u64().unwrap_or(0) {
        n if n < 1024 => format!("{n} bytes"),
        n if n < 1 << 20 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / 1048576.0),
    }
}

fn list(title: &str, v: &Value, out: &mut String) {
    let Some(items) = v.as_array().filter(|a| !a.is_empty()) else { return };
    out.push_str(&format!("{title} ({}):\n", items.len()));
    for f in items.iter().take(40) {
        match f.as_str() {
            Some(s) => out.push_str(&format!("  {s}\n")),
            None => out.push_str(&format!("  {}: {}\n", f["path"].as_str().unwrap_or(""), f["what"].as_str().unwrap_or(""))),
        }
    }
    if items.len() > 40 {
        out.push_str(&format!("  ... and {} more (--json lists all)\n", items.len() - 40));
    }
}

fn human_snap(v: &Value) -> String {
    let mut s = format!(
        "tree {}  {} files, {} folders, {}  ({} read, {} new objects, {} stored, {} ms)\n",
        id12(&v["tree"]),
        v["files"],
        v["folders"],
        mb(&v["bytes"]),
        v["read_files"],
        v["new_objects"],
        mb(&v["stored_bytes"]),
        v["ms"]
    );
    match v["seq"].as_u64() {
        Some(n) => s.push_str(&format!("recorded as log entry {n}\n")),
        None => s.push_str("unchanged since the last snapshot; nothing recorded\n"),
    }
    list("not captured", &v["failures"], &mut s);
    list("links, skipped and never followed", &v["skipped_links"], &mut s);
    if let Some(n) = v["swept"].as_u64().filter(|n| *n > 0) {
        s.push_str(&format!("removed {n} temporary file(s) left by a restore that did not finish\n"));
    }
    list(LEFTOVERS, &v["restore_leftovers"], &mut s);
    s
}

/// Heading for temporary files a restore that did not finish left in the folder.
const LEFTOVERS: &str = "left by a restore that did not finish (not part of any tree; the next snapshot or --apply removes them)";

fn human_changes(changes: &Value, out: &mut String) {
    let Some(items) = changes.as_array() else { return };
    for c in items.iter().take(200) {
        let mark = match c["change"].as_str().unwrap_or("") {
            "added" => "+",
            "removed" => "-",
            "modified" => "M",
            "attributes" => "a",
            _ => "?",
        };
        let slash = if c["folder"] == true { "/" } else { "" };
        let when = c["written"].as_str().map(|w| format!("   (written {w})")).unwrap_or_default();
        out.push_str(&format!("  {mark} {}{slash}{when}\n", c["path"].as_str().unwrap_or("")));
    }
    if items.len() > 200 {
        out.push_str(&format!("  ... and {} more (--json lists all)\n", items.len() - 200));
    }
}

fn human_plan(v: &Value) -> String {
    let p = &v["plan"];
    let applied = v["applied"] == true;
    let mut s = String::new();
    if let Some(n) = v["detail"]["undoes"].as_u64() {
        let cmd = v["detail"]["cmd"].as_array().map(|c| c.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default();
        s.push_str(&format!("undoing step {n} ({} {cmd})\n", v["detail"]["of"].as_str().unwrap_or("")));
        if let Some(passed) = v["detail"]["passed_over"].as_array().filter(|p| !p.is_empty()) {
            s.push_str(&format!("passed over {} later step(s) that changed nothing\n", passed.len()));
        }
    }
    const KEPT: &str = "folders that stay (because of what they hold, or of who is using them)";
    if v["nothing_to_do"] == true {
        s.push_str(if v["complete"] == true {
            "nothing to do: the folder already is as asked. No step was recorded.\n"
        } else {
            "nothing was done: everything this would change is held back, as listed. No step was recorded.\n"
        });
        list("conflicts, left as they are", &p["conflicts"], &mut s);
        list("blocked", &p["blocked"], &mut s);
        list(KEPT, &p["kept"], &mut s);
        return s;
    }
    if applied {
        let r = &v["result"];
        s.push_str(&format!(
            "{} applied as step {}: wrote {} files ({}), deleted {}, made {} folders, removed {}{} ({} ms)\n",
            v["kind"].as_str().unwrap_or(""),
            v["step"],
            r["written"],
            mb(&r["bytes"]),
            r["deleted"],
            r["folders_made"],
            r["folders_removed"],
            r["folders_renamed"].as_u64().filter(|n| *n > 0).map(|n| format!(", renamed {n}")).unwrap_or_default(),
            v["apply_ms"]
        ));
        s.push_str(&format!("tree before {}  after {}\n", id12(&v["before"]), id12(&v["after"])));
        list("refused at the last moment", &r["blocked"], &mut s);
        list("failed", &r["failed"], &mut s);
        list("folders that could not be removed at the last moment (not empty, or in use)", &r["kept_folders"], &mut s);
    } else {
        s.push_str(&format!(
            "DRY RUN: nothing was changed. {} would write {} files ({}), delete {}, make {} folders, remove {}{}.\n",
            v["kind"].as_str().unwrap_or(""),
            p["write"],
            mb(&p["bytes"]),
            p["delete"],
            p["make_folders"],
            p["remove_folders"],
            p["rename_folders"].as_u64().filter(|n| *n > 0).map(|n| format!(", rename {n}")).unwrap_or_default()
        ));
    }
    for op in p["ops"].as_array().into_iter().flatten().take(if applied { 0 } else { 400 }) {
        let (verb, path) = (op["do"].as_str().unwrap_or(""), op["path"].as_str().unwrap_or(""));
        let was = op["from"].as_str().map(|from| format!("   (now {from})")).unwrap_or_default();
        s.push_str(&format!("  {verb:<13} {path}{was}\n"));
    }
    if !applied && p["ops"].as_array().is_some_and(|o| o.len() > 400) {
        s.push_str("  ... (--json lists all)\n");
    }
    list("conflicts, left as they are", &p["conflicts"], &mut s);
    list("blocked", &p["blocked"], &mut s);
    list(KEPT, &p["kept"], &mut s);
    list("not captured", &v["failures"], &mut s);
    list(LEFTOVERS, &v["restore_leftovers"], &mut s);
    if applied {
        s.push_str(if v["complete"] == true { "complete.\n" } else { "NOT complete: see above.\n" });
        s.push_str("to take this back: proteus chronicle undo --apply\n");
    } else {
        if v["would_be_complete"] == false {
            s.push_str("with --apply this would NOT be complete: what is listed above would be left as it is.\n");
        }
        s.push_str("to do it: add --apply (the current state is snapshotted first, so it can be undone)\n");
    }
    s
}

fn data() -> PathBuf {
    crate::paths::data_dir()
}

fn profile() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

/// What a command's bare words may be. `folder`: one word is the folder, as
/// --root would give it (`chronicle watch DIR`). Otherwise up to `trees`
/// names of trees. A word no command asked for is an error: a folder named
/// by mistake must never be swapped for the default one in silence.
fn words(mut a: Args, folder: bool, trees: usize) -> R<Args> {
    if folder && a.rest.len() == 1 && a.root.is_none() {
        a.root = a.rest.pop();
    }
    match a.rest.get(trees) {
        Some(extra) => Err(format!("unexpected argument {extra:?}")),
        None => Ok(a),
    }
}

/// `proteus chronicle ...`
pub fn cli(args: &[String]) -> crate::Res<()> {
    const USAGE: &str = "usage: proteus chronicle track|status|snap|log|undo|watch [DIR] | diff A [B] | restore TREE   [--root DIR] [--json]";
    let Some(sub) = args.first() else { return Err(USAGE.into()) };
    let rest = &args[1..];
    match sub.as_str() {
        "track" => {
            let a = words(parse(rest, &["--max-files", "--max-mb"])?, true, 0)?;
            let root = find_root(a.root.as_deref(), &here());
            let (store, snap) = Store::track(&data(), &root, a.max_files, a.max_mb, profile().as_deref(), false)?;
            let mut v = snap.json();
            v["root"] = json!(show(&store.root));
            v["store"] = json!(show(&store.dir));
            v["max_files"] = json!(store.max_files);
            v["max_mb"] = json!(store.max_bytes >> 20);
            emit(a.json, &v, |v| {
                format!(
                    "tracking {}\nhistory in {}\nbudget {} files, {} MB\n{}ignored files, version-control folders and links are not captured and will not be restored.",
                    v["root"].as_str().unwrap_or(""),
                    v["store"].as_str().unwrap_or(""),
                    v["max_files"],
                    v["max_mb"],
                    human_snap(v)
                )
            });
        }
        "status" => {
            let a = words(parse(rest, &[])?, true, 0)?;
            let root = find_root(a.root.as_deref(), &here());
            let mut store = match Store::open(&data(), &root) {
                Ok(s) => s,
                Err(why) => {
                    let v = json!({ "tracked": false, "root": show(&canon(&root)?), "note": why });
                    emit(a.json, &v, |v| v["note"].as_str().unwrap_or("").to_string());
                    return Ok(());
                }
            };
            store.load()?;
            let now = store.snapshot(&SnapOpts { label: None, reason: "status", record: false, rehash: false })?;
            let head = store.head();
            let changes: Vec<Value> = match head {
                Some(h) => diff(&store.tree(&h)?, &now.manifest).iter().map(Change::json).collect(),
                None => Vec::new(),
            };
            let (objects, stored) = store.objects.stats();
            let v = json!({
                "tracked": true, "root": show(&store.root), "store": show(&store.dir),
                "snapshots": store.log.iter().filter(|r| r["t"] == "snap").count(),
                "steps": store.log.iter().filter(|r| r["t"] == "step").count(),
                "last": head.map(|h| hex(&h)), "now": hex(&now.tree), "clean": head == Some(now.tree),
                "files": now.files, "folders": now.dirs, "bytes": now.bytes,
                "objects": objects, "stored_bytes": stored, "max_files": store.max_files, "max_mb": store.max_bytes >> 20,
                "changes": changes, "failures": failures_json(&now.failures), "skipped_links": now.links, "restore_leftovers": now.leftovers,
            });
            emit(a.json, &v, |v| {
                let mut s = format!(
                    "{}\n{} files, {} folders, {}  |  {} snapshots, {} steps  |  store: {} objects, {}\n",
                    v["root"].as_str().unwrap_or(""),
                    v["files"],
                    v["folders"],
                    mb(&v["bytes"]),
                    v["snapshots"],
                    v["steps"],
                    v["objects"],
                    mb(&v["stored_bytes"])
                );
                if v["clean"] == true {
                    s.push_str("nothing has changed since the last snapshot\n");
                } else {
                    s.push_str("changed since the last snapshot:\n");
                    human_changes(&v["changes"], &mut s);
                }
                list("cannot be read", &v["failures"], &mut s);
                list("links, skipped and never followed", &v["skipped_links"], &mut s);
                list(LEFTOVERS, &v["restore_leftovers"], &mut s);
                s
            });
        }
        "snap" => {
            let a = words(parse(rest, &["-m", "--label", "--rehash"])?, true, 0)?;
            let mut store = Store::open(&data(), &find_root(a.root.as_deref(), &here()))?;
            let _guard = store.begin()?;
            let snap = store.snapshot(&SnapOpts { label: a.label.as_deref(), reason: "snap", record: true, rehash: a.rehash })?;
            emit(a.json, &snap.json(), human_snap);
        }
        "log" => {
            let a = words(parse(rest, &["-n"])?, true, 0)?;
            let store = Store::open(&data(), &find_root(a.root.as_deref(), &here()))?;
            let n = a.n.unwrap_or(30) as usize;
            let entries: Vec<&Value> = store.log.iter().skip(store.log.len().saturating_sub(n)).collect();
            emit(a.json, &json!({ "root": show(&store.root), "total": store.log.len(), "entries": entries }), |v| {
                let mut s = String::new();
                for r in v["entries"].as_array().into_iter().flatten() {
                    let when = stamp(r["time_ms"].as_u64().unwrap_or(0));
                    let id = |f: &str| id12(&r[f]);
                    if r["t"] == "snap" {
                        let label = r["label"].as_str().map(|l| format!("  \"{l}\"")).unwrap_or_default();
                        s.push_str(&format!("{:>5}  {when}  snap  {}  {} files {}  ({}){label}\n", r["seq"], id("tree"), r["files"], mb(&r["bytes"]), r["reason"].as_str().unwrap_or("")));
                    } else {
                        let what = match r["kind"].as_str().unwrap_or("") {
                            "run" => format!(
                                "exit {} in {} ms: {}",
                                r["exit"],
                                r["ms"],
                                r["detail"]["cmd"].as_array().map(|c| c.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")).unwrap_or_default()
                            ),
                            "undo" => format!("of step {}", r["detail"]["undoes"]),
                            _ => format!("to {}", id12(&r["detail"]["target"])),
                        };
                        s.push_str(&format!("{:>5}  {when}  STEP  {} -> {}  {} {what}\n", r["seq"], id("before"), id("after"), r["kind"].as_str().unwrap_or("")));
                    }
                }
                if s.is_empty() {
                    s.push_str("nothing recorded yet");
                }
                s
            });
        }
        "diff" => {
            let a = words(parse(rest, &[])?, false, 2)?;
            let mut store = Store::open(&data(), &find_root(a.root.as_deref(), &here()))?;
            store.load()?;
            let from = store.resolve(a.rest.first().ok_or("usage: proteus chronicle diff A [B]   (B defaults to the folder as it is now)")?)?;
            let (to_id, to, written) = match a.rest.get(1) {
                Some(b) => {
                    let id = store.resolve(b)?;
                    (id, store.tree(&id)?, store.written(&id))
                }
                None => {
                    let now = store.snapshot(&SnapOpts { label: None, reason: "diff", record: false, rehash: false })?;
                    let written = now.manifest.iter().filter(|(_, e)| e.kind != Kind::Dir).map(|(p, e)| (p.clone(), ft_to_unix_ms(e.stamp.mtime))).collect();
                    (now.tree, now.manifest, written)
                }
            };
            let from_tree = store.tree(&from)?;
            let changes: Vec<Value> = diff(&from_tree, &to)
                .iter()
                .map(|c| {
                    let mut v = c.json();
                    if let Some(ms) = written.get(c.path).filter(|_| c.after.is_some()) {
                        v["written_ms"] = json!(ms);
                        v["written"] = json!(stamp(*ms));
                    }
                    v
                })
                .collect();
            emit(a.json, &json!({ "from": hex(&from), "to": hex(&to_id), "changes": changes }), |v| {
                let mut s = format!("{} -> {}\n", id12(&v["from"]), id12(&v["to"]));
                human_changes(&v["changes"], &mut s);
                if v["changes"].as_array().is_some_and(|c| c.is_empty()) {
                    s.push_str("  the same\n");
                }
                s
            });
        }
        "restore" | "undo" => {
            let a = parse(rest, if sub == "undo" { &["--apply", "--force", "--step"] } else { &["--apply"] })?;
            let a = words(a, sub == "undo", if sub == "undo" { 0 } else { 1 })?;
            let mut store = Store::open(&data(), &find_root(a.root.as_deref(), &here()))?;
            let v = if sub == "undo" {
                store.undo(a.step, a.apply, a.force)?
            } else {
                store.load()?;
                let target = store.resolve(a.rest.first().ok_or("usage: proteus chronicle restore TREE [--apply]   (TREE: a log number, `last`, or the start of a tree id)")?)?;
                store.restore(&target, a.apply)?
            };
            emit(a.json, &v, human_plan);
            if a.apply && v["complete"] != true {
                std::process::exit(3);
            }
        }
        "watch" => {
            let a = words(parse(rest, &["--debounce", "--buffer-kb"])?, true, 0)?;
            let mut store = Store::open(&data(), &find_root(a.root.as_deref(), &here()))?;
            let debounce = Duration::from_millis(a.debounce.unwrap_or(200).clamp(10, 60_000));
            let buffer = a.buffer_kb.map(|k| ((k as usize) << 10, watch::DEFAULT_QUEUE));
            let (root, json_out) = (show(&store.root), a.json);
            if !json_out {
                println!("watching {root} (captures {} ms after the last change; Ctrl+C stops)", debounce.as_millis());
            }
            watch(&mut store, debounce, buffer, |c| {
                match c {
                    Ok(c) if json_out => {
                        let mut v = c.snap.json();
                        v["events"] = json!(c.events);
                        v["rescans"] = json!(c.rescans);
                        v["latency_ms"] = json!((c.latency_ms * 10.0).round() / 10.0);
                        v["recorded_ms"] = json!(unix_ms());
                        println!("{v}");
                    }
                    Ok(c) if c.snap.seq.is_some() || c.events == 0 => println!(
                        "{}  tree {}  {} files  {} read  {} events  recorded {:.0} ms after the first{}",
                        stamp(unix_ms()),
                        short(&c.snap.tree),
                        c.snap.files,
                        c.snap.read_files,
                        c.events,
                        c.latency_ms,
                        if c.snap.failures.is_empty() { String::new() } else { format!("  ({} not captured)", c.snap.failures.len()) }
                    ),
                    Ok(_) => {}
                    Err(e) if json_out => println!("{}", json!({ "error": e })),
                    Err(e) => println!("capture failed: {e}"),
                }
                let _ = io::stdout().flush();
                true
            })?;
        }
        other => return Err(format!("unknown chronicle command {other:?}. {USAGE}").into()),
    }
    Ok(())
}

unsafe extern "system" fn ignore_ctrl_c(kind: u32) -> i32 {
    // Ctrl+C and Ctrl+Break go to the command being run; this process waits
    // for it so that the snapshot after it is still taken.
    (kind <= 1) as i32
}

/// `proteus run [--root DIR] [--json] [-m LABEL] [--rehash] [--] CMD ARGS...`
/// `--rehash` reads every file before and after the command (see `run_step_with`).
/// Exits with the command's own exit code whenever the command ran (if the
/// state after it could not be recorded, that is said on stderr). 125: the
/// state before could not be recorded, so the command was not run. 127: the
/// command could not be started.
pub fn run_cli(args: &[String]) -> crate::Res<()> {
    let split = args.iter().position(|a| a == "--");
    let (ours, cmd): (&[String], &[String]) = match split {
        Some(i) => (&args[..i], &args[i + 1..]),
        None => {
            // No `--`: our options end at the first word that is not one.
            let mut i = 0;
            while i < args.len() && args[i].starts_with('-') {
                i += if matches!(args[i].as_str(), "--root" | "-m" | "--label") { 2 } else { 1 };
            }
            let i = i.min(args.len());
            (&args[..i], &args[i..])
        }
    };
    let a = parse(ours, &["-m", "--label", "--rehash"])?;
    if cmd.is_empty() || !a.rest.is_empty() {
        return Err("usage: proteus run [--root DIR] [--json] [-m LABEL] [--rehash] -- CMD ARGS...".into());
    }
    let cwd = here();
    fn fail(code: i32, why: String) -> ! {
        eprintln!("proteus run: {why}");
        std::process::exit(code)
    }
    let mut store = match Store::open(&data(), &find_root(a.root.as_deref(), &cwd)) {
        Ok(s) => s,
        Err(why) => fail(125, format!("{why}\nThe command was not run.")),
    };
    unsafe { windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(ignore_ctrl_c), 1) };
    match store.run_step_with(cmd, &cwd, a.label.as_deref(), a.rehash) {
        Ok(step) => {
            let code = step["exit"].as_i64().unwrap_or(1) as i32;
            if a.json {
                eprintln!("{step}");
            } else {
                eprintln!(
                    "proteus: step {} | exit {code} | {} ms | +{} -{} ~{} | undo: proteus chronicle undo{}",
                    step["seq"],
                    step["ms"],
                    step["added"],
                    step["removed"],
                    step["modified"],
                    if step["failures"].as_u64().unwrap_or(0) > 0 { " | some files could not be captured: proteus chronicle status" } else { "" }
                );
            }
            std::process::exit(code)
        }
        Err(RunFailed { exit: Some(code), why }) => {
            if a.json {
                eprintln!("{}", json!({ "t": "step", "kind": "run", "exit": code, "recorded": false, "error": why }));
            } else {
                eprintln!("proteus run: the command ran and exited with {code}, but this step was NOT recorded and cannot be undone: {why}");
            }
            std::process::exit(code)
        }
        Err(RunFailed { exit: None, why }) if why.starts_with("could not run") => fail(127, why),
        Err(RunFailed { exit: None, why }) => fail(125, format!("{why}\nThe command was not run.")),
    }
}

#[cfg(test)]
#[path = "chronicle_tests.rs"]
mod tests;
