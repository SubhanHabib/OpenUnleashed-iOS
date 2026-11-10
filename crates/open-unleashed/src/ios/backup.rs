//! **PLATFORM GLUE (iOS)**, not in the original: the saves out to the Files app and back.
//!
//! An iOS app's saves live in its own container, which only the app (and a device backup) can reach. The settings
//! page's **Export saves** zips `SaveData` and hands the `.zip` to the system's export sheet (iCloud Drive, another
//! folder, AirDrop); **Import saves** takes such a `.zip` back (or one the Files app compressed from a `SaveData`
//! folder) and replaces `SaveData` with it. The saves it replaces are zipped into `Documents/Save Backups` first, so
//! an import can be undone. The game reads its saves at start-up, so the app closes after an import.

use std::path::{Path, PathBuf};

/// The file in `SaveData` an archive must hold to be taken as a backup of it: the profile list.
fn is_key(name: &str) -> bool {
    name.eq_ignore_ascii_case("player.lst")
}

fn stamp() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    crate::app::photo::utc_stamp(secs)
}

/// `SaveData` zipped into the app's temporary folder, for the export sheet.
pub(crate) fn export() -> Result<PathBuf, String> {
    let saves = nfs_game::save::save_dir();
    if !saves.join("player.lst").exists() {
        return Err("There are no saves yet.".into());
    }
    let dir = std::env::temp_dir().join("save-export");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let zip = dir.join(format!("{} saves {}.zip", crate::brand::TITLE, stamp()));
    let n = nfs_assets::import::zip_folder(&saves, &zip).map_err(|e| e.to_string())?;
    tracing::info!(files = n, zip = %zip.display(), "saves exported");
    Ok(zip)
}

/// `SaveData` replaced with the archive at `zip`, after the saves it replaces are zipped into
/// `Documents/Save Backups`. The number of files restored.
pub(crate) fn import(zip: &Path) -> Result<usize, String> {
    let saves = nfs_game::save::save_dir();
    if saves.join("player.lst").exists()
        && let Some(docs) = nfs_assets::platform::documents_dir()
    {
        let dir = docs.join("Save Backups");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let before = dir.join(format!("before import {}.zip", stamp()));
        nfs_assets::import::zip_folder(&saves, &before).map_err(|e| format!("The current saves could not be backed up first: {e}"))?;
        tracing::info!(zip = %before.display(), "current saves backed up before the import");
    }
    let n = nfs_assets::import::unzip_folder(zip, &saves, is_key).map_err(|e| match e {
        nfs_assets::import::ImportError::Zip { msg, .. } if msg == "no saved game in it" => "That .zip holds no saves (no player.lst in it).".to_string(),
        e => e.to_string(),
    })?;
    tracing::info!(files = n, zip = %zip.display(), "saves imported");
    Ok(n)
}
