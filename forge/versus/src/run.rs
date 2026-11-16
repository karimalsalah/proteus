//! Start a terminal inside a job object and account for everything it does.
//! Only the processes in this job are ever stopped (TerminateJobObject), so
//! nothing the person or another agent runs can be touched.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject, TerminateJobObject,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, OpenProcess, QueryFullProcessImageNameW, ResumeThread, CREATE_SUSPENDED, DETACHED_PROCESS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW,
};

use crate::{js, opt, opt_num, qpc};

const SW_SHOWNOACTIVATE: u16 = 4;

struct Proc {
    name: String,
    h: HANDLE,
    peak_ws: u64,
}

/// Machine-wide load, so a run taken on a busy machine is visible as such.
pub struct Load {
    idle: u64,
    busy: u64,
    pub min_avail_mb: f64,
}

fn ft(f: windows_sys::Win32::Foundation::FILETIME) -> u64 {
    (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64
}

/// Whether the session's input desktop can be opened (it cannot while the
/// session is locked or at the sign-in screen).
pub fn unlocked() -> bool {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, OpenInputDesktop};
    let d = unsafe { OpenInputDesktop(0, 0, 0x0001) };
    if d.is_null() {
        return false;
    }
    unsafe { CloseDesktop(d) };
    true
}

impl Load {
    fn times() -> (u64, u64) {
        let z = windows_sys::Win32::Foundation::FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut i, mut k, mut u) = (z, z, z);
        unsafe { windows_sys::Win32::System::Threading::GetSystemTimes(&mut i, &mut k, &mut u) };
        // Kernel time includes idle time.
        (ft(i), ft(k) + ft(u) - ft(i))
    }
    pub fn avail_mb() -> f64 {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        unsafe { GlobalMemoryStatusEx(&mut m) };
        m.ullAvailPhys as f64 / 1048576.0
    }
    pub fn start() -> Load {
        let (idle, busy) = Self::times();
        Load { idle, busy, min_avail_mb: Self::avail_mb() }
    }
    pub fn sample(&mut self) {
        self.min_avail_mb = self.min_avail_mb.min(Self::avail_mb());
    }
    /// All-core busy percentage since start, and the least free RAM seen.
    pub fn json(&self) -> String {
        let (i, b) = Self::times();
        let (di, db) = ((i - self.idle) as f64, (b - self.busy) as f64);
        format!(",\"machine_cpu_busy_pct\":{:.1},\"min_free_ram_mb\":{:.0}", db / (di + db).max(1.0) * 100.0, self.min_avail_mb)
    }
}

pub struct Job {
    h: HANDLE,
    procs: BTreeMap<u32, Proc>,
}

impl Job {
    pub fn new() -> Result<Job, String> {
        unsafe {
            // Named, so the workload can tell whether it was born inside it
            // (some terminals start their child outside the caller's job).
            let name: Vec<u16> = format!("versus-job-{}", std::process::id()).encode_utf16().chain(Some(0)).collect();
            std::env::set_var("VERSUS_JOB", format!("versus-job-{}", std::process::id()));
            let h = CreateJobObjectW(std::ptr::null(), name.as_ptr());
            if h.is_null() {
                return Err("CreateJobObject failed".into());
            }
            let mut l: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            // If this runner dies, nothing it started outlives it.
            l.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(h, JobObjectExtendedLimitInformation, &l as *const _ as _, std::mem::size_of_val(&l) as u32);
            Ok(Job { h, procs: BTreeMap::new() })
        }
    }

    /// Create `cmdline` suspended, put it in the job, let it go. Returns the
    /// QPC value taken just before creation (substituted for `{T0}`).
    pub fn spawn(&self, cmdline: &str) -> Result<(i64, u32), String> {
        let t0 = qpc();
        let line = cmdline.replace("{T0}", &t0.to_string());
        let mut w: Vec<u16> = line.encode_utf16().chain(Some(0)).collect();
        unsafe {
            let mut si: STARTUPINFOW = std::mem::zeroed();
            si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
            si.dwFlags = STARTF_USESHOWWINDOW;
            si.wShowWindow = SW_SHOWNOACTIVATE;
            let mut pi: PROCESS_INFORMATION = std::mem::zeroed();
            let ok = CreateProcessW(
                std::ptr::null(),
                w.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                0,
                CREATE_SUSPENDED | DETACHED_PROCESS,
                std::ptr::null(),
                std::ptr::null(),
                &si,
                &mut pi,
            );
            if ok == 0 {
                return Err(format!("CreateProcess `{line}`: {}", std::io::Error::last_os_error()));
            }
            if AssignProcessToJobObject(self.h, pi.hProcess) == 0 {
                let e = std::io::Error::last_os_error();
                windows_sys::Win32::System::Threading::TerminateProcess(pi.hProcess, 1);
                return Err(format!("AssignProcessToJobObject: {e}"));
            }
            ResumeThread(pi.hThread);
            CloseHandle(pi.hThread);
            CloseHandle(pi.hProcess);
            Ok((t0, pi.dwProcessId))
        }
    }

