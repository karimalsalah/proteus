//! `proteus procs`: every process a session has started, with parent, command
//! line, life, exit code and CPU time, as the OS reported them.
//!
//!   proteus procs [--session ID | --journal FILE] [--json] [--tree]
//!
//! With neither `--session` nor `--journal` it shows the session this command
//! is itself running in (the one whose running processes include this
//! command's parent), or the only session there is.
//!
//! It reads the session's journal, so it works the same for a session that
//! ended last week. For a live one the daemon writes its journal out within a
//! quarter of a second of the last event, so the newest lines can be that late.
use std::io::Write;
use std::path::PathBuf;

use serde_json::Value;

use crate::procs::{self, FromJournal};
use crate::Res;

const USAGE: &str = "usage: proteus procs [--session ID | --journal FILE] [--json] [--tree]";

/// The journal of daemon session `id`, or of the session this process runs in.
fn find_journal(id: Option<u64>) -> Res<PathBuf> {
    let list = crate::remote::list().map_err(|e| format!("no session daemon to ask ({e}); give --journal FILE"))?;
    let sessions: Vec<(u64, PathBuf)> = list["sessions"]
        .as_array()
        .map(|a| a.iter().filter_map(|s| Some((s["id"].as_u64()?, PathBuf::from(s["journal"].as_str()?)))).collect())
        .unwrap_or_default();
    if let Some(id) = id {
        return match sessions.into_iter().find(|s| s.0 == id) {
            Some((_, path)) => Ok(path),
            None => Err(format!("no session {id} (see `proteus sessions`)").into()),
        };
    }
    // This command was started by something inside the session it wants.
    if let Some(parent) = crate::procs_win::parent_pid() {
        for (_, path) in &sessions {
            if procs::read_journal(path).is_ok_and(|j| j.ledger.by_pid(parent).is_some()) {
                return Ok(path.clone());
            }
        }
    }
    match sessions.len() {
        1 => Ok(sessions[0].1.clone()),
        0 => Err("no sessions are running; give --journal FILE".into()),
        n => Err(format!("{n} sessions are running and this command is in none of them; give --session ID (see `proteus sessions`)").into()),
    }
}

/// The moment a running process's life is measured to: now, or, for a
/// session that is over, the last moment it was heard from.
fn clock(j: &FromJournal) -> u64 {
    if !j.ended {
        return crate::procs_win::now_us();
    }
    j.ledger.procs().iter().map(|p| p.end.as_ref().map_or(p.seen_us, |e| e.end_us)).max().unwrap_or(0)
}

fn to_json(path: &std::path::Path, j: &FromJournal, tree: bool) -> Value {
    let now = clock(j);
    let mut v = j.ledger.to_json(now);
    if tree {
        // Depth first, each process after its parent, with how deep it is.
        let rows: Vec<Value> = j
            .ledger
            .tree()
            .into_iter()
            .map(|(depth, p)| {
                let mut row = p.to_json(now);
                row["depth"] = depth.into();
                row
            })
            .collect();
        v["procs"] = rows.into();
    }
    v["journal"] = path.to_string_lossy().into_owned().into();
    v["session"] = j.meta["session"].clone();
    v["session_ended"] = j.ended.into();
    v["journal_epoch_us"] = j.epoch_us.into();
    v
}

pub fn run(args: &[String]) -> Res<()> {
    let (mut session, mut journal, mut json, mut tree) = (None, None, false, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => json = true,
            "--tree" => tree = true,
            "--session" => {
                i += 1;
                session = Some(args.get(i).and_then(|s| s.parse::<u64>().ok()).ok_or(USAGE)?);
            }
            "--journal" => {
                i += 1;
                journal = Some(PathBuf::from(args.get(i).ok_or(USAGE)?));
            }
            _ => return Err(USAGE.into()),
        }
        i += 1;
    }
    let path = match journal {
        Some(p) => p,
        None => find_journal(session)?,
    };
    let j = procs::read_journal(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let text = if json {
        serde_json::to_string_pretty(&to_json(&path, &j, tree)).unwrap_or_default() + "\n"
    } else {
        let mut t = j.ledger.render(tree, clock(&j));
        t.push_str(&format!(
            "{} processes, {} running{}\n",
            j.ledger.total,
            j.ledger.live_count(),
            if j.ended { " when the session ended" } else { "" }
        ));
        t
    };
    // Whoever is reading may stop early (a pager, `Select-Object -First`).
    let _ = std::io::stdout().lock().write_all(text.as_bytes());
    Ok(())
}
