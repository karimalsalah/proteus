//! Wire format between a window and the session daemon: `u8 type | u32 len | payload`.
use std::io::{self, Read, Write};
use std::path::PathBuf;

pub const PROTO: u64 = 1;

// Window -> daemon.
pub const C_SPAWN: u8 = 1; // json {exe, args, cwd, cols, rows}
pub const C_ATTACH: u8 = 2; // u64 session id; 0 = most recent session nobody is attached to
pub const C_INPUT: u8 = 3;
pub const C_RESIZE: u8 = 4; // u16 cols, u16 rows
pub const C_KILL: u8 = 5;
pub const C_LIST: u8 = 6;
pub const C_QUIT: u8 = 7; // leave now if no sessions are running (a newer build wants to take over)

// Daemon -> window.
pub const D_OUTPUT: u8 = 10;
pub const D_EXIT: u8 = 11; // u32 exit code
pub const D_INFO: u8 = 12; // json, first reply to a successful spawn/attach
pub const D_REPLAY_RESIZE: u8 = 13; // size the terminal had at this point of the replay
pub const D_REPLAY_DONE: u8 = 14;
pub const D_LIST: u8 = 15;
pub const D_ERROR: u8 = 16;
/// A process started or ended in the session: one JSON object (`procs.rs`).
/// Sent live, and again in journal order when a window attaches. A window
/// that does not know this frame skips it, like any unknown frame. Numbered
/// apart from the frames above (ASCII 'P') to avoid colliding with others.
pub const D_PROC: u8 = 0x50;

pub type Frame = (u8, Vec<u8>);

pub fn write_frame<W: Write>(w: &mut W, ty: u8, payload: &[u8]) -> io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = ty;
    head[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Frame> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
    if len > 64 << 20 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut p = vec![0u8; len];
    r.read_exact(&mut p)?;
    Ok((head[0], p))
}

/// The daemon's socket lives in the per-user data directory, so only this
/// user can reach it.
pub fn socket_path() -> PathBuf {
    match std::env::var_os("PROTEUS_SOCKET") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        // The number is the daemon generation. Builds that cannot hand over to
        // each other get different sockets and so never share a daemon.
        // 3: the daemon senses the processes its sessions start (D_PROC).
        _ => crate::paths::data_dir().join("proteusd-3.sock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, C_INPUT, b"ls\r").unwrap();
        write_frame(&mut buf, C_KILL, b"").unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), (C_INPUT, b"ls\r".to_vec()));
        assert_eq!(read_frame(&mut r).unwrap(), (C_KILL, vec![]));
        assert!(read_frame(&mut r).is_err());
    }
}
