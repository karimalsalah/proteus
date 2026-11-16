//! What lets PROTEUS be a single file: the pieces that must exist on disk
//! (Windows' pseudo-console host, and a private copy of this program for the
//! daemon) are carried inside the executable and unpacked on first use.
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Once;

use crate::paths;

static CONPTY_DLL: &[u8] = include_bytes!("../vendor/conpty/conpty.dll");
static OPENCONSOLE: &[u8] = include_bytes!("../vendor/conpty/OpenConsole.exe");

pub const LICENSES: &str = concat!(
    "PROTEUS bundles the following third-party components.\n\n",
    "== Microsoft.Windows.Console.ConPTY (conpty.dll, OpenConsole.exe) ==\n",
    include_str!("../vendor/conpty/LICENSE.txt"),
    "\n== Maple Mono NF (embedded font) ==\n",
    include_str!("../assets/fonts/NOTICE.txt"),
    "\n",
    include_str!("../assets/fonts/OFL.txt"),
    "\n",
    include_str!("../assets/THIRD-PARTY.txt"),
);

/// Identifies this exact build; the same in every copy of the executable.
pub fn build_id() -> String {
    format!("{}-{}", env!("CARGO_PKG_VERSION"), env!("PROTEUS_BUILD"))
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

pub fn dir() -> PathBuf {
    let d = paths::data_dir().join("runtime");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Make `path` hold exactly `bytes`. Safe when several processes do it at
/// once: each writes its own temporary file and the rename is atomic.
fn place(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let matches = |p: &Path| std::fs::read(p).map(|b| b == bytes).unwrap_or(false);
    if matches(path) {
        return Ok(());
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        if !matches(path) {
            return Err(e);
        }
    }
    Ok(())
}

#[cfg(windows)]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut std::ffi::c_void;
}

fn bundled_dir() -> PathBuf {
    dir().join(format!("conpty-{:016x}", fnv(CONPTY_DLL) ^ fnv(OPENCONSOLE).rotate_left(1)))
}

/// The unpacked bundled console host's folder, without loading it (the
/// console-host bench loads the one it is told to by full path).
#[cfg(windows)]
pub fn conpty_dir() -> Option<PathBuf> {
    let d = bundled_dir();
    std::fs::create_dir_all(&d).ok()?;
    place(&d.join("OpenConsole.exe"), OPENCONSOLE).ok()?;
    place(&d.join("conpty.dll"), CONPTY_DLL).ok()?;
    let _ = std::fs::write(d.join("VERSION.txt"), include_str!("../vendor/conpty/VERSION.txt"));
    Some(d)
}

/// Unpack the pseudo-console host and load it, before the first terminal is
/// opened. Loading it by full path here means the later lookup by name finds
/// this copy and never one that happens to be elsewhere on the machine.
pub fn ensure_conpty() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let d = bundled_dir();
        let result = std::fs::create_dir_all(&d)
            .and_then(|_| place(&d.join("OpenConsole.exe"), OPENCONSOLE))
            .and_then(|_| place(&d.join("conpty.dll"), CONPTY_DLL));
        match result {
            Ok(()) => {
                #[cfg(windows)]
                {
                    use std::os::windows::ffi::OsStrExt;
                    let wide: Vec<u16> = d.join("conpty.dll").as_os_str().encode_wide().chain(Some(0)).collect();
                    if unsafe { LoadLibraryW(wide.as_ptr()) }.is_null() {
                        paths::log("runtime: bundled conpty.dll failed to load; falling back to the system console host");
                    }
                }
            }
            Err(e) => paths::log(&format!("runtime: could not unpack the console host ({e}); falling back to the system one")),
        }
    });
}

/// A private copy of this program for the daemon to run from, so the file
/// the person launched is never held open by a background process and can be
/// replaced at any time.
pub fn daemon_exe() -> PathBuf {
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("proteus.exe"));
    let copy = dir().join(format!("proteus-{}.exe", build_id()));
    if copy.is_file() {
        return copy;
    }
    let tmp = copy.with_extension(format!("tmp{}", std::process::id()));
    let made = std::fs::copy(&me, &tmp).and_then(|_| std::fs::rename(&tmp, &copy));
    if made.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    if copy.is_file() {
        copy
    } else {
        me
    }
}

/// Remove leftovers from older builds. Files still in use refuse to go and
/// are simply tried again next time.
pub fn sweep() {
    let keep = format!("proteus-{}.exe", build_id());
    let Ok(entries) = std::fs::read_dir(dir()) else { return };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let old_build = name.starts_with("proteus-") && name != keep;
        let stray = name.contains(".tmp") || name.starts_with("retired-");
        if old_build || stray {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn place_is_idempotent_and_repairs_damage() {
        let p = std::env::temp_dir().join(format!("proteus-place-{}.bin", std::process::id()));
        place(&p, b"payload").unwrap();
        place(&p, b"payload").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"payload");
        std::fs::write(&p, b"tampered").unwrap();
        place(&p, b"payload").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"payload");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn bundled_console_host_is_present() {
        assert!(CONPTY_DLL.len() > 50_000 && &CONPTY_DLL[..2] == b"MZ");
        assert!(OPENCONSOLE.len() > 500_000 && &OPENCONSOLE[..2] == b"MZ");
        assert!(LICENSES.contains("MIT License"));
    }
}
