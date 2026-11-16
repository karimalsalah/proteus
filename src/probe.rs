//! `proteus keys`: what a program inside the terminal actually receives.
//!
//!   proteus keys [--flags N] [--records] [--leave] [--status] [--seconds S] [--out FILE] [--until TEXT]
//!
//! Run it from a shell inside the terminal and press keys: each read is
//! printed as the bytes that arrived. `--flags N` asks for the kitty keyboard
//! protocol first (1 = tell chords apart, 3 = also report releases, 31 =
//! everything), exactly as an agent interface would. `--records` reads the
//! console's key records instead of bytes, which is what PowerShell's line
//! editor and other native Windows programs see. Ctrl+C twice ends it.
//! `--leave` ends without switching the protocol off again, as a program
//! that crashed would, to see what that does to the shell underneath.
//!
//! With `--out FILE` nothing is printed: input is collected until `--until`
//! text arrives (or `--seconds` pass) and written to the file as JSON. That
//! is how the tests prove, with a real console host in the middle, that what
//! the terminal sends is what a program gets.
use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crate::Res;

#[cfg(windows)]
mod sys {
    use std::ffi::c_void;
    pub const ATTACH_PARENT_PROCESS: u32 = u32::MAX;
    pub const ENABLE_PROCESSED_OUTPUT: u32 = 0x0001;
    pub const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;
    pub const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
    pub const KEY_EVENT: u16 = 1;

    /// `INPUT_RECORD` holding a `KEY_EVENT_RECORD` (the union's other members are no larger).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct InputRecord {
        pub event_type: u16,
        pub _pad: u16,
        pub down: i32,
        pub repeat: u16,
        pub vk: u16,
        pub scan: u16,
        pub ch: u16,
        pub state: u32,
    }

    extern "system" {
        pub fn AttachConsole(process_id: u32) -> i32;
        pub fn GetConsoleMode(handle: *mut c_void, mode: *mut u32) -> i32;
        pub fn SetConsoleMode(handle: *mut c_void, mode: u32) -> i32;
        pub fn SetConsoleCP(code_page: u32) -> i32;
        pub fn SetConsoleOutputCP(code_page: u32) -> i32;
        pub fn ReadConsoleInputW(handle: *mut c_void, buffer: *mut InputRecord, length: u32, read: *mut u32) -> i32;
    }
}

/// Bytes as a person can read them: printable ASCII as itself, the rest as `\xHH`.
pub fn show(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            0x1b => s.push_str("\\e"),
            b'\\' => s.push_str("\\\\"),
            0x20..=0x7e => s.push(b as char),
            _ => s.push_str(&format!("\\x{b:02x}")),
        }
    }
    s
}

struct Args {
    flags: Option<u8>,
    records: bool,
    leave: bool,
    status: bool,
    seconds: f32,
    out: Option<std::path::PathBuf>,
    until: String,
}

fn parse(args: &[String]) -> Res<Args> {
    let mut a = Args { flags: None, records: false, leave: false, status: false, seconds: 0.0, out: None, until: "~done~".into() };
    let mut i = 0;
    while i < args.len() {
        let val = args.get(i + 1).cloned().unwrap_or_default();
        match args[i].as_str() {
            "--records" => {
                a.records = true;
                i += 1;
                continue;
            }
            "--leave" => {
                a.leave = true;
                i += 1;
                continue;
            }
            "--status" => {
                a.status = true;
                i += 1;
                continue;
            }
            "--flags" => a.flags = Some(val.parse()?),
            "--seconds" => a.seconds = val.parse()?,
            "--out" => a.out = Some(val.into()),
            "--until" => a.until = val,
            other => return Err(format!("unknown option {other}").into()),
        }
        i += 2;
    }
    Ok(a)
}

#[cfg(not(windows))]
pub fn run(_args: &[String]) -> Res<()> {
    Err("proteus keys reads the Windows console".into())
}

