//! PLATFORM GLUE (not a rule of the original): bringing the player's own copy of the game into a sandboxed app's
//! storage (iOS), from a folder or from a `.zip` of it.
//!
//! The source is whatever the player picked: the installed game folder (the one that holds `GameData` and `FEData`),
//! a folder above it, or a `.zip` of either. [`find_game_root`] / [`zip_root`] find the game folder in it whatever
//! the case of its names (copies from FAT / exFAT drives and Windows zips mix cases, and iOS volumes are
//! case-sensitive). Only what the engine reads is taken (see `docs/en/game-data.md`):
//!
//! * `GameData/` and `FEData/`, written under exactly those names (the engine joins them literally);
//! * `version.txt`, `fe.txt`, `nfs5.ini`, `install.txt`, and the executable as `nfs5.exe` (the intro movies' decoder
//!   reads its tables from it; some releases call it `Porsche.exe`), when present.
//!
//! macOS metadata (`._*` AppleDouble files, `.DS_Store`) and `Thumbs.db` are skipped. Everything is written into a
//! staging folder beside the destination, which replaces the destination only once the whole copy has succeeded,
//! so an interrupted import never leaves a half game behind.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The top-level files taken besides the two folders: `(name in the source, case-insensitive; name written)`.
/// The first executable found wins.
const FILES: &[(&str, &str)] = &[
    ("version.txt", "version.txt"),
    ("fe.txt", "fe.txt"),
    ("nfs5.ini", "nfs5.ini"),
    ("install.txt", "install.txt"),
    ("nfs5.exe", "nfs5.exe"),
    ("porsche.exe", "nfs5.exe"),
];
const DIRS: &[&str] = &["GameData", "FEData"];

/// How far below the picked folder the game folder may sit (`Games/NFS/game_info/data` = 3).
pub const SEARCH_DEPTH: usize = 4;

/// Shared between the import thread and the UI.
#[derive(Default)]
pub struct Progress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// Set by the UI to stop the import at the next file.
    pub cancel: AtomicBool,
    /// What is happening, for the UI ("Copying FEData/Movies/ch1.mad").
    pub stage: Mutex<String>,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        let (d, t) = (self.done.load(Ordering::Relaxed), self.total.load(Ordering::Relaxed));
        if t == 0 { 0.0 } else { (d as f64 / t as f64).min(1.0) as f32 }
    }
    fn set_stage(&self, s: impl Into<String>) {
        if let Ok(mut g) = self.stage.lock() {
            *g = s.into();
        }
    }
    fn check_cancel(&self) -> io::Result<()> {
        if self.cancel.load(Ordering::Relaxed) { Err(io::Error::new(io::ErrorKind::Interrupted, "import cancelled")) } else { Ok(()) }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ImportError {
    #[error(
        "no game folder found in {0}: pick the folder of your installed Need for Speed: Porsche Unleashed (the one that holds GameData and FEData), or a .zip of it"
    )]
    NoGame(PathBuf),
    #[error("{path}: {source}")]
    Io { path: PathBuf, #[source] source: io::Error },
    #[error("{path}: not a readable .zip ({msg})")]
    Zip { path: PathBuf, msg: String },
    #[error("import cancelled")]
    Cancelled,
}

fn io_at(path: &Path) -> impl FnOnce(io::Error) -> ImportError + '_ {
    move |source| if source.kind() == io::ErrorKind::Interrupted { ImportError::Cancelled } else { ImportError::Io { path: path.to_path_buf(), source } }
}

/// A file the player's OS left beside the data, never game data.
pub fn is_junk(name: &str) -> bool {
    name.starts_with("._") || name.eq_ignore_ascii_case(".DS_Store") || name.eq_ignore_ascii_case("Thumbs.db") || name == "__MACOSX"
}

