//! What a session's processes did: the event the OS sensor emits, and the
//! ledger built from those events. Pure data, no OS calls (the sensor is
//! `procs_win.rs`), so the daemon, the window, the journal reader and the
//! command line all share one implementation.
//!
//! An event travels as one JSON object: in the journal as a `K_PROC` record,
//! on the wire as a `D_PROC` frame. Times are microseconds since the Unix
//! epoch and come from the OS (a process's own creation and exit times), so
//! they are exact even when the sensor itself was late.
//!
//! Process ids are reused by Windows, so the ledger gives every process its
//! own `id` and never treats a pid as an identity across time.
use std::collections::HashMap;
use std::io::{self, BufReader, Read};
use std::path::Path;

use serde_json::{json, Map, Value};

/// A process was added to the session.
#[derive(Clone, Debug, PartialEq)]
pub struct Start {
    pub pid: u32,
    /// The process that created it. `None`: it was gone before it could be asked.
    pub ppid: Option<u32>,
    /// Full path of the executable.
    pub image: Option<String>,
    /// The command line exactly as the process received it. `None` is "could
    /// not be read" (the process had already ended), not "empty".
    pub cmd: Option<String>,
    /// Creation time, from the OS.
    pub start_us: Option<u64>,
    /// When the sensor recorded this event. `seen_us - start_us` is the
    /// sensor's latency for this process.
    pub seen_us: u64,
}

/// A process in the session ended.
#[derive(Clone, Debug, PartialEq)]
pub struct Exit {
    pub pid: u32,
    pub code: Option<u32>,
    /// Exit time, from the OS.
    pub end_us: Option<u64>,
    /// CPU time spent in user and kernel mode over the process's whole life.
    pub user_us: Option<u64>,
    pub kernel_us: Option<u64>,
    /// The OS classed the exit as abnormal (an unhandled exception, a kill
    /// with a status code).
    pub abnormal: bool,
    pub seen_us: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProcEvent {
    Start(Start),
    Exit(Exit),
}

fn put<T: Into<Value>>(m: &mut Map<String, Value>, key: &str, v: Option<T>) {
    if let Some(v) = v {
        m.insert(key.into(), v.into());
    }
}

impl ProcEvent {
    /// When the sensor recorded the event.
    pub fn seen_us(&self) -> u64 {
        match self {
            ProcEvent::Start(s) => s.seen_us,
            ProcEvent::Exit(e) => e.seen_us,
        }
    }

    /// Unknown fields are left out rather than written as null, so a reader
    /// can tell "not known" from "known to be empty".
    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        match self {
            ProcEvent::Start(s) => {
                m.insert("ev".into(), "start".into());
                m.insert("pid".into(), s.pid.into());
                put(&mut m, "ppid", s.ppid);
                put(&mut m, "image", s.image.clone());
                put(&mut m, "cmd", s.cmd.clone());
                put(&mut m, "start_us", s.start_us);
                m.insert("seen_us".into(), s.seen_us.into());
            }
            ProcEvent::Exit(e) => {
                m.insert("ev".into(), "exit".into());
                m.insert("pid".into(), e.pid.into());
                put(&mut m, "code", e.code);
                put(&mut m, "end_us", e.end_us);
                put(&mut m, "user_us", e.user_us);
                put(&mut m, "kernel_us", e.kernel_us);
                if e.abnormal {
                    m.insert("abnormal".into(), true.into());
                }
                m.insert("seen_us".into(), e.seen_us.into());
            }
        }
        Value::Object(m)
    }

    /// `None` for anything this build does not understand: a newer build may
    /// add event kinds, and an older reader must skip them, not fail.
    pub fn from_json(v: &Value) -> Option<Self> {
        let pid = u32::try_from(v["pid"].as_u64()?).ok()?;
        let seen_us = v["seen_us"].as_u64()?;
        let num = |k: &str| v[k].as_u64();
        let text = |k: &str| v[k].as_str().map(String::from);
        match v["ev"].as_str()? {
            "start" => Some(ProcEvent::Start(Start {
                pid,
                ppid: num("ppid").and_then(|p| u32::try_from(p).ok()),
                image: text("image"),
                cmd: text("cmd"),
                start_us: num("start_us"),
                seen_us,
            })),
            "exit" => Some(ProcEvent::Exit(Exit {
                pid,
                code: num("code").and_then(|c| u32::try_from(c).ok()),
                end_us: num("end_us"),
                user_us: num("user_us"),
                kernel_us: num("kernel_us"),
                abnormal: v["abnormal"].as_bool().unwrap_or(false),
                seen_us,
            })),
            _ => None,
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.to_json()).unwrap_or_default()
    }

    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        Self::from_json(&serde_json::from_slice(b).ok()?)
    }
}

