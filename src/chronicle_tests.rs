//! Tests for chronicle. Every one works on real files in a folder it makes
//! under the system temp folder and removes afterwards; none touches the
//! data folder of an installed PROTEUS or changes this process's
//! environment or current folder.
use super::*;
use std::os::windows::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, AtomicU32};
use std::sync::Arc;

static NEXT: AtomicU32 = AtomicU32::new(0);

/// `<temp>/proteus-chronicle-test-<pid>-<n>-<name>/` with `root` (the tree
/// under test) and `data` (where its history goes) inside.
struct Lab {
    base: PathBuf,
}

impl Lab {
    fn new(name: &str) -> Lab {
        let base = std::env::temp_dir().join(format!("proteus-chronicle-test-{}-{}-{name}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("root")).unwrap();
        fs::create_dir_all(base.join("data")).unwrap();
        Lab { base }
    }

    fn root(&self) -> PathBuf {
        self.base.join("root")
    }

    fn data(&self) -> PathBuf {
        self.base.join("data")
    }

    fn at(&self, rel: &str) -> PathBuf {
        join_rel(&self.root(), rel)
    }

    fn put(&self, rel: &str, content: &[u8]) {
        let p = self.at(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.at(rel)).ok()
    }

    fn track(&self) -> (Store, Snap) {
        Store::track(&self.data(), &self.root(), None, None, None, true).unwrap()
    }

    fn open(&self) -> Store {
        let mut s = Store::open(&self.data(), &self.root()).unwrap();
        s.hermetic = true;
        s
    }
}

impl Drop for Lab {
    fn drop(&mut self) {
        free_all(&self.base);
        let _ = fs::remove_dir_all(&self.base);
    }
}

/// Clear read-only and hidden everywhere below `dir` (links are not entered).
fn free_all(dir: &Path) {
    for e in fs::read_dir(dir).into_iter().flatten().flatten() {
        let _ = set_attrs(&e.path(), 0);
        if fs::symlink_metadata(e.path()).is_ok_and(|m| m.is_dir()) {
            free_all(&e.path());
        }
    }
}

fn snap(s: &mut Store) -> Snap {
    let _guard = s.begin().unwrap();
    s.snapshot(&SnapOpts { label: None, reason: "test", record: true, rehash: false }).unwrap()
}

/// The tree id worked out a second way: plain std::fs, no ignore rules, no
/// cache, no store, its own walk and its own hashing.
fn independent(dir: &Path) -> Hash {
    let mut kids: Vec<(String, PathBuf, fs::Metadata)> = fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let e = e.unwrap();
            (e.file_name().into_string().unwrap(), e.path(), fs::symlink_metadata(e.path()).unwrap())
        })
        .collect();
    kids.sort_by(|a, b| a.0.cmp(&b.0));
    let mut text = String::from("proteus-tree 1\n");
    for (name, path, md) in kids {
        let attrs = md.file_attributes() & 7;
        if md.is_dir() {
            text.push_str(&format!("d {attrs:x} {} {name}\n", hex(&independent(&path))));
        } else {
            let bytes = fs::read(&path).unwrap();
            text.push_str(&format!("f {attrs:x} {} {} {name}\n", bytes.len(), hex(&Sha256::digest(&bytes))));
        }
    }
    Sha256::digest(text.as_bytes()).into()
}

/// Everything in a folder, byte for byte: path -> (attributes, content or None for a folder).
type Picture = BTreeMap<String, (u32, Option<Vec<u8>>)>;

fn picture(dir: &Path) -> Picture {
    fn fill(dir: &Path, prefix: &str, out: &mut Picture) {
        for e in fs::read_dir(dir).unwrap() {
            let e = e.unwrap();
            let name = e.file_name().into_string().unwrap();
            let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
            let md = fs::symlink_metadata(e.path()).unwrap();
            let attrs = md.file_attributes() & 7;
            if md.is_dir() {
                out.insert(rel.clone(), (attrs, None));
                fill(&e.path(), &rel, out);
            } else if md.is_file() {
                out.insert(rel, (attrs, Some(fs::read(e.path()).unwrap())));
            }
        }
    }
    let mut out = Picture::new();
    fill(dir, "", &mut out);
    out
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            v.extend_from_slice(&self.next().to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

#[test]
fn names_that_could_leave_the_root_are_refused() {
    for good in ["a", "a.txt", ".gitignore", ".env", "with space.txt", "Über.rs", "a.b.c", "com", "COM10", "console.log", "nullable.rs", "lpt", "..a", "a..b"] {
        assert!(valid_name(good).is_ok(), "{good:?} should be allowed");
    }
    for bad in [
        "", ".", "..", "a/b", "a\\b", "C:", "c:evil", "file:stream", "file::$DATA", "NUL", "nul", "nul.txt", "CON", "con.tar.gz", "PRN", "AUX", "COM1", "com9.log",
        "LPT1", "COM\u{b9}", "CONIN$", "CONOUT$", "trail.", "trail ", "a\0b", "a\nb", "a\tb", "what?", "star*", "<x>", "pipe|", "quo\"te", "nul .txt",
    ] {
        assert!(valid_name(bad).is_err(), "{bad:?} should be refused");
    }
    assert!(valid_name(&"x".repeat(255)).is_ok());
    assert!(valid_name(&"x".repeat(256)).is_err());

    for good in ["a", "a/b/c.txt", ".github/workflows/ci.yml"] {
        assert!(valid_rel(good).is_ok(), "{good:?}");
    }
    for bad in ["", "/a", "a/", "a//b", "../a", "a/../b", "a/./b", "C:/Windows", "//server/share/x", "\\\\server\\share", "a/b:c", "a/NUL/b", "..\\..\\x"] {
        assert!(valid_rel(bad).is_err(), "{bad:?} should be refused");
    }
}

#[test]
fn dates_are_written_correctly() {
    assert_eq!(stamp(0), "1970-01-01 00:00:00Z");
    assert_eq!(stamp(951_782_400_000), "2000-02-29 00:00:00Z");
    assert_eq!(stamp(951_868_799_000), "2000-02-29 23:59:59Z");
    assert_eq!(stamp(1_709_251_199_000), "2024-02-29 23:59:59Z");
    assert_eq!(stamp(1_735_689_600_000), "2025-01-01 00:00:00Z");
}

#[test]
fn drives_the_profile_and_proteus_own_data_are_never_tracked() {
    let lab = Lab::new("refuse");
    let data = lab.data();
    // Checks only: nothing here walks or tracks the folders named.
    let drive = canon(lab.base.ancestors().last().unwrap()).unwrap();
    let why = check_root(&drive, &data, None).unwrap_err();
    assert!(why.contains("whole drive"), "{why}");
    if let Some(profile) = profile() {
        let p = canon(&profile).unwrap();
        assert!(check_root(&p, &data, Some(&profile)).unwrap_err().contains("user profile"));
        assert!(check_root(p.parent().unwrap(), &data, Some(&profile)).is_err(), "a folder that holds the profile must be refused");
    }
    assert!(check_root(&canon(&data).unwrap(), &data, None).unwrap_err().contains("own data"));
    assert!(check_root(&canon(&lab.root()).unwrap(), &data, profile().as_deref()).is_ok(), "a project folder inside the profile is fine");

    // Through the real entry point, with a temp folder standing in for the profile.
    fs::create_dir_all(lab.at("home/project")).unwrap();
    lab.put("home/project/a.txt", b"a");
    let refused = Store::track(&data, &lab.root(), None, None, Some(&lab.at("home")), true);
    assert!(refused.is_err_and(|e| e.contains("user profile")));
    assert!(Store::track(&data, &lab.at("home"), None, None, Some(&lab.at("home")), true).is_err());
    assert!(!data.join("chronicle").exists(), "a refused folder must leave nothing behind");
    assert!(Store::track(&data, &lab.at("home/project"), None, None, Some(&lab.at("home")), true).is_ok());
    assert!(Store::open(&data, &lab.root()).is_err_and(|e| e.contains("not tracked")));
}

#[test]
fn the_budget_is_checked_before_anything_is_stored() {
    let lab = Lab::new("budget");
    for i in 0..5 {
        lab.put(&format!("f{i}.txt"), b"x");
    }
    let over = Store::track(&lab.data(), &lab.root(), Some(3), None, None, true);
    assert!(over.is_err_and(|e| e.contains("5 files") && e.contains("budget is 3 files")));
    assert!(!lab.data().join("chronicle").exists(), "nothing may be created for a tree that is refused");

    lab.put("big.bin", &vec![7u8; 2 << 20]);
    let over = Store::track(&lab.data(), &lab.root(), None, Some(1), None, true);
    assert!(over.is_err_and(|e| e.contains("budget is 200000 files and 1 MB")));

    let (mut store, first) = Store::track(&lab.data(), &lab.root(), Some(6), Some(3), None, true).unwrap();
    assert_eq!(first.files, 6);
    // The budget is kept, and a tree that grows past it is not recorded in part.
    lab.put("one-too-many.txt", b"x");
    let mut again = lab.open();
    assert_eq!((again.max_files, again.max_bytes), (6, 3 << 20));
    let _guard = again.begin().unwrap();
    assert!(again.snapshot(&SnapOpts { label: None, reason: "test", record: true, rehash: false }).is_err_and(|e| e.contains("7 files")));
    drop(_guard);
    assert_eq!(store.log.len(), 1);
    assert_eq!(log_read(&store.dir).len(), 1, "a snapshot over budget must not reach the log");
    // Tracking again with a larger budget is how it is raised.
    let (_, grown) = Store::track(&lab.data(), &lab.root(), Some(100), None, None, true).unwrap();
    assert_eq!(grown.files, 7);
    assert!(store.begin().is_ok());
}

#[test]
fn an_unchanged_tree_is_not_read_again_and_a_sly_change_is_still_seen() {
    let lab = Lab::new("cache");
    for i in 0..40 {
        lab.put(&format!("src/f{i}.txt"), format!("file number {i}\n").as_bytes());
    }
    lab.put("x.bin", b"XXXXXXXX");
    lab.put("y.bin", b"YYYYYYYY");
    // Files written in the last two seconds are deliberately not trusted to the cache.
    std::thread::sleep(Duration::from_millis(2300));
    let (mut store, first) = lab.track();
    assert_eq!((first.files, first.read_files), (42, 42));
    assert_eq!(first.tree, independent(&lab.root()));

    let second = snap(&mut store);
    assert_eq!(second.read_files, 0, "an unchanged tree must not be read again");
    assert_eq!(second.tree, first.tree);
    assert!(!second.changed && second.seq.is_none(), "an unchanged tree adds nothing to the log");
    assert_eq!(second.new_objects, 0);

    // Same size, and the modified time put back: only the change time and the content differ.
    let p = lab.at("src/f7.txt");
    let (before, mtime) = (fs::read(&p).unwrap(), fs::metadata(&p).unwrap().modified().unwrap());
    let mut sly = before.clone();
    sly[0] ^= 0x20;
    fs::write(&p, &sly).unwrap();
    fs::File::options().write(true).open(&p).unwrap().set_modified(mtime).unwrap();
    assert_eq!(fs::metadata(&p).unwrap().modified().unwrap(), mtime);
    let third = snap(&mut store);
    assert_eq!(third.read_files, 1, "a file rewritten with its old size and time must be read again");
    assert_ne!(third.tree, first.tree);
    assert_eq!(third.tree, independent(&lab.root()));

    // Another file of the same size and time moved onto a path: a different file, so read again.
    let t = fs::metadata(lab.at("x.bin")).unwrap().modified().unwrap();
    fs::File::options().write(true).open(lab.at("y.bin")).unwrap().set_modified(t).unwrap();
    std::thread::sleep(Duration::from_millis(2300));
    let settled = snap(&mut store);
    assert_eq!(snap(&mut store).read_files, 0);
    fs::rename(lab.at("y.bin"), lab.at("x.bin")).unwrap();
    let moved = snap(&mut store);
    assert_ne!(moved.tree, settled.tree);
    assert_eq!(moved.manifest["x.bin"].hash, sha(b"YYYYYYYY"));
    assert_eq!(moved.tree, independent(&lab.root()));

    // --rehash reads everything whatever the cache says.
    let _guard = store.begin().unwrap();
    let all = store.snapshot(&SnapOpts { label: None, reason: "test", record: true, rehash: true }).unwrap();
    assert_eq!(all.read_files, all.files);
    assert_eq!(all.tree, moved.tree);
}

#[test]
fn the_same_content_is_stored_once_and_text_is_deflated() {
    let lab = Lab::new("dedupe");
    let text = "the terminal is the one chokepoint every command passes through\n".repeat(1500);
    let noise = Rng(0x5eed).bytes(60_000);
    lab.put("a.txt", text.as_bytes());
    lab.put("copy/of/a.txt", text.as_bytes());
    lab.put("noise.bin", &noise);
    lab.put("empty", b"");
    let (mut store, first) = lab.track();
    let (t, n) = (store.objects.map[&sha(text.as_bytes())], store.objects.map[&sha(&noise)]);
    assert!(t.flags & F_DEFLATE != 0 && t.stored * 20 < t.raw, "repetitive text should deflate well: {} of {}", t.stored, t.raw);
    assert!(n.flags == 0 && n.stored == n.raw, "noise is stored as it is");
    assert!(first.stored_bytes < (noise.len() + text.len() / 10) as u64, "stored {} bytes", first.stored_bytes);
    // 3 distinct contents (text, noise, empty), and for each of the 3
    // folders (root, copy, copy/of) its listing and its list of times.
    assert_eq!(first.new_objects, 9);

    // A copy made later costs a listing and a list of times for each folder on its path, and no content.
    lab.put("copy/third.txt", text.as_bytes());
    let second = snap(&mut store);
    assert_eq!(second.new_objects, 4);
    assert!(second.stored_bytes < 1500, "a duplicate cost {} bytes", second.stored_bytes);

    for f in ["a.txt", "copy/of/a.txt", "copy/third.txt", "noise.bin", "empty"] {
        fs::remove_file(lab.at(f)).unwrap();
    }
    let back = store.restore(&second.tree, true).unwrap();
    assert!(back["complete"] == true && back["verified"] == true && back["reached_target"] == true, "{back}");
    assert_eq!(lab.read("copy/third.txt").unwrap(), text.as_bytes());
    assert_eq!(lab.read("noise.bin").unwrap(), noise);
    assert_eq!(lab.read("empty").unwrap(), b"");
}

#[test]
fn each_snapshot_records_when_files_were_written_and_restore_writes_now() {
    let lab = Lab::new("times");
    lab.put("a.txt", b"a");
    lab.put("dir/b.txt", b"b");
    let (mut store, first) = lab.track();
    let on_disk = |rel: &str| fs::metadata(lab.at(rel)).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
    let w = store.written(&first.tree);
    assert_eq!(w.keys().map(String::as_str).collect::<Vec<_>>(), vec!["a.txt", "dir/b.txt"]);
    assert_eq!((w["a.txt"], w["dir/b.txt"]), (on_disk("a.txt"), on_disk("dir/b.txt")));

    // Touched: the same content with another time is the same tree. Nothing
    // is recorded unless asked for with a label, and then the new time is.
    let ahead = SystemTime::now() + Duration::from_secs(3600);
    fs::File::options().write(true).open(lab.at("a.txt")).unwrap().set_modified(ahead).unwrap();
    let quiet = snap(&mut store);
    assert!(quiet.seq.is_none() && quiet.tree == first.tree);
    {
        let _guard = store.begin().unwrap();
        let labelled = store.snapshot(&SnapOpts { label: Some("touched"), reason: "test", record: true, rehash: false }).unwrap();
        assert!(labelled.seq.is_some() && labelled.tree == first.tree);
    }
    assert_eq!(store.written(&first.tree)["a.txt"], on_disk("a.txt"));
    assert!(store.written(&first.tree)["a.txt"] > w["a.txt"] + 3_000_000);

    // A restored file is written now: its time is not put back, so a build
    // tool that compares times sees it as changed.
    lab.put("a.txt", b"edited");
    snap(&mut store);
    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == true && r["reached_target"] == true, "{r}");
    let now = unix_ms();
    assert!(on_disk("a.txt") <= now + 1000 && on_disk("a.txt") + 60_000 > now, "restored a.txt carries {} at {now}", on_disk("a.txt"));
    assert!(store.written(&sha(b"no such tree")).is_empty());
}

#[test]
fn long_paths_round_trip_and_a_dry_run_writes_nothing() {
    let lab = Lab::new("long");
    // Deeper than the 260 characters most Windows programs stop at.
    let deep = (0..9).map(|i| format!("a-folder-with-a-fairly-long-name-number-{i:02}")).collect::<Vec<_>>().join("/");
    let rel = format!("{deep}/the file at the bottom.txt");
    assert!(lab.at(&rel).as_os_str().len() > 400);
    lab.put(&rel, b"deep down");
    lab.put("top.txt", b"top");
    let (mut store, first) = lab.track();
    assert!(first.failures.is_empty(), "{:?}", first.failures);
    assert_eq!(first.manifest[&rel].hash, sha(b"deep down"));
    assert_eq!(first.tree, independent(&lab.root()));

    fs::remove_dir_all(lab.at("a-folder-with-a-fairly-long-name-number-00")).unwrap();
    lab.put("top.txt", b"changed");
    let gone = snap(&mut store);
    assert!(!gone.manifest.contains_key(&rel));

    // The dry run: the same plan, and not one byte written to the store or the tree.
    let size = |d: &Path| -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = fs::read_dir(d).unwrap().map(|e| e.unwrap()).map(|e| (e.file_name().into_string().unwrap(), e.metadata().unwrap().len())).collect();
        v.sort();
        v
    };
    let (store_before, tree_before) = (size(&store.dir), picture(&lab.root()));
    let dry = store.restore(&first.tree, false).unwrap();
    assert_eq!((dry["applied"].as_bool(), dry["plan"]["write"].as_u64(), dry["plan"]["make_folders"].as_u64()), (Some(false), Some(2), Some(9)));
    assert_eq!((size(&store.dir), picture(&lab.root())), (store_before, tree_before), "a dry run changed something");

    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == true && r["verified"] == true && r["reached_target"] == true, "{r}");
    assert_eq!(lab.read(&rel).unwrap(), b"deep down");
    assert_eq!(independent(&lab.root()), first.tree);
}