/// The entry of `dir` named `name` in any case.
fn child_ci(dir: &Path, name: &str, want_dir: bool) -> Option<PathBuf> {
    let exact = dir.join(name);
    if (want_dir && exact.is_dir()) || (!want_dir && exact.is_file()) {
        return Some(exact);
    }
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let n = e.file_name();
        let n = n.to_str()?;
        let p = e.path();
        (n.eq_ignore_ascii_case(name) && !is_junk(n) && if want_dir { p.is_dir() } else { p.is_file() }).then_some(p)
    })
}

/// Whether `dir` is a game folder: it holds `GameData` and `FEData` (any case).
pub fn is_game_root(dir: &Path) -> bool {
    DIRS.iter().all(|d| child_ci(dir, d, true).is_some())
}

/// The game folder at or below `dir` (breadth first, at most [`SEARCH_DEPTH`] levels down); `None` when there is none.
pub fn find_game_root(dir: &Path) -> Option<PathBuf> {
    let mut level = vec![dir.to_path_buf()];
    for _ in 0..=SEARCH_DEPTH {
        let mut next = Vec::new();
        for d in level {
            if is_game_root(&d) {
                return Some(d);
            }
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            let mut subs: Vec<PathBuf> = rd
                .flatten()
                .filter(|e| e.file_name().to_str().is_some_and(|n| !is_junk(n)) && e.path().is_dir())
                .map(|e| e.path())
                .collect();
            subs.sort();
            next.extend(subs);
        }
        level = next;
    }
    None
}

/// Every `(source, relative destination)` file of the game folder `root` the import takes.
fn plan_folder(root: &Path) -> io::Result<Vec<(PathBuf, PathBuf, u64)>> {
    let mut out = Vec::new();
    let mut have_exe = false;
    for (src, dst) in FILES {
        if *dst == "nfs5.exe" && have_exe {
            continue;
        }
        if let Some(p) = child_ci(root, src, false) {
            have_exe |= *dst == "nfs5.exe";
            let len = p.metadata()?.len();
            out.push((p, PathBuf::from(dst), len));
        }
    }
    for d in DIRS {
        let Some(top) = child_ci(root, d, true) else { continue };
        let mut stack = vec![(top, PathBuf::from(d))];
        while let Some((src, rel)) = stack.pop() {
            for e in std::fs::read_dir(&src)?.flatten() {
                let name = e.file_name();
                let Some(n) = name.to_str() else { continue };
                if is_junk(n) {
                    continue;
                }
                let ty = e.file_type()?;
                let p = e.path();
                // a symlink is followed for what it names (`metadata` below), so a linked folder counts
                if ty.is_dir() || (ty.is_symlink() && p.is_dir()) {
                    stack.push((p, rel.join(n)));
                } else if p.is_file() {
                    let len = p.metadata()?.len();
                    out.push((p, rel.join(n), len));
                }
            }
        }
    }
    Ok(out)
}

/// The staging folder beside `dest`.
fn staging(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".importing");
    PathBuf::from(s)
}

/// Puts a complete staging folder in place of `dest`.
fn commit(stage: &Path, dest: &Path) -> Result<(), ImportError> {
    if dest.exists() {
        std::fs::remove_dir_all(dest).map_err(io_at(dest))?;
    }
    std::fs::rename(stage, dest).map_err(io_at(dest))
}

fn fresh_staging(dest: &Path) -> Result<PathBuf, ImportError> {
    let stage = staging(dest);
    if stage.exists() {
        std::fs::remove_dir_all(&stage).map_err(io_at(&stage))?;
    }
    std::fs::create_dir_all(&stage).map_err(io_at(&stage))?;
    Ok(stage)
}