/// How a process's life ended, as far as it is known.
#[derive(Clone, Debug, PartialEq)]
pub struct End {
    pub code: Option<u32>,
    /// Exit time; when the OS could not give one, the moment the exit was seen.
    pub end_us: u64,
    pub end_exact: bool,
    pub user_us: Option<u64>,
    pub kernel_us: Option<u64>,
    pub abnormal: bool,
    /// The exit itself was never reported: its pid came back as a new process.
    pub lost: bool,
}

/// One process in a session, from creation to exit.
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    /// Order of arrival in this ledger. Unique for the session, unlike the pid.
    pub id: u64,
    pub pid: u32,
    pub ppid: Option<u32>,
    /// The parent's `id`, when the parent is a process of this session.
    pub parent: Option<u64>,
    pub image: Option<String>,
    pub cmd: Option<String>,
    /// Creation time; when unknown, the moment it was first seen.
    pub start_us: u64,
    pub start_exact: bool,
    pub seen_us: u64,
    pub end: Option<End>,
}

impl Proc {
    pub fn running(&self) -> bool {
        self.end.is_none()
    }

    /// Wall-clock life so far (`now_us` is only used while it is running).
    pub fn duration_us(&self, now_us: u64) -> u64 {
        self.end.as_ref().map_or(now_us, |e| e.end_us).saturating_sub(self.start_us)
    }

    pub fn cpu_us(&self) -> Option<u64> {
        let e = self.end.as_ref()?;
        Some(e.user_us? + e.kernel_us?)
    }

    pub fn exit_code(&self) -> Option<u32> {
        self.end.as_ref().and_then(|e| e.code)
    }

    /// How long after the process was created the sensor recorded it.
    pub fn latency_us(&self) -> Option<u64> {
        self.start_exact.then(|| self.seen_us.saturating_sub(self.start_us))
    }

    pub fn to_json(&self, now_us: u64) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), self.id.into());
        m.insert("pid".into(), self.pid.into());
        put(&mut m, "ppid", self.ppid);
        put(&mut m, "parent", self.parent);
        put(&mut m, "image", self.image.clone());
        put(&mut m, "cmd", self.cmd.clone());
        m.insert("start_us".into(), self.start_us.into());
        if !self.start_exact {
            m.insert("start_exact".into(), false.into());
        }
        put(&mut m, "latency_us", self.latency_us());
        m.insert("running".into(), self.running().into());
        m.insert("duration_us".into(), self.duration_us(now_us).into());
        if let Some(e) = &self.end {
            m.insert("end_us".into(), e.end_us.into());
            if !e.end_exact {
                m.insert("end_exact".into(), false.into());
            }
            put(&mut m, "exit", self.exit_code());
            put(&mut m, "user_us", e.user_us);
            put(&mut m, "kernel_us", e.kernel_us);
            put(&mut m, "cpu_us", self.cpu_us());
            if e.abnormal {
                m.insert("abnormal".into(), true.into());
            }
            if e.lost {
                m.insert("exit_lost".into(), true.into());
            }
        }
        Value::Object(m)
    }
}

/// Every process a session has started, in order, with the running set.
#[derive(Default)]
pub struct Ledger {
    /// Ascending by `id`.
    procs: Vec<Proc>,
    /// pid -> id of the process currently holding that pid.
    live: HashMap<u32, u64>,
    next_id: u64,
    /// Ended processes kept before the oldest are forgotten; 0 keeps all.
    cap: usize,
    ended: usize,
    /// Ended processes forgotten to respect `cap`.
    pub dropped: u64,
    /// Everything ever seen, including what has been forgotten.
    pub total: u64,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    /// A ledger for a long-lived holder (a window): the running set is always
    /// complete, history is limited to the most recent `cap` ended processes.
    pub fn bounded(cap: usize) -> Self {
        Self { cap, ..Self::default() }
    }

    pub fn procs(&self) -> &[Proc] {
        &self.procs
    }

