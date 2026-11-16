//! The console-host stage alone: the workload through a pseudo-console whose
//! output is read and thrown away, so the figure is what ConPTY itself
//! sustains, with no terminal behind it.
//!
//!   versus sink --conpty DIR|system --out FILE.json [--buf BYTES]
//!               [--cols 123 --rows 30] -- EMIT ARGS...
//!
//! DIR holds conpty.dll and OpenConsole.exe (as each terminal bundles them);
//! `system` uses the inbox CreatePseudoConsole in kernel32. The reader uses
//! one synchronous ReadFile of --buf bytes at a time (65536 unless given).
use std::time::Instant;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::Storage::FileSystem::ReadFile;
use windows_sys::Win32::System::Console::{COORD, HPCON};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CreateProcessW, DeleteProcThreadAttributeList, InitializeProcThreadAttributeList, UpdateProcThreadAttribute, WaitForSingleObject,
    EXTENDED_STARTUPINFO_PRESENT, INFINITE, LPPROC_THREAD_ATTRIBUTE_LIST, PROCESS_INFORMATION, STARTUPINFOEXW,
};

use crate::{js, opt, opt_num};

const PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE: usize = 0x0002_0016;

type CreatePc = unsafe extern "system" fn(COORD, HANDLE, HANDLE, u32, *mut HPCON) -> i32;
type ClosePc = unsafe extern "system" fn(HPCON);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

pub fn main(args: &[String]) -> Result<(), String> {
    let sep = args.iter().position(|a| a == "--").ok_or("sink: -- EMIT ARGS is required")?;
    let (opts, emit) = (&args[..sep], &args[sep + 1..]);
    let which = opt(opts, "--conpty").ok_or("--conpty DIR|system is required")?.to_string();
    let out = opt(opts, "--out").ok_or("--out FILE is required")?.to_string();
    let buf_len: usize = opt_num(opts, "--buf", 65536);
    let cols: i16 = opt_num(opts, "--cols", 123);
    let rows: i16 = opt_num(opts, "--rows", 30);

    unsafe {
        let lib = if which == "system" { wide("kernel32.dll") } else { wide(&format!("{which}\\conpty.dll")) };
        let m = LoadLibraryW(lib.as_ptr());
        if m.is_null() {
            return Err(format!("LoadLibrary {which}: {}", std::io::Error::last_os_error()));
        }
        let create: CreatePc = std::mem::transmute(GetProcAddress(m, c"CreatePseudoConsole".as_ptr() as _).ok_or("no CreatePseudoConsole")?);
        let close: ClosePc = std::mem::transmute(GetProcAddress(m, c"ClosePseudoConsole".as_ptr() as _).ok_or("no ClosePseudoConsole")?);

        let (mut in_r, mut in_w, mut out_r, mut out_w): (HANDLE, HANDLE, HANDLE, HANDLE) =
            (std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut());
        if CreatePipe(&mut in_r, &mut in_w, std::ptr::null(), 0) == 0 || CreatePipe(&mut out_r, &mut out_w, std::ptr::null(), 0) == 0 {
            return Err("CreatePipe failed".into());
        }
        let mut pc: HPCON = 0;
        let hr = create(COORD { X: cols, Y: rows }, in_r, out_w, 0, &mut pc);
        if hr != 0 {
            return Err(format!("CreatePseudoConsole: 0x{hr:08x}"));
        }
        CloseHandle(in_r);
        CloseHandle(out_w);

        // Reader: drain and discard, counting.
        let reader = out_r as usize;
        let t = std::thread::spawn(move || {
            let mut buf = vec![0u8; buf_len];
            let (mut total, mut reads) = (0u64, 0u64);
            let mut first: Option<Instant> = None;
            loop {
                let mut n = 0u32;
                if ReadFile(reader as HANDLE, buf.as_mut_ptr(), buf.len() as u32, &mut n, std::ptr::null_mut()) == 0 || n == 0 {
                    break;
                }
                first.get_or_insert_with(Instant::now);
                total += n as u64;
                reads += 1;
            }
            (total, reads)
        });

        let mut size = 0usize;
        InitializeProcThreadAttributeList(std::ptr::null_mut(), 1, 0, &mut size);
        let mut attr = vec![0u8; size];
        let list = attr.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        InitializeProcThreadAttributeList(list, 1, 0, &mut size);
        UpdateProcThreadAttribute(list, 0, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, pc as _, std::mem::size_of::<HPCON>(), std::ptr::null_mut(), std::ptr::null());
        let mut si: STARTUPINFOEXW = std::mem::zeroed();
        si.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
        si.lpAttributeList = list;
        // Without this the child inherits this process's own std handles
        // and never writes to the pseudo-console at all.
        si.StartupInfo.dwFlags = windows_sys::Win32::System::Threading::STARTF_USESTDHANDLES;
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let line = format!("\"{}\" emit {}", exe.display(), emit.iter().map(|a| if a.contains(' ') { format!("\"{a}\"") } else { a.clone() }).collect::<Vec<_>>().join(" "));
        let mut w = wide(&line);
        let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
        if CreateProcessW(std::ptr::null(), w.as_mut_ptr(), std::ptr::null(), std::ptr::null(), 0, EXTENDED_STARTUPINFO_PRESENT, std::ptr::null(), std::ptr::null(), &si.StartupInfo, &mut pi) == 0 {
            return Err(format!("CreateProcess: {}", std::io::Error::last_os_error()));
        }
        WaitForSingleObject(pi.hProcess, INFINITE);
        CloseHandle(pi.hThread);
        CloseHandle(pi.hProcess);
        // Closing the pseudo-console ends the output pipe; the reader returns.
        close(pc);
        CloseHandle(in_w);
        let (total, reads) = t.join().unwrap();
        CloseHandle(out_r);
        DeleteProcThreadAttributeList(list);
        let json = format!(
            "{{\"conpty\":{},\"buf\":{buf_len},\"bytes_read\":{total},\"reads\":{reads},\"avg_read\":{:.0}}}\n",
            js(&which),
            total as f64 / reads.max(1) as f64
        );
        std::fs::write(&out, json).map_err(|e| e.to_string())?;
    }
    Ok(())
}
