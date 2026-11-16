//! Self-update. A release is three files: `PROTEUS.exe`, `PROTEUS.manifest`
//! (version, build, size and SHA-256 of the exe) and `PROTEUS.manifest.sig`
//! (an Ed25519 signature of the manifest). The public key is in this exe; the
//! secret key exists only on the release machine, never on a server.
//!
//! An update is applied only if the signature verifies against that key, the
//! manifest names a build newer than this one (no downgrades), and the
//! downloaded exe has exactly the size and hash the manifest states.
//! Anything else is refused and nothing on disk changes.
//!
//!   proteus update                 check, download, verify and apply
//!   proteus update --check         say what is available, change nothing
//!   proteus update --source DIR    take the three files from a folder
//!   proteus update --exe FILE      update that file instead of this exe
//!
//! `update` in config.json: "auto" (apply when found; takes effect at the next
//! launch), "check" (download and verify, apply with `proteus update`), "off".
//! Nothing is fetched while `PROTEUS_DATA` is set (tests) or when running
//! from a build folder.
use std::io::Read;
use std::path::{Path, PathBuf};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::paths;

/// Releases are signed with the secret half of this key.
pub const PUBLIC_KEY: &str = "aa7bddd684b32560e91fb1afc67aba2d9ec33b6741e475c338e0ee4c147a74ac";
/// Where releases are published.
pub const RELEASES: &str = "https://github.com/karimalsalah/proteus/releases/latest/download";
const MAX_EXE: u64 = 256 << 20;

#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Already this build or newer.
    Current { have: String, offered: String },
    /// A newer build exists (reported by `--check`).
    Available { have: String, offered: String },
    Applied { from: String, to: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Manifest {
    pub version: (u64, u64, u64),
    pub stamp: u64,
    pub build: String,
    pub size: u64,
    pub sha256: String,
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || !s.is_ascii() {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok()).collect()
}

/// "0.5.0-1794795373" as ((0, 5, 0), 1794795373).
pub fn parse_build(build: &str) -> Option<((u64, u64, u64), u64)> {
    let (v, stamp) = build.trim().split_once('-')?;
    let mut n = v.split('.').map(|p| p.parse::<u64>().ok());
    let version = (n.next()??, n.next()??, n.next()??);
    if n.next().is_some() {
        return None;
    }
    Some((version, stamp.parse().ok()?))
}

/// Check the signature, then read the manifest. Nothing in an unsigned or
/// wrongly signed manifest is ever looked at.
pub fn verify_manifest(manifest: &[u8], sig_hex: &str, public_key_hex: &str) -> Result<Manifest, String> {
    let key: [u8; 32] = unhex(public_key_hex).and_then(|v| v.try_into().ok()).ok_or("bad public key")?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| "bad public key")?;
    let sig: [u8; 64] = unhex(sig_hex).and_then(|v| v.try_into().ok()).ok_or("signature is not 64 bytes of hex")?;
    key.verify(manifest, &Signature::from_bytes(&sig)).map_err(|_| "signature does not match the release key")?;
    let text = std::str::from_utf8(manifest).map_err(|_| "manifest is not text")?;
    let mut lines = text.lines();
    if lines.next() != Some("proteus-release 1") {
        return Err("unknown manifest format".into());
    }
    let (mut build, mut size, mut sha) = (None, None, None);
    for l in lines {
        match l.split_once(' ') {
            Some(("build", v)) => build = Some(v.trim().to_string()),
            Some(("size", v)) => size = v.trim().parse::<u64>().ok(),
            Some(("sha256", v)) => sha = Some(v.trim().to_ascii_lowercase()),
            _ => {}
        }
    }
    let build = build.ok_or("manifest names no build")?;
    let (version, stamp) = parse_build(&build).ok_or("manifest build is malformed")?;
    let size = size.filter(|s| *s > 0 && *s <= MAX_EXE).ok_or("manifest size is missing or absurd")?;
    let sha256 = sha.filter(|s| s.len() == 64 && unhex(s).is_some()).ok_or("manifest hash is malformed")?;
    Ok(Manifest { version, stamp, build, size, sha256 })
}