    pub fn get(&self, id: u64) -> Option<&Proc> {
        self.procs.binary_search_by_key(&id, |p| p.id).ok().map(|i| &self.procs[i])
    }

    pub fn live_count(&self) -> usize {
        self.live.len()
    }

    /// The running process holding `pid`, if any.
    pub fn by_pid(&self, pid: u32) -> Option<&Proc> {
        self.live.get(&pid).and_then(|&id| self.get(id))
    }

    /// The parent of a process created at `start_us` by `ppid`: the holder of
    /// that pid at that moment. A pid alone is not enough, since it may since
    /// have passed to an unrelated process.
    fn parent_of(&self, ppid: u32, start_us: u64, exact: bool) -> Option<u64> {
        if let Some(p) = self.by_pid(ppid) {
            if !exact || !p.start_exact || p.start_us <= start_us {
                return Some(p.id);
            }
        }
        // The parent may have ended before its child's start was recorded.
        self.procs.iter().rev().take(4096).find_map(|p| {
            let e = p.end.as_ref()?;
            (p.pid == ppid && p.start_us <= start_us && (!e.end_exact || !exact || start_us <= e.end_us)).then_some(p.id)
        })
    }

    fn close(&mut self, id: u64, end: End) {
        if let Ok(i) = self.procs.binary_search_by_key(&id, |p| p.id) {
            if self.procs[i].end.is_none() {
                self.procs[i].end = Some(end);
                self.ended += 1;
            }
        }
    }

    /// Fold one event in. Returns the `id` of the process it concerned.
    /// Tolerates what a best-effort sensor can produce: an exit with no start,
    /// a start with no exit, and the same start twice.
    pub fn apply(&mut self, ev: &ProcEvent) -> u64 {
        match ev {
            ProcEvent::Start(s) => {
                let start_us = s.start_us.unwrap_or(s.seen_us);
                if let Some(&old) = self.live.get(&s.pid) {
                    let same = self.get(old).is_some_and(|p| p.start_exact && s.start_us == Some(p.start_us));
                    if same {
                        return old;
                    }
                    // The pid has a new owner, so the old one ended unseen.
                    let lost = End { code: None, end_us: start_us, end_exact: false, user_us: None, kernel_us: None, abnormal: false, lost: true };
                    self.close(old, lost);
                }
                let id = self.next_id;
                self.next_id += 1;
                self.total += 1;
                self.procs.push(Proc {
                    id,
                    pid: s.pid,
                    ppid: s.ppid,
                    parent: s.ppid.and_then(|pp| self.parent_of(pp, start_us, s.start_us.is_some())),
                    image: s.image.clone(),
                    cmd: s.cmd.clone(),
                    start_us,
                    start_exact: s.start_us.is_some(),
                    seen_us: s.seen_us,
                    end: None,
                });
                self.live.insert(s.pid, id);
                self.prune();
                id
            }
            ProcEvent::Exit(e) => {
                let end = End {
                    code: e.code,
                    end_us: e.end_us.unwrap_or(e.seen_us),
                    end_exact: e.end_us.is_some(),
                    user_us: e.user_us,
                    kernel_us: e.kernel_us,
                    abnormal: e.abnormal,
                    lost: false,
                };
                let id = match self.live.remove(&e.pid) {
                    Some(id) => id,
                    None => {
                        // Its start was never reported: it existed, and that
                        // is all that is known.
                        let id = self.next_id;
                        self.next_id += 1;
                        self.total += 1;
                        self.procs.push(Proc {
                            id,
                            pid: e.pid,
                            ppid: None,
                            parent: None,
                            image: None,
                            cmd: None,
                            start_us: end.end_us,
                            start_exact: false,
                            seen_us: e.seen_us,
                            end: None,
                        });
                        id
                    }
                };
                self.close(id, end);
                self.prune();
                id
            }
        }
    }

    /// Forget the oldest ended processes once a quarter over the cap, so the
    /// cost is paid in rare batches rather than on every exit.
    fn prune(&mut self) {
        if self.cap == 0 || self.ended <= self.cap + self.cap / 4 {
            return;
        }
        let mut excess = self.ended - self.cap;
        self.dropped += excess as u64;
        self.ended = self.cap;
        self.procs.retain(|p| {
            if excess > 0 && !p.running() {
                excess -= 1;
                return false;
            }
            true
        });
    }

