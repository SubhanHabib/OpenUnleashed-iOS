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
    #[error("no texture pack in {0}: a pack is PNG files named after the texture they replace (16 hex digits and .png)")]
    NoPack(PathBuf),
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

/// Whether the `.zip` at `zip_path` holds a game folder (reads only its list of entries). A saves backup or any
/// other archive the player keeps next to the game is not one.
#[cfg(feature = "import")]
pub fn zip_holds_game(zip_path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(zip_path) else { return false };
    let Ok(zip) = zip::ZipArchive::new(std::io::BufReader::new(file)) else { return false };
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    zip_root(names.iter().map(String::as_str)).is_some()
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

/// PLATFORM GLUE (iOS save backup): the files of `dir` and its sub-folders into a new `.zip` at `zip_path`
/// (paths relative to `dir`, `/`-separated); the number of files.
#[cfg(feature = "import")]
pub fn zip_folder(dir: &Path, zip_path: &Path) -> Result<usize, ImportError> {
    use std::io::Write;
    let zerr = |e: zip::result::ZipError| ImportError::Zip { path: zip_path.to_path_buf(), msg: e.to_string() };
    let file = std::fs::File::create(zip_path).map_err(io_at(zip_path))?;
    let mut zip = zip::ZipWriter::new(std::io::BufWriter::new(file));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let mut stack = vec![dir.to_path_buf()];
    let mut n = 0;
    while let Some(d) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&d).map_err(io_at(&d))?.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            if is_junk(&e.file_name().to_string_lossy()) {
                continue;
            }
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            let Ok(rel) = p.strip_prefix(dir) else { continue };
            let name = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect::<Vec<_>>().join("/");
            let data = std::fs::read(&p).map_err(io_at(&p))?;
            zip.start_file(name, opts).map_err(zerr)?;
            zip.write_all(&data).map_err(io_at(zip_path))?;
            n += 1;
        }
    }
    zip.finish().map_err(zerr)?.flush().map_err(io_at(zip_path))?;
    Ok(n)
}