pub fn sha256_file(path: &Path) -> std::io::Result<(u64, String)> {
    let mut f = std::fs::File::open(path)?;
    let (mut h, mut buf, mut n) = (Sha256::new(), vec![0u8; 1 << 16], 0u64);
    loop {
        let k = f.read(&mut buf)?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((n, h.finalize().iter().map(|b| format!("{b:02x}")).collect()))
}

/// Where the three release files come from.
pub enum Source {
    Folder(PathBuf),
    Web(String),
}

impl Source {
    /// Put `name` at `to`. The web source uses the curl that ships with
    /// Windows, by full path, with the system's certificate store.
    fn fetch(&self, name: &str, to: &Path) -> Result<(), String> {
        match self {
            Source::Folder(dir) => std::fs::copy(dir.join(name), to).map(|_| ()).map_err(|e| format!("{name}: {e}")),
            Source::Web(base) => {
                use std::os::windows::process::CommandExt;
                let curl = PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into())).join("System32").join("curl.exe");
                let status = std::process::Command::new(curl)
                    .args(["--silent", "--show-error", "--fail", "--location", "--proto", "=https", "--tlsv1.2", "--max-time", "600", "--max-filesize"])
                    .arg(MAX_EXE.to_string())
                    .arg("--output")
                    .arg(to)
                    .arg(format!("{base}/{name}"))
                    .creation_flags(0x0800_0000) // no console window
                    .status()
                    .map_err(|e| format!("could not run curl: {e}"))?;
                if status.success() {
                    Ok(())
                } else {
                    Err(format!("{name}: download failed ({status})"))
                }
            }
        }
    }
}

/// Check `source` for a build newer than `have` and, unless `check_only`,
/// replace `target` with it. `work` is a scratch folder on the same volume as
/// `target`; the replaced exe is moved into it (a running exe can be moved,
/// not deleted) and cleaned up later.
pub fn update(target: &Path, have: &str, source: &Source, public_key_hex: &str, work: &Path, check_only: bool) -> Result<Outcome, String> {
    let (have_v, have_stamp) = parse_build(have).ok_or("this build has no readable version")?;
    std::fs::create_dir_all(work).map_err(|e| e.to_string())?;
    let (m_path, s_path) = (work.join("PROTEUS.manifest"), work.join("PROTEUS.manifest.sig"));
    source.fetch("PROTEUS.manifest", &m_path)?;
    source.fetch("PROTEUS.manifest.sig", &s_path)?;
    let manifest = std::fs::read(&m_path).map_err(|e| e.to_string())?;
    if manifest.len() > 4096 {
        return Err("manifest is too large".into());
    }
    let sig = std::fs::read_to_string(&s_path).map_err(|e| e.to_string())?;
    let m = verify_manifest(&manifest, &sig, public_key_hex)?;
    if (m.version, m.stamp) <= (have_v, have_stamp) {
        return Ok(Outcome::Current { have: have.to_string(), offered: m.build });
    }
    if check_only {
        return Ok(Outcome::Available { have: have.to_string(), offered: m.build });
    }
    // Already that exact file (another window got there first): nothing to do.
    if sha256_file(target).is_ok_and(|(n, h)| n == m.size && h == m.sha256) {
        return Ok(Outcome::Current { have: have.to_string(), offered: m.build });
    }
    let fresh = work.join(format!("PROTEUS-{}.exe", m.build));
    source.fetch("PROTEUS.exe", &fresh)?;
    let (size, sha) = sha256_file(&fresh).map_err(|e| e.to_string())?;
    if size != m.size || sha != m.sha256 {
        let _ = std::fs::remove_file(&fresh);
        return Err(format!("the downloaded exe is not the one the manifest signs (size {size} vs {}, hash {sha})", m.size));
    }
    // Swap: the old exe steps aside, the new one takes its name. If the
    // second step fails the old one is put back.
    let retired = work.join(format!("retired-{}.exe", have.replace(|c: char| !c.is_ascii_alphanumeric() && c != '.' && c != '-', "_")));
    let _ = std::fs::remove_file(&retired);
    let had_old = target.exists();
    if had_old {
        std::fs::rename(target, &retired).map_err(|e| format!("could not move the current exe aside: {e}"))?;
    }
    if let Err(e) = std::fs::rename(&fresh, target).or_else(|_| std::fs::copy(&fresh, target).map(|_| ())) {
        if had_old {
            let _ = std::fs::rename(&retired, target);
        }
        return Err(format!("could not put the new exe in place: {e}"));
    }
    let _ = std::fs::remove_file(&fresh);
    paths::log(&format!("update: {have} -> {} applied to {}", m.build, target.display()));
    Ok(Outcome::Applied { from: have.to_string(), to: m.build })
}