    pub fn pids(&self) -> Vec<u32> {
        // JOBOBJECT_BASIC_PROCESS_ID_LIST: two u32 counts then ULONG_PTR ids.
        let mut buf = vec![0usize; 1026];
        let ok = unsafe {
            QueryInformationJobObject(self.h, JobObjectBasicProcessIdList, buf.as_mut_ptr() as _, (buf.len() * 8) as u32, std::ptr::null_mut())
        };
        if ok == 0 {
            return Vec::new();
        }
        let listed = (buf[0] >> 32) as usize;
        buf[1..1 + listed.min(1024)].iter().map(|&p| p as u32).collect()
    }

    pub fn accounting(&self) -> (f64, f64, u32, u32) {
        let mut a: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        unsafe {
            QueryInformationJobObject(self.h, JobObjectBasicAccountingInformation, &mut a as *mut _ as _, std::mem::size_of_val(&a) as u32, std::ptr::null_mut())
        };
        (a.TotalUserTime as f64 / 1e7, a.TotalKernelTime as f64 / 1e7, a.TotalProcesses, a.ActiveProcesses)
    }

    /// Sum of working sets now; records names and per-process peaks.
    pub fn sample(&mut self) -> u64 {
        let mut sum = 0u64;
        for pid in self.pids() {
            let p = self.procs.entry(pid).or_insert_with(|| unsafe {
                let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
                let mut name = [0u16; 512];
                let mut n = name.len() as u32;
                let s = if !h.is_null() && QueryFullProcessImageNameW(h, 0, name.as_mut_ptr(), &mut n) != 0 {
                    let full = String::from_utf16_lossy(&name[..n as usize]);
                    full.rsplit('\\').next().unwrap_or(&full).to_string()
                } else {
                    format!("pid{pid}")
                };
                Proc { name: s, h, peak_ws: 0 }
            });
            if p.h.is_null() {
                continue;
            }
            let mut m: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
            m.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if unsafe { GetProcessMemoryInfo(p.h, &mut m, m.cb) } != 0 {
                sum += m.WorkingSetSize as u64;
                p.peak_ws = p.peak_ws.max(m.PeakWorkingSetSize as u64);
            }
        }
        sum
    }