#[test]
fn a_large_file_is_streamed_and_comes_back_exactly() {
    let lab = Lab::new("large");
    let noise = Rng(42).bytes((SMALL + 123_457) as usize);
    let text: Vec<u8> = "0123456789 the same line again and again\n".bytes().cycle().take((SMALL + 99) as usize).collect();
    lab.put("noise.bin", &noise);
    lab.put("text.log", &text);
    lab.put("twin.bin", &noise);
    let (mut store, first) = lab.track();
    assert!(first.failures.is_empty(), "{:?}", first.failures);
    let (n, t) = (store.objects.map[&sha(&noise)], store.objects.map[&sha(&text)]);
    assert_eq!((n.raw, n.flags), (noise.len() as u64, 0));
    assert!(t.flags & F_DEFLATE != 0 && t.stored * 20 < t.raw);
    // More than one pack: under test a pack is closed at 256 KB, so that
    // reading and repairing across packs is exercised by ordinary tests.
    assert!(store.objects.packs().len() >= 2, "{:?}", store.objects.packs());
    // The twin was copied in, found to be known, and cut back out.
    let (_, stored) = store.objects.stats();
    assert!(stored < noise.len() as u64 + t.stored + 4096, "pack holds {stored} bytes");
    assert_eq!(first.tree, independent(&lab.root()));

    fs::write(lab.at("noise.bin"), b"gone").unwrap();
    fs::remove_file(lab.at("text.log")).unwrap();
    let back = store.restore(&first.tree, true).unwrap();
    assert!(back["complete"] == true && back["verified"] == true, "{back}");
    assert_eq!(sha(&lab.read("noise.bin").unwrap()), sha(&noise));
    assert_eq!(sha(&lab.read("text.log").unwrap()), sha(&text));
}

/// One random change to the tree. Returns what it did, for failure messages.
fn mutate(rng: &mut Rng, root: &Path) -> String {
    const FILES: [&str; 9] = ["a.txt", "b.rs", "notes.md", "data.bin", "with space.txt", "Über.txt", "Makefile", ".hidden", "x"];
    const DIRS: [&str; 6] = ["src", "lib", "docs", "deep", "two words", ".config"];
    let pic = picture(root);
    let files: Vec<&String> = pic.iter().filter(|(_, v)| v.1.is_some()).map(|(k, _)| k).collect();
    let mut dirs: Vec<String> = vec![String::new()];
    dirs.extend(pic.iter().filter(|(_, v)| v.1.is_none()).map(|(k, _)| k.clone()));
    let join = |dir: &str, name: &str| if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") };
    // Windows treats names that differ only in case as one entry.
    let folded: HashMap<String, bool> = pic.iter().map(|(k, v)| (k.to_lowercase(), v.1.is_none())).collect();
    let is_folder = |rel: &str| folded.get(&rel.to_lowercase()).copied() == Some(true);
    let taken = |rel: &str| folded.contains_key(&rel.to_lowercase());
    let content = |rng: &mut Rng| -> Vec<u8> {
        match rng.below(10) {
            0 => Vec::new(),
            1 if !files.is_empty() => pic[files[rng.below(files.len())]].1.clone().unwrap(),
            2 => format!("line {}\n", rng.next() % 7).repeat(rng.below(400)).into_bytes(),
            3 => {
                let n = 20_000 + rng.below(60_000);
                rng.bytes(n)
            }
            _ => {
                let n = rng.below(1500);
                rng.bytes(n)
            }
        }
    };
    let free = |rel: &str| {
        let _ = set_attrs(&join_rel(root, rel), 0);
    };
    let roll = rng.below(100);
    match roll {
        0..=27 => {
            let rel = join(&dirs[rng.below(dirs.len())], FILES[rng.below(FILES.len())]);
            if is_folder(&rel) {
                return format!("skip: {rel} is a folder");
            }
            free(&rel);
            fs::write(join_rel(root, &rel), content(rng)).unwrap();
            format!("write {rel}")
        }
        28..=42 if !files.is_empty() => {
            let rel = files[rng.below(files.len())].clone();
            let mut bytes = pic[&rel].1.clone().unwrap();
            match rng.below(3) {
                0 => {
                    let n = 1 + rng.below(300);
                    bytes.extend_from_slice(&rng.bytes(n));
                }
                1 => bytes.truncate(bytes.len() / 2),
                // The same length with one byte different: only the content says so.
                _ if !bytes.is_empty() => {
                    let i = rng.below(bytes.len());
                    bytes[i] ^= 0x55;
                }
                _ => bytes.push(b'!'),
            }
            free(&rel);
            fs::write(join_rel(root, &rel), bytes).unwrap();
            format!("modify {rel}")
        }
        43..=52 if !files.is_empty() => {
            let rel = files[rng.below(files.len())].clone();
            free(&rel);
            fs::remove_file(join_rel(root, &rel)).unwrap();
            format!("delete {rel}")
        }
        53..=62 if !files.is_empty() => {
            let from = files[rng.below(files.len())].clone();
            let to = join(&dirs[rng.below(dirs.len())], FILES[rng.below(FILES.len())]);
            if is_folder(&to) || from.to_lowercase() == to.to_lowercase() {
                return format!("skip: rename {from} -> {to}");
            }
            free(&from);
            free(&to);
            fs::rename(join_rel(root, &from), join_rel(root, &to)).unwrap();
            format!("rename {from} -> {to}")
        }
        63..=74 => {
            let rel = join(&dirs[rng.below(dirs.len())], DIRS[rng.below(DIRS.len())]);
            if taken(&rel) || rel.matches('/').count() > 3 {
                return format!("skip: mkdir {rel}");
            }
            fs::create_dir(join_rel(root, &rel)).unwrap();
            format!("mkdir {rel}")
        }
        75..=80 if dirs.len() > 1 => {
            let rel = dirs[1 + rng.below(dirs.len() - 1)].clone();
            free(&rel);
            free_all(&join_rel(root, &rel));
            fs::remove_dir_all(join_rel(root, &rel)).unwrap();
            format!("remove folder {rel}")
        }
        81..=86 if dirs.len() > 1 => {
            let from = dirs[1 + rng.below(dirs.len() - 1)].clone();
            let to = join(&dirs[rng.below(dirs.len())], DIRS[rng.below(DIRS.len())]);
            if taken(&to) || to.starts_with(&format!("{from}/")) || to.matches('/').count() > 3 {
                return format!("skip: move folder {from} -> {to}");
            }
            free(&from);
            fs::rename(join_rel(root, &from), join_rel(root, &to)).unwrap();
            format!("move folder {from} -> {to}")
        }
        87..=92 if !files.is_empty() => {
            let rel = files[rng.below(files.len())].clone();
            let bit = [A_READONLY, A_HIDDEN, A_READONLY | A_HIDDEN, 0][rng.below(4)];
            set_attrs(&join_rel(root, &rel), bit).unwrap();
            format!("attributes {bit} on {rel}")
        }
        93..=95 if dirs.len() > 1 => {
            let rel = dirs[1 + rng.below(dirs.len() - 1)].clone();
            let bit = [A_HIDDEN, A_READONLY, 0][rng.below(3)];
            set_attrs(&join_rel(root, &rel), bit).unwrap();
            format!("attributes {bit} on folder {rel}")
        }
        96..=97 if !files.is_empty() => {
            // The same name in other letters' case: one entry to Windows, two paths to a tree.
            let from = files[rng.below(files.len())].clone();
            let (dir, name) = (parent_rel(&from), from.rsplit('/').next().unwrap());
            let other = if name.chars().any(|c| c.is_ascii_lowercase()) { name.to_ascii_uppercase() } else { name.to_ascii_lowercase() };
            if other == name {
                return format!("skip: {name} has no case");
            }
            free(&from);
            fs::rename(join_rel(root, &from), join_rel(root, &join(dir, &other))).unwrap();
            format!("recase {from} -> {other}")
        }
        98 if !files.is_empty() => {
            let rel = files[rng.below(files.len())].clone();
            free(&rel);
            fs::remove_file(join_rel(root, &rel)).unwrap();
            fs::create_dir(join_rel(root, &rel)).unwrap();
            fs::write(join_rel(root, &rel).join("inside.txt"), b"was a file").unwrap();
            format!("file {rel} becomes a folder")
        }
        99 if dirs.len() > 1 => {
            let rel = dirs[1 + rng.below(dirs.len() - 1)].clone();
            free(&rel);
            free_all(&join_rel(root, &rel));
            fs::remove_dir_all(join_rel(root, &rel)).unwrap();
            fs::write(join_rel(root, &rel), b"was a folder").unwrap();
            format!("folder {rel} becomes a file")
        }
        _ => format!("skip: nothing to do for {roll}"),
    }
}