    /// The ledger as a forest, depth first, siblings in order of creation.
    /// A process whose parent is outside the session (or forgotten) is a root.
    pub fn tree(&self) -> Vec<(usize, &Proc)> {
        let mut kids: HashMap<u64, Vec<usize>> = HashMap::new();
        let mut roots = Vec::new();
        for (i, p) in self.procs.iter().enumerate() {
            match p.parent.filter(|&id| self.get(id).is_some()) {
                Some(parent) => kids.entry(parent).or_default().push(i),
                None => roots.push(i),
            }
        }
        let mut out = Vec::with_capacity(self.procs.len());
        let mut stack: Vec<(usize, usize)> = roots.into_iter().rev().map(|i| (0, i)).collect();
        while let Some((depth, i)) = stack.pop() {
            let p = &self.procs[i];
            out.push((depth, p));
            if let Some(k) = kids.get(&p.id) {
                stack.extend(k.iter().rev().map(|&c| (depth + 1, c)));
            }
        }
        out
    }

    pub fn to_json(&self, now_us: u64) -> Value {
        json!({
            "total": self.total,
            "running": self.live_count(),
            "dropped": self.dropped,
            "procs": self.procs.iter().map(|p| p.to_json(now_us)).collect::<Vec<_>>(),
        })
    }

    /// A table for people. `tree` indents each command under its parent.
    pub fn render(&self, tree: bool, now_us: u64) -> String {
        let t0 = self.procs.iter().map(|p| p.start_us).min().unwrap_or(0);
        let rows: Vec<(usize, &Proc)> = if tree { self.tree() } else { self.procs.iter().map(|p| (0, p)).collect() };
        let mut out = format!("{:>7} {:>7} {:>11} {:>8} {:>10} {:>8}  COMMAND\n", "PID", "PPID", "START", "LIFE", "EXIT", "CPU");
        for (depth, p) in rows {
            let exit = match &p.end {
                None => "running".to_string(),
                Some(e) if e.lost => "lost".into(),
                Some(End { code: Some(c), .. }) if *c >= 0x8000_0000 => format!("0x{c:08X}"),
                Some(End { code: Some(c), .. }) => c.to_string(),
                Some(_) => "?".into(),
            };
            let what = match (&p.cmd, &p.image) {
                (Some(c), _) if !c.trim().is_empty() => c.trim(),
                (_, Some(i)) => i.as_str(),
                _ => "(ended before it could be read)",
            };
            let mut what: String = what.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
            if what.chars().count() > 160 {
                what = what.chars().take(159).chain(Some('…')).collect();
            }
            out.push_str(&format!(
                "{:>7} {:>7} {:>11} {:>8} {:>10} {:>8}  {}{}\n",
                p.pid,
                p.ppid.map_or("?".into(), |v| v.to_string()),
                format!("+{:.3}s", p.start_us.saturating_sub(t0) as f64 / 1e6),
                span(p.duration_us(now_us)),
                exit,
                p.cpu_us().map_or("-".into(), span),
                "  ".repeat(depth),
                what,
            ));
        }
        if self.dropped > 0 {
            out.push_str(&format!("({} older ended processes are no longer held)\n", self.dropped));
        }
        out
    }
}

/// A length of time in the unit a person would say it in.
pub fn span(us: u64) -> String {
    match us {
        0..=9_999 => format!("{:.1}ms", us as f64 / 1e3),
        10_000..=999_999 => format!("{}ms", us / 1000),
        1_000_000..=59_999_999 => format!("{:.2}s", us as f64 / 1e6),
        60_000_000..=3_599_999_999 => format!("{}m{:02}s", us / 60_000_000, us / 1_000_000 % 60),
        _ => format!("{}h{:02}m", us / 3_600_000_000, us / 60_000_000 % 60),
    }
}

/// What a journal says about a session's processes.
pub struct FromJournal {
    pub meta: Value,
    pub ledger: Ledger,
    /// The session's own end is in the journal, so nothing in it is still running.
    pub ended: bool,
    /// The Unix time (microseconds) of the journal's time zero, estimated from
    /// its process records; lets a process be placed on the journal's own
    /// clock next to the output it produced. `None` with no process records.
    pub epoch_us: Option<u64>,
}