/// Copies the game found at or below `src` into `dest` (replacing what was there). With `move_files` the files are
/// renamed instead of copied where the volume allows it (a folder the player dropped into the app's own
/// `Documents`): no second copy of the 700 MB is ever made.
pub fn import_folder(src: &Path, dest: &Path, move_files: bool, progress: &Progress) -> Result<PathBuf, ImportError> {
    progress.set_stage("Looking for the game folder");
    let root = find_game_root(src).ok_or_else(|| ImportError::NoGame(src.to_path_buf()))?;
    tracing::info!(root = %root.display(), dest = %dest.display(), move_files, "importing the game data");
    let plan = plan_folder(&root).map_err(io_at(&root))?;
    progress.total.store(plan.iter().map(|p| p.2).sum(), Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let stage = fresh_staging(dest)?;
    let r = (|| {
        let mut buf = vec![0u8; 1 << 20];
        for (from, rel, len) in &plan {
            progress.check_cancel().map_err(io_at(from))?;
            progress.set_stage(format!("Copying {}", rel.display()));
            let to = stage.join(rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(io_at(parent))?;
            }
            if move_files && std::fs::rename(from, &to).is_ok() {
                progress.done.fetch_add(*len, Ordering::Relaxed);
                continue;
            }
            copy_file(from, &to, &mut buf, progress).map_err(io_at(from))?;
        }
        Ok(())
    })();
    if let Err(e) = r {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(e);
    }
    commit(&stage, dest)?;
    Ok(root)
}

fn copy_file(from: &Path, to: &Path, buf: &mut [u8], progress: &Progress) -> io::Result<()> {
    use std::io::{Read, Write};
    let mut r = std::fs::File::open(from)?;
    let mut w = std::fs::File::create(to)?;
    loop {
        let n = r.read(buf)?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n])?;
        progress.done.fetch_add(n as u64, Ordering::Relaxed);
        progress.check_cancel()?;
    }
    Ok(())
}

/// The prefix (the entry path of the game folder, `""` at the archive's top) inside a zip whose entry names are
/// `names`: the shallowest folder that holds both `GameData/` and `FEData/` in any case.
pub fn zip_root<'a>(names: impl IntoIterator<Item = &'a str>) -> Option<String> {
    use std::collections::{BTreeMap, BTreeSet};
    // parent folder (lowercase path) -> the two flags, keeping the parent's original spelling
    let mut found: BTreeMap<String, (String, BTreeSet<usize>)> = BTreeMap::new();
    for name in names {
        let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
        if parts.iter().any(|p| is_junk(p)) {
            continue;
        }
        // every component that is `GameData` / `FEData` and has something below it (or is a folder entry)
        for (i, p) in parts.iter().enumerate() {
            let Some(k) = DIRS.iter().position(|d| d.eq_ignore_ascii_case(p)) else { continue };
            let is_dir = i + 1 < parts.len() || name.ends_with('/');
            if !is_dir {
                continue;
            }
            let parent = parts[..i].join("/");
            found.entry(parent.to_ascii_lowercase()).or_insert_with(|| (parent.clone(), BTreeSet::new())).1.insert(k);
        }
    }
    found.into_values().filter(|(_, k)| k.len() == DIRS.len()).map(|(p, _)| p).min_by_key(|p| (p.matches('/').count() + usize::from(!p.is_empty()), p.clone()))
}

/// Where the zip entry `name` goes under the game folder whose prefix is `root`, normalised (`GameData` / `FEData`
/// in their canonical case, the executable as `nfs5.exe`); `None` for an entry the import does not take.
pub fn zip_target(root: &str, name: &str) -> Option<PathBuf> {
    let parts: Vec<&str> = name.split(['/', '\\']).filter(|p| !p.is_empty()).collect();
    let root_parts: Vec<&str> = root.split('/').filter(|p| !p.is_empty()).collect();
    if parts.len() <= root_parts.len() || !parts.iter().zip(&root_parts).all(|(a, b)| a.eq_ignore_ascii_case(b)) {
        return None;
    }
    let rest = &parts[root_parts.len()..];
    if rest.iter().any(|p| is_junk(p) || *p == ".." || *p == ".") {
        return None;
    }
    if rest.len() == 1 {
        return FILES.iter().find(|(s, _)| s.eq_ignore_ascii_case(rest[0])).map(|(_, d)| PathBuf::from(d));
    }
    let dir = DIRS.iter().find(|d| d.eq_ignore_ascii_case(rest[0]))?;
    let mut p = PathBuf::from(dir);
    p.extend(&rest[1..]);
    Some(p)
}