#[test]
fn any_recorded_tree_can_be_brought_back_exactly() {
    // Random creates, edits, deletes, renames, folders made, moved and
    // removed, attributes and type changes; a snapshot after each. Then
    // every recorded tree is restored, in a shuffled order, and what is on
    // disk is compared three ways: with the tree id the store recomputes,
    // with a tree id computed by separate code, and byte for byte with a
    // copy of the folder kept in memory.
    const STEPS: usize = 240;
    let seed = 0x9E37_79B9_7F4A_7C15u64;
    let lab = Lab::new("property");
    let mut rng = Rng(seed);
    lab.put("a.txt", b"first");
    let (mut store, first) = lab.track();
    let mut history: Vec<(Hash, Picture, String)> = vec![(first.tree, picture(&lab.root()), "start".into())];
    let mut distinct = HashSet::new();
    for step in 0..STEPS {
        let did = mutate(&mut rng, &lab.root());
        let s = snap(&mut store);
        assert!(s.failures.is_empty(), "step {step} ({did}): {:?}", s.failures);
        assert_eq!(s.tree, independent(&lab.root()), "step {step} ({did}): the tree id does not match the folder (seed {seed:#x})");
        distinct.insert(s.tree);
        history.push((s.tree, picture(&lab.root()), did));
    }
    assert!(distinct.len() > STEPS / 2, "only {} distinct trees in {STEPS} steps: the changes are not exercising much", distinct.len());

    let mut order: Vec<usize> = (0..history.len()).collect();
    for i in (1..order.len()).rev() {
        order.swap(i, rng.below(i + 1));
    }
    let mut recorded = 0;
    for (n, &i) in order.iter().enumerate() {
        let (tree, want, did) = &history[i];
        let r = store.restore(tree, true).unwrap_or_else(|e| panic!("restore {n} to step {i} ({did}): {e}"));
        recorded += r["step"].is_u64() as usize;
        assert!(
            r["complete"] == true && r["verified"] == true && r["reached_target"] == true,
            "restore {n} to step {i} ({did}), seed {seed:#x}: {}",
            serde_json::to_string_pretty(&r).unwrap()
        );
        assert_eq!(independent(&lab.root()), *tree, "restore {n} to step {i} ({did}): the folder does not hash to the tree");
        let got = picture(&lab.root());
        if &got != want {
            let differ: Vec<&String> = got.keys().chain(want.keys()).filter(|k| got.get(*k) != want.get(*k)).collect();
            panic!("restore {n} to step {i} ({did}), seed {seed:#x}: these differ from what was there: {differ:?}");
        }
    }
    // Each restore that changed anything was itself a step and can be listed.
    assert_eq!(store.log.iter().filter(|r| r["t"] == "step" && r["kind"] == "restore").count(), recorded);
    assert!(recorded > order.len() / 2, "only {recorded} of {} restores changed anything", order.len());
    assert!(store.objects.packs().len() > 1, "the history was meant to span several packs: {:?}", store.objects.packs());
}

fn junction(link: &Path, target: &Path) {
    let out = std::process::Command::new("cmd").args(["/c", "mklink", "/J"]).arg(link).arg(target).output().unwrap();
    assert!(out.status.success(), "mklink /J failed: {}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn a_junction_is_reported_and_never_followed() {
    let lab = Lab::new("junction");
    let outside = lab.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"outside the root").unwrap();
    lab.put("real/a.txt", b"inside");
    lab.put("real/sub/b.txt", b"inside too");
    let (mut store, first) = lab.track();

    // 1. A junction in the tree is listed as a link; nothing behind it is read.
    junction(&lab.at("link"), &outside);
    assert!(lab.at("link/secret.txt").exists(), "the junction itself works");
    let with_link = snap(&mut store);
    assert_eq!(with_link.links, vec!["link".to_string()]);
    assert!(with_link.manifest.keys().all(|k| !k.starts_with("link")), "{:?}", with_link.manifest.keys());
    assert!(!store.objects.has(&sha(b"outside the root")), "content from behind the junction reached the store");
    assert_eq!(with_link.tree, first.tree, "a link is not part of a tree");

    // 2. A folder of the tree is replaced by a junction pointing outside.
    //    Restoring the tree must not write, delete or make anything through it.
    fs::remove_dir_all(lab.at("real")).unwrap();
    junction(&lab.at("real"), &outside);
    let dry = store.restore(&first.tree, false).unwrap();
    assert_eq!(dry["applied"], false);
    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == false, "{r}");
    // Refused in the plan itself (the dry run above said the same), not found out half-way.
    assert_eq!(refused(&r), ["real", "real/a.txt", "real/sub", "real/sub/b.txt"], "{r}");
    assert_eq!(refused(&dry), refused(&r), "{dry}");
    let seen: Vec<String> = fs::read_dir(&outside).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
    assert_eq!(seen, vec!["secret.txt".to_string()], "something was written through the junction");
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"outside the root");

    // 3. A tree that has no such folder: the junction is not removed and nothing behind it is deleted.
    fs::remove_dir(lab.at("real")).unwrap();
    fs::remove_dir(lab.at("link")).unwrap();
    let empty = snap(&mut store);
    junction(&lab.at("real"), &outside);
    let r = store.restore(&empty.tree, true).unwrap();
    assert!(r["complete"] == true, "{r}");
    assert!(lab.at("real").exists() && outside.join("secret.txt").exists());

    // 4. Straight at the layer below: a plan that names paths under the junction.
    let blob = Entry { kind: Kind::File, attrs: 0, size: 6, hash: sha(b"inside"), stamp: Stamp::default() };
    let mut p = Plan::default();
    p.ops = vec![Op::Write("real/new.txt".into(), blob), Op::Delete("real/secret.txt".into()), Op::MkDir("real/made".into(), 0), Op::RmDir("real/sub".into())];
    let mut cur = Manifest::new();
    let there = stat_path(&outside.join("secret.txt")).unwrap();
    cur.insert("real/secret.txt".into(), Entry { kind: Kind::File, attrs: 0, size: there.size, hash: sha(b"outside the root"), stamp: there.stamp });
    store.load().unwrap();
    let done = store.apply(&p, &cur);
    assert_eq!((done.written, done.deleted, done.folders_made, done.folders_removed), (0, 0, 0, 0));
    assert_eq!(done.blocked.len(), 4, "{:?}", done.blocked);
    assert!(done.blocked.iter().all(|b| b.what.contains("is a link")), "{:?}", done.blocked);
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    fs::remove_dir(lab.at("real")).unwrap();
}

/// A symbolic link. None when this account may not make them (that needs
/// Developer Mode or an elevated account).
fn symlink(link: &Path, target: &Path, to_folder: bool) -> Option<()> {
    let mut c = std::process::Command::new("cmd");
    c.args(["/c", "mklink"]);
    if to_folder {
        c.arg("/D");
    }
    c.arg(link).arg(target).output().unwrap().status.success().then_some(())
}

#[test]
fn symbolic_links_are_reported_and_never_written_through() {
    let lab = Lab::new("symlink");
    let outside = lab.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"outside the root").unwrap();
    lab.put("plain.txt", b"plain");
    if symlink(&lab.at("probe"), &outside.join("secret.txt"), false).is_none() {
        eprintln!("SKIPPED symbolic_links_are_reported_and_never_written_through: this account cannot create symbolic links");
        return;
    }
    fs::remove_file(lab.at("probe")).unwrap();
    let (mut store, first) = lab.track();
    let link_all = || {
        symlink(&lab.at("to-file.txt"), &outside.join("secret.txt"), false).unwrap();
        symlink(&lab.at("dangling.txt"), &outside.join("not-there.txt"), false).unwrap();
        symlink(&lab.at("to-dir"), &outside, true).unwrap();
    };
    let unlink_all = || {
        fs::remove_file(lab.at("to-file.txt")).unwrap();
        fs::remove_file(lab.at("dangling.txt")).unwrap();
        fs::remove_dir(lab.at("to-dir")).unwrap();
    };
    let untouched = |when: &str| {
        let seen: Vec<String> = fs::read_dir(&outside).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        assert_eq!(seen, vec!["secret.txt".to_string()], "{when}: something was made behind a link");
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"outside the root", "{when}");
        assert_eq!(fs::metadata(outside.join("secret.txt")).unwrap().file_attributes() & KEEP_ATTRS, 0, "{when}");
    };

    // 1. Links to a file, to a folder and to nothing: listed, not read, not part of the tree.
    link_all();
    let s = snap(&mut store);
    assert_eq!(s.links, vec!["dangling.txt".to_string(), "to-dir".into(), "to-file.txt".into()]);
    assert!(s.failures.is_empty(), "{:?}", s.failures);
    assert_eq!(s.tree, first.tree);
    assert!(!store.objects.has(&sha(b"outside the root")));

    // 2. A tree with real files at those three paths. With the links back in
    //    place, restoring it must write nothing through any of them: not
    //    into the linked file, not into the linked folder, and not into
    //    existence behind the link that points at nothing.
    unlink_all();
    lab.put("to-file.txt", b"real content");
    lab.put("dangling.txt", b"more real content");
    lab.put("to-dir/inner.txt", b"real, in a real folder");
    let real = snap(&mut store);
    for f in ["to-file.txt", "dangling.txt", "to-dir/inner.txt"] {
        fs::remove_file(lab.at(f)).unwrap();
    }
    fs::remove_dir(lab.at("to-dir")).unwrap();
    link_all();
    let r = store.restore(&real.tree, true).unwrap();
    assert!(r["complete"] == false, "{r}");
    assert_eq!(refused(&r), ["dangling.txt", "to-dir", "to-dir/inner.txt", "to-file.txt"], "{r}");
    assert_eq!(r["result"]["written"], 0);
    untouched("restore over links");
    assert!(fs::symlink_metadata(lab.at("dangling.txt")).unwrap().file_type().is_symlink(), "the link itself was replaced");

    // 3. The swap happens after the snapshot: the plan still believes the
    //    path holds the file it captured (same size, same stamps as the
    //    file the link points at). It is a link now, and that is enough.
    let there = stat_path(&outside.join("secret.txt")).unwrap();
    let mut cur = Manifest::new();
    cur.insert("to-file.txt".into(), Entry { kind: Kind::File, attrs: 0, size: there.size, hash: sha(b"outside the root"), stamp: there.stamp });
    let blob = Entry { kind: Kind::File, attrs: A_READONLY, size: 5, hash: sha(b"plain"), stamp: Stamp::default() };
    let p = Plan { ops: vec![Op::Write("to-file.txt".into(), blob), Op::Delete("to-file.txt".into()), Op::Attrs("to-file.txt".into(), A_READONLY)], ..Plan::default() };
    let done = store.apply(&p, &cur);
    assert_eq!((done.written, done.deleted, done.attributes), (0, 0, 0));
    assert_eq!(done.blocked.len() + done.failed.len(), 3, "{:?} {:?}", done.blocked, done.failed);
    untouched("a plan that thinks the link is a file");
    assert!(lab.at("to-file.txt").exists());
    unlink_all();
}