#[cfg(windows)]
pub fn run(args: &[String]) -> Res<()> {
    use std::os::windows::io::AsRawHandle;
    let a = parse(args)?;
    // A windowed program has no console of its own; use the one of the
    // shell that started it. Fails harmlessly if there already is one.
    unsafe { sys::AttachConsole(sys::ATTACH_PARENT_PROCESS) };
    let open = |name: &str| std::fs::OpenOptions::new().read(true).write(true).open(name);
    let mut con_in = open("CONIN$").map_err(|e| format!("no console to read keys from ({e}); run this from a shell inside the terminal"))?;
    let mut con_out = open("CONOUT$")?;
    let (h_in, h_out) = (con_in.as_raw_handle() as *mut std::ffi::c_void, con_out.as_raw_handle() as *mut std::ffi::c_void);
    let (mut old_in, mut old_out) = (0u32, 0u32);
    unsafe {
        sys::GetConsoleMode(h_in, &mut old_in);
        sys::GetConsoleMode(h_out, &mut old_out);
        // No line editing, no echo, Ctrl+C as a key. Bytes mode asks the
        // console for terminal sequences; records mode takes keys as they are.
        sys::SetConsoleMode(h_in, if a.records { 0 } else { sys::ENABLE_VIRTUAL_TERMINAL_INPUT });
        sys::SetConsoleMode(h_out, old_out | sys::ENABLE_PROCESSED_OUTPUT | sys::ENABLE_VIRTUAL_TERMINAL_PROCESSING);
        sys::SetConsoleCP(65001);
        sys::SetConsoleOutputCP(65001);
    }
    let quiet = a.out.is_some();
    let mut say = |s: &str| {
        let _ = con_out.write_all(s.as_bytes());
        let _ = con_out.flush();
    };
    if !quiet {
        say("proteus keys: each line is one read, exactly as a program gets it. Ctrl+C twice to stop.\r\n");
    }
    if let Some(f) = a.flags {
        // Push the flags, then ask what is in force: the reply arrives as input.
        say(&format!("\x1b[>{f}u\x1b[?u"));
    }
    if a.status {
        // Ask whether the terminal takes program status reports, then ask
        // what kind of terminal it is: a program takes the second answer
        // arriving first to mean "no".
        say("\x1b]7501;?\x1b\\\x1b[c");
    }
    if quiet {
        // Whoever is driving this knows from here on that keys will be read raw.
        say("keys-ready\r\n");
    }

    // Reads block, so they happen on their own thread.
    let (tx, rx) = crossbeam_channel::unbounded::<(Vec<u8>, Vec<[u32; 5]>)>();
    let records = a.records;
    let h_in_addr = h_in as usize;
    std::thread::spawn(move || {
        if records {
            let mut buf = [sys::InputRecord::default(); 64];
            loop {
                let mut n = 0u32;
                if unsafe { sys::ReadConsoleInputW(h_in_addr as *mut _, buf.as_mut_ptr(), buf.len() as u32, &mut n) } == 0 {
                    break;
                }
                let keys: Vec<[u32; 5]> = buf[..n as usize]
                    .iter()
                    .filter(|r| r.event_type == sys::KEY_EVENT)
                    .map(|r| [(r.down != 0) as u32, r.vk as u32, r.scan as u32, r.ch as u32, r.state])
                    .collect();
                let typed: String = char::decode_utf16(keys.iter().filter(|k| k[0] == 1 && k[3] != 0).map(|k| k[3] as u16)).flatten().collect();
                if tx.send((typed.into_bytes(), keys)).is_err() {
                    break;
                }
            }
        } else {
            let mut buf = [0u8; 4096];
            loop {
                match con_in.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send((buf[..n].to_vec(), Vec::new())).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let started = Instant::now();
    let limit = if a.seconds > 0.0 { Some(Duration::from_secs_f32(a.seconds)) } else { None };
    let (mut bytes, mut keys): (Vec<u8>, Vec<[u32; 5]>) = (Vec::new(), Vec::new());
    let mut last_was_interrupt = false;
    loop {
        let wait = match limit {
            Some(l) => match l.checked_sub(started.elapsed()) {
                Some(w) => w,
                None => break,
            },
            None => Duration::from_secs(3600),
        };
        let Ok((chunk, recs)) = rx.recv_timeout(wait) else {
            if limit.is_some() {
                break;
            }
            continue;
        };
        if !quiet {
            if records {
                for k in &recs {
                    let ch = char::from_u32(k[3]).filter(|c| !c.is_control()).map(|c| format!(" '{c}'")).unwrap_or_default();
                    say(&format!("{} vk={:#04x} scan={:#04x} char={:#06x}{ch} state={:#06x}\r\n", if k[0] == 1 { "down" } else { "up  " }, k[1], k[2], k[3], k[4]));
                }
            } else {
                say(&format!("{}\r\n", show(&chunk)));
            }
        }
        // Ctrl+C twice in a row ends an interactive run, in whichever form it arrives.
        let interrupt = chunk == [0x03] || chunk.ends_with(b"\x1b[99;5u") || recs.iter().any(|k| k[0] == 1 && k[1] == 0x43 && k[4] & 0x000c != 0);
        let only_releases = !recs.is_empty() && recs.iter().all(|k| k[0] == 0);
        if !quiet && interrupt && last_was_interrupt {
            break;
        }
        if !only_releases && !chunk.ends_with(b":3u") {
            last_was_interrupt = interrupt;
        }
        bytes.extend_from_slice(&chunk);
        keys.extend(recs);
        if quiet && !a.until.is_empty() && bytes.windows(a.until.len()).any(|w| w == a.until.as_bytes()) {
            break;
        }
    }

    if a.flags.is_some() && !a.leave {
        say("\x1b[<u");
    }
    unsafe {
        sys::SetConsoleMode(h_in, old_in);
        sys::SetConsoleMode(h_out, old_out);
    }
    if let Some(path) = &a.out {
        let v = serde_json::json!({ "flags": a.flags, "received": show(&bytes), "keys": keys });
        // Written whole, then renamed: a reader never sees half a file.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(&v)?)?;
        std::fs::rename(&tmp, path)?;
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::keys::{self, Action, Code, Fk, Key};
    use crate::session::{self, SessionCmd, SessionSpec};
    use crate::term::{Notify, Terminal};
    use crate::theme::Theme;
    use alacritty_terminal::term::TermMode;
    use std::sync::Arc;

    /// Not a test of anything: when this test binary is started with
    /// `PROTEUS_PROBE` set, it *is* the program under the terminal. The tests
    /// below start it that way on a real pseudo-terminal.
    #[test]
    fn child() {
        let Ok(args) = std::env::var("PROTEUS_PROBE") else { return };
        let args: Vec<String> = args.split_whitespace().map(String::from).collect();
        let code = match run(&args) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("probe: {e}");
                3
            }
        };
        std::process::exit(code);
    }

    struct Probe {
        term: Terminal,
        cmds: crossbeam_channel::Sender<SessionCmd>,
        out: std::path::PathBuf,
    }

    /// Start this binary as a console program on a real pseudo-terminal,
    /// with a real terminal (parser, replies to queries) on the other side.
    fn start(name: &str, probe_args: &str) -> Probe {
        start_under(name, probe_args, "/c")
    }

    /// `shell` is `/c` (the shell ends with the program) or `/k` (it stays
    /// and shows its prompt again).
    fn start_under(name: &str, probe_args: &str, shell: &str) -> Probe {
        let dir = std::env::temp_dir().join(format!("proteus-probe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        if std::env::var_os("PROTEUS_DATA").is_none() {
            std::env::set_var("PROTEUS_DATA", &dir);
        }
        let out = dir.join(format!("{name}.json"));
        let _ = std::fs::remove_file(&out);
        let me = std::env::current_exe().unwrap();
        let mut env = session::clean_env();
        env.push(("PROTEUS_PROBE".into(), format!("{probe_args} --seconds 20 --out {}", out.display())));
        // The shell's stock prompt, whatever this machine has set: one test waits for it.
        env.retain(|(k, _)| !k.eq_ignore_ascii_case("PROMPT"));
        env.push(("PROMPT".into(), "$P$G".into()));
        // `start /wait` because this binary may be a windowed program, which
        // a shell does not otherwise wait for; it attaches to cmd's console.
        let args = ["/d", shell, "start", "/wait", "/b", &me.to_string_lossy(), "probe::tests::child", "--exact", "--nocapture", "--test-threads=1"];
        let h = session::spawn_local(SessionSpec {
            exe: "cmd.exe".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: Some(dir),
            cols: 100,
            rows: 30,
            journal: false,
            env: Some(env),
        })
        .expect("pseudo-terminal");
        let notify: Notify = Arc::new(|_| {});
        let term = Terminal::new(100, 30, Theme::void_light(), notify, Some(h.cmds.clone()));
        term.attach(h.events);
        Probe { term, cmds: h.cmds, out }
    }

    fn wait_for(what: &str, secs: u64, mut ready: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(secs);
        while !ready() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    impl Probe {
        fn mode(&self) -> TermMode {
            *self.term.term.lock().unwrap().mode()
        }
        fn send(&self, bytes: Vec<u8>) {
            self.cmds.send(SessionCmd::Input(bytes)).unwrap();
        }
        fn screen(&self) -> String {
            let t = self.term.term.lock().unwrap();
            t.renderable_content().display_iter.map(|cell| cell.c).collect()
        }
        /// The screen's rows, without trailing blanks.
        fn rows(&self) -> Vec<String> {
            let all: Vec<char> = self.screen().chars().collect();
            all.chunks(100).map(|r| r.iter().collect::<String>().trim_end().to_string()).collect()
        }
        /// Type the end marker and read back what the program received.
        fn finish(self) -> serde_json::Value {
            self.send(b"~done~".to_vec());
            wait_for("the program's report", 25, || self.out.is_file());
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&self.out).unwrap()).unwrap();
            let _ = self.cmds.send(SessionCmd::Kill);
            v
        }
    }

    fn fk(f: Fk, mods: u8, action: Action) -> Key<'static> {
        Key { code: Code::Fn(f), shifted: None, base: None, text: "", mods, action }
    }
    fn ch(c: char, mods: u8, action: Action) -> Key<'static> {
        Key { code: Code::Char(c), shifted: None, base: None, text: "", mods, action }
    }

    /// An agent interface asks whether the terminal takes status reports
    /// (OSC 7501) and then what kind of terminal it is. The first answer must
    /// reach it through the console host's input side, whole, and before the
    /// second: otherwise it never reports what it is doing.
    #[test]
    fn a_status_query_is_answered_through_the_console_host_before_device_attributes() {
        let p = start("status", "--status");
        wait_for("the program to be reading", 20, || p.screen().contains("keys-ready"));
        std::thread::sleep(Duration::from_millis(600));
        let got = p.finish().to_string();
        let status = got.find("7501;?:states=idle,working,done,blocked,error:kinds=permission,question,auth");
        let attrs = got.find("[?6c");
        assert!(status.is_some(), "no status answer reached the program: {got}");
        assert!(attrs.is_some(), "no device attributes answer reached the program: {got}");
        assert!(status < attrs, "the status answer must come first: {got}");
    }

    /// The whole path an agent interface relies on, with the bundled console
    /// host in the middle: the program asks for the protocol, the terminal
    /// hears it, the query is answered, and Shift+Enter arrives as itself.
    #[test]
    fn kitty_keys_reach_a_real_program_through_the_console_host() {
        let p = start("kitty", "--flags 3");
        wait_for("the program's request to reach the terminal", 20, || p.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES));
        wait_for("the program to start reading", 20, || p.screen().contains("keys-ready"));
        let mode = p.mode();
        assert!(mode.contains(TermMode::REPORT_EVENT_TYPES));
        let flags = crate::input::kitty_flags(mode);
        assert_eq!(flags, 3);
        let sent: Vec<(&str, Key)> = vec![
            ("shift+enter", fk(Fk::Enter, keys::SHIFT, Action::Press)),
            ("ctrl+enter", fk(Fk::Enter, keys::CTRL, Action::Press)),
            ("alt+enter", fk(Fk::Enter, keys::ALT, Action::Press)),
            ("escape", fk(Fk::Escape, 0, Action::Press)),
            ("ctrl+shift+p", ch('p', keys::CTRL | keys::SHIFT, Action::Press)),
            ("ctrl+shift+p released", ch('p', keys::CTRL | keys::SHIFT, Action::Release)),
            ("ctrl+i", ch('i', keys::CTRL, Action::Press)),
            ("alt+b", ch('b', keys::ALT, Action::Press)),
            ("ctrl+c", ch('c', keys::CTRL, Action::Press)),
            ("shift+tab", fk(Fk::Tab, keys::SHIFT, Action::Press)),
            ("ctrl+up", fk(Fk::Up, keys::CTRL, Action::Press)),
            ("f5 released", fk(Fk::F(5), 0, Action::Release)),
        ];
        let mut want = String::new();
        for (_, k) in &sent {
            let bytes = keys::encode(k, flags, false);
            assert!(!bytes.is_empty());
            want.push_str(&show(&bytes));
            p.send(bytes);
        }
        let v = p.finish();
        let got = v["received"].as_str().unwrap();
        println!("program received: {got}");
        assert!(got.contains("\\e[?3u"), "the flags query was not answered to the program: {got}");
        assert!(got.contains(&want), "keys did not arrive as sent.\n sent: {want}\n  got: {got}");
        assert!(got.contains("\\e[13;2u"), "shift+enter");
    }

    /// A program that switches the protocol on and ends without switching it
    /// off: a crash, a dropped connection. Nothing tells the terminal, so the
    /// flags stay on for the shell underneath. That shell must still be
    /// usable (typing and Enter are plain bytes by the protocol's design),
    /// and dropping the flags, which is what Ctrl+Shift+Delete does, must
    /// put the keyboard back.
    #[test]
    fn a_program_that_ends_with_the_protocol_on_leaves_a_usable_shell() {
        let p = start_under("left-on", "--flags 1 --leave", "/k");
        wait_for("the program to start reading", 20, || p.screen().contains("keys-ready"));
        assert_eq!(crate::input::kitty_flags(p.mode()), 1);
        p.send(b"~done~".to_vec());
        wait_for("the program's report", 25, || p.out.is_file());
        wait_for("the shell's prompt to come back", 20, || p.rows().iter().rev().find(|r| !r.is_empty()).is_some_and(|r| r.ends_with('>')));
        let flags = crate::input::kitty_flags(p.mode());
        println!("flags in force after the program ended without switching the protocol off: {flags}");
        assert_eq!(flags, 1, "nothing switched the protocol off: neither the program nor the console host");

        // Type a command and press Enter, encoded as the stale flags dictate.
        for c in "set /a 1000+234".chars() {
            p.send(keys::encode(&ch(c, 0, Action::Press), flags, false));
        }
        p.send(keys::encode(&fk(Fk::Enter, 0, Action::Press), flags, false));
        wait_for("the shell to run what was typed", 20, || p.rows().iter().any(|r| r.starts_with("1234")));
        // What does not work until the flags are dropped: Ctrl+C is a code the shell cannot read.
        assert_eq!(keys::encode(&ch('c', keys::CTRL, Action::Press), flags, false), b"\x1b[99;5u");
        p.term.feed(crate::input::DROP_KEYBOARD_FLAGS);
        let flags = crate::input::kitty_flags(p.mode());
        assert_eq!(flags, 0);
        assert_eq!(keys::encode(&ch('c', keys::CTRL, Action::Press), flags, false), b"\x03");
        p.send(b"exit\r".to_vec());
        let _ = p.cmds.send(SessionCmd::Kill);
    }

    /// Native Windows programs (PowerShell's line editor, anything built on
    /// the console API) do not read bytes: the console host turns what the
    /// terminal sends into key records. This is what they see.
    #[test]
    fn what_a_program_reading_key_records_sees() {
        let p = start("records", "--records");
        wait_for("the program to start reading", 20, || p.screen().contains("keys-ready"));
        p.send(keys::encode(&ch('a', 0, Action::Press), 0, false));
        p.send(keys::encode(&fk(Fk::Enter, keys::SHIFT, Action::Press), 0, false));
        p.send(keys::encode(&ch('c', keys::CTRL, Action::Press), 0, false));
        p.send(keys::encode(&fk(Fk::Up, keys::CTRL, Action::Press), 0, false));
        let v = p.finish();
        let downs: Vec<(u64, u64, u64)> = v["keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|k| k[0] == 1)
            .map(|k| (k[1].as_u64().unwrap(), k[3].as_u64().unwrap(), k[4].as_u64().unwrap() & 0x1f))
            .collect();
        println!("key records (virtual key, character, modifier state): {downs:x?}");
        const ALT: u64 = 0x02;
        const CTRL: u64 = 0x08;
        assert!(downs.contains(&(0x41, 0x61, 0)), "a");
        // Legacy Shift+Enter is ESC CR, which the console host reads as Alt+Enter.
        assert!(downs.contains(&(0x0d, 0x0d, ALT)), "shift+enter arrives as alt+enter");
        assert!(downs.contains(&(0x43, 0x03, CTRL)), "ctrl+c");
        assert!(downs.contains(&(0x26, 0, CTRL)), "ctrl+up");
    }

    /// Not used by PROTEUS yet; measured here so the choice is made on facts.
    /// The console host also accepts whole key records from the terminal
    /// (win32-input-mode, `CSI Vk;Sc;Uc;Kd;Cs;Rc _`). A program reading
    /// records then sees Shift+Enter exactly. But a program reading bytes
    /// that has not asked for the kitty protocol is handed a plain CR for the
    /// same key: it can no longer tell Shift+Enter from Enter, which the
    /// legacy ESC CR lets it do. That trade is why this mode is not switched on.
    #[test]
    fn win32_input_mode_through_the_console_host() {
        const SHIFT_ENTER_DOWN: &[u8] = b"\x1b[13;28;13;1;16;1_";
        const SHIFT_ENTER_UP: &[u8] = b"\x1b[13;28;13;0;16;1_";
        let p = start("win32-records", "--records");
        wait_for("the program to start reading", 20, || p.screen().contains("keys-ready"));
        p.send(SHIFT_ENTER_DOWN.to_vec());
        p.send(SHIFT_ENTER_UP.to_vec());
        let v = p.finish();
        let enter: Vec<(u64, u64)> = v["keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|k| k[1] == 0x0d)
            .map(|k| (k[0].as_u64().unwrap(), k[4].as_u64().unwrap() & 0x1f))
            .collect();
        println!("key records for Enter (down, modifier state): {enter:x?}");
        assert_eq!(enter, [(1, 0x10), (0, 0x10)], "a record reader sees Shift+Enter go down and come up");

        let p = start("win32-bytes", "");
        wait_for("the program to start reading", 20, || p.screen().contains("keys-ready"));
        p.send(SHIFT_ENTER_DOWN.to_vec());
        p.send(SHIFT_ENTER_UP.to_vec());
        let v = p.finish();
        let got = v["received"].as_str().unwrap();
        println!("a byte reader received: {got}");
        assert_eq!(got, "\\x0d~done~", "a byte reader is handed a plain CR");
    }

    /// A program that asked for nothing gets the legacy bytes, and those too
    /// survive the console host: Shift+Enter is ESC CR, which is not Enter.
    #[test]
    fn legacy_shift_enter_reaches_a_real_program() {
        let p = start("legacy", "");
        wait_for("the program to start reading", 20, || p.screen().contains("keys-ready"));
        assert_eq!(crate::input::kitty_flags(p.mode()), 0);
        p.send(keys::encode(&ch('a', 0, Action::Press), 0, false));
        p.send(keys::encode(&fk(Fk::Enter, keys::SHIFT, Action::Press), 0, false));
        p.send(keys::encode(&ch('b', 0, Action::Press), 0, false));
        p.send(keys::encode(&fk(Fk::Enter, 0, Action::Press), 0, false));
        let v = p.finish();
        let got = v["received"].as_str().unwrap();
        println!("program received: {got}");
        assert!(got.contains("a\\e\\x0db\\x0d"), "legacy keys did not arrive as sent: {got}");
    }
}
