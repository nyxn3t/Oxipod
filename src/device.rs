//! Finding iPods among the drives the desktop has mounted, and renaming them.

use std::path::{Path, PathBuf};

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
    /// The kind of file system, like "vfat" (made by Windows) or "hfsplus"
    /// (made by a Mac), if it can be read.
    pub filesystem: Option<String>,
    /// Whether the drive is mounted read-only. An iPod formatted by a Mac
    /// usually is on Linux.
    pub read_only: bool,
    /// What libgpod knows about the model, like "Nano Video (3rd Gen.)" and "B261".
    pub generation: Option<String>,
    pub model_number: Option<String>,
}

/// What the drive an iPod is on has to say about itself.
struct Space {
    free: u64,
    total: u64,
    filesystem: Option<String>,
    read_only: bool,
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

            let space = disk_space(&mount.root()).ok();
            // `.ok()` turns a Result into an Option, dropping the error;
            // `and_then` only runs when there is a value.
            let db = gpod::Database::open(&path).ok();
            let db_name = db.as_ref().and_then(|db| db.name());
            let info = db.as_ref().map(|db| db.device_info());
            Some(Ipod {
                name: db_name.unwrap_or_else(|| mount.name().to_string()),
                mount_path: path,
                free_bytes: space.as_ref().map_or(0, |space| space.free),
                total_bytes: space.as_ref().map_or(0, |space| space.total),
                filesystem: space.as_ref().and_then(|space| space.filesystem.clone()),
                read_only: space.as_ref().is_some_and(|space| space.read_only),
                generation: info.as_ref().and_then(|info| info.generation.clone()),
                model_number: info.and_then(|info| info.model_number),
            })
        })
        .collect()
}

/// How an iPod's drive is mounted.
pub struct Access {
    /// The kind of file system, like "msdos" or "hfsplus", if it can be read.
    pub filesystem: Option<String>,
    /// Whether Oxipod can look at the iPod but not change it.
    pub read_only: bool,
}

/// How the drive that `path` is on is mounted. If that can't be found out,
/// the iPod counts as one that can be changed (it will say so when it can't).
pub fn access(path: &Path) -> Access {
    match disk_space(&gio::File::for_path(path)) {
        Ok(space) => Access { filesystem: space.filesystem, read_only: space.read_only },
        Err(_) => Access { filesystem: None, read_only: false },
    }
}

/// What to tell the person about an iPod that is mounted read-only: why it
/// probably is, and what can be done about it.
pub fn read_only_explanation(filesystem: Option<&str>) -> String {
    let looks = "so Oxipod can look at it and copy songs off it, but not change it";
    match filesystem {
        Some("hfsplus") => format!(
            "This iPod is formatted for a Mac (HFS+). Linux mounts those read-only as long as \
             their journal is turned on, which is how a Mac sets them up, {looks}.\n\n\
             To change that, turn the journal off on a Mac (in Terminal: diskutil disableJournal \
             /Volumes/NAME_OF_THE_IPOD), or reformat the iPod as FAT32, which erases everything \
             on it. If it is still read-only after that, the disk may need a repair."
        ),
        Some("msdos" | "vfat") => format!(
            "This iPod is mounted read-only, {looks}.\n\n\
             With a FAT iPod that usually means it was unplugged without being ejected, and the \
             computer wants the disk checked before it allows changes. Plug it in again, and if \
             it stays read-only, let a computer check the disk (fsck on Linux, Check Disk on \
             Windows). Eject it from Oxipod next time."
        ),
        Some(other) => format!(
            "This iPod is mounted read-only (its file system is {other}), {looks}.\n\n\
             If you didn't expect that, unplug the iPod and plug it in again."
        ),
        None => format!(
            "This iPod is mounted read-only, {looks}.\n\n\
             If you didn't expect that, unplug the iPod and plug it in again."
        ),
    }
}

/// Free space on the drive that `path` is on, in bytes, if it can be read.
pub fn free_space(path: &Path) -> Option<u64> {
    disk_space(&gio::File::for_path(path)).ok().map(|space| space.free)
}

