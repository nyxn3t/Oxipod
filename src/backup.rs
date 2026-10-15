//! Backing up the part of an iPod that Oxipod writes to.
//!
//! Oxipod changes the iPod's database: the song list in `iPod_Control/iTunes`,
//! the covers in `iPod_Control/Artwork` and a few facts about the iPod in
//! `iPod_Control/Device`. A backup is a copy of those three folders. That is
//! small (about 80 MB on a nano with covers) next to the music itself, which
//! Oxipod only touches when a song is deleted ("Copy All to Computer" saves
//! the music). Nothing here uses GTK, so it can be tested with plain files.

use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::export;

/// The folders that are backed up, inside the iPod's `iPod_Control`.
const FOLDERS: [&str; 3] = ["iTunes", "Device", "Artwork"];

/// A file to back up: where it is, relative to the iPod, and how big it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub relative: PathBuf,
    pub size: u64,
}

/// What a finished backup looks like.
#[derive(Debug)]
pub struct Done {
    /// The new folder that holds the backup.
    pub folder: PathBuf,
    pub files: usize,
    pub bytes: u64,
}

/// Everything that would be backed up, in a steady order.
///
/// A folder the iPod doesn't have is skipped (an old iPod has no `Artwork`),
/// but without a song list there is nothing to back up.
pub fn plan(ipod: &Path) -> io::Result<Vec<Item>> {
    let control = ipod.join("iPod_Control");
    if !control.join("iTunes").is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this doesn't look like an iPod: there is no iPod_Control/iTunes folder",
        ));
    }
    let mut items = Vec::new();
    for folder in FOLDERS {
        collect(ipod, &control.join(folder), &mut items)?;
    }
    items.sort_by(|a, b| a.relative.cmp(&b.relative));
    Ok(items)
}

/// Adds the files in `folder`, and in the folders inside it, to `items`.
fn collect(ipod: &Path, folder: &Path, items: &mut Vec<Item>) -> io::Result<()> {
    let entries = match fs::read_dir(folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(ipod, &path, items)?;
        } else if kind.is_file() {
            // Every path found here starts with the iPod's own path.
            let relative = path.strip_prefix(ipod).unwrap_or(&path).to_path_buf();
            items.push(Item { relative, size: entry.metadata()?.len() });
        }
    }
    Ok(())
}

/// The name of the folder a backup goes into, like
/// "Nyxnet's iPod backup 2026-10-15 1345". `when` is the date and time.
pub fn folder_name(ipod_name: &str, when: &str) -> String {
    let name = export::clean(ipod_name);
    let name = if name.is_empty() { "iPod".to_string() } else { export::shorten(&name, 80) };
    format!("{name} backup {when}")
}

/// The note that goes into every backup, so it is clear later what it is
/// and how to put it back.
pub fn note(ipod_name: &str, when: &str) -> String {
    format!(
        "This is a backup of the database of \"{ipod_name}\", made by Oxipod on {when}.\n\
         \n\
         It has the song list (iTunes), the covers (Artwork) and a few facts about\n\
         the iPod itself (Device). It does not have the music.\n\
         \n\
         To put it back: plug in the iPod, close Oxipod, and copy the iPod_Control\n\
         folder from here onto the iPod, replacing the files there. Songs you added\n\
         after this backup will be gone from the iPod's list (Oxipod offers to clean\n\
         up their files), and songs you deleted since then stay deleted.\n"
    )
}

/// Copies the planned files into a new folder called `folder_name` inside
/// `destination`, keeping their places (`iPod_Control/iTunes/iTunesDB`...), and
/// adds `note` to it as `README.txt`.
///
/// `copied` counts the bytes done so far, so a progress bar can follow along
/// from another thread; setting `cancel` stops the backup. When anything goes
/// wrong, or the backup is cancelled, the unfinished folder is removed again,
/// so a half-made backup can't be mistaken for a real one.
pub fn back_up(
    ipod: &Path,
    items: &[Item],
    destination: &Path,
    folder_name: &str,
    note: &str,
    copied: &AtomicU64,
    cancel: &AtomicBool,
) -> io::Result<Done> {
    let folder = new_folder(destination, folder_name)?;
    // `and_then` runs the second step only if the first one worked.
    let result = copy_items(ipod, items, &folder, copied, cancel)
        .and_then(|bytes| fs::write(folder.join("README.txt"), note).map(|()| bytes));
    match result {
        Ok(bytes) => Ok(Done { folder, files: items.len(), bytes }),
        Err(error) => {
            // Best effort: if the leftovers can't be removed either, so be it.
            let _ = fs::remove_dir_all(&folder);
            Err(error)
        }
    }
}