/// Unpacks the game out of the `.zip` at `zip_path` into `dest` (replacing what was there).
#[cfg(feature = "import")]
pub fn import_zip(zip_path: &Path, dest: &Path, progress: &Progress) -> Result<(), ImportError> {
    let zerr = |e: zip::result::ZipError| ImportError::Zip { path: zip_path.to_path_buf(), msg: e.to_string() };
    progress.set_stage("Reading the archive");
    let file = std::fs::File::open(zip_path).map_err(io_at(zip_path))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(zerr)?;
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    let root = zip_root(names.iter().map(String::as_str)).ok_or_else(|| ImportError::NoGame(zip_path.to_path_buf()))?;
    tracing::info!(zip = %zip_path.display(), root, dest = %dest.display(), "unpacking the game data");
    // the executable: `nfs5.exe` before `Porsche.exe`, as for a folder
    let mut targets: Vec<(usize, PathBuf)> = Vec::new();
    let mut exe: Option<(usize, usize)> = None;
    let mut total = 0u64;
    for i in 0..zip.len() {
        let e = zip.by_index_raw(i).map_err(zerr)?;
        if e.is_dir() {
            continue;
        }
        let Ok(name) = e.name().map(|n| n.into_owned()) else { continue };
        let Some(t) = zip_target(&root, &name) else { continue };
        if t == Path::new("nfs5.exe") {
            let rank = FILES.iter().position(|(s, _)| name.rsplit(['/', '\\']).next().is_some_and(|n| n.eq_ignore_ascii_case(s))).unwrap_or(usize::MAX);
            if exe.is_some_and(|(r, _)| r <= rank) {
                continue;
            }
            if let Some((_, old)) = exe {
                targets.retain(|(j, _)| *j != old);
            }
            exe = Some((rank, i));
        }
        total += e.size();
        targets.push((i, t));
    }
    progress.total.store(total, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let stage = fresh_staging(dest)?;
    let r = (|| {
        let mut buf = vec![0u8; 1 << 20];
        for (i, rel) in &targets {
            progress.check_cancel().map_err(io_at(zip_path))?;
            progress.set_stage(format!("Unpacking {}", rel.display()));
            let to = stage.join(rel);
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent).map_err(io_at(parent))?;
            }
            let mut e = zip.by_index(*i).map_err(zerr)?;
            let mut w = std::fs::File::create(&to).map_err(io_at(&to))?;
            loop {
                use std::io::{Read, Write};
                let n = e.read(&mut buf).map_err(io_at(zip_path))?;
                if n == 0 {
                    break;
                }
                w.write_all(&buf[..n]).map_err(io_at(&to))?;
                progress.done.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
        Ok(())
    })();
    if let Err(e) = r {
        let _ = std::fs::remove_dir_all(&stage);
        return Err(e);
    }
    commit(&stage, dest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zip_roots() {
        let names = ["NFS/GameData/Track/a.frd", "NFS/FEData/Art/x.fsh", "NFS/version.txt", "__MACOSX/NFS/._GameData"];
        assert_eq!(zip_root(names).as_deref(), Some("NFS"));
        assert_eq!(zip_root(["gamedata/a", "FEDATA/b"]).as_deref(), Some(""));
        assert_eq!(zip_root(["GameData/a"]), None);
        // the shallowest game folder wins
        assert_eq!(zip_root(["a/b/GameData/x", "a/b/FEData/y", "a/GameData/x", "a/FEData/y"]).as_deref(), Some("a"));
        assert_eq!(zip_target("NFS", "nfs/gamedata/Track/a.frd"), Some(PathBuf::from("GameData/Track/a.frd")));
        assert_eq!(zip_target("NFS", "NFS/Porsche.exe"), Some(PathBuf::from("nfs5.exe")));
        assert_eq!(zip_target("NFS", "NFS/Drivers/x.dll"), None);
        assert_eq!(zip_target("NFS", "NFS/GameData/._x"), None);
        assert_eq!(zip_target("NFS", "NFS/GameData/../../evil"), None);
        assert_eq!(zip_target("", "FEData/Art/x"), Some(PathBuf::from("FEData/Art/x")));
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("nfs-import-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_folder_one_level_too_high_in_any_case() {
        let src = tmp("src");
        let game = src.join("Games").join("NFS");
        std::fs::create_dir_all(game.join("GAMEDATA/Track")).unwrap();
        std::fs::create_dir_all(game.join("fedata/Art")).unwrap();
        std::fs::create_dir_all(game.join("Drivers")).unwrap();
        std::fs::write(game.join("GAMEDATA/Track/a.frd"), b"track").unwrap();
        std::fs::write(game.join("GAMEDATA/._a.frd"), b"junk").unwrap();
        std::fs::write(game.join("fedata/Art/x.fsh"), b"art").unwrap();
        std::fs::write(game.join("Porsche.exe"), b"MZ").unwrap();
        std::fs::write(game.join("Version.txt"), b"3.5").unwrap();
        std::fs::write(game.join("Drivers/x.dll"), b"no").unwrap();
        let dest = tmp("dest").join("game");
        std::fs::create_dir_all(dest.join("old")).unwrap();
        let p = Progress::default();
        let root = import_folder(&src, &dest, false, &p).unwrap();
        assert_eq!(root, game);
        assert_eq!(std::fs::read(dest.join("GameData/Track/a.frd")).unwrap(), b"track");
        assert_eq!(std::fs::read(dest.join("FEData/Art/x.fsh")).unwrap(), b"art");
        assert_eq!(std::fs::read(dest.join("nfs5.exe")).unwrap(), b"MZ");
        assert_eq!(std::fs::read(dest.join("version.txt")).unwrap(), b"3.5");
        assert!(!dest.join("GameData/._a.frd").exists() && !dest.join("Drivers").exists() && !dest.join("old").exists());
        assert!(!staging(&dest).exists());
        assert_eq!(p.done.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed));
        assert!(matches!(import_folder(&src.join("Games/NFS/Drivers"), &dest, false, &p), Err(ImportError::NoGame(_))));
        // the failed import left the earlier one alone
        assert!(dest.join("GameData/Track/a.frd").is_file());
    }

    #[cfg(feature = "import")]
    #[test]
    fn a_zip() {
        use std::io::Write;
        let dir = tmp("zip");
        let zp = dir.join("game.zip");
        {
            let mut z = zip::ZipWriter::new(std::fs::File::create(&zp).unwrap());
            let o = zip::write::SimpleFileOptions::default();
            for (n, d) in [("PU/GameData/Track/a.frd", &b"track"[..]), ("PU/FEData/Art/x.fsh", b"art"), ("PU/Porsche.exe", b"MZp"), ("PU/nfs5.exe", b"MZn"), ("PU/Drivers/x", b"no")] {
                z.start_file(n, o).unwrap();
                z.write_all(d).unwrap();
            }
            z.finish().unwrap();
        }
        let dest = dir.join("game");
        import_zip(&zp, &dest, &Progress::default()).unwrap();
        assert_eq!(std::fs::read(dest.join("GameData/Track/a.frd")).unwrap(), b"track");
        assert_eq!(std::fs::read(dest.join("nfs5.exe")).unwrap(), b"MZn");
        assert!(!dest.join("Drivers").exists());
    }
}
