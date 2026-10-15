//! Finding iPods among the drives the desktop has mounted, and renaming them.

use std::path::PathBuf;

use gtk::gio;
use gtk::prelude::*;

use crate::gpod;

/// A mounted iPod.
pub struct Ipod {
    /// The name set in iTunes, e.g. "dylan's iPod". Falls back to the drive
    /// label (e.g. "DYLAN_S IPO") when the database can't be read.
    pub name: String,
    /// Where the drive is mounted, e.g. /run/media/dylan/DYLAN_S IPO.
    pub mount_path: PathBuf,
    pub free_bytes: u64,
    pub total_bytes: u64,
}

/// Returns every mounted drive that looks like an iPod.
///
/// An iPod is recognised by its music database folder, `iPod_Control/iTunes`.
pub fn find_ipods(monitor: &gio::VolumeMonitor) -> Vec<Ipod> {
    monitor
        .mounts()
        .into_iter()
        .filter_map(|mount| {
            // `?` inside filter_map: if the mount has no local path, skip it.
            let path = mount.root().path()?;
            if !path.join("iPod_Control").join("iTunes").is_dir() {
                return None;
            }

            let (free_bytes, total_bytes) = disk_space(&mount.root()).unwrap_or((0, 0));
            // `.ok()` turns a Result into an Option, dropping the error;
            // `and_then` only runs when there is a value.
            let db_name = gpod::Database::open(&path).ok().and_then(|db| db.name());
            Some(Ipod {
                name: db_name.unwrap_or_else(|| mount.name().to_string()),
                mount_path: path,
                free_bytes,
                total_bytes,
            })
        })
        .collect()
}

/// Free and total space of the filesystem `root` lives on, in bytes.
fn disk_space(root: &gio::File) -> Result<(u64, u64), gtk::glib::Error> {
    let attributes = format!(
        "{},{}",
        gio::FILE_ATTRIBUTE_FILESYSTEM_FREE,
        gio::FILE_ATTRIBUTE_FILESYSTEM_SIZE
    );
    let info = root.query_filesystem_info(&attributes, gio::Cancellable::NONE)?;
    Ok((
        info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE),
        info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_SIZE),
    ))
}

/// Gives the iPod mounted at `mount_path` a new name.
///
/// The database is opened, changed and saved on a background thread: saving
/// takes a second or two because the iPod is a slow USB drive. A `Database`
/// can't be passed between threads, so all of it happens inside the closure
/// and only the result comes back.
pub async fn rename(mount_path: PathBuf, new_name: String) -> Result<(), gpod::Error> {
    let done = gio::spawn_blocking(move || {
        let mut db = gpod::Database::open(&mount_path)?;
        db.set_name(&new_name)?;
        db.save()
    })
    .await;
    // The outer Result is only an Err if the thread panicked.
    done.unwrap_or_else(|_| Err(gpod::Error::new("the rename crashed")))
}