/// Makes a new, empty folder called `name` inside `destination`. If there is
/// one with that name already, the new one is "name (2)" and so on: a backup
/// never goes into (or over) another one.
fn new_folder(destination: &Path, name: &str) -> io::Result<PathBuf> {
    let mut number = 1;
    loop {
        let folder = match number {
            1 => destination.join(name),
            number => destination.join(format!("{name} ({number})")),
        };
        // `create_dir` fails when the folder exists, which is what we want to know.
        match fs::create_dir(&folder) {
            Ok(()) => return Ok(folder),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => number += 1,
            Err(error) => return Err(error),
        }
    }
}

/// Copies the files and returns how many bytes that was.
fn copy_items(
    ipod: &Path,
    items: &[Item],
    folder: &Path,
    copied: &AtomicU64,
    cancel: &AtomicBool,
) -> io::Result<u64> {
    let mut total = 0;
    for item in items {
        let target = folder.join(&item.relative);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        // A big buffer makes the copy of the big cover files quicker.
        let mut source = BufReader::with_capacity(1 << 20, File::open(ipod.join(&item.relative))?);
        let mut file = Counting { inner: File::create(&target)?, copied, cancel };
        let written = io::copy(&mut source, &mut file)?;
        // Make sure it has reached the disk, then check that all of it did:
        // a backup that is wrong is worse than none.
        file.inner.sync_all()?;
        let on_disk = fs::metadata(&target)?.len();
        if on_disk != written {
            return Err(io::Error::other(format!(
                "{} came out {on_disk} bytes long instead of {written}",
                item.relative.display()
            )));
        }
        total += written;
    }
    Ok(total)
}

/// A file that counts the bytes written to it, and refuses to take more once
/// the backup is cancelled.
struct Counting<'a> {
    inner: File,
    copied: &'a AtomicU64,
    cancel: &'a AtomicBool,
}