/// Build the ledger from a journal file. Only process records are read; the
/// (possibly very large) output records are skipped over, not loaded.
pub fn read_journal(path: &Path) -> io::Result<FromJournal> {
    use crate::journal::{K_EXIT, K_PROC, MAGIC};
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut r = BufReader::with_capacity(1 << 16, std::fs::File::open(path)?);
    let mut head = [0u8; 12];
    r.read_exact(&mut head).map_err(|_| bad("not a PROTEUS journal"))?;
    if &head[..8] != MAGIC {
        return Err(bad("not a PROTEUS journal"));
    }
    let mut meta = vec![0u8; u32::from_le_bytes(head[8..12].try_into().unwrap()) as usize];
    r.read_exact(&mut meta).map_err(|_| bad("journal header truncated"))?;
    let mut out = FromJournal { meta: serde_json::from_slice(&meta).unwrap_or(Value::Null), ledger: Ledger::new(), ended: false, epoch_us: None };
    let mut rec = [0u8; 13];
    // A record cut short by a crash ends the read; what came before stands.
    while r.read_exact(&mut rec).is_ok() {
        let t_us = u64::from_le_bytes(rec[1..9].try_into().unwrap());
        let len = u32::from_le_bytes(rec[9..13].try_into().unwrap()) as usize;
        match rec[0] {
            K_PROC => {
                let mut data = vec![0u8; len];
                if r.read_exact(&mut data).is_err() {
                    break;
                }
                if let Some(ev) = ProcEvent::from_bytes(&data) {
                    // A record is written just after the sensor saw the
                    // event, so the largest difference is the closest
                    // estimate of where the journal's clock starts.
                    out.epoch_us = out.epoch_us.max(Some(ev.seen_us().saturating_sub(t_us)));
                    out.ledger.apply(&ev);
                }
            }
            kind => {
                out.ended |= kind == K_EXIT;
                if r.seek_relative(len as i64).is_err() {
                    break;
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(pid: u32, ppid: u32, t: u64, cmd: &str) -> ProcEvent {
        ProcEvent::Start(Start {
            pid,
            ppid: Some(ppid),
            image: Some(format!("C:\\bin\\{}.exe", cmd.split(' ').next().unwrap())),
            cmd: Some(cmd.into()),
            start_us: Some(t),
            seen_us: t + 40,
        })
    }

    fn exit(pid: u32, t: u64, code: u32) -> ProcEvent {
        ProcEvent::Exit(Exit { pid, code: Some(code), end_us: Some(t), user_us: Some(1500), kernel_us: Some(500), abnormal: false, seen_us: t + 25 })
    }

    fn running(l: &Ledger) -> Vec<u32> {
        l.procs().iter().filter(|p| p.running()).map(|p| p.pid).collect()
    }

    #[test]
    fn events_survive_json_exactly() {
        let odd = "pwsh -c \"echo 'ünï → 日本'; exit 3\"\t\\\\server\\share\n";
        let full = ProcEvent::Start(Start {
            pid: 4_294_967_295,
            ppid: Some(4),
            image: Some("C:\\Program Files\\PowerShell\\7\\pwsh.exe".into()),
            cmd: Some(odd.into()),
            start_us: Some(1_794_800_000_123_456),
            seen_us: 1_794_800_000_123_499,
        });
        let bare = ProcEvent::Start(Start { pid: 8, ppid: None, image: None, cmd: None, start_us: None, seen_us: 7 });
        let empty_cmd = ProcEvent::Start(Start { pid: 8, ppid: Some(0), image: None, cmd: Some(String::new()), start_us: None, seen_us: 7 });
        let crashed = ProcEvent::Exit(Exit { pid: 9, code: Some(0xC000_0005), end_us: Some(99), user_us: Some(0), kernel_us: Some(15_625), abnormal: true, seen_us: 120 });
        let unknown = ProcEvent::Exit(Exit { pid: 9, code: None, end_us: None, user_us: None, kernel_us: None, abnormal: false, seen_us: 120 });
        for ev in [full, bare.clone(), empty_cmd.clone(), crashed, unknown] {
            assert_eq!(ProcEvent::from_bytes(&ev.to_bytes()), Some(ev));
        }
        // "Not read" and "read, and empty" stay different things.
        assert_ne!(bare.to_bytes(), empty_cmd.to_bytes());
        // Extra keys from a newer build are ignored; an unknown kind is skipped.
        let newer = br#"{"ev":"start","pid":8,"seen_us":7,"integrity":"high"}"#;
        assert_eq!(ProcEvent::from_bytes(newer), Some(bare));
        assert_eq!(ProcEvent::from_bytes(br#"{"ev":"thread","pid":8,"seen_us":7}"#), None);
        assert_eq!(ProcEvent::from_bytes(b"not json"), None);
    }

    #[test]
    fn ledger_builds_the_tree_with_lives_and_the_running_set() {
        let mut l = Ledger::new();
        let shell = l.apply(&start(100, 1, 1_000_000, "pwsh -NoLogo"));
        let cargo = l.apply(&start(200, 100, 2_000_000, "cargo build"));
        let rustc = l.apply(&start(300, 200, 2_500_000, "rustc main.rs"));
        let git = l.apply(&start(400, 100, 2_600_000, "git status"));
        assert_eq!(l.apply(&exit(300, 4_500_000, 1)), rustc);
        l.apply(&exit(200, 4_600_000, 101));

        assert_eq!(l.get(shell).unwrap().parent, None, "the shell's parent is outside the session");
        assert_eq!(l.get(rustc).unwrap().parent, Some(cargo));
        assert_eq!(l.get(git).unwrap().parent, Some(shell));
        let r = l.get(rustc).unwrap();
        assert_eq!((r.exit_code(), r.duration_us(0), r.cpu_us(), r.latency_us()), (Some(1), 2_000_000, Some(2000), Some(40)));
        // Still running: its life is measured against the clock it is given.
        assert_eq!(l.get(git).unwrap().duration_us(3_000_000), 400_000);
        assert_eq!(running(&l), vec![100, 400]);
        assert_eq!((l.total, l.live_count()), (4, 2));

        let shape: Vec<(usize, u32)> = l.tree().into_iter().map(|(d, p)| (d, p.pid)).collect();
        assert_eq!(shape, vec![(0, 100), (1, 200), (2, 300), (1, 400)]);

        let table = l.render(true, 5_000_000);
        let line = |needle: &str| table.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("no row for {needle}:\n{table}")).to_string();
        assert!(line("rustc main.rs").contains("    rustc main.rs"), "depth two is indented twice:\n{table}");
        assert!(line("cargo build").contains(" 101 "), "{table}");
        assert!(line("git status").contains("running"), "{table}");
        let j = l.to_json(5_000_000);
        assert_eq!((j["total"].as_u64(), j["running"].as_u64()), (Some(4), Some(2)));
        assert_eq!(j["procs"][2]["exit"], 1);
        assert_eq!(j["procs"][2]["parent"], cargo);
        assert!(j["procs"][3]["exit"].is_null() && j["procs"][3]["running"] == true);
    }

    #[test]
    fn a_reused_pid_is_a_different_process() {
        let mut l = Ledger::new();
        l.apply(&start(100, 1, 1_000, "cmd"));
        let first = l.apply(&start(200, 100, 2_000, "old.exe"));
        l.apply(&exit(200, 3_000, 0));
        let second = l.apply(&start(200, 100, 5_000, "new.exe"));
        assert_ne!(first, second);
        // A child created by pid 200 at t=6000 belongs to the second holder...
        let kid = l.apply(&start(300, 200, 6_000, "kid.exe"));
        assert_eq!(l.get(kid).unwrap().parent, Some(second));
        // ...and one created while the first held it, recorded late, to the first.
        let late = l.apply(&start(301, 200, 2_500, "late.exe"));
        assert_eq!(l.get(late).unwrap().parent, Some(first));
        // The same start reported twice is one process.
        assert_eq!(l.apply(&start(200, 100, 5_000, "new.exe")), second);
        assert_eq!(l.total, 5);
    }

    #[test]
    fn a_best_effort_sensor_never_corrupts_the_ledger() {
        let mut l = Ledger::new();
        // Exit with no start: it existed, and that is all that is known.
        let ghost = l.apply(&exit(50, 9_000, 3));
        let g = l.get(ghost).unwrap();
        assert!(!g.running() && !g.start_exact && g.exit_code() == Some(3) && g.cmd.is_none());
        // Start with no exit, then the pid comes back: the first is closed as lost.
        let a = l.apply(&start(60, 1, 10_000, "a.exe"));
        let b = l.apply(&start(60, 1, 20_000, "b.exe"));
        let lost = l.get(a).unwrap().end.clone().unwrap();
        assert!(lost.lost && lost.code.is_none() && !lost.end_exact);
        assert!(l.get(b).unwrap().running());
        assert_eq!(l.live_count(), 1);
        // A process gone before it could be asked anything.
        let bare = l.apply(&ProcEvent::Start(Start { pid: 70, ppid: None, image: None, cmd: None, start_us: None, seen_us: 30_000 }));
        let p = l.get(bare).unwrap();
        assert_eq!((p.start_us, p.start_exact, p.latency_us(), p.parent), (30_000, false, None, None));
        assert!(l.render(false, 40_000).contains("(ended before it could be read)"));
    }

    #[test]
    fn a_bounded_ledger_forgets_old_history_but_never_a_running_process() {
        let mut l = Ledger::bounded(100);
        l.apply(&start(1, 0, 1, "shell"));
        for i in 0..1000u32 {
            l.apply(&start(1000 + i, 1, 10 + i as u64 * 10, "job"));
            l.apply(&exit(1000 + i, 15 + i as u64 * 10, i));
        }
        assert!(l.procs().len() <= 1 + 125, "kept {}", l.procs().len());
        assert_eq!(l.total, 1001);
        assert_eq!(l.dropped as usize + l.procs().len(), 1001);
        assert_eq!(running(&l), vec![1], "the long-lived shell must survive pruning");
        assert_eq!(l.procs().last().unwrap().exit_code(), Some(999), "the newest history is what is kept");
        // Parents that were forgotten make their children roots, not orphans lost from the tree.
        assert_eq!(l.tree().len(), l.procs().len());
        assert!(Ledger::new().render(true, 0).starts_with("    PID"));
    }

    #[test]
    fn spans_read_naturally() {
        let got: Vec<String> = [0, 420, 9_999, 12_345, 999_999, 1_240_000, 59_999_999, 182_000_000, 7_380_000_000].into_iter().map(span).collect();
        assert_eq!(got, ["0.0ms", "0.4ms", "10.0ms", "12ms", "999ms", "1.24s", "60.00s", "3m02s", "2h03m"]);
    }

    #[test]
    fn journal_scan_reads_only_process_records_and_survives_a_cut_tail() {
        use crate::journal::{JournalWriter, K_EXIT, K_OUTPUT, K_PROC, K_RESIZE};
        let p = std::env::temp_dir().join(format!("proteus-procs-scan-{}.prtj", std::process::id()));
        let now = 1_794_800_000_000_000u64;
        {
            let mut j = JournalWriter::create(&p, &json!({"v": 1, "session": "s"})).unwrap();
            j.record(K_RESIZE, &[80, 0, 24, 0]).unwrap();
            j.record(K_PROC, &start(10, 1, now, "cmd.exe").to_bytes()).unwrap();
            j.record(K_OUTPUT, &vec![b'x'; 300_000]).unwrap();
            j.record(K_PROC, b"{\"ev\":\"from-the-future\",\"pid\":1,\"seen_us\":1}").unwrap();
            j.record(K_PROC, &start(11, 10, now + 500, "git status").to_bytes()).unwrap();
            j.record(K_PROC, &exit(11, now + 900, 128).to_bytes()).unwrap();
            j.flush().unwrap();
        }
        let got = read_journal(&p).unwrap();
        assert_eq!(got.meta["session"], "s");
        assert!(!got.ended);
        assert_eq!((got.ledger.total, got.ledger.live_count()), (2, 1));
        assert_eq!(got.ledger.procs()[1].exit_code(), Some(128));
        // Records were written within moments of `now`, so time zero is just before it.
        assert!(got.epoch_us.unwrap().abs_diff(now) < 5_000_000, "epoch {:?}", got.epoch_us);

        // The session ends, then the file is cut mid-record.
        let mut bytes = std::fs::read(&p).unwrap();
        let cut = bytes.len() - 20;
        bytes.extend_from_slice(&{
            let mut tail = vec![K_EXIT];
            tail.extend_from_slice(&7u64.to_le_bytes());
            tail.extend_from_slice(&4u32.to_le_bytes());
            tail.extend_from_slice(&0u32.to_le_bytes());
            tail
        });
        std::fs::write(&p, &bytes).unwrap();
        assert!(read_journal(&p).unwrap().ended);
        std::fs::write(&p, &bytes[..cut]).unwrap();
        let cut = read_journal(&p).unwrap();
        assert_eq!((cut.ledger.total, cut.ledger.live_count()), (2, 2), "the damaged exit record is dropped, the rest stands");
        std::fs::write(&p, b"PRTJ").unwrap();
        assert!(read_journal(&p).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
