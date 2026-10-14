//! Finding iPods among the drives the desktop has mounted.

use std::path::PathBuf;

use gtk::gio;
use gtk::prelude::*;

/// A mounted iPod.
pub struct Ipod {
    /// The drive label, e.g. "DYLAN'S IPO".
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
            Some(Ipod {
                name: mount.name().to_string(),
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