    /// Whether the foreground window belongs to a process in the job: a
    /// terminal that took the keyboard from whoever is typing.
    pub fn owns_foreground(&self) -> bool {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(GetForegroundWindow(), &mut pid) };
        pid != 0 && self.procs.contains_key(&pid)
    }

    /// Top-level visible windows owned by processes in the job.
    pub fn windows(&self) -> Vec<windows_sys::Win32::Foundation::HWND> {
        use windows_sys::Win32::Foundation::{HWND, LPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible};
        unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> i32 {
            (*(l as *mut Vec<HWND>)).push(h);
            1
        }
        let mut all: Vec<HWND> = Vec::new();
        unsafe { EnumWindows(Some(cb), &mut all as *mut _ as LPARAM) };
        all.into_iter()
            .filter(|&h| {
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(h, &mut pid) };
                self.procs.contains_key(&pid) && unsafe { IsWindowVisible(h) } != 0
            })
            .collect()
    }

    /// Make every window of the job one that cannot be activated: not by a
    /// click, and not by Windows handing activation down the z-order when
    /// the foreground window closes. A person types on this machine; their
    /// keys must never land in a test window. Returns how many were marked.
    pub fn guard(&self) -> usize {
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, SetWindowLongPtrW, GWL_EXSTYLE, WS_EX_NOACTIVATE};
        let mut n = 0;
        for h in self.windows() {
            unsafe {
                let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
                if ex & WS_EX_NOACTIVATE as isize == 0 {
                    SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE as isize);
                    n += 1;
                }
            }
        }
        n
    }

    /// Share (0 to 1) of a 9x9 grid of points over the job's largest window
    /// where that window is what is on top: 1 is uncovered, 0 is hidden.
    pub fn visible_share(&self) -> Option<f64> {
        use windows_sys::Win32::Foundation::{POINT, RECT};
        use windows_sys::Win32::UI::WindowsAndMessaging::{GetAncestor, GetWindowRect, GetWindowThreadProcessId, WindowFromPoint, GA_ROOT};
        let mut best: Option<(i64, RECT)> = None;
        for h in self.windows() {
            let mut r: RECT = unsafe { std::mem::zeroed() };
            if unsafe { GetWindowRect(h, &mut r) } == 0 {
                continue;
            }
            let a = (r.right - r.left) as i64 * (r.bottom - r.top) as i64;
            if a > 20_000 && best.map(|b| a > b.0).unwrap_or(true) {
                best = Some((a, r));
            }
        }
        let (_, r) = best?;
        let mut on = 0;
        for i in 1..=9 {
            for j in 1..=9 {
                let p = POINT { x: r.left + (r.right - r.left) * i / 10, y: r.top + (r.bottom - r.top) * j / 10 };
                let mut pid = 0u32;
                unsafe { GetWindowThreadProcessId(GetAncestor(WindowFromPoint(p), GA_ROOT), &mut pid) };
                if self.procs.contains_key(&pid) {
                    on += 1;
                }
            }
        }
        Some(on as f64 / 81.0)
    }

    pub fn terminate(&self) {
        unsafe { TerminateJobObject(self.h, 0xBE) };
    }

    pub fn procs_json(&self) -> String {
        let v: Vec<String> = self
            .procs
            .iter()
            .map(|(pid, p)| format!("{{\"pid\":{pid},\"name\":{},\"peak_ws_mb\":{:.1}}}", js(&p.name), p.peak_ws as f64 / 1048576.0))
            .collect();
        format!("[{}]", v.join(","))
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        for p in self.procs.values() {
            if !p.h.is_null() {
                unsafe { CloseHandle(p.h) };
            }
        }
        unsafe { CloseHandle(self.h) };
    }
}