#[test]
fn a_hand_made_tree_cannot_reach_outside_the_root() {
    let lab = Lab::new("crafted");
    lab.put("ok.txt", b"fine");
    fs::write(lab.base.join("victim.txt"), b"do not touch").unwrap();
    let (mut store, _) = lab.track();
    let around = || -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&lab.base).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    };
    let before = (around(), picture(&lab.root()));

    // 1. Listings written into the store by hand, each naming something a
    //    path must never contain. They hash correctly; they are still refused.
    let payload = {
        let _guard = store.begin().unwrap();
        let h = store.objects.put_bytes(b"pwned").unwrap();
        store.objects.commit().unwrap();
        h
    };
    let bad_names = [
        "..", "../victim.txt", "..\\victim.txt", "C:", "C:evil.txt", "ok.txt:stream", "NUL", "nul.txt", "COM1", "trail.", "trail ", "a\\b", "\\\\server\\share\\x", "/abs",
        "", ".",
    ];
    for name in bad_names {
        for line in [format!("f 0 5 {} {name}\n", hex(&payload)), format!("d 0 {} {name}\n", hex(&sha(TREE_HEAD.as_bytes()))), format!("x 0 5 {name}\n")] {
            let id = {
                let _guard = store.begin().unwrap();
                let empty = store.objects.put_bytes(TREE_HEAD.as_bytes()).unwrap();
                assert_eq!(empty, sha(TREE_HEAD.as_bytes()));
                let id = store.objects.put_bytes(format!("{TREE_HEAD}{line}").as_bytes()).unwrap();
                store.objects.commit().unwrap();
                id
            };
            assert!(store.tree(&id).is_err(), "a listing naming {name:?} was accepted");
            for apply in [false, true] {
                let r = store.restore(&id, apply);
                assert!(r.is_err(), "restore (apply {apply}) of a listing naming {name:?} went ahead: {:?}", r);
            }
        }
    }
    // Nested: a good root listing that points at a bad one.
    let inner = format!("{TREE_HEAD}f 0 5 {} ..\n", hex(&payload));
    let id = {
        let _guard = store.begin().unwrap();
        let inner_id = store.objects.put_bytes(inner.as_bytes()).unwrap();
        let id = store.objects.put_bytes(format!("{TREE_HEAD}d 0 {} sub\n", hex(&inner_id)).as_bytes()).unwrap();
        store.objects.commit().unwrap();
        id
    };
    assert!(store.restore(&id, true).is_err());
    // Out of order, twice the same name, attributes that are not ours, junk.
    for body in [
        format!("f 0 5 {h} b\nf 0 5 {h} a\n", h = hex(&payload)),
        format!("f 0 5 {h} a\nf 0 5 {h} a\n", h = hex(&payload)),
        format!("f 410 5 {} a\n", hex(&payload)),
        "f 0 5 nothex a\n".to_string(),
        "q 0 5 a\n".to_string(),
        "f 0\n".to_string(),
    ] {
        let id = {
            let _guard = store.begin().unwrap();
            let id = store.objects.put_bytes(format!("{TREE_HEAD}{body}").as_bytes()).unwrap();
            store.objects.commit().unwrap();
            id
        };
        assert!(store.tree(&id).is_err(), "accepted: {body:?}");
    }
    assert!(store.tree(&payload).is_err(), "a file's content was read as a listing");

    // 2. Below the listings: plans made by hand, given straight to the code that touches the disk.
    let blob = Entry { kind: Kind::File, attrs: 0, size: 5, hash: payload, stamp: Stamp::default() };
    let victim = stat_path(&lab.base.join("victim.txt")).unwrap();
    let mut cur = Manifest::new();
    for path in ["../victim.txt", "..\\victim.txt", "sub/../../victim.txt"] {
        cur.insert(path.into(), Entry { kind: Kind::File, attrs: 0, size: victim.size, hash: sha(b"do not touch"), stamp: victim.stamp });
    }
    let abs_victim = show(&canon(&lab.base.join("victim.txt")).unwrap()).replace('\\', "/");
    cur.insert(abs_victim.clone(), Entry { kind: Kind::File, attrs: 0, size: victim.size, hash: sha(b"do not touch"), stamp: victim.stamp });
    let mut p = Plan::default();
    for path in ["../escape.txt", "..\\escape.txt", "sub/../../escape.txt", "C:/proteus-escape.txt", "ok.txt:stream", "NUL", "../victim.txt", abs_victim.as_str()] {
        p.ops.push(Op::Write(path.into(), blob.clone()));
        p.ops.push(Op::Delete(path.into()));
        p.ops.push(Op::MkDir(path.into(), 0));
        p.ops.push(Op::RmDir(path.into()));
        p.ops.push(Op::Attrs(path.into(), A_READONLY));
    }
    store.load().unwrap();
    let done = store.apply(&p, &cur);
    assert_eq!((done.written, done.deleted, done.folders_made, done.folders_removed, done.attributes), (0, 0, 0, 0, 0));
    assert_eq!(done.blocked.len() + done.failed.len(), p.ops.len());
    // And the planner refuses them before that.
    let mut want = BTreeMap::new();
    want.insert("../escape.txt".to_string(), Some(blob.clone()));
    want.insert("../victim.txt".to_string(), None);
    let planned = plan(&cur, &want);
    assert!(planned.ops.is_empty() && planned.blocked.len() == 2, "{:?}", planned.ops);

    assert_eq!(fs::read(lab.base.join("victim.txt")).unwrap(), b"do not touch");
    assert_eq!(fs::metadata(lab.base.join("victim.txt")).unwrap().file_attributes() & A_READONLY, 0);
    assert_eq!((around(), picture(&lab.root())), before, "something outside the plan's reach changed");
    // Where each escape would have landed had a check failed: beside the
    // root, and (for the drive-letter path, which Windows reads as "the
    // current folder of drive C") in this process's own current folder.
    assert!(!Path::new("C:/proteus-escape.txt").exists());
    assert!(!std::env::current_dir().unwrap().join("proteus-escape.txt").exists());
    assert!(!lab.base.join("escape.txt").exists());
}

#[test]
fn restore_never_destroys_what_the_store_does_not_hold() {
    let lab = Lab::new("uncaptured");
    lab.put(".gitignore", b"*.secret\n");
    lab.put("f.txt", b"one");
    lab.put("g.txt", b"g");
    let (mut store, t1) = lab.track();
    lab.put("f.txt", b"two");
    fs::remove_file(lab.at("g.txt")).unwrap();
    let t2 = snap(&mut store);

    // The file changes after the snapshot that was meant to protect it.
    lab.put("f.txt", b"three, never captured");
    let p = store.plan_restore(&t2.manifest, &store.tree(&t1.tree).unwrap());
    let done = store.apply(&p, &t2.manifest);
    assert_eq!(done.blocked.iter().map(|b| b.path.as_str()).collect::<Vec<_>>(), vec!["f.txt"]);
    assert_eq!(lab.read("f.txt").unwrap(), b"three, never captured");
    assert_eq!(lab.read("g.txt").unwrap(), b"g", "the rest of the plan still ran");
    // The same for a delete.
    let p = Plan { ops: vec![Op::Delete("f.txt".into())], ..Plan::default() };
    assert_eq!(store.apply(&p, &t2.manifest).blocked.len(), 1);
    assert!(lab.at("f.txt").exists());

    // Something the tree cannot see (ignored) sits where a file should be written.
    let now = snap(&mut store);
    let mut want = BTreeMap::new();
    want.insert("key.secret".to_string(), Some(now.manifest["g.txt"].clone()));
    lab.put("key.secret", b"an ignored file");
    let again = snap(&mut store);
    assert!(!again.manifest.contains_key("key.secret"));
    let done = store.apply(&plan(&again.manifest, &want), &again.manifest);
    assert_eq!(done.blocked.len(), 1, "{:?}", done.failed);
    assert_eq!(lab.read("key.secret").unwrap(), b"an ignored file");

    // A folder that should go but holds an ignored file is kept, with the file.
    lab.put("build/out.txt", b"tracked");
    let with_build = snap(&mut store);
    lab.put("build/cache.secret", b"ignored, inside");
    let r = store.restore(&again.tree, true).unwrap();
    assert_eq!(r["plan"]["kept"][0]["path"], "build", "{r}");
    assert!(r["complete"] == false && r["reached_target"] == false, "a folder that stays means the tree asked for was not reached: {r}");
    assert!(!lab.at("build/out.txt").exists());
    assert_eq!(lab.read("build/cache.secret").unwrap(), b"ignored, inside");
    assert_ne!(with_build.tree, again.tree);
    // No temporary file is left behind by any of it.
    assert!(picture(&lab.root()).keys().all(|k| !k.contains(TMP_PREFIX)));
}

#[test]
fn a_file_another_program_is_writing_is_never_replaced() {
    let lab = Lab::new("writer");
    lab.put("db.bin", b"AAAAAAAAAAAAAAAA");
    lab.put("other.txt", b"one");
    let (mut store, first) = lab.track();
    lab.put("db.bin", b"BBBBBBBBBBBBBBBB");
    lab.put("other.txt", b"two");
    // Long enough ago that the cache trusts what it knows about these files.
    std::thread::sleep(Duration::from_millis(2300));
    let second = snap(&mut store);
    assert_eq!(snap(&mut store).read_files, 0);

    // A program opens the file for writing, lets everyone else do anything,
    // and overwrites it in place: the same size, and (this is how Windows
    // works) no new size or time in the folder's listing until it closes.
    let mut writer = fs::File::options().read(true).write(true).share_mode(7).open(lab.at("db.bin")).unwrap();
    writer.write_all(b"CCCCCCCCCCCCCCCC").unwrap();
    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == false, "{r}");
    assert_eq!(r["plan"]["blocked"][0]["path"], "db.bin");
    assert!(r["plan"]["blocked"][0]["what"].as_str().unwrap().contains("open for writing"), "{r}");
    assert_eq!(lab.read("other.txt").unwrap(), b"one", "the rest of the restore went ahead");
    drop(writer);
    assert_eq!(lab.read("db.bin").unwrap(), b"CCCCCCCCCCCCCCCC", "what the other program wrote was destroyed");

    // The writer has gone. Its bytes are captured by the snapshot taken first, and only then replaced.
    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == true && r["reached_target"] == true, "{r}");
    assert_eq!(lab.read("db.bin").unwrap(), b"AAAAAAAAAAAAAAAA");
    let kept = store.tree(&unhex(r["before"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(kept["db.bin"].hash, sha(b"CCCCCCCCCCCCCCCC"));

    // A snapshot that is wrong about a file (right size, right times, other
    // content) still cannot make restore destroy it: the file is read at
    // the last moment, and what is read decides.
    let now = snap(&mut store);
    let mut fooled = now.manifest.clone();
    fooled.get_mut("db.bin").unwrap().hash = sha(b"what the snapshot believes");
    let p = Plan { ops: vec![Op::Delete("db.bin".into()), Op::Write("db.bin".into(), second.manifest["db.bin"].clone())], ..Plan::default() };
    let done = store.apply(&p, &fooled);
    assert_eq!((done.deleted, done.written, done.blocked.len()), (0, 0, 2), "{:?}", done.failed);
    assert_eq!(lab.read("db.bin").unwrap(), b"AAAAAAAAAAAAAAAA");
    assert!(picture(&lab.root()).keys().all(|k| !k.contains(TMP_PREFIX)));
}

#[test]
fn undo_leaves_later_changes_alone_unless_forced() {
    let lab = Lab::new("conflict");
    lab.put("a.txt", b"a0");
    lab.put("b.txt", b"b0");
    let (mut store, _) = lab.track();
    let cmd: Vec<String> = ["cmd", "/c", "echo a1> a.txt& echo b1> b.txt& echo c1> c.txt"].map(String::from).to_vec();
    let step = store.run_step(&cmd, &lab.root(), None).unwrap();
    assert_eq!(step["exit"], 0);
    let run_seq = step["seq"].as_u64().unwrap();
    assert_eq!(lab.read("b.txt").unwrap(), b"b1\r\n");

    // After the step ended, b.txt is edited by hand.
    lab.put("b.txt", b"b2, typed by a person");
    let dry = store.undo(None, false, false).unwrap();
    assert_eq!(dry["applied"], false);
    assert_eq!(dry["plan"]["conflicts"].as_array().unwrap().len(), 1);
    assert_eq!(dry["plan"]["conflicts"][0]["path"], "b.txt");
    assert_eq!((dry["plan"]["write"].as_u64(), dry["plan"]["delete"].as_u64()), (Some(1), Some(1)));
    assert_eq!(lab.read("a.txt").unwrap(), b"a1\r\n", "a dry run changes nothing");
    assert!(lab.at("c.txt").exists());

    let r = store.undo(None, true, false).unwrap();
    assert!(r["complete"] == false && r["verified"] == true, "{r}");
    assert_eq!(r["plan"]["conflicts"][0]["path"], "b.txt");
    assert_eq!(lab.read("a.txt").unwrap(), b"a0");
    assert!(!lab.at("c.txt").exists());
    assert_eq!(lab.read("b.txt").unwrap(), b"b2, typed by a person", "the conflicting file was left alone");
    let undo_seq = r["step"].as_u64().unwrap();
    let kept = unhex(r["before"].as_str().unwrap()).unwrap();

    // Forced: b.txt goes back too, and what the person typed is still in the store.
    let r = store.undo(Some(run_seq), true, true).unwrap();
    assert!(r["complete"] == true && r["verified"] == true && r["back_to_before"] == true, "{r}");
    assert_eq!(lab.read("b.txt").unwrap(), b"b0");
    assert_eq!(store.tree(&kept).unwrap()["b.txt"].hash, sha(b"b2, typed by a person"));
    let r = store.restore(&kept, true).unwrap();
    assert!(r["complete"] == true, "{r}");
    assert_eq!(lab.read("b.txt").unwrap(), b"b2, typed by a person");
    assert!(undo_seq > run_seq);

    // A step number that is not a step, and a store with no steps.
    assert!(store.undo(Some(1), false, false).is_err());
    let other = Lab::new("conflict-none");
    other.put("x", b"x");
    assert!(other.track().0.undo(None, false, false).is_err_and(|e| e.contains("no step")));
}

#[test]
fn run_then_undo_then_undo_again() {
    let lab = Lab::new("run");
    lab.put("keep.txt", b"kept");
    lab.put("edit.txt", b"old");
    lab.put("victim.txt", b"doomed");
    lab.put("dir/gone.txt", b"in a folder that will be removed");
    let (mut store, _) = lab.track();
    let before = picture(&lab.root());

    let line = "echo new> created.txt& echo changed> edit.txt& del victim.txt& mkdir made& echo x> made\\inner.txt& rmdir /s /q dir";
    let cmd: Vec<String> = ["cmd", "/c", line].map(String::from).to_vec();
    let step = store.run_step(&cmd, &lab.root(), Some("the test command")).unwrap();
    let after = picture(&lab.root());
    assert_ne!(before, after);
    assert_eq!((step["kind"].as_str(), step["exit"].as_i64()), (Some("run"), Some(0)));
    assert_eq!(step["detail"]["cmd"][2], line);
    assert!(step["ms"].as_f64().unwrap() >= 0.0 && step["before"] != step["after"]);
    assert_eq!((step["added"].as_u64(), step["removed"].as_u64(), step["modified"].as_u64()), (Some(3), Some(3), Some(1)));
    // The step is in the log on disk with its command, exit code and both trees.
    let logged = log_read(&store.dir).into_iter().rev().find(|r| r["t"] == "step").unwrap();
    assert_eq!((&logged["exit"], &logged["before"], &logged["after"], &logged["label"]), (&step["exit"], &step["before"], &step["after"], &json!("the test command")));

    let dry = store.undo(None, false, false).unwrap();
    assert_eq!(picture(&lab.root()), after, "a dry run changes nothing");
    assert_eq!((dry["plan"]["write"].as_u64(), dry["plan"]["delete"].as_u64()), (Some(3), Some(2)));

    let undone = store.undo(None, true, false).unwrap();
    assert!(undone["complete"] == true && undone["verified"] == true && undone["back_to_before"] == true, "{undone}");
    assert_eq!(picture(&lab.root()), before, "undo did not bring the folder back");
    assert_eq!(independent(&lab.root()), unhex(step["before"].as_str().unwrap()).unwrap());

    // The undo was itself a step: undoing it brings the command's result back.
    let redone = store.undo(None, true, false).unwrap();
    assert!(redone["complete"] == true && redone["back_to_before"] == true, "{redone}");
    assert_eq!(redone["detail"]["of"], "undo");
    assert_eq!(picture(&lab.root()), after);
    assert_eq!(independent(&lab.root()), unhex(step["after"].as_str().unwrap()).unwrap());

    // The exit code is the command's own.
    let seven: Vec<String> = ["cmd", "/c", "exit 7"].map(String::from).to_vec();
    let empty = store.run_step(&seven, &lab.root(), None).unwrap();
    assert_eq!(empty["exit"], 7);
    assert_eq!(empty["before"], empty["after"]);

    // That command changed nothing, so there is nothing in it to undo: undo
    // passes over it to the last step that did change something, and says so.
    let dry = store.undo(None, false, false).unwrap();
    assert_eq!(dry["detail"]["undoes"], redone["step"]);
    assert_eq!(dry["detail"]["passed_over"], json!([empty["seq"]]));
    let back = store.undo(None, true, false).unwrap();
    assert!(back["complete"] == true && back["back_to_before"] == true, "{back}");
    assert_eq!(picture(&lab.root()), before);
    // Named outright, the empty step has nothing to do, and no step is recorded for doing nothing.
    let entries = log_read(&store.dir).len();
    let nothing = store.undo(empty["seq"].as_u64(), true, false).unwrap();
    assert!(nothing["nothing_to_do"] == true && nothing["step"].is_null() && nothing["complete"] == true, "{nothing}");
    assert_eq!(log_read(&store.dir).len(), entries);
    assert_eq!(picture(&lab.root()), before);

    // A script found the way a shell finds it (PATHEXT), and a program that does not exist.
    lab.put("hello.cmd", b"@echo hi> said.txt\r\n");
    let hello = store.run_step(&["hello".to_string()], &lab.root(), None).unwrap();
    assert_eq!(hello["exit"], 0);
    assert_eq!(lab.read("said.txt").unwrap(), b"hi\r\n");
    assert!(store.run_step(&["proteus-no-such-program-4711".to_string()], &lab.root(), None).is_err_and(|e| e.exit.is_none() && e.why.starts_with("could not run")));
}

#[test]
fn a_command_keeps_its_exit_code_when_its_step_cannot_be_recorded() {
    // The command makes more files than the budget allows, so the state
    // after it cannot be recorded. It still ran: its exit code is what the
    // caller must get, with the reason the step is missing.
    let lab = Lab::new("run-over");
    lab.put("a.txt", b"a");
    let (mut store, _) = Store::track(&lab.data(), &lab.root(), Some(3), None, None, true).unwrap();
    let steps = store.log.len();
    let cmd: Vec<String> = ["cmd", "/c", "(for %i in (1 2 3 4 5) do @echo x> made%i.txt) & exit 5"].map(String::from).to_vec();
    let failed = store.run_step(&cmd, &lab.root(), None).unwrap_err();
    assert_eq!(failed.exit, Some(5));
    assert!(failed.why.contains("budget is 3 files"), "{}", failed.why);
    assert!(lab.at("made5.txt").exists(), "the command did run");
    assert_eq!(log_read(&store.dir).len(), steps, "nothing half-recorded");

    // And when the state before cannot be recorded, the command is not run.
    let before = store.run_step(&["cmd".to_string(), "/c".into(), "echo x> never.txt".into()], &lab.root(), None).unwrap_err();
    assert_eq!(before.exit, None);
    assert!(!lab.at("never.txt").exists(), "the command ran although the step could not be recorded");
}

#[test]
fn a_locked_file_is_recorded_as_a_failure_and_nothing_stops() {
    let lab = Lab::new("locked");
    lab.put("free.txt", b"readable");
    lab.put("held.txt", b"locked away");
    let hold = fs::File::options().read(true).write(true).share_mode(0).open(lab.at("held.txt")).unwrap();
    let (mut store, first) = lab.track();
    assert_eq!(first.failures.len(), 1, "{:?}", first.failures);
    assert_eq!(first.failures[0].path, "held.txt");
    assert_eq!(first.manifest["held.txt"].kind, Kind::Unknown);
    assert_eq!(first.manifest["free.txt"].hash, sha(b"readable"));
    assert_eq!(log_read(&store.dir)[0]["failures"], 1);
    drop(hold);

    let second = snap(&mut store);
    assert!(second.failures.is_empty());
    assert_eq!(second.manifest["held.txt"].hash, sha(b"locked away"));
    assert_ne!(second.tree, first.tree);

    // Going back to the tree where it could not be read: the file is not
    // touched (there is nothing to put there) and that is said out loud.
    lab.put("held.txt", b"changed since");
    lab.put("free.txt", b"changed too");
    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == false, "{r}");
    assert_eq!(r["plan"]["blocked"][0]["path"], "held.txt");
    assert_eq!(lab.read("held.txt").unwrap(), b"changed since");
    assert_eq!(lab.read("free.txt").unwrap(), b"readable");

    // A file that cannot be read now is never overwritten: its content could not be saved first.
    let hold = fs::File::options().read(true).write(true).share_mode(0).open(lab.at("held.txt")).unwrap();
    let r = store.restore(&second.tree, true).unwrap();
    assert!(r["complete"] == false, "{r}");
    assert_eq!(r["plan"]["blocked"][0]["path"], "held.txt");
    drop(hold);
    assert_eq!(lab.read("held.txt").unwrap(), b"changed since");
}

#[test]
fn ignored_files_are_neither_captured_nor_deleted() {
    let lab = Lab::new("ignore");
    lab.put(".gitignore", b"ignored/\n*.log\n");
    lab.put(".proteusignore", b"scratch/\n");
    lab.put("src/a.rs", b"fn a() {}");
    lab.put("src/.gitignore", b"*.tmp\n!keep.tmp\n");
    lab.put("src/x.tmp", b"ignored by the nested rule");
    lab.put("src/keep.tmp", b"let back in by the nested rule");
    lab.put("x.tmp", b"captured: the nested rule does not reach here");
    lab.put("ignored/big.bin", b"build output");
    lab.put("debug.log", b"log");
    lab.put("scratch/notes", b"mine");
    lab.put(".env.example", b"A=1");
    lab.put(".github/ci.yml", b"on: push");
    lab.put(".git/config", b"[core]");
    lab.put("sub/.git/HEAD", b"ref: refs/heads/main");
    lab.put("sub/code.rs", b"// in a nested repository");
    let (mut store, first) = lab.track();
    let have: Vec<&str> = first.manifest.keys().map(String::as_str).collect();
    assert_eq!(
        have,
        vec![".env.example", ".github", ".github/ci.yml", ".gitignore", ".proteusignore", "src", "src/.gitignore", "src/a.rs", "src/keep.tmp", "sub", "sub/code.rs", "x.tmp"]
    );

    fs::remove_file(lab.at("src/a.rs")).unwrap();
    lab.put("new.txt", b"made later");
    for f in ["ignored/big.bin", "debug.log", "scratch/notes", ".git/config", "sub/.git/HEAD", "src/x.tmp"] {
        lab.put(f, b"changed after the snapshot");
    }
    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == true && r["reached_target"] == true, "{r}");
    assert_eq!(lab.read("src/a.rs").unwrap(), b"fn a() {}");
    assert!(!lab.at("new.txt").exists());
    for f in ["ignored/big.bin", "debug.log", "scratch/notes", ".git/config", "sub/.git/HEAD", "src/x.tmp"] {
        assert_eq!(lab.read(f).unwrap(), b"changed after the snapshot", "{f} is outside the tree and must be left exactly as it is");
    }
}