impl Write for Counting<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            // Not `Interrupted`: `io::copy` would just try again.
            return Err(io::Error::other("cancelled"));
        }
        let written = self.inner.write(buffer)?;
        self.copied.fetch_add(written as u64, Ordering::Relaxed);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pretend iPod with a database, covers, and music that is not backed up.
    fn ipod(folder: &Path) -> PathBuf {
        let ipod = folder.join("ipod");
        for (path, content) in [
            ("iPod_Control/iTunes/iTunesDB", "the song list"),
            ("iPod_Control/iTunes/Play Counts", "plays"),
            ("iPod_Control/Device/SysInfo", "model"),
            ("iPod_Control/Artwork/ArtworkDB", "covers"),
            ("iPod_Control/Artwork/F1_1.ithmb", "a lot of pixels"),
            ("iPod_Control/Music/F00/ABCD.mp3", "the music"),
            ("Notes/readme.txt", "not part of the database"),
        ] {
            let path = ipod.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        ipod
    }

    fn names(items: &[Item]) -> Vec<String> {
        items.iter().map(|item| item.relative.display().to_string()).collect()
    }

    /// A whole backup with nobody cancelling it.
    fn run(ipod: &Path, destination: &Path, name: &str) -> io::Result<Done> {
        let items = plan(ipod)?;
        back_up(ipod, &items, destination, name, "a note", &AtomicU64::new(0), &AtomicBool::new(false))
    }

    #[test]
    fn plans_the_database_folders_and_nothing_else() {
        let folder = tempfile::tempdir().unwrap();
        let items = plan(&ipod(folder.path())).unwrap();
        assert_eq!(
            names(&items),
            [
                "iPod_Control/Artwork/ArtworkDB",
                "iPod_Control/Artwork/F1_1.ithmb",
                "iPod_Control/Device/SysInfo",
                "iPod_Control/iTunes/Play Counts",
                "iPod_Control/iTunes/iTunesDB",
            ]
        );
        assert_eq!(items.iter().map(|item| item.size).sum::<u64>(), 6 + 15 + 5 + 5 + 13);
    }

    #[test]
    fn copes_with_an_ipod_that_has_no_covers_or_device_folder() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        fs::remove_dir_all(ipod.join("iPod_Control/Artwork")).unwrap();
        fs::remove_dir_all(ipod.join("iPod_Control/Device")).unwrap();
        assert_eq!(names(&plan(&ipod).unwrap()), ["iPod_Control/iTunes/Play Counts", "iPod_Control/iTunes/iTunesDB"]);
    }

    #[test]
    fn refuses_a_drive_that_is_not_an_ipod() {
        let folder = tempfile::tempdir().unwrap();
        let error = plan(folder.path()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn copies_the_files_in_their_places_and_counts_the_bytes() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let destination = folder.path().join("backups");
        fs::create_dir(&destination).unwrap();

        let items = plan(&ipod).unwrap();
        let copied = AtomicU64::new(0);
        let done = back_up(&ipod, &items, &destination, "My iPod backup", "a note", &copied, &AtomicBool::new(false)).unwrap();

        assert_eq!(done.folder, destination.join("My iPod backup"));
        assert_eq!((done.files, done.bytes), (5, 44));
        assert_eq!(copied.load(Ordering::Relaxed), 44);
        for item in &items {
            let original = fs::read(ipod.join(&item.relative)).unwrap();
            assert_eq!(fs::read(done.folder.join(&item.relative)).unwrap(), original, "{:?}", item.relative);
        }
        assert_eq!(fs::read_to_string(done.folder.join("README.txt")).unwrap(), "a note");
        // The music is not in it.
        assert!(!done.folder.join("iPod_Control/Music").exists());
    }

    #[test]
    fn never_goes_into_another_backup() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let first = run(&ipod, folder.path(), "backup").unwrap();
        let second = run(&ipod, folder.path(), "backup").unwrap();
        let third = run(&ipod, folder.path(), "backup").unwrap();
        assert_eq!(first.folder, folder.path().join("backup"));
        assert_eq!(second.folder, folder.path().join("backup (2)"));
        assert_eq!(third.folder, folder.path().join("backup (3)"));
    }

    #[test]
    fn a_cancelled_backup_leaves_nothing_behind() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let destination = folder.path().join("backups");
        fs::create_dir(&destination).unwrap();
        let items = plan(&ipod).unwrap();
        let cancel = AtomicBool::new(true);
        assert!(back_up(&ipod, &items, &destination, "backup", "note", &AtomicU64::new(0), &cancel).is_err());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn a_backup_that_fails_halfway_leaves_nothing_behind() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let destination = folder.path().join("backups");
        fs::create_dir(&destination).unwrap();
        let items = plan(&ipod).unwrap();
        // The iPod loses a file while it is being backed up. It is the last
        // one in the list, so the others have been copied by then.
        fs::remove_file(ipod.join("iPod_Control/iTunes/iTunesDB")).unwrap();
        let result = back_up(&ipod, &items, &destination, "backup", "note", &AtomicU64::new(0), &AtomicBool::new(false));
        assert!(result.is_err());
        assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    }

    #[test]
    fn the_folder_name_is_safe_on_every_drive() {
        assert_eq!(folder_name("Nyxnet's iPod", "2026-10-15 1345"), "Nyxnet's iPod backup 2026-10-15 1345");
        assert_eq!(folder_name("Who: me? / you", "now"), "Who_ me_ _ you backup now");
        assert_eq!(folder_name("   ", "now"), "iPod backup now");
        assert!(folder_name(&"é".repeat(300), "now").len() < 120);
    }

    // Backs up a real iPod into a temporary folder and compares every file with
    // the original. Only reads from the iPod. Run with:
    // OXIPOD_TEST_IPOD=/run/media/... cargo test backs_up_a_mounted_ipod -- --ignored --nocapture
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn backs_up_a_mounted_ipod() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let ipod = Path::new(&mount);
        let items = plan(ipod).expect("could not plan the backup");
        let planned: u64 = items.iter().map(|item| item.size).sum();
        println!("{} files, {planned} bytes", items.len());
        assert!(items.iter().any(|item| item.relative.ends_with("iTunesDB")));

        let destination = tempfile::tempdir().unwrap();
        let start = std::time::Instant::now();
        let copied = AtomicU64::new(0);
        let done = back_up(ipod, &items, destination.path(), "test", "note", &copied, &AtomicBool::new(false)).unwrap();
        println!("backed up in {:?}: {} files, {} bytes", start.elapsed(), done.files, done.bytes);

        assert_eq!(done.files, items.len());
        assert_eq!(done.bytes, copied.load(Ordering::Relaxed));
        for item in &items {
            let original = fs::read(ipod.join(&item.relative)).unwrap();
            let copy = fs::read(done.folder.join(&item.relative)).unwrap();
            assert!(original == copy, "{} differs", item.relative.display());
        }
    }
}