/// PLATFORM GLUE (iOS save restore): unpacks a `.zip` of a folder into `dest`, replacing it. The folder in the
/// archive is the one that holds the first entry `is_key` accepts (a backup of [`zip_folder`], or one the Files
/// app compressed, with the folder's own name in front); entries outside it, and any that would leave it, are
/// skipped. Nothing is changed when the archive has no such entry or cannot be read. The number of files.
#[cfg(feature = "import")]
pub fn unzip_folder(zip_path: &Path, dest: &Path, is_key: impl Fn(&str) -> bool) -> Result<usize, ImportError> {
    let zerr = |e: zip::result::ZipError| ImportError::Zip { path: zip_path.to_path_buf(), msg: e.to_string() };
    let file = std::fs::File::open(zip_path).map_err(io_at(zip_path))?;
    let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(zerr)?;
    let mut root: Option<PathBuf> = None;
    for i in 0..zip.len() {
        let e = zip.by_index_raw(i).map_err(zerr)?;
        let Some(p) = e.enclosed_name() else { continue };
        if !e.is_dir() && p.file_name().is_some_and(|n| is_key(&n.to_string_lossy())) && !p.components().any(|c| is_junk(&c.as_os_str().to_string_lossy())) {
            root = Some(p.parent().map(Path::to_path_buf).unwrap_or_default());
            break;
        }
    }
    let root = root.ok_or_else(|| ImportError::Zip { path: zip_path.to_path_buf(), msg: "no saved game in it".into() })?;
    let staging = dest.with_extension("restoring");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(io_at(&staging))?;
    let r = (|| {
        let mut n = 0;
        for i in 0..zip.len() {
            let mut e = zip.by_index(i).map_err(zerr)?;
            let Some(p) = e.enclosed_name() else { continue };
            let Ok(rel) = p.strip_prefix(&root) else { continue };
            if e.is_dir() || rel.as_os_str().is_empty() || rel.components().any(|c| is_junk(&c.as_os_str().to_string_lossy())) {
                continue;
            }
            let out = staging.join(rel);
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(io_at(parent))?;
            }
            let mut f = std::fs::File::create(&out).map_err(io_at(&out))?;
            io::copy(&mut e, &mut f).map_err(io_at(&out))?;
            n += 1;
        }
        Ok(n)
    })();
    match r {
        Ok(n) => {
            let _ = std::fs::remove_dir_all(dest);
            std::fs::rename(&staging, dest).map_err(io_at(dest))?;
            Ok(n)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// `0123456789abcdef.png` (any case): the name of a texture-pack file.
pub fn is_pack_texture(name: &str) -> bool {
    name.len() == 20 && name[16..].eq_ignore_ascii_case(".png") && name.as_bytes()[..16].iter().all(u8::is_ascii_hexdigit)
}

/// The folder name a pack imported from `src` gets: its file or folder name without `.zip`, without a leading `_` or
/// `.` (the pack loader skips such folders), `pack` when nothing is left.
pub fn pack_name(src: &Path) -> String {
    let n = src.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let n = if n.len() >= 4 && n.is_char_boundary(n.len() - 4) && n[n.len() - 4..].eq_ignore_ascii_case(".zip") { &n[..n.len() - 4] } else { &n[..] };
    let n = n.trim_start_matches(['_', '.']).trim();
    if n.is_empty() { "pack".into() } else { n.replace(['/', '\\', ':'], "-") }
}

/// PLATFORM GLUE (iOS texture packs): the pack at `src` (a `.zip` or a folder) into `textures/<pack_name>`, replacing
/// a pack of that name. Only texture files ([`is_pack_texture`]) are taken, from anywhere in it, into one folder;
/// when two have the same name, the one whose path sorts first wins. Folders whose name starts with `_` or `.`
/// are skipped, as the pack loader skips them. Everything goes into a `_`-named staging folder first, so a failed
/// import leaves the old pack as it was. The pack's folder name and the number of textures.
#[cfg(feature = "import")]
pub fn import_texture_pack(src: &Path, textures: &Path, progress: &Progress) -> Result<(String, usize), ImportError> {
    use std::collections::BTreeMap;
    let name = pack_name(src);
    let dest = textures.join(&name);
    let staging = textures.join(format!("_importing {name}"));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(io_at(&staging))?;
    let skipped = |c: &str| is_junk(c) || c.starts_with('_') || c.starts_with('.');
    let r = (|| {
        let mut n = 0;
        if src.is_dir() {
            progress.set_stage("Looking for textures");
            // file name (lower case) -> the source path that sorts first
            let mut found: BTreeMap<String, PathBuf> = BTreeMap::new();
            let mut stack = vec![(src.to_path_buf(), 0)];
            while let Some((d, depth)) = stack.pop() {
                for e in std::fs::read_dir(&d).map_err(io_at(&d))?.flatten() {
                    let p = e.path();
                    let fname = e.file_name().to_string_lossy().into_owned();
                    if p.is_dir() {
                        if depth < 6 && !skipped(&fname) {
                            stack.push((p, depth + 1));
                        }
                    } else if is_pack_texture(&fname) {
                        let k = fname.to_ascii_lowercase();
                        if found.get(&k).is_none_or(|old| p < *old) {
                            found.insert(k, p);
                        }
                    }
                }
            }
            progress.total.store(found.len() as u64, Ordering::Relaxed);
            for (k, p) in &found {
                progress.check_cancel().map_err(io_at(p))?;
                let out = staging.join(k);
                std::fs::copy(p, &out).map_err(io_at(p))?;
                n += 1;
                progress.done.store(n as u64, Ordering::Relaxed);
            }
        } else {
            let zerr = |e: zip::result::ZipError| ImportError::Zip { path: src.to_path_buf(), msg: e.to_string() };
            progress.set_stage("Reading the archive");
            let file = std::fs::File::open(src).map_err(io_at(src))?;
            let mut zip = zip::ZipArchive::new(std::io::BufReader::new(file)).map_err(zerr)?;
            // file name (lower case) -> (entry path, entry index), the path that sorts first
            let mut found: BTreeMap<String, (PathBuf, usize)> = BTreeMap::new();
            for i in 0..zip.len() {
                let e = zip.by_index_raw(i).map_err(zerr)?;
                let Some(p) = e.enclosed_name() else { continue };
                if e.is_dir() {
                    continue;
                }
                let Some(fname) = p.file_name().map(|f| f.to_string_lossy().into_owned()) else { continue };
                let parents_ok = p.parent().is_none_or(|d| d.components().all(|c| !skipped(&c.as_os_str().to_string_lossy())));
                if !parents_ok || !is_pack_texture(&fname) {
                    continue;
                }
                let k = fname.to_ascii_lowercase();
                if found.get(&k).is_none_or(|(old, _)| p < *old) {
                    found.insert(k, (p, i));
                }
            }
            progress.total.store(found.len() as u64, Ordering::Relaxed);
            for (k, (_, i)) in &found {
                let out = staging.join(k);
                progress.check_cancel().map_err(io_at(&out))?;
                let mut e = zip.by_index(*i).map_err(zerr)?;
                let mut f = std::fs::File::create(&out).map_err(io_at(&out))?;
                io::copy(&mut e, &mut f).map_err(io_at(&out))?;
                n += 1;
                progress.done.store(n as u64, Ordering::Relaxed);
            }
        }
        if n == 0 {
            return Err(ImportError::NoPack(src.to_path_buf()));
        }
        Ok(n)
    })();
    match r {
        Ok(n) => {
            let _ = std::fs::remove_dir_all(&dest);
            std::fs::rename(&staging, &dest).map_err(io_at(&dest))?;
            Ok((name, n))
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

#[cfg(all(test, feature = "import"))]
mod pack_tests {
    use super::*;

    #[test]
    fn pack_names() {
        assert!(is_pack_texture("0123456789abcdef.png") && is_pack_texture("0123456789ABCDEF.PNG"));
        assert!(!is_pack_texture("0123456789abcdeg.png") && !is_pack_texture("README.txt") && !is_pack_texture("123.png"));
        assert_eq!(pack_name(Path::new("/x/esrgan.zip")), "esrgan");
        assert_eq!(pack_name(Path::new("/x/_dump")), "dump");
        assert_eq!(pack_name(Path::new("/x/.zip")), "pack");
    }

    #[test]
    fn a_pack_imports_from_a_zip_and_a_folder() {
        let base = std::env::temp_dir().join(format!("ou-pack-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("My Pack");
        std::fs::create_dir_all(src.join("a/b")).unwrap();
        std::fs::create_dir_all(src.join("_skip")).unwrap();
        std::fs::write(src.join("0000000000000001.png"), b"one").unwrap();
        std::fs::write(src.join("a/b/0000000000000002.png"), b"two").unwrap();
        std::fs::write(src.join("a/0000000000000002.png"), b"two-first").unwrap();
        std::fs::write(src.join("_skip/0000000000000003.png"), b"skipped").unwrap();
        std::fs::write(src.join("notes.txt"), b"x").unwrap();
        let out = std::env::temp_dir().join(format!("ou-pack-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&out);
        let tex = out.join("Textures");
        std::fs::create_dir_all(&tex).unwrap();
        let p = Progress::default();
        assert_eq!(import_texture_pack(&src, &tex, &p).unwrap(), ("My Pack".into(), 2));
        assert_eq!(std::fs::read(tex.join("My Pack/0000000000000002.png")).unwrap(), b"two-first");
        assert!(!tex.join("My Pack/0000000000000003.png").exists() && !tex.join("My Pack/notes.txt").exists());
        // the Files app's "Compress": the folder's name in front; it replaces the pack of the same name
        let zip = out.join("My Pack.zip");
        zip_folder(&base, &zip).unwrap();
        std::fs::write(tex.join("My Pack/old.png"), b"old").unwrap();
        let (name, n) = import_texture_pack(&zip, &tex, &p).unwrap();
        assert_eq!((name.as_str(), n), ("My Pack", 2));
        assert!(!tex.join("My Pack/old.png").exists());
        assert_eq!(std::fs::read(tex.join("My Pack/0000000000000001.png")).unwrap(), b"one");
        // no textures in it: an error, and the pack there stays
        let empty = base.join("Empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(import_texture_pack(&empty, &tex, &p), Err(ImportError::NoPack(_))));
        assert!(!tex.join("Empty").exists() && !tex.join("_importing Empty").exists());
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&out);
    }
}

#[cfg(all(test, feature = "import"))]
mod backup_tests {
    use super::*;

    #[test]
    fn a_backup_restores_and_a_compressed_folder_too() {
        let base = std::env::temp_dir().join(format!("ou-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let saves = base.join("SaveData");
        std::fs::create_dir_all(saves.join("sub")).unwrap();
        std::fs::write(saves.join("player.lst"), b"list").unwrap();
        std::fs::write(saves.join("0Me.sav"), b"career").unwrap();
        std::fs::write(saves.join("sub/x.bin"), b"x").unwrap();
        let zip = base.join("b.zip");
        assert_eq!(zip_folder(&saves, &zip).unwrap(), 3);
        let key = |n: &str| n.eq_ignore_ascii_case("player.lst");
        let back = base.join("Restored");
        std::fs::create_dir_all(&back).unwrap();
        std::fs::write(back.join("old.sav"), b"old").unwrap();
        assert_eq!(unzip_folder(&zip, &back, key).unwrap(), 3);
        assert_eq!(std::fs::read(back.join("0Me.sav")).unwrap(), b"career");
        assert_eq!(std::fs::read(back.join("sub/x.bin")).unwrap(), b"x");
        assert!(!back.join("old.sav").exists());
        // the Files app's "Compress": the folder's name in front
        let outer = base.join("Outer");
        std::fs::create_dir_all(&outer).unwrap();
        std::fs::rename(&saves, outer.join("SaveData")).unwrap();
        let zip2 = base.join("c.zip");
        zip_folder(&outer, &zip2).unwrap();
        let again = base.join("Again");
        assert_eq!(unzip_folder(&zip2, &again, key).unwrap(), 3);
        assert!(again.join("player.lst").exists());
        // no save in it: nothing changes
        let empty = base.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("a.txt"), b"a").unwrap();
        let zip3 = base.join("d.zip");
        zip_folder(&empty, &zip3).unwrap();
        assert!(unzip_folder(&zip3, &again, key).is_err());
        assert!(again.join("player.lst").exists());
        // a saves backup is not mistaken for a .zip of the game; a zipped game folder is one
        assert!(!zip_holds_game(&zip) && !zip_holds_game(&zip2));
        let game = base.join("Game/Porsche");
        std::fs::create_dir_all(game.join("GameData")).unwrap();
        std::fs::create_dir_all(game.join("FEData")).unwrap();
        std::fs::write(game.join("GameData/a.bin"), b"a").unwrap();
        std::fs::write(game.join("FEData/b.bin"), b"b").unwrap();
        let zip4 = base.join("g.zip");
        zip_folder(&base.join("Game"), &zip4).unwrap();
        assert!(zip_holds_game(&zip4));
        let _ = std::fs::remove_dir_all(&base);
    }
}