#[test]
fn what_git_ignores_is_what_chronicle_ignores() {
    // A real repository made by the real git, and git itself asked which
    // files it does not ignore. The tree must hold exactly those.
    let lab = Lab::new("git-parity");
    let git = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(lab.root()).output();
    match git(&["init", "-q"]) {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("SKIPPED what_git_ignores_is_what_chronicle_ignores: git is not on PATH");
            return;
        }
    }
    lab.put(".gitignore", b"# comments and blank lines\n\n*.log\n!keep.log\n/only-at-top.txt\nbuild/\nBIN/\n**/gen/*.rs\n!**/gen/mod.rs\ndocs/*.tmp\ntemp?.dat\n[ab]-class.txt\nspaced name.txt\n");
    lab.put("sub/.gitignore", b"*.secret\n!public.secret\n/anchored-here.txt\nnested-dir/\n");
    lab.put("sub/deeper/.gitignore", b"!again.log\n*.md\n");
    fs::create_dir_all(lab.at(".git/info")).unwrap();
    fs::write(lab.at(".git/info/exclude"), b"excluded-privately.txt\n").unwrap();
    for f in [
        "a.log", "keep.log", "only-at-top.txt", "sub/only-at-top.txt", "build/out.o", "sub/build/out.o", "bin/tool.exe", "src/gen/a.rs", "src/gen/mod.rs", "gen/b.rs",
        "docs/x.tmp", "docs/deep/x.tmp", "temp1.dat", "temp12.dat", "a-class.txt", "c-class.txt", "spaced name.txt", "sub/x.secret", "sub/public.secret",
        "sub/anchored-here.txt", "sub/deeper/anchored-here.txt", "sub/nested-dir/f.txt", "sub/deeper/again.log", "sub/deeper/other.log", "sub/deeper/notes.md",
        "notes.md", "excluded-privately.txt", "sub/excluded-privately.txt", "plain.txt", "sub/plain.txt", "Keep.LOG", "A.LOG",
    ] {
        lab.put(f, f.as_bytes());
    }
    let out = git(&["ls-files", "--others", "--exclude-standard", "-z"]).unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut by_git: Vec<String> = String::from_utf8(out.stdout).unwrap().split('\0').filter(|s| !s.is_empty()).map(String::from).collect();
    by_git.sort();

    // Not hermetic: the rules as PROTEUS applies them (a repository, its private
    // exclude file, the user's global file), which is what git was just asked.
    let (_, first) = Store::track(&lab.data(), &lab.root(), None, None, None, false).unwrap();
    let by_chronicle: Vec<String> = first.manifest.iter().filter(|(_, e)| e.kind == Kind::File).map(|(p, _)| p.clone()).collect();
    assert_eq!(by_chronicle, by_git, "chronicle and git disagree about what is ignored");
    assert!(by_git.contains(&"keep.log".to_string()) && !by_git.contains(&"a.log".to_string()), "git's own answer is not what this test assumes: {by_git:?}");
}

#[test]
fn the_store_survives_a_lost_index_and_a_torn_pack() {
    let lab = Lab::new("recover");
    lab.put("a.txt", b"alpha");
    lab.put("dir/b.txt", &Rng(9).bytes(5000));
    let (mut store, t1) = lab.track();
    lab.put("a.txt", b"beta");
    let t2 = snap(&mut store);
    let (dir, want) = (store.dir.clone(), picture(&lab.root()));
    drop(store);

    // The index is gone, and the pack ends in a record that was never finished.
    let pack = dir.join("pack-0000.bin");
    let whole = fs::metadata(&pack).unwrap().len();
    fs::remove_file(dir.join("objects.idx")).unwrap();
    let mut torn = rec_head(&sha(b"never written"), 0, 1 << 30, 1 << 30).to_vec();
    torn.extend_from_slice(&[0xAB; 300]);
    fs::OpenOptions::new().append(true).open(&pack).unwrap().write_all(&torn).unwrap();

    let mut store = lab.open();
    store.load().unwrap();
    assert!(store.tree(&t1.tree).is_ok(), "reading without the index (no repair yet)");
    assert_eq!(fs::metadata(&pack).unwrap().len(), whole + torn.len() as u64, "reading alone repairs nothing");
    drop(store.begin().unwrap());
    assert_eq!(fs::metadata(&pack).unwrap().len(), whole, "the unfinished record was cut off, and nothing else");
    assert!(dir.join("objects.idx").exists(), "the index was rebuilt");
    let r = store.restore(&t1.tree, true).unwrap();
    assert!(r["complete"] == true && r["verified"] == true, "{r}");
    assert_eq!(lab.read("a.txt").unwrap(), b"alpha");
    let r = store.restore(&t2.tree, true).unwrap();
    assert!(r["complete"] == true, "{r}");
    assert_eq!(picture(&lab.root()), want);

    // New content after the repair is appended where the torn record was and reads back.
    lab.put("c.txt", b"after the repair");
    let t3 = snap(&mut store);
    fs::remove_file(lab.at("c.txt")).unwrap();
    assert!(store.restore(&t3.tree, true).unwrap()["complete"] == true);
    assert_eq!(lab.read("c.txt").unwrap(), b"after the repair");
    drop(store);

    // An index cut off in the middle of a record, and a log cut off in the middle of a line.
    let idx = fs::read(dir.join("objects.idx")).unwrap();
    fs::write(dir.join("objects.idx"), &idx[..idx.len() - 23]).unwrap();
    fs::OpenOptions::new().append(true).open(dir.join("log.jsonl")).unwrap().write_all(b"{\"t\":\"snap\",\"seq\":99,\"tr").unwrap();
    let mut store = lab.open();
    let entries = store.log.len();
    lab.put("d.txt", b"one more");
    let t4 = snap(&mut store);
    assert_eq!(t4.seq, Some(entries as u64 + 1));
    assert_eq!(lab.open().log.len(), entries + 1, "the line after a torn one must still be read");
    assert!(lab.open().restore(&t3.tree, true).unwrap()["complete"] == true);

    // Content that no longer matches its hash is refused, and the file on disk is left as it was.
    let l = store.objects.map[&sha(b"alpha")];
    let f = fs::OpenOptions::new().write(true).open(dir.join(format!("pack-{:04}.bin", l.pack))).unwrap();
    f.seek_write(b"X", l.off).unwrap();
    drop(f);
    let r = lab.open().restore(&t1.tree, true).unwrap();
    assert!(r["complete"] == false, "{r}");
    assert_eq!(r["result"]["failed"][0]["path"], "a.txt");
    assert!(r["result"]["failed"][0]["what"].as_str().unwrap().contains("damaged"));
    assert_eq!(lab.read("a.txt").unwrap(), b"beta", "a damaged object must not replace a good file");
}