/// May this process fetch updates by itself? Never under test, never from a
/// build folder, never when switched off.
pub fn automatic_allowed(exe: &Path, mode: &str) -> bool {
    let p = exe.to_string_lossy().to_ascii_lowercase().replace('/', "\\");
    mode != "off" && std::env::var_os("PROTEUS_DATA").is_none() && !p.contains("\\dist\\") && !p.contains("\\target\\") && !p.contains("\\runtime\\")
}

/// Called once when a window starts: at most one check a day, on its own
/// thread, never in the way of the terminal.
pub fn background(mode: String) {
    let Ok(exe) = std::env::current_exe() else { return };
    if !automatic_allowed(&exe, &mode) {
        return;
    }
    let stamp = paths::data_dir().join("update-checked");
    let recent = stamp.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d.as_secs() < 86_400);
    if recent {
        return;
    }
    let _ = std::thread::Builder::new().name("update".into()).spawn(move || {
        let _ = std::fs::write(&stamp, b"");
        // Copies replaced by earlier updates go once nothing runs them.
        if let Ok(rd) = std::fs::read_dir(paths::data_dir().join("runtime").join("update")) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        // Scratch beside the exe's volume: the data folder's runtime directory.
        let work = paths::data_dir().join("runtime").join("update");
        let have = crate::runtime::build_id();
        match update(&exe, &have, &Source::Web(RELEASES.into()), PUBLIC_KEY, &work, mode != "auto") {
            Ok(Outcome::Applied { from, to }) => paths::log(&format!("update: {from} -> {to}; the next window is the new build")),
            Ok(Outcome::Available { offered, .. }) => paths::log(&format!("update: {offered} is available; run `proteus update`")),
            Ok(Outcome::Current { .. }) => {}
            Err(e) => paths::log(&format!("update: not applied: {e}")),
        }
    });
}

