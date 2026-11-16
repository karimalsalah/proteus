//! PROTEUS: a GPU-native terminal engine for people and agents.
//!
//!   proteus [--cwd DIR] [--font PT] [--no-journal] [--no-reveal] [-e EXE ARGS...]
//!   proteus shot ...        render to a PNG without a window (see shot.rs)
//!   proteus ears ...        render every sound to a WAV file (see ears.rs)
//!   proteus keys ...        show what a program inside the terminal receives per key (see probe.rs)
//!   proteus recall ...      a session's past: find text, read a moment, record (see recall_cli.rs)
//!   proteus sessions        list what the session daemon is running
//!   proteus send ID [--raw] TEXT  type into a running session ("\r" presses Enter; --raw types TEXT exactly)
//!   proteus mcp             serve agents: sessions to spawn, drive, wait on, read and see (mcp.rs)
//!   proteus --attach ID     open a window on a session an agent is driving
//!   proteus procs ...       every process a session has started (see procs_cli.rs)
//!   proteus update [--check] signed self-update (see update.rs)
//!   proteus licenses        third-party notices for what is bundled inside
//!   proteus chronicle ...   history of a working tree: snapshot, diff, restore, undo (see chronicle.rs)
//!   proteus run -- CMD ...  run a command as one undoable step
//!
//! The executable is self-contained: fonts, icon and the Windows console host
//! are inside it and unpacked to %LOCALAPPDATA%\proteus on first use.
//! Settings: %LOCALAPPDATA%\proteus\config.json (created on first run).
//!   proteus daemon          run the session daemon (started automatically)
//!
//! Ctrl+Shift+D detaches: the window closes, the session keeps running, and
//! the next window picks it up. Closing the window ends the session.
//! Ctrl+Shift+Delete returns the keyboard to plain keys if a program died
//! with the kitty keyboard protocol switched on.
//! Ctrl+Shift+Left and Right step through the session's past (Home and End:
//! its start, and now); typing or Esc returns to the present.
//! Ctrl+Shift+W shows the World view: the session's processes and output as
//! one scene (world.rs); Esc returns. `proteus shot --world` renders it.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod atlas;
mod builtin;
mod chronicle;
mod config;
#[cfg(test)]
mod conform;
#[cfg(windows)]
mod conhost;
mod daemon;
mod deco;
mod ears;
mod endpoint;
mod flash;
mod fonts;
mod gpu;
mod icon;
mod input;
mod journal;
mod keys;
mod marks;
mod lean;
mod links;
mod mcp;
mod motion;
mod paths;
mod probe;
mod procs;
mod procs_cli;
#[cfg(test)]
mod procs_measure;
mod procs_win;
mod proto;
mod recall;
mod recall_cli;
mod remote;
mod renderer;
mod runtime;
mod scrub;
mod session;
mod shot;
mod sound;
mod synth;
mod term;
mod theme;
mod update;
mod watch;
mod world;
mod world_shot;

pub type Res<T> = Result<T, Box<dyn std::error::Error>>;

#[cfg(windows)]
extern "system" {
    fn AttachConsole(process_id: u32) -> i32;
}

/// `\r`, `\n`, `\t`, `\e` (Escape) and `\xHH` in text given on the command line.
fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() {
            let (byte, used) = match b[i + 1] {
                b'r' => (Some(b'\r'), 2),
                b'n' => (Some(b'\n'), 2),
                b't' => (Some(b'\t'), 2),
                b'e' => (Some(0x1b), 2),
                b'\\' => (Some(b'\\'), 2),
                b'x' => (s.get(i + 2..i + 4).and_then(|h| u8::from_str_radix(h, 16).ok()), 4),
                _ => (None, 0),
            };
            if let Some(v) = byte {
                out.push(v);
                i += used;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn main() {
    // A windowed program has no console; borrow the parent's so command-line
    // modes can print when started from a shell. Not for `mcp`: it speaks
    // only through its pipes, and a borrowed console would hand it the
    // client's Ctrl+C.
    #[cfg(windows)]
    // Nor for the console-host bench's child: it must write into the
    // pseudo-console it was given, or fail, never into a borrowed one.
    if std::env::args().nth(1).as_deref() != Some("mcp") && std::env::args().nth(2).as_deref() != Some("emit") {
        unsafe {
            AttachConsole(u32::MAX);
        }
    }
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        paths::log(&format!("PANIC: {info}\n{bt}"));
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("shot") => shot::run(&args[1..]),
        Some("ears") => ears::run(&args[1..]),
        Some("keys") => probe::run(&args[1..]),
        #[cfg(windows)]
        Some("conhost") => conhost::run(&args[1..]),
        Some("recall") => recall_cli::run(&args[1..]),
        Some("procs") => procs_cli::run(&args[1..]),
        Some("update") => update::cli(&args[1..]),
        Some("daemon") => daemon::run(true),
        // Both end in an error or an exit code; neither can open a window.
        Some("chronicle") => chronicle::cli(&args[1..]),
        Some("run") => chronicle::run_cli(&args[1..]),
        Some("mcp") => mcp::run(&args[1..]),
        // proteus send <session id> <text>   ("\r" in the text presses Enter)
        Some("send") => {
            // --raw types the text exactly: Windows paths are full of backslashes (issue #20).
            let raw = args.get(2).is_some_and(|a| a == "--raw");
            let id = args.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
            let text = args.get(if raw { 3 } else { 2 }..).map(|a| a.join(" ")).unwrap_or_default();
            let bytes = if raw { text.into_bytes() } else { unescape(&text) };
            remote::send(id, &bytes).map_err(|e| e.into())
        }
        Some("sessions") => match remote::list() {
            Ok(v) => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
                Ok(())
            }
            Err(e) => {
                println!("{{\"sessions\": [], \"note\": \"{e}\"}}");
                Ok(())
            }
        },
        Some("--version") | Some("-V") => {
            println!("proteus {}", runtime::build_id());
            Ok(())
        }
        Some("licenses") => {
            println!("{}", runtime::LICENSES);
            Ok(())
        }
        // proteus icon OUT.png [SIZE]   write the application icon as an image
        Some("icon") => {
            let size = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(256u32).clamp(16, 1024);
            match args.get(1) {
                Some(out) => shot::write_png(std::path::Path::new(out), size, size, &icon::rgba(size)),
                None => Err("usage: proteus icon OUT.png [SIZE]".into()),
            }
        }
        _ => app::run(&args),
    };
    if let Err(e) = result {
        paths::log(&format!("fatal: {e}"));
        eprintln!("proteus: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::unescape;

    #[test]
    fn unescape_command_line_text() {
        assert_eq!(unescape(r"ls\r"), b"ls\r");
        assert_eq!(unescape(r"\x03\e[A\t"), b"\x03\x1b[A\t");
        assert_eq!(unescape(r"C:\Users\\x"), b"C:\\Users\\x");
        assert_eq!(unescape(r"tail\"), b"tail\\");
        assert_eq!(unescape(r"\xZZ"), b"\\xZZ");
    }
}