#[test]
fn trees_are_named_by_number_or_by_the_start_of_their_id() {
    let lab = Lab::new("resolve");
    lab.put("a", b"1");
    let (mut store, t1) = lab.track();
    lab.put("a", b"2");
    let t2 = snap(&mut store);
    assert_eq!(store.resolve("last").unwrap(), t2.tree);
    assert_eq!(store.resolve("1").unwrap(), t1.tree);
    assert_eq!(store.resolve("2").unwrap(), t2.tree);
    assert_eq!(store.resolve(&hex(&t1.tree)[..10]).unwrap(), t1.tree);
    assert_eq!(store.resolve(&hex(&t1.tree).to_uppercase()).unwrap(), t1.tree);
    assert!(store.resolve("3").is_err());
    assert!(store.resolve("abc").is_err());
    assert!(store.resolve(&"0".repeat(64)).is_err(), "only trees the log knows can be named");
    assert!(store.resolve(&hex(&sha(b"1"))).is_err(), "a file's content hash is not a tree");

    assert_eq!(find_root(Some("X:\\given"), Path::new("C:\\somewhere")), PathBuf::from("X:\\given"));
    fs::create_dir_all(lab.at(".git")).unwrap();
    fs::create_dir_all(lab.at("deep/er")).unwrap();
    assert_eq!(find_root(None, &lab.at("deep/er")), lab.root());
    let bare = lab.data();
    if bare.ancestors().all(|a| !a.join(".git").exists()) {
        assert_eq!(find_root(None, &bare), bare, "with no .git above, the folder itself");
    }
}

#[test]
fn the_command_line_refuses_what_it_does_not_know() {
    let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| x.to_string()).collect() };
    // None of these reaches a store, a data folder or a window.
    assert!(cli(&[]).is_err());
    assert!(cli(&s(&["frobnicate"])).is_err_and(|e| e.to_string().contains("unknown chronicle command")));
    assert!(cli(&s(&["snap", "--bogus"])).is_err_and(|e| e.to_string().contains("unknown option --bogus")));
    assert!(cli(&s(&["restore", "--force"])).is_err(), "restore has no --force: nothing about it can be forced");
    assert!(cli(&s(&["log", "-n", "many"])).is_err());
    // A bare word is the folder where a command takes one, and an error where it does not:
    // a folder named by mistake is never swapped for the default in silence.
    for stray in [&["track", "a", "b"][..], &["status", "--root", "a", "b"], &["snap", "a", "b"], &["log", "a", "b"], &["undo", "a", "b"], &["watch", "a", "b"], &["diff", "a", "b", "c"], &["restore", "a", "b"]] {
        assert!(cli(&s(stray)).is_err_and(|e| e.to_string().contains("unexpected argument")), "{stray:?}");
    }
    let a = words(parse(&s(&["C:\\some\\folder", "--json"]), &[]).unwrap(), true, 0).unwrap();
    assert_eq!((a.root.as_deref(), a.rest.len()), (Some("C:\\some\\folder"), 0));
    assert!(words(parse(&s(&["--root", "R", "other"]), &[]).unwrap(), true, 0).is_err(), "two folders named");
    assert_eq!(words(parse(&s(&["t1", "t2"]), &[]).unwrap(), false, 2).unwrap().rest, s(&["t1", "t2"]));
    assert!(run_cli(&[]).is_err());
    assert!(run_cli(&s(&["--root", "x"])).is_err(), "no command given");
    let a = parse(&s(&["--root", "R", "--json", "-m", "a label", "--apply", "tree"]), &["-m", "--apply"]).unwrap();
    assert_eq!((a.root.as_deref(), a.json, a.label.as_deref(), a.apply, a.rest), (Some("R"), true, Some("a label"), true, s(&["tree"])));
}

#[test]
fn watching_records_each_change_and_survives_a_flood() {
    let lab = Lab::new("watch");
    lab.put("a.txt", b"one");
    lab.put(".gitignore", b"target/\n");
    let (mut store, first) = lab.track();
    let (tx, rx) = std::sync::mpsc::channel::<Result<(Hash, u64, u64, u64), String>>();
    let stop = Arc::new(AtomicBool::new(false));
    let stopper = stop.clone();
    // A 4 KB event buffer and room for 64 waiting events, so that the flood
    // below overflows while a capture is busy.
    let watcher = std::thread::spawn(move || {
        watch(&mut store, Duration::from_millis(60), Some((4096, 64)), |c| {
            let _ = tx.send(c.map(|c| (c.snap.tree, c.events, c.rescans, c.snap.read_files)).map_err(String::from));
            !stopper.load(Ordering::Relaxed)
        })
    });
    let started = rx.recv_timeout(Duration::from_secs(20)).unwrap().unwrap();
    assert_eq!((started.0, started.1), (first.tree, 0));

    // After each change, a capture arrives whose tree is exactly the folder as it now is.
    let settle = |what: &str, wait: u64| -> (u64, u64) {
        let want = independent(&lab.root());
        let deadline = Instant::now() + Duration::from_secs(wait);
        let (mut events, mut rescans) = (0, 0);
        loop {
            let got = rx.recv_deadline_or(deadline).unwrap_or_else(|| panic!("{what}: no capture matched the folder in {wait} s"));
            let (tree, e, r, _) = got.unwrap_or_else(|e| panic!("{what}: capture failed: {e}"));
            events += e;
            rescans += r;
            if tree == want {
                return (events, rescans);
            }
        }
    };
    lab.put("b.txt", b"created");
    settle("create", 20);
    lab.put("b.txt", b"modified, and longer");
    settle("modify", 20);
    fs::rename(lab.at("b.txt"), lab.at("c.txt")).unwrap();
    settle("rename", 20);
    fs::remove_file(lab.at("c.txt")).unwrap();
    settle("delete", 20);
    lab.put("dir/in/side.txt", b"nested");
    settle("nested create", 20);

    // 10,000 files as fast as they can be written.
    fs::create_dir_all(lab.at("flood")).unwrap();
    for i in 0..10_000 {
        fs::write(lab.at(&format!("flood/f{i:05}.txt")), format!("{i}")).unwrap();
    }
    let (events, rescans) = settle("flood", 120);
    assert!(rescans >= 1, "10,000 changes through a 4 KB buffer and no overflow was reported ({events} events)");

    // Still alive afterwards.
    lab.put("after.txt", b"still watching");
    settle("after the flood", 20);

    // Changes that cannot matter cause no capture at all: an ignored folder and .git.
    while rx.recv_timeout(Duration::from_millis(700)).is_ok() {}
    lab.put("target/debug/out.bin", b"build output");
    lab.put(".git/index", b"git's own");
    assert!(rx.recv_timeout(Duration::from_millis(900)).is_err(), "a change in an ignored folder caused a capture");

    stop.store(true, Ordering::Relaxed);
    lab.put("last.txt", b"wake up and stop");
    assert!(watcher.join().unwrap().is_ok());
    let log = log_read(&Store::dir_for(&lab.data(), &canon(&lab.root()).unwrap()));
    assert!(log.iter().filter(|r| r["reason"] == "watch").count() >= 7);
}

/// `recv_deadline` for std's channel, as an Option.
trait RecvBy<T> {
    fn recv_deadline_or(&self, deadline: Instant) -> Option<T>;
}

impl<T> RecvBy<T> for std::sync::mpsc::Receiver<T> {
    fn recv_deadline_or(&self, deadline: Instant) -> Option<T> {
        self.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok()
    }
}

// ---------------------------------------------------------------- found in review
//
// Each test below failed on the commit under review (5de467c) and passes now.