/// Asks the filesystem `root` lives on for its space and kind.
fn disk_space(root: &gio::File) -> Result<Space, gtk::glib::Error> {
    let attributes = format!(
        "{},{},{},{}",
        gio::FILE_ATTRIBUTE_FILESYSTEM_FREE,
        gio::FILE_ATTRIBUTE_FILESYSTEM_SIZE,
        gio::FILE_ATTRIBUTE_FILESYSTEM_TYPE,
        gio::FILE_ATTRIBUTE_FILESYSTEM_READONLY
    );
    let info = root.query_filesystem_info(&attributes, gio::Cancellable::NONE)?;
    Ok(Space {
        free: info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_FREE),
        total: info.attribute_uint64(gio::FILE_ATTRIBUTE_FILESYSTEM_SIZE),
        filesystem: info.attribute_string(gio::FILE_ATTRIBUTE_FILESYSTEM_TYPE).map(String::from),
        read_only: info.boolean(gio::FILE_ATTRIBUTE_FILESYSTEM_READONLY),
    })
}

/// A line about an iPod for bug reports: its model, file system and space.
/// It leaves out the name and the mount path, which can have the person's
/// own name in them.
pub fn describe(ipod: &Ipod) -> String {
    let model = match (&ipod.generation, &ipod.model_number) {
        (Some(generation), Some(number)) => format!("{generation}, model {number}"),
        (Some(generation), None) => generation.clone(),
        (None, Some(number)) => format!("model {number}"),
        (None, None) => "unknown model".to_string(),
    };
    let filesystem = ipod.filesystem.as_deref().unwrap_or("unknown file system");
    let access = if ipod.read_only { ", read-only" } else { "" };
    format!(
        "{model}, {filesystem}{access}, {} free of {}",
        gtk::glib::format_size(ipod.free_bytes),
        gtk::glib::format_size(ipod.total_bytes)
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn ipod() -> Ipod {
        Ipod {
            name: "Dylan's iPod".into(),
            mount_path: "/run/media/dylan/IPOD".into(),
            free_bytes: 2_000_000_000,
            total_bytes: 4_000_000_000,
            filesystem: Some("hfsplus".into()),
            read_only: true,
            generation: Some("Nano Video (3rd Gen.)".into()),
            model_number: Some("B261".into()),
        }
    }

    #[test]
    fn describes_an_ipod_without_its_name_or_place() {
        let text = describe(&ipod());
        assert!(text.starts_with("Nano Video (3rd Gen.), model B261, hfsplus, read-only, "), "{text}");
        assert!(text.contains("free of"), "{text}");
        assert!(!text.contains("Dylan") && !text.contains("/run/media"), "{text}");
    }

    #[test]
    fn explains_a_read_only_mac_ipod_with_the_way_out() {
        let text = read_only_explanation(Some("hfsplus"));
        assert!(text.contains("Mac") && text.contains("diskutil disableJournal"), "{text}");
        assert!(text.contains("look at it and copy songs off it"), "{text}");
    }

    #[test]
    fn explains_a_read_only_fat_ipod_differently() {
        for filesystem in ["msdos", "vfat"] {
            let text = read_only_explanation(Some(filesystem));
            assert!(text.contains("ejected") && text.contains("fsck"), "{text}");
            assert!(!text.contains("diskutil"), "{text}");
        }
    }

    #[test]
    fn explains_a_read_only_ipod_whatever_the_file_system() {
        assert!(read_only_explanation(Some("ntfs")).contains("file system is ntfs"));
        let unknown = read_only_explanation(None);
        assert!(unknown.contains("read-only") && !unknown.contains("file system is"), "{unknown}");
    }

    #[test]
    fn a_folder_that_is_not_an_ipod_still_has_an_answer() {
        // Wherever this runs, asking about a folder that exists works and
        // asking about one that doesn't does not make a mess.
        let folder = tempfile::tempdir().unwrap();
        let _ = access(folder.path());
        assert!(!access(Path::new("/this/does/not/exist")).read_only);
    }

    // Says how a mounted iPod is mounted. Only looks. Run with:
    // OXIPOD_TEST_IPOD=/run/media/... cargo test how_a_mounted_ipod_is_mounted -- --ignored --nocapture
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn how_a_mounted_ipod_is_mounted() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let access = access(Path::new(&mount));
        println!("file system: {:?}, read-only: {}", access.filesystem, access.read_only);
        println!("{}", read_only_explanation(access.filesystem.as_deref()));
    }

    #[test]
    fn copes_with_what_it_does_not_know() {
        let unknown = Ipod { filesystem: None, read_only: false, generation: None, model_number: None, ..ipod() };
        assert!(describe(&unknown).starts_with("unknown model, unknown file system, "));
        let only_number = Ipod { generation: None, read_only: false, ..ipod() };
        assert!(describe(&only_number).starts_with("model B261, hfsplus, "));
    }
}