pub fn cli(args: &[String]) -> crate::Res<()> {
    let (mut check, mut source, mut exe) = (false, None, None);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--check" => check = true,
            "--source" => {
                source = Some(Source::Folder(PathBuf::from(args.get(i + 1).ok_or("--source needs a folder")?)));
                i += 1;
            }
            "--exe" => {
                exe = Some(PathBuf::from(args.get(i + 1).ok_or("--exe needs a file")?));
                i += 1;
            }
            other => return Err(format!("unknown option {other}").into()),
        }
        i += 1;
    }
    let explicit = exe.is_some();
    let target = match exe {
        Some(p) => p,
        None => std::env::current_exe()?,
    };
    // Updating some other file: ask it what build it is.
    let have = if explicit {
        let out = std::process::Command::new(&target).arg("--version").output()?;
        String::from_utf8_lossy(&out.stdout).trim().trim_start_matches("proteus ").to_string()
    } else {
        crate::runtime::build_id()
    };
    let work = target.parent().map(|p| p.join(".proteus-update")).filter(|_| explicit).unwrap_or_else(|| paths::data_dir().join("runtime").join("update"));
    let source = source.unwrap_or_else(|| Source::Web(RELEASES.into()));
    match update(&target, &have, &source, PUBLIC_KEY, &work, check)? {
        Outcome::Current { have, offered } => println!("up to date: this is {have}; the latest release is {offered}"),
        Outcome::Available { have, offered } => println!("available: {offered} (this is {have}); run `proteus update` to apply"),
        Outcome::Applied { from, to } => println!("updated {from} -> {to}; the next window is the new build"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// A release folder signed with a throwaway key: (folder, public key hex).
    fn release(name: &str, build: &str, exe: &[u8], key: &SigningKey) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("proteus-update-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("PROTEUS.exe"), exe).unwrap();
        let sha: String = Sha256::digest(exe).iter().map(|b| format!("{b:02x}")).collect();
        let manifest = format!("proteus-release 1\nbuild {build}\nsize {}\nsha256 {sha}\n", exe.len());
        std::fs::write(dir.join("PROTEUS.manifest"), &manifest).unwrap();
        std::fs::write(dir.join("PROTEUS.manifest.sig"), hex(&key.sign(manifest.as_bytes()).to_bytes())).unwrap();
        dir
    }

    fn installed(name: &str, content: &[u8]) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("proteus-update-test-{}-{name}-installed", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("PROTEUS.exe");
        std::fs::write(&exe, content).unwrap();
        (exe, dir.join("work"))
    }

    #[test]
    fn a_signed_newer_release_replaces_the_exe_and_nothing_else_does() {
        let key = SigningKey::from_bytes(&[7u8; 32]);
        let other = SigningKey::from_bytes(&[9u8; 32]);
        let public = hex(key.verifying_key().as_bytes());
        let have = "0.5.0-1000";

        // The real thing: applied, and the old exe is kept aside.
        let good = release("good", "0.5.1-2000", b"new build", &key);
        let (exe, work) = installed("good", b"old build");
        assert_eq!(update(&exe, have, &Source::Folder(good.clone()), &public, &work, true), Ok(Outcome::Available { have: have.into(), offered: "0.5.1-2000".into() }));
        assert_eq!(std::fs::read(&exe).unwrap(), b"old build", "--check changes nothing");
        assert_eq!(update(&exe, have, &Source::Folder(good.clone()), &public, &work, false), Ok(Outcome::Applied { from: have.into(), to: "0.5.1-2000".into() }));
        assert_eq!(std::fs::read(&exe).unwrap(), b"new build");
        assert_eq!(std::fs::read(work.join("retired-0.5.0-1000.exe")).unwrap(), b"old build");
        // Offered again to the build it produced: nothing to do.
        assert!(matches!(update(&exe, "0.5.1-2000", &Source::Folder(good.clone()), &public, &work, false), Ok(Outcome::Current { .. })));

        // Every way of being wrong leaves the exe exactly as it was.
        let refused = |name: &str, dir: &Path, pk: &str, have: &str| {
            let (exe, work) = installed(name, b"old build");
            let r = update(&exe, have, &Source::Folder(dir.to_path_buf()), pk, &work, false);
            assert_eq!(std::fs::read(&exe).unwrap(), b"old build", "{name}: the exe changed");
            r
        };
        // An older or equal build: a downgrade is never applied.
        assert!(matches!(refused("older", &good, &public, "0.6.0-1"), Ok(Outcome::Current { .. })));
        assert!(matches!(refused("same-version-older-stamp", &good, &public, "0.5.1-2001"), Ok(Outcome::Current { .. })));
        // Signed by someone else.
        let forged = release("forged", "9.9.9-9999", b"evil", &other);
        assert!(refused("forged", &forged, &public, have).unwrap_err().contains("signature"));
        // A real manifest with a different exe behind it.
        let swapped = release("swapped", "0.5.1-2000", b"new build", &key);
        std::fs::write(swapped.join("PROTEUS.exe"), b"evil one").unwrap();
        assert!(refused("swapped", &swapped, &public, have).unwrap_err().contains("not the one the manifest signs"));
        // A manifest edited after signing.
        let edited = release("edited", "0.5.1-2000", b"new build", &key);
        let m = std::fs::read_to_string(edited.join("PROTEUS.manifest")).unwrap().replace("0.5.1", "0.9.1");
        std::fs::write(edited.join("PROTEUS.manifest"), m).unwrap();
        assert!(refused("edited", &edited, &public, have).unwrap_err().contains("signature"));
        // A signature that is not one, and files that are not there.
        let nosig = release("nosig", "0.5.1-2000", b"new build", &key);
        std::fs::write(nosig.join("PROTEUS.manifest.sig"), "abcd").unwrap();
        assert!(refused("nosig", &nosig, &public, have).is_err());
        assert!(refused("missing", &std::env::temp_dir().join("proteus-update-test-nowhere"), &public, have).is_err());
    }

    #[test]
    fn builds_compare_by_version_then_stamp_and_tests_never_fetch() {
        assert_eq!(parse_build("0.5.0-1794795373"), Some(((0, 5, 0), 1794795373)));
        assert!(parse_build("0.5.0-9").unwrap() < parse_build("0.5.1-1").unwrap());
        assert!(parse_build("0.10.0-1").unwrap() > parse_build("0.9.9-9").unwrap());
        for bad in ["0.5-1", "0.5.0", "a.b.c-1", "0.5.0.1-2", "0.5.0-x"] {
            assert_eq!(parse_build(bad), None, "{bad}");
        }
        // The key embedded in this exe is a real Ed25519 public key.
        assert!(VerifyingKey::from_bytes(&<[u8; 32]>::try_from(unhex(PUBLIC_KEY).unwrap()).unwrap()).is_ok());
        assert!(!automatic_allowed(Path::new(r"C:\src\proteus\dist\proteus-1.exe"), "auto"));
        assert!(!automatic_allowed(Path::new(r"C:\src\proteus\target\release\proteus.exe"), "auto"));
        assert!(!automatic_allowed(Path::new(r"C:\Users\x\Desktop\PROTEUS.exe"), "off"));
    }
}