/// Every path a restore or undo refused, whether the plan said so beforehand
/// or it was found at the last moment.
fn refused(r: &Value) -> Vec<String> {
    let mut v: Vec<String> = [&r["plan"]["blocked"], &r["result"]["blocked"]]
        .iter()
        .flat_map(|list| list.as_array().into_iter().flatten())
        .map(|b| b["path"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn a_dry_run_names_everything_apply_will_refuse() {
    let lab = Lab::new("preflight");
    let outside = lab.base.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.txt"), b"outside the root").unwrap();
    // The ignore file ignores itself, so the rules can change without the tree changing.
    lab.put(".gitignore", b"*.secret\n.gitignore\n");
    lab.put("plain.txt", b"plain v1");
    lab.put("key.txt", b"key v1");
    lab.put("notes.txt", b"notes v1");
    lab.put("db.bin", b"AAAAAAAAAAAAAAAA");
    lab.put("log.txt", b"log v1");
    lab.put("real/in.txt", b"inside");
    let (mut store, first) = lab.track();

    lab.put("plain.txt", b"plain v2");
    lab.put("db.bin", b"BBBBBBBBBBBBBBBB");
    lab.put("log.txt", b"log v2");
    lab.put("extra/out.txt", b"made later");
    fs::remove_file(lab.at("key.txt")).unwrap();
    fs::remove_dir_all(lab.at("real")).unwrap();
    lab.put(".gitignore", b"*.secret\n.gitignore\nnotes.txt\n");
    std::thread::sleep(Duration::from_millis(2300));
    snap(&mut store);

    // Six things now stand in the way of going back to the first tree:
    junction(&lab.at("key.txt"), &outside); // a link where a file should be written
    junction(&lab.at("real"), &outside); // a link where a folder should be made
    lab.put("notes.txt", b"edited while ignored: never captured"); // an ignored file where a file should be written
    lab.put("extra/cache.secret", b"ignored, in a folder that should go");
    let mut writer = fs::File::options().read(true).write(true).share_mode(7).open(lab.at("db.bin")).unwrap();
    writer.write_all(b"CCCCCCCCCCCCCCCC").unwrap(); // a program that holds a file open for writing
    let reader = fs::File::options().read(true).share_mode(1).open(lab.at("log.txt")).unwrap(); // one that only reads, and lets nobody replace the file meanwhile

    let before = picture(&lab.root());
    let dry = store.restore(&first.tree, false).unwrap();
    assert_eq!(dry["applied"], false);
    assert_eq!(picture(&lab.root()), before, "a dry run changed something");
    assert_eq!(refused(&dry), ["db.bin", "key.txt", "log.txt", "notes.txt", "real", "real/in.txt"], "the dry run must name what will be refused: {dry}");
    assert_eq!(dry["plan"]["kept"][0]["path"], "extra", "the dry run must say which folder will be kept: {dry}");
    let count = |v: &Value, k: &str| v[k].as_u64().unwrap();
    assert_eq!((count(&dry["plan"], "write"), count(&dry["plan"], "delete"), count(&dry["plan"], "make_folders"), count(&dry["plan"], "remove_folders")), (1, 1, 0, 0), "{dry}");

    // Applied, it does what the dry run said: no more and no less.
    let r = store.restore(&first.tree, true).unwrap();
    assert_eq!(refused(&r), refused(&dry), "{r}");
    assert_eq!(r["plan"]["ops"], dry["plan"]["ops"], "the plan applied is not the plan shown");
    let done = &r["result"];
    assert_eq!((count(done, "written"), count(done, "deleted"), count(done, "folders_made"), count(done, "folders_removed")), (1, 1, 0, 0), "{r}");
    assert!(done["blocked"].as_array().unwrap().is_empty() && done["failed"].as_array().unwrap().is_empty(), "nothing was left to find at the last moment: {r}");
    assert!(r["verified"] == true, "what was planned is what happened: {r}");
    assert!(r["complete"] == false && r["reached_target"] == false, "a restore that kept a folder and refused six paths is not complete: {r}");
    drop((writer, reader));

    assert_eq!(lab.read("plain.txt").unwrap(), b"plain v1");
    assert_eq!(lab.read("log.txt").unwrap(), b"log v2");
    assert_eq!(lab.read("notes.txt").unwrap(), b"edited while ignored: never captured");
    assert_eq!(lab.read("db.bin").unwrap(), b"CCCCCCCCCCCCCCCC");
    assert_eq!(lab.read("extra/cache.secret").unwrap(), b"ignored, in a folder that should go");
    assert!(!lab.at("extra/out.txt").exists());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1, "something was made behind a link");
    assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"outside the root");
    fs::remove_dir(lab.at("key.txt")).unwrap();
    fs::remove_dir(lab.at("real")).unwrap();
}

#[test]
fn what_an_interrupted_restore_left_behind_is_reported_then_removed() {
    let lab = Lab::new("leftover");
    lab.put("a.txt", b"a");
    lab.put("dir/b.txt", b"b");
    lab.put(".proteus-restore-notes.tmp", b"a person's own file with a similar name");
    let (mut store, first) = lab.track();
    assert!(first.manifest.contains_key(".proteus-restore-notes.tmp"), "only names restore itself makes are left out of a tree");
    fs::remove_dir_all(lab.at("dir")).unwrap();
    let without_dir = snap(&mut store);
    assert!(store.restore(&first.tree, true).unwrap()["complete"] == true);

    // What a restore killed part-way leaves: its temporary files, beside the files it was writing.
    lab.put(".proteus-restore-4242-0.tmp", b"half of a file");
    lab.put("dir/.proteus-restore-4242-17.tmp", b"half of another");

    // Looking (status, a dry run) removes nothing and says they are there.
    store.load().unwrap();
    let look = store.snapshot(&SnapOpts { label: None, reason: "status", record: false, rehash: false }).unwrap();
    assert_eq!(look.json()["restore_leftovers"], json!([".proteus-restore-4242-0.tmp", "dir/.proteus-restore-4242-17.tmp"]));
    assert!(lab.at(".proteus-restore-4242-0.tmp").exists() && lab.at("dir/.proteus-restore-4242-17.tmp").exists());
    assert_eq!(look.tree, first.tree, "they are not part of a tree");

    // The next restore clears them away, so the folder that should go can go.
    let r = store.restore(&without_dir.tree, true).unwrap();
    assert!(r["complete"] == true && r["reached_target"] == true, "{r}");
    assert!(!lab.at("dir").exists(), "a leftover kept the folder alive");
    assert!(!lab.at(".proteus-restore-4242-0.tmp").exists(), "the leftover is still in the tree");
    assert_eq!(lab.read(".proteus-restore-notes.tmp").unwrap(), b"a person's own file with a similar name");

    // So does an ordinary snapshot.
    lab.put(".proteus-restore-77-3.tmp", b"another");
    let s = snap(&mut store);
    assert!(!lab.at(".proteus-restore-77-3.tmp").exists());
    assert_eq!(s.json()["swept"], 1);
}

/// What `icacls` prints for a file: who may do what with it.
fn permissions(p: &Path) -> Option<String> {
    let out = std::process::Command::new("icacls").arg(p).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn a_replaced_file_keeps_its_permissions_and_named_streams() {
    let lab = Lab::new("meta");
    lab.put("key.pem", b"private v1");
    lab.put("plain.txt", b"plain");
    let (mut store, first) = lab.track();
    let key = lab.at("key.pem");
    let stream = PathBuf::from(format!("{}:notes", key.display()));

    // After the snapshot the file is edited, locked down to this account alone
    // (nothing inherited from the folder), and given a named stream. None of
    // that is content, so none of it is in the store.
    fs::OpenOptions::new().append(true).open(&key).unwrap().write_all(b", then edited").unwrap();
    let me = std::env::var("USERNAME").unwrap_or_default();
    let locked = std::process::Command::new("icacls").arg(&key).args(["/inheritance:r", "/grant:r"]).arg(format!("{me}:(F)")).output().is_ok_and(|o| o.status.success());
    fs::write(&stream, b"a stream the store never held").unwrap();
    let allowed = permissions(&key);
    if !locked || allowed.is_none() {
        eprintln!("SKIPPED the permissions half of a_replaced_file_keeps_its_permissions_and_named_streams: icacls could not set them");
    }
    snap(&mut store);

    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == true && r["reached_target"] == true, "{r}");
    assert_eq!(fs::read(&key).unwrap(), b"private v1");
    assert_eq!(fs::read(&stream).ok().as_deref(), Some(&b"a stream the store never held"[..]), "replacing the file destroyed a stream that was never captured");
    if locked {
        assert_eq!(permissions(&key), allowed, "replacing the file changed who may read it");
    }

    // And the same on the way back.
    let r = store.undo(None, true, false).unwrap();
    assert!(r["complete"] == true && r["back_to_before"] == true, "{r}");
    assert_eq!(fs::read(&key).unwrap(), b"private v1, then edited");
    assert_eq!(fs::read(&stream).ok().as_deref(), Some(&b"a stream the store never held"[..]));
    if locked {
        assert_eq!(permissions(&key), allowed);
    }
}

#[test]
fn a_new_file_appears_at_its_path_only_when_it_is_whole() {
    let lab = Lab::new("whole");
    lab.put("keep.txt", b"kept");
    let big = Rng(5).bytes(3 << 20);
    lab.put("new.bin", &big);
    lab.put("sub/also.txt", b"also new");
    let (mut store, first) = lab.track();
    fs::remove_file(lab.at("new.bin")).unwrap();
    fs::remove_dir_all(lab.at("sub")).unwrap();
    snap(&mut store);

    // The OS reports how each file arrives. A file that is created at its own
    // path and then filled is, for a moment (or for ever, if the restore is
    // killed), half a file under a real name.
    let w = watch::Watcher::start(&canon(&lab.root()).unwrap()).unwrap();
    let r = store.restore(&first.tree, true).unwrap();
    assert!(r["complete"] == true && r["verified"] == true, "{r}");
    let mut events = Vec::new();
    while let Ok(e) = w.events.recv_timeout(Duration::from_millis(400)) {
        events.push(e);
    }
    for name in ["new.bin", "sub\\also.txt"] {
        let at = |e: &&watch::Event| e.path == Path::new(name);
        assert!(events.iter().filter(at).any(|e| matches!(e.change, watch::Change::Rename { .. })), "{name} was not moved into place: {events:?}");
        assert!(!events.iter().filter(at).any(|e| e.change == watch::Change::Create), "{name} was created at its own path and filled there: {events:?}");
    }
    assert_eq!(sha(&lab.read("new.bin").unwrap()), sha(&big));
    assert!(picture(&lab.root()).keys().all(|k| !k.contains(TMP_PREFIX)));
}

#[test]
fn many_files_go_back_into_folders_that_are_gone() {
    // A forced undo after the folders the step wrote into were removed: every
    // file needs its folder made, and eight threads find that out at once.
    let lab = Lab::new("parents");
    let rels: Vec<String> = (0..40).flat_map(|d| (0..5).map(move |f| format!("d{d:02}/f{f}.txt"))).collect();
    for rel in &rels {
        lab.put(rel, format!("{rel} before").as_bytes());
    }
    let (mut store, before) = lab.track();
    for rel in &rels {
        lab.put(rel, format!("{rel} after the step").as_bytes());
    }
    let after = snap(&mut store);
    for d in 0..40 {
        fs::remove_dir_all(lab.at(&format!("d{d:02}"))).unwrap();
    }
    let now = snap(&mut store);
    assert_eq!(now.files, 0);
    let step = json!({ "seq": 1, "before": hex(&before.tree), "after": hex(&after.tree) });
    let p = store.plan_undo(&step, &now.manifest, true).unwrap();
    assert_eq!(p.ops.len(), 200);
    let done = store.apply(&p, &now.manifest);
    assert!(done.blocked.is_empty() && done.failed.is_empty(), "{:?} {:?}", done.blocked.first(), done.failed.first());
    assert_eq!(done.written, 200);
    assert_eq!(lab.read("d39/f4.txt").unwrap(), b"d39/f4.txt before");
}

#[test]
fn what_vanishes_while_a_snapshot_is_taken_is_absent_not_unreadable() {
    let lab = Lab::new("vanish");
    lab.put("stays.txt", b"here");
    let (mut store, first) = lab.track();
    // The walk listed a file and a folder; by the time they are read they are
    // gone (a build's temporary files do this all day).
    let _guard = store.begin().unwrap();
    let mut w = store.walk().unwrap();
    let base = RuleBase::new(&store.root, true, &mut Vec::new());
    w.items.push(Seen { rel: "ghost.txt".into(), dir: false, size: 5, stamp: Stamp::default(), attrs: 0 });
    w.items.push(Seen { rel: "ghost-dir".into(), dir: true, size: 0, stamp: Stamp::default(), attrs: A_DIRECTORY });
    assert!(store.walk_dir("ghost-dir", None, &base, &mut w, &mut Vec::new()).is_empty());
    w.files += 1;
    w.bytes += 5;
    let s = store.snapshot_of(w, &SnapOpts { label: None, reason: "test", record: true, rehash: false }, Instant::now()).unwrap();
    assert!(s.failures.is_empty(), "something that is not there is not a failure: {:?}", s.failures);
    assert_eq!(s.manifest.keys().collect::<Vec<_>>(), ["stays.txt"], "a tree must not say something was there when it was gone");
    assert_eq!((s.tree, s.files, s.bytes), (first.tree, 1, 4));
}

#[test]
fn a_folder_renamed_only_in_letter_case_comes_back_with_what_it_holds() {
    let lab = Lab::new("recase");
    lab.put(".gitignore", b"*.o\n");
    lab.put("src/lib/util.rs", b"util");
    lab.put("src/lib/deep/more.rs", b"more");
    lab.put("src/lib/util.o", b"ignored: an object file");
    lab.put("src/lib/deep/more.o", b"ignored: another");
    let (mut store, lower) = lab.track();
    let names = |dir: &str| -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(lab.at(dir)).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        v.sort();
        v
    };
    fs::rename(lab.at("src/lib"), lab.at("src/LIB")).unwrap();
    assert_eq!(names("src"), ["LIB"]);
    let upper = snap(&mut store);
    assert_ne!(upper.tree, lower.tree, "a tree tells letter case apart");

    // The folder cannot be removed and made again (it holds ignored files),
    // and does not need to be: it is the same folder under another name.
    let dry = store.restore(&lower.tree, false).unwrap();
    assert!(dry["plan"]["kept"].as_array().is_none_or(|k| k.is_empty()) && refused(&dry).is_empty(), "{dry}");
    assert_eq!((&dry["plan"]["rename_folders"], &dry["plan"]["make_folders"], &dry["plan"]["remove_folders"]), (&json!(2), &json!(0), &json!(0)), "{dry}");
    let r = store.restore(&lower.tree, true).unwrap();
    assert!(r["complete"] == true && r["verified"] == true && r["reached_target"] == true, "{r}");
    assert_eq!(r["plan"]["ops"], dry["plan"]["ops"], "the plan applied is not the plan shown");
    assert_eq!((&r["result"]["folders_renamed"], &r["result"]["folders_made"], &r["result"]["folders_removed"]), (&json!(2), &json!(0), &json!(0)), "{r}");
    assert_eq!(names("src"), ["lib"]);
    assert_eq!(lab.read("src/lib/util.o").unwrap(), b"ignored: an object file");
    assert_eq!(lab.read("src/lib/deep/more.o").unwrap(), b"ignored: another");
    assert_eq!(lab.read("src/lib/deep/more.rs").unwrap(), b"more");

    let r = store.undo(None, true, false).unwrap();
    assert!(r["complete"] == true && r["back_to_before"] == true, "{r}");
    assert_eq!(names("src"), ["LIB"]);
    assert_eq!(lab.read("src/LIB/util.o").unwrap(), b"ignored: an object file");
}

#[test]
fn a_change_made_through_another_name_of_a_file_is_seen() {
    // One file with two names (a hard link). Written through one name,
    // Windows leaves the size and times in the listing of the other as they
    // were, so the listing alone says that name did not change.
    let lab = Lab::new("hardlink");
    let overwrite = |rel: &str, content: &[u8]| {
        // In place and the same length: no new file, no new size.
        let mut f = fs::File::options().write(true).open(lab.at(rel)).unwrap();
        f.write_all(content).unwrap();
    };
    lab.put("a.txt", b"AAAAAAAA");
    lab.put("alone.txt", b"11111111");
    fs::create_dir_all(lab.at("links")).unwrap();
    if fs::hard_link(lab.at("a.txt"), lab.at("links/b.txt")).is_err() {
        eprintln!("SKIPPED a_change_made_through_another_name_of_a_file_is_seen: this volume has no hard links");
        return;
    }
    let (mut store, _) = lab.track();
    std::thread::sleep(Duration::from_millis(2300));
    snap(&mut store);

    overwrite("a.txt", b"BBBBBBBB");
    let s = snap(&mut store);
    assert_eq!(s.manifest["a.txt"].hash, sha(b"BBBBBBBB"));
    assert_eq!(s.manifest["links/b.txt"].hash, sha(b"BBBBBBBB"), "the other name of the file was recorded with content it no longer holds");
    assert_eq!(s.tree, independent(&lab.root()));

    // A file that was alone (and so is trusted to the cache) gets a second
    // name, and is then written through the new one.
    assert_eq!(snap(&mut store).manifest["alone.txt"].hash, sha(b"11111111"));
    fs::hard_link(lab.at("alone.txt"), lab.at("links/twin.txt")).unwrap();
    overwrite("links/twin.txt", b"22222222");
    let s = snap(&mut store);
    assert_eq!(s.manifest["links/twin.txt"].hash, sha(b"22222222"));
    assert_eq!(s.manifest["alone.txt"].hash, sha(b"22222222"), "the first name of the file was recorded with content it no longer holds");
    assert_eq!(s.tree, independent(&lab.root()));
    // And again, now that both names are known to be one file.
    overwrite("alone.txt", b"33333333");
    let s = snap(&mut store);
    assert_eq!((s.manifest["alone.txt"].hash, s.manifest["links/twin.txt"].hash), (sha(b"33333333"), sha(b"33333333")));
}

#[test]
fn files_deeper_than_260_characters_are_replaced_made_and_deleted() {
    // Not a defect of the commit under review: this holds the review's own
    // changes (how a file is swapped in and moved into place) to long paths.
    let lab = Lab::new("deep");
    let deep = (0..9).map(|i| format!("a-folder-with-a-fairly-long-name-number-{i:02}")).collect::<Vec<_>>().join("/");
    let (edited, removed, added) = (format!("{deep}/edited.txt"), format!("{deep}/removed.txt"), format!("{deep}/added.txt"));
    assert!(lab.at(&edited).as_os_str().len() > 400);
    lab.put(&edited, b"v1");
    lab.put(&removed, b"here at first");
    let (mut store, first) = lab.track();
    let long = |rel: &str| canon(&lab.root()).unwrap().join(rel.replace('/', "\\"));
    fs::write(long(&edited), b"v2, longer").unwrap();
    fs::remove_file(long(&removed)).unwrap();
    fs::write(long(&added), b"made later").unwrap();
    let second = snap(&mut store);
    for (tree, want) in [(first.tree, [Some(&b"v1"[..]), Some(&b"here at first"[..]), None]), (second.tree, [Some(&b"v2, longer"[..]), None, Some(&b"made later"[..])])] {
        let r = store.restore(&tree, true).unwrap();
        assert!(r["complete"] == true && r["verified"] == true && r["reached_target"] == true, "{r}");
        let got = [&edited, &removed, &added].map(|rel| fs::read(long(rel)).ok());
        assert_eq!(got.iter().map(|g| g.as_deref()).collect::<Vec<_>>(), want);
    }
}