pub fn main(args: &[String]) -> Result<(), String> {
    let sep = args.iter().position(|a| a == "--").ok_or("run: -- COMMAND LINE is required")?;
    let (opts, cmd) = (&args[..sep], &args[sep + 1..]);
    if cmd.is_empty() {
        return Err("run: empty command".into());
    }
    // The command line is passed through as one string when given as one
    // argument (so a terminal's own quoting rules are kept).
    let cmdline = if cmd.len() == 1 { cmd[0].clone() } else { cmd.join(" ") };
    let out = opt(opts, "--out").ok_or("--out FILE is required")?.to_string();
    let timeout = Duration::from_secs_f64(opt_num(opts, "--timeout", 120.0));
    let grace = Duration::from_secs_f64(opt_num(opts, "--grace", 3.0));
    let report = opt(opts, "--report").map(|s| s.to_string());
    let idle: f64 = opt_num(opts, "--idle", 0.0);
    let settle: f64 = opt_num(opts, "--settle", 5.0);

    let mut job = Job::new()?;
    let mut load = Load::start();
    let start = Instant::now();
    let (t0, pid) = job.spawn(&cmdline)?;
    let tick = Duration::from_millis(20);
    let mut peak = 0u64;
    let mut reported: Option<Instant> = None;
    let mut closed_by_harness = false;
    let mut timed_out = false;
    let mut idle_json = String::new();
    let mut cpu_at_start: Option<f64> = None;
    let mut cpu_at_report: Option<f64> = None;
    let mut took_foreground = false;
    let mut guarded = 0usize;
    let mut ever_locked = false;
    let mut vis_min: Option<f64> = None;
    let mut vis_max: Option<f64> = None;
    let mut last_look = Instant::now() - Duration::from_secs(1);
    // Every 200 ms: mark new windows unactivatable, note lock state and how
    // much of the window is uncovered.
    let mut look = |job: &Job| {
        if last_look.elapsed() < Duration::from_millis(200) {
            return;
        }
        last_look = Instant::now();
        guarded += job.guard();
        ever_locked |= !unlocked();
        if let Some(v) = job.visible_share() {
            vis_min = Some(vis_min.map_or(v, |m: f64| m.min(v)));
            vis_max = Some(vis_max.map_or(v, |m: f64| m.max(v)));
        }
    };
    let cpu_now = |j: &Job| {
        let (u, k, _, _) = j.accounting();
        u + k
    };

    if idle > 0.0 {
        let settle_end = start + Duration::from_secs_f64(settle);
        while Instant::now() < settle_end {
            peak = peak.max(job.sample());
            took_foreground |= job.owns_foreground();
            look(&job);
            std::thread::sleep(tick);
        }
        let (u0, k0, _, _) = job.accounting();
        let a = Instant::now();
        let end = a + Duration::from_secs_f64(idle);
        let mut sum = 0f64;
        let mut n = 0u32;
        let mut max = 0u64;
        // Sample memory once a second only: the sampler must not be what
        // the terminal sees as activity, and its own CPU is not in the job.
        while Instant::now() < end {
            let s = job.sample();
            load.sample();
            took_foreground |= job.owns_foreground();
            look(&job);
            max = max.max(s);
            sum += s as f64;
            n += 1;
            std::thread::sleep(Duration::from_millis(1000).min(end.saturating_duration_since(Instant::now())));
        }
        let (u1, k1, _, active) = job.accounting();
        let span = a.elapsed().as_secs_f64();
        let cpu = (u1 - u0) + (k1 - k0);
        idle_json = format!(
            ",\"idle\":{{\"seconds\":{span:.3},\"cpu_s\":{cpu:.4},\"cpu_pct_of_one_core\":{:.3},\"ws_mb_avg\":{:.1},\"ws_mb_max\":{:.1},\"active_processes\":{active}}}",
            cpu / span * 100.0,
            sum / n.max(1) as f64 / 1048576.0,
            max as f64 / 1048576.0
        );
        job.terminate();
        closed_by_harness = true;
    } else {
        loop {
            peak = peak.max(job.sample());
            load.sample();
            took_foreground |= job.owns_foreground();
            look(&job);
            if cpu_at_start.is_none() {
                if let Some(r) = &report {
                    if std::fs::metadata(format!("{r}.start")).is_ok() {
                        cpu_at_start = Some(cpu_now(&job));
                    }
                }
            }
            let (_, _, _, active) = job.accounting();
            if active == 0 {
                break;
            }
            if reported.is_none() {
                if let Some(r) = &report {
                    if std::fs::metadata(r).map(|m| m.len() > 0).unwrap_or(false) {
                        reported = Some(Instant::now());
                        cpu_at_report = Some(cpu_now(&job));
                    }
                }
            }
            if let Some(t) = reported {
                if t.elapsed() > grace {
                    job.terminate();
                    closed_by_harness = true;
                }
            }
            if start.elapsed() > timeout {
                job.terminate();
                timed_out = true;
                closed_by_harness = true;
            }
            std::thread::sleep(tick);
        }
    }
    // Let terminated processes finish leaving, so accounting is final.
    let wait_end = Instant::now() + Duration::from_secs(5);
    while job.accounting().3 > 0 && Instant::now() < wait_end {
        std::thread::sleep(tick);
    }
    let (u, k, total, active) = job.accounting();
    let json = format!(
        "{{\"cmdline\":{},\"pid\":{pid},\"t0\":{t0},\"wall_s\":{:.3},\"report_after_s\":{},\"cpu_s\":{:.4},\"user_s\":{u:.4},\"kernel_s\":{k:.4},\"total_processes\":{total},\"active_at_end\":{active},\"peak_ws_mb\":{:.1},\"closed_by_harness\":{closed_by_harness},\"timed_out\":{timed_out},\"payload_cpu_s\":{},\"took_foreground\":{took_foreground},\"windows_made_unactivatable\":{guarded},\"session_locked_at_some_point\":{ever_locked},\"visible_share_min\":{},\"visible_share_max\":{},\"procs\":{}{idle_json}{}}}\n",
        js(&cmdline),
        start.elapsed().as_secs_f64(),
        reported.map(|t| format!("{:.3}", (t - start).as_secs_f64())).unwrap_or("null".into()),
        u + k,
        peak as f64 / 1048576.0,
        match (cpu_at_start, cpu_at_report) {
            (Some(a), Some(b)) => format!("{:.4}", b - a),
            _ => "null".into(),
        },
        vis_min.map(|v| format!("{v:.3}")).unwrap_or("null".into()),
        vis_max.map(|v| format!("{v:.3}")).unwrap_or("null".into()),
        job.procs_json(),
        load.json()
    );
    std::fs::write(&out, json).map_err(|e| format!("{out}: {e}"))?;
    Ok(())
}