#[test]
fn a_record_whose_content_never_reached_the_disk_is_not_believed() {
    // What a power cut in the middle of a snapshot leaves in a pack: the
    // header of a record, the room for its content, and zeros in that room
    // (the header was on disk, the content was still on its way). The next
    // writer finds the record by reading the pack. If it believes the header,
    // it will never store that content again ("already there"), and every
    // later tree that names it cannot be brought back.
    let lab = Lab::new("powercut");
    lab.put("a.txt", b"a");
    let (store, _) = lab.track();
    let dir = store.dir.clone();
    drop(store);
    let lost = b"this line was on its way to the disk when the power went\n".repeat(30);
    let survivor = b"written after it in the same run, and whole\n".repeat(30);
    let record = |content: &[u8], on_disk: &[u8]| {
        let mut r = rec_head(&sha(content), 0, content.len() as u64, content.len() as u64).to_vec();
        r.extend_from_slice(on_disk);
        r
    };
    let pack = dir.join("pack-0000.bin");
    let append = |bytes: &[u8]| fs::OpenOptions::new().append(true).open(&pack).unwrap().write_all(bytes).unwrap();
    let whole = fs::metadata(&pack).unwrap().len();

    // 1. Torn, with a whole record after it: the whole one is adopted, the torn one is not.
    append(&record(&lost, &vec![0u8; lost.len()]));
    append(&record(&survivor, &survivor));
    let mut store = lab.open();
    drop(store.begin().unwrap());
    assert!(!store.objects.has(&sha(&lost)), "a record that does not hold what its header says was believed");
    assert!(store.objects.has(&sha(&survivor)), "the whole record after it was lost with it");
    assert_eq!(store.objects.get(&sha(&survivor)).unwrap(), survivor);

    // 2. The content that was lost is captured for real the next time it is seen, and comes back.
    lab.put("lost.txt", &lost);
    let with = snap(&mut store);
    assert_eq!(with.manifest["lost.txt"].hash, sha(&lost));
    fs::remove_file(lab.at("lost.txt")).unwrap();
    snap(&mut store);
    let r = store.restore(&with.tree, true).unwrap();
    assert!(r["complete"] == true && r["verified"] == true && r["reached_target"] == true, "{r}");
    assert_eq!(lab.read("lost.txt").unwrap(), lost);
    drop(store);

    // 3. Torn at the very end of a pack: cut off, and nothing else is.
    let last = Objects::new(&dir).packs().into_iter().next_back().unwrap();
    let last_path = dir.join(format!("pack-{:04}.bin", last.0));
    let never = b"never stored anywhere else\n".repeat(20);
    fs::OpenOptions::new().append(true).open(&last_path).unwrap().write_all(&record(&never, &vec![0u8; never.len()])).unwrap();
    let mut store = lab.open();
    drop(store.begin().unwrap());
    assert!(!store.objects.has(&sha(&never)));
    assert_eq!(fs::metadata(&last_path).unwrap().len(), last.1, "the torn record at the end was not cut off, or more than it was");
    assert!(fs::metadata(&pack).unwrap().len() >= whole);
    assert!(store.restore(&with.tree, false).is_ok(), "the trees recorded before are still readable");
}

#[test]
fn a_dry_run_names_a_folder_that_a_shell_is_sitting_in() {
    // A command makes a folder; a shell then changes into it. Windows will
    // not remove a folder that is some program's current folder, so undo
    // cannot take it away: the dry run has to say so, and --apply has to do
    // what the dry run said.
    let lab = Lab::new("in-use");
    lab.put("keep.txt", b"kept");
    let (mut store, _) = lab.track();
    let cmd: Vec<String> = ["cmd", "/c", "mkdir build& echo x> build\\out.txt"].map(String::from).to_vec();
    let step = store.run_step(&cmd, &lab.root(), None).unwrap();
    let run_seq = step["seq"].as_u64().unwrap();

    // A real shell, waiting for commands on a pipe that never sends any, with
    // the new folder as its current folder. Stopped below through its own handle.
    let mut shell = std::process::Command::new("cmd")
        .current_dir(lab.at("build"))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // It holds the folder once it has started: asking for the right to delete the folder is then refused.
    let held = || fs::OpenOptions::new().access_mode(0x1_0000).share_mode(7).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(lab.at("build")).is_err();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !held() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    let checked = (|| -> Result<(), String> {
        if !held() {
            return Err("the shell never took hold of its current folder".into());
        }
        let dry = store.undo(None, false, false)?;
        let count = |v: &Value, k: &str| v[k].as_u64().unwrap_or(99);
        if dry["plan"]["kept"][0]["path"] != "build" || dry["would_be_complete"] != false {
            return Err(format!("the dry run must say the folder stays: {dry}"));
        }
        if (count(&dry["plan"], "delete"), count(&dry["plan"], "remove_folders")) != (1, 0) {
            return Err(format!("the dry run must not promise to remove the folder: {dry}"));
        }
        let r = store.undo(None, true, false)?;
        if r["plan"]["ops"] != dry["plan"]["ops"] {
            return Err(format!("the plan applied is not the plan shown: {r}"));
        }
        if !r["result"]["kept_folders"].as_array().is_some_and(|k| k.is_empty()) || r["verified"] != true || r["complete"] != false {
            return Err(format!("apply found out at the last moment what the dry run should have said: {r}"));
        }
        Ok(())
    })();
    shell.kill().unwrap();
    shell.wait().unwrap();
    checked.unwrap();
    assert!(lab.at("build").is_dir() && !lab.at("build/out.txt").exists());

    // The shell has gone: now the folder can go too.
    let r = store.undo(Some(run_seq), true, false).unwrap();
    assert!(r["complete"] == true && r["back_to_before"] == true, "{r}");
    assert!(!lab.at("build").exists());
}

#[test]
fn a_dry_run_names_content_the_store_no_longer_has() {
    // A tree that names content which is not in the store (a pack file was
    // lost, say): the file cannot be put back. The dry run says so; --apply
    // does not find it out half-way.
    let lab = Lab::new("missing");
    lab.put("here.txt", b"in the store");
    let (mut store, _) = lab.track();
    let gone = sha(b"this content was in a pack file that is gone");
    let id = {
        let _guard = store.begin().unwrap();
        let listing = format!("{TREE_HEAD}f 0 12 {} here.txt\nf 0 44 {} lost.txt\n", hex(&sha(b"in the store")), hex(&gone));
        let id = store.objects.put_bytes(listing.as_bytes()).unwrap();
        store.objects.commit().unwrap();
        id
    };
    let dry = store.restore(&id, false).unwrap();
    assert_eq!(refused(&dry), ["lost.txt"], "the dry run must name the file whose content is missing: {dry}");
    assert!(dry["plan"]["blocked"][0]["what"].as_str().unwrap().contains("missing from the store"), "{dry}");
    assert_eq!((dry["plan"]["write"].as_u64(), dry["would_be_complete"].as_bool()), (Some(0), Some(false)), "{dry}");
    let r = store.restore(&id, true).unwrap();
    assert_eq!(r["plan"]["ops"], dry["plan"]["ops"], "{r}");
    assert!(r["result"]["failed"].as_array().is_some_and(|f| f.is_empty()), "found out only while applying: {r}");
    assert!(r["complete"] == false, "{r}");
    assert!(!lab.at("lost.txt").exists());
    assert_eq!(lab.read("here.txt").unwrap(), b"in the store");
}

#[test]
fn a_write_through_a_mapped_view_is_recorded_when_every_file_is_read() {
    // A real program changes one byte of a file in place through a
    // memory-mapped view. Windows then changes neither the file's size nor
    // any of its times (measured on NTFS, see forge/lanes/chronicle.json),
    // so a step recorded by sizes and times cannot see the write.
    // `proteus run --rehash` reads every file before and after the command.
    let lab = Lab::new("mapped");
    let zeros = vec![0u8; 64 << 10];
    lab.put("array.bin", &zeros);
    lab.put("other.txt", b"other");
    // Long enough ago that the cache trusts what it knows about these files.
    std::thread::sleep(Duration::from_millis(2300));
    let (mut store, _) = lab.track();
    assert_eq!(snap(&mut store).read_files, 0);
    let poke = |at: usize, value: u8| -> Vec<String> {
        let script = format!(
            "$m=[IO.MemoryMappedFiles.MemoryMappedFile]::CreateFromFile((Join-Path (Get-Location) 'array.bin'),'Open');$v=$m.CreateViewAccessor({at},16);$v.Write(0,[byte]{value});$v.Flush();$v.Dispose();$m.Dispose()"
        );
        ["powershell", "-NoProfile", "-NonInteractive", "-Command", script.as_str()].map(String::from).to_vec()
    };
    let (mut once, mut twice) = (zeros.clone(), zeros.clone());
    once[1000] = 90;
    twice[1000] = 90;
    twice[2000] = 77;

    // The first write, in a step recorded the ordinary way. Whether that step
    // saw it is Windows' business and is not asserted; that it happened is.
    let first = store.run_step(&poke(1000, 90), &lab.root(), None);
    if !first.as_ref().is_ok_and(|s| s["exit"] == 0) || lab.read("array.bin").unwrap() != once {
        eprintln!("SKIPPED a_write_through_a_mapped_view_is_recorded_when_every_file_is_read: powershell could not make the mapped write: {first:?}");
        return;
    }

    // The second, with every file read: the state before holds the first
    // write (whatever the step above made of it), the state after the second.
    let step = store.run_step_with(&poke(2000, 77), &lab.root(), None, true).unwrap();
    assert_eq!(lab.read("array.bin").unwrap(), twice);
    assert_eq!((step["exit"].as_i64(), step["modified"].as_u64(), step["rehash"].as_bool()), (Some(0), Some(1), Some(true)), "{step}");
    let tree = |field: &str| store.tree(&unhex(step[field].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(tree("before")["array.bin"].hash, sha(&once), "the state before the command is not what was on disk before it");
    assert_eq!(tree("after")["array.bin"].hash, sha(&twice), "the write made through the mapped view was not recorded");

    // So the step can be undone, to exactly what was there before it.
    let r = store.undo(None, true, false).unwrap();
    assert!(r["complete"] == true && r["back_to_before"] == true, "{r}");
    assert_eq!(lab.read("array.bin").unwrap(), once);
}

#[test]
fn a_file_that_could_not_be_deleted_after_all_keeps_its_attributes() {
    // A read-only, hidden file is to be deleted. Between the plan and the
    // deed a program opens it for reading and lets nobody delete it. The
    // delete fails, as it must; the file must then be as it was, not left
    // writable and visible because the bits were cleared on the way.
    let lab = Lab::new("undeleted");
    lab.put("keep.txt", b"kept");
    let (mut store, first) = lab.track();
    lab.put("guarded.txt", b"read-only and hidden");
    set_attrs(&lab.at("guarded.txt"), A_READONLY | A_HIDDEN).unwrap();
    let with = snap(&mut store);
    let p = store.plan_restore(&with.manifest, &store.tree(&first.tree).unwrap());
    assert!(matches!(p.ops.as_slice(), [Op::Delete(path)] if path == "guarded.txt"), "{:?}", p.ops);

    let reader = fs::File::options().read(true).share_mode(SHARE_READ).open(lab.at("guarded.txt")).unwrap();
    let done = store.apply(&p, &with.manifest);
    assert_eq!((done.deleted, done.failed.len() + done.blocked.len()), (0, 1), "{:?} {:?}", done.failed, done.blocked);
    drop(reader);
    assert_eq!(lab.read("guarded.txt").unwrap(), b"read-only and hidden");
    assert_eq!(attrs_of(&lab.at("guarded.txt")).unwrap() & KEEP_ATTRS, A_READONLY | A_HIDDEN, "a failed delete changed the file's attributes");

    // With the reader gone it goes, read-only or not.
    let done = store.apply(&p, &with.manifest);
    assert_eq!((done.deleted, done.failed.len()), (1, 0), "{:?}", done.failed);
    assert!(!lab.at("guarded.txt").exists());
}

#[test]
fn a_stored_object_larger_than_it_claims_is_cut_off() {
    // An index (or pack header) that understates an object's size: reading it
    // must stop there, not write out whatever the deflate stream expands to.
    let lab = Lab::new("bomb");
    lab.put("a.txt", b"a");
    let (mut store, _) = lab.track();
    let zeros = vec![0u8; 8 << 20];
    let packed = deflate(&zeros).unwrap();
    assert!(packed.len() < 64 << 10);
    let hash = sha(&zeros);
    {
        let _guard = store.begin().unwrap();
        store.objects.put(&hash, 100, F_DEFLATE, &packed).unwrap();
        store.objects.commit().unwrap();
    }
    let mut out = Vec::new();
    let r = store.objects.copy_out(&hash, &mut HashMap::new(), &mut out);
    assert!(r.is_err_and(|e| e.to_string().contains("damaged")));
    assert!(out.len() <= 4096, "{} bytes were written for an object recorded as 100", out.len());
}
