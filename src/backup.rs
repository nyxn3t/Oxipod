//! Backing up the part of an iPod that Oxipod writes to.
//!
//! Oxipod changes the iPod's database: the song list in `iPod_Control/iTunes`,
//! the covers in `iPod_Control/Artwork` and a few facts about the iPod in
//! `iPod_Control/Device`. A backup is a copy of those three folders. That is
//! small (about 80 MB on a nano with covers) next to the music itself, which
//! Oxipod only touches when a song is deleted ("Copy All to Computer" saves
//! the music). The same module puts a backup back. Nothing here uses GTK, so
//! it can be tested with plain files.

use std::fs::{self, File};
use std::io::{self, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::export;
use crate::gpod::{self, Database};

/// The folders that are backed up, inside the iPod's `iPod_Control`.
const FOLDERS: [&str; 3] = ["iTunes", "Device", "Artwork"];

/// The order the folders are put back in: the covers, then the iPod's own
/// facts, then the song list.
const RESTORE_ORDER: [&str; 3] = ["Artwork", "Device", "iTunes"];

/// Of the files in `iPod_Control/Device` a restore only puts back the iPod's
/// description. The rest (the settings, the clock, the alarms) belongs to the
/// iPod itself, and a restore of the song list shouldn't set them back.
const DEVICE_FILES_TO_RESTORE: [&str; 2] = ["SysInfo", "SysInfoExtended"];

/// Files in `iPod_Control/iTunes` that belong to one particular version of the
/// song list: the plays, and the playlists made on the iPod itself, point at
/// songs by their place in it. After a restore they would land on the wrong
/// songs, so the ones the backup doesn't have are removed.
const TIED_TO_THE_SONG_LIST: [&str; 2] = ["Play Counts", "OTGPlaylistInfo"];

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
    format!("{} backup {when}", safe_name(ipod_name))
}

/// The name of the folder for the copy Oxipod makes of what is on the iPod
/// right before it restores another backup, like
/// "Nyxnet's iPod before restore 2026-10-15 1530".
pub fn safety_folder_name(ipod_name: &str, when: &str) -> String {
    format!("{} before restore {when}", safe_name(ipod_name))
}

/// The iPod's name made safe to use in a folder name on any drive.
fn safe_name(ipod_name: &str) -> String {
    let name = export::clean(ipod_name);
    if name.is_empty() { "iPod".to_string() } else { export::shorten(&name, 80) }
}

/// The note that goes into every backup, so it is clear later what it is
/// and how to put it back.
pub fn note(ipod_name: &str, when: &str) -> String {
    format!(
        "This is a backup of the database of \"{ipod_name}\", made by Oxipod on {when}.\n\
         \n\
         {CONTENTS_AND_RESTORE}"
    )
}

/// The note for the copy Oxipod makes of an iPod right before it restores
/// another backup onto it.
pub fn note_before_restore(ipod_name: &str, when: &str) -> String {
    format!(
        "This is a copy of the database of \"{ipod_name}\" as it was on {when}, made by\n\
         Oxipod right before it put another backup on the iPod. Restoring this\n\
         one undoes that.\n\
         \n\
         {CONTENTS_AND_RESTORE}"
    )
}

/// What a backup holds and how to put it back, for the notes above.
const CONTENTS_AND_RESTORE: &str = "It has the song list (iTunes), the covers (Artwork) and a few facts about\n\
     the iPod itself (Device). It does not have the music.\n\
     \n\
     To put it back: plug in the iPod, open Oxipod, click the ⋮ button next to\n\
     the iPod and choose Restore from Backup…, then pick this folder. (By hand:\n\
     close Oxipod and copy the iPod_Control folder from here onto the iPod,\n\
     replacing the files there.) Songs you added after this backup are gone from\n\
     the iPod's list (Oxipod offers to clean up their files), and songs you\n\
     deleted since then are in the list again but their files are gone (Oxipod\n\
     offers to remove them from the list).\n";

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
        total += copy_file(&ipod.join(&item.relative), &target, copied, cancel)?;
    }
    Ok(total)
}

/// Copies one file and returns how many bytes that was.
fn copy_file(source: &Path, target: &Path, copied: &AtomicU64, cancel: &AtomicBool) -> io::Result<u64> {
    // A big buffer makes the copy of the big cover files quicker.
    let mut reader = BufReader::with_capacity(1 << 20, File::open(source)?);
    let mut file = Counting { inner: File::create(target)?, copied, cancel };
    let written = io::copy(&mut reader, &mut file)?;
    // Make sure it has reached the disk, then check that all of it did: a
    // copy that is wrong is worse than none.
    file.inner.sync_all()?;
    let on_disk = fs::metadata(target)?.len();
    if on_disk != written {
        return Err(io::Error::other(format!(
            "{} came out {on_disk} bytes long instead of {written}",
            target.file_name().unwrap_or_default().to_string_lossy()
        )));
    }
    Ok(written)
}

/// Finds the folder with `iTunes`, `Device` and `Artwork` in it, inside the
/// backup folder the person chose. Backups made by Oxipod keep them in
/// `iPod_Control`; one that was copied by hand may have them right in the folder.
pub fn locate(folder: &Path) -> io::Result<PathBuf> {
    [folder.join("iPod_Control"), folder.to_path_buf()]
        .into_iter()
        .find(|candidate| candidate.join("iTunes").join("iTunesDB").is_file())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "there is no backup in this folder: it has no iPod_Control/iTunes/iTunesDB (the song list). \
                 Choose the folder of one backup, the one with the README.txt in it, \
                 not the folder that holds them all",
            )
        })
}

/// Everything a restore would copy back, in the order it has to happen: the
/// covers first, then the iPod's description, then the song list. (Of the
/// `Device` folder only the description comes back, see `DEVICE_FILES_TO_RESTORE`.)
/// Within a folder the database comes after the files it points to
/// (`ArtworkDB` after the pictures, `iTunesDB` last of all), so that if the
/// restore is interrupted the song list stays as it was for as long as possible.
///
/// `control` is what `locate` found. The paths are relative to it
/// ("iTunes/iTunesDB"), unlike the ones of `plan`, which are relative to the
/// iPod ("iPod_Control/iTunes/iTunesDB").
pub fn restore_plan(control: &Path) -> io::Result<Vec<Item>> {
    let mut items = Vec::new();
    for folder in FOLDERS {
        collect(control, &control.join(folder), &mut items)?;
    }
    items.retain(|item| is_restored(&item.relative));
    items.sort_by_key(|item| restore_rank(&item.relative));
    Ok(items)
}

/// Whether a restore puts this file back (`relative` is relative to the
/// folder `restore_plan` was given).
fn is_restored(relative: &Path) -> bool {
    let mut parts = relative.components();
    let in_device = parts.next().is_some_and(|folder| folder.as_os_str().eq_ignore_ascii_case("Device"));
    let name = relative.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    !in_device || DEVICE_FILES_TO_RESTORE.iter().any(|wanted| wanted.eq_ignore_ascii_case(name))
}

/// What the files of a restore are sorted by: their folder's turn, then
/// databases last, then their names (so the order is the same every time).
fn restore_rank(relative: &Path) -> (usize, bool, PathBuf) {
    let folder = relative.components().next().map(|part| part.as_os_str().to_string_lossy().into_owned());
    let turn = RESTORE_ORDER
        .iter()
        .position(|name| folder.as_deref().is_some_and(|folder| folder.eq_ignore_ascii_case(name)))
        .unwrap_or(RESTORE_ORDER.len());
    let is_database = relative
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("iTunesDB") || name.eq_ignore_ascii_case("ArtworkDB"));
    (turn, is_database, relative.to_path_buf())
}

/// What a finished restore did.
#[derive(Debug)]
pub struct Restored {
    pub files: usize,
    pub bytes: u64,
}

/// Puts the files of a backup (`items` from `restore_plan`) back on the iPod,
/// replacing the ones that are there. Files on the iPod that the backup doesn't
/// have are left alone, except the ones that only fit the song list they were
/// made with (see `TIED_TO_THE_SONG_LIST`); the music is never touched.
///
/// Each file is written next to the old one under a temporary name and then
/// renamed over it, so a file is either the old one or the new one, never half
/// of each. `copied` counts the bytes done, for a progress bar. When something
/// goes wrong the files that were already put back stay, and the song list is
/// the last to change, so running the restore again fixes it.
pub fn restore(control: &Path, items: &[Item], ipod: &Path, copied: &AtomicU64) -> io::Result<Restored> {
    let target_root = ipod.join("iPod_Control");
    if !target_root.join("iTunes").is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "this doesn't look like an iPod: there is no iPod_Control/iTunes folder",
        ));
    }
    // Nobody cancels a restore halfway: an iPod with half a database is worse than either.
    let never = AtomicBool::new(false);
    let mut bytes = 0;
    for item in items {
        bytes += put_back(&control.join(&item.relative), &target_root.join(&item.relative), copied, &never)?;
    }
    for name in TIED_TO_THE_SONG_LIST {
        let in_backup = items.iter().any(|item| item.relative == Path::new("iTunes").join(name));
        if !in_backup {
            match fs::remove_file(target_root.join("iTunes").join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(Restored { files: items.len(), bytes })
}

/// Copies `source` over `target` by way of `target.part`. A failed copy leaves
/// the old file where it was and no `.part` behind.
fn put_back(source: &Path, target: &Path, copied: &AtomicU64, cancel: &AtomicBool) -> io::Result<u64> {
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut part_name = target.file_name().unwrap_or_default().to_os_string();
    part_name.push(".part");
    let part = target.with_file_name(part_name);
    let result = copy_file(source, &part, copied, cancel).and_then(|bytes| fs::rename(&part, target).map(|()| bytes));
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

/// How much room the iPod needs for a restore: what the files grow by, plus the
/// biggest file, which exists twice for a moment (the old one and the new one
/// being written next to it).
pub fn space_needed(items: &[Item], ipod: &Path) -> u64 {
    let target_root = ipod.join("iPod_Control");
    let growth: u64 = items
        .iter()
        .map(|item| {
            let now = fs::metadata(target_root.join(&item.relative)).map(|metadata| metadata.len()).unwrap_or(0);
            item.size.saturating_sub(now)
        })
        .sum();
    growth + items.iter().map(|item| item.size).max().unwrap_or(0)
}

/// The unique ID of the iPod that a backup (or an iPod) belongs to: its
/// FireWire GUID, which the iPod's description (`Device/SysInfoExtended`, or the
/// older `Device/SysInfo`) says. `control` is the folder with `Device` in it.
/// None when the files aren't there or don't say.
pub fn ipod_id(control: &Path) -> Option<String> {
    ["SysInfoExtended", "SysInfo"].into_iter().find_map(|file| {
        let bytes = fs::read(control.join("Device").join(file)).ok()?;
        find_id(&String::from_utf8_lossy(&bytes))
    })
}

/// The FireWire GUID in the text of one of those files, in capitals and
/// without a "0x" in front.
fn find_id(text: &str) -> Option<String> {
    let clean = |value: &str| {
        let value = value.trim();
        let value = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).unwrap_or(value);
        (!value.is_empty() && value.chars().all(|c| c.is_ascii_hexdigit())).then(|| value.to_ascii_uppercase())
    };
    // SysInfoExtended is a plist: the key, and its value right after it.
    if let Some((_, after_key)) = text.split_once("<key>FireWireGUID</key>") {
        let value = after_key.trim_start().strip_prefix("<string>")?;
        return clean(value.split_once("</string>")?.0);
    }
    // SysInfo is lines of "Name: value".
    text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim().eq_ignore_ascii_case("FirewireGuid").then(|| clean(value))?
    })
}

/// What a song list holds, to tell backups apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contents {
    /// The iPod's name at the time.
    pub name: Option<String>,
    pub songs: usize,
    pub playlists: usize,
}

/// Reads the song list in `control` (a backup, or an iPod's `iPod_Control`)
/// without changing anything. libgpod only opens a folder shaped like an iPod,
/// so the song list and the iPod's description go into a temporary one first.
/// That also keeps libgpod's hands off the backup itself.
pub fn contents(control: &Path) -> Result<Contents, gpod::Error> {
    let failed = |error: io::Error| gpod::Error::new(error.to_string());
    let scratch = tempfile::tempdir().map_err(failed)?;
    let stand_in = scratch.path().join("iPod_Control");
    fs::create_dir_all(stand_in.join("iTunes")).map_err(failed)?;
    fs::create_dir_all(stand_in.join("Device")).map_err(failed)?;
    fs::copy(control.join("iTunes").join("iTunesDB"), stand_in.join("iTunes").join("iTunesDB"))
        .map_err(|error| gpod::Error::new(format!("can't read the song list: {error}")))?;
    // These only help libgpod tell what it is reading, so they may be missing.
    for file in ["SysInfo", "SysInfoExtended"] {
        let _ = fs::copy(control.join("Device").join(file), stand_in.join("Device").join(file));
    }
    let database = Database::open(scratch.path())?;
    Ok(Contents { name: database.name(), songs: database.tracks().len(), playlists: database.playlists().len() })
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

    /// Every file under `folder`, as paths relative to it.
    fn files_under(folder: &Path) -> Vec<String> {
        fn walk(base: &Path, folder: &Path, found: &mut Vec<String>) {
            for entry in fs::read_dir(folder).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(base, &path, found);
                } else {
                    found.push(path.strip_prefix(base).unwrap().display().to_string());
                }
            }
        }
        let mut found = Vec::new();
        walk(folder, folder, &mut found);
        found.sort();
        found
    }

    /// A backup of the pretend iPod, made the way Oxipod makes them. Gives the
    /// folder with `iTunes`, `Device` and `Artwork` in it.
    fn backed_up(folder: &Path, ipod: &Path) -> PathBuf {
        locate(&run(ipod, folder, "backup").unwrap().folder).unwrap()
    }

    fn song_list(ipod: &Path) -> String {
        fs::read_to_string(ipod.join("iPod_Control/iTunes/iTunesDB")).unwrap()
    }

    #[test]
    fn finds_a_backup_in_either_layout() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let done = run(&ipod, folder.path(), "backup").unwrap();
        assert_eq!(locate(&done.folder).unwrap(), done.folder.join("iPod_Control"));

        // Copied by hand, without the iPod_Control level.
        let by_hand = folder.path().join("by hand");
        fs::rename(done.folder.join("iPod_Control"), &by_hand).unwrap();
        assert_eq!(locate(&by_hand).unwrap(), by_hand);

        // A folder that holds backups, one that is empty, and one that isn't there.
        assert_eq!(locate(folder.path()).unwrap_err().kind(), io::ErrorKind::NotFound);
        let empty = folder.path().join("empty");
        fs::create_dir(&empty).unwrap();
        assert!(locate(&empty).is_err());
        assert!(locate(&folder.path().join("nope")).is_err());
        // A folder with an iTunes folder but no song list in it is no backup either.
        fs::create_dir_all(empty.join("iTunes")).unwrap();
        assert!(locate(&empty).is_err());
    }

    #[test]
    fn puts_the_covers_first_and_the_databases_last() {
        let folder = tempfile::tempdir().unwrap();
        let control = backed_up(folder.path(), &ipod(folder.path()));
        assert_eq!(
            names(&restore_plan(&control).unwrap()),
            ["Artwork/F1_1.ithmb", "Artwork/ArtworkDB", "Device/SysInfo", "iTunes/Play Counts", "iTunes/iTunesDB"]
        );
    }

    #[test]
    fn leaves_the_ipods_own_settings_alone() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        // The iPod keeps its settings and alarms next to its description.
        for (name, text) in [("Preferences", "settings"), ("alarms", "alarm"), ("SysInfoExtended", "the description")] {
            fs::write(ipod.join("iPod_Control/Device").join(name), text).unwrap();
        }
        let control = backed_up(folder.path(), &ipod);
        // They change after the backup.
        for name in ["Preferences", "alarms", "SysInfoExtended"] {
            fs::write(ipod.join("iPod_Control/Device").join(name), "changed since").unwrap();
        }

        let items = restore_plan(&control).unwrap();
        assert_eq!(
            names(&items),
            [
                "Artwork/F1_1.ithmb",
                "Artwork/ArtworkDB",
                "Device/SysInfo",
                "Device/SysInfoExtended",
                "iTunes/Play Counts",
                "iTunes/iTunesDB"
            ]
        );
        restore(&control, &items, &ipod, &AtomicU64::new(0)).unwrap();
        let device = |name: &str| fs::read_to_string(ipod.join("iPod_Control/Device").join(name)).unwrap();
        assert_eq!(device("SysInfoExtended"), "the description");
        assert_eq!(device("Preferences"), "changed since");
        assert_eq!(device("alarms"), "changed since");
    }

    #[test]
    fn puts_back_what_was_there_and_leaves_the_rest() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let control = backed_up(folder.path(), &ipod);
        // The iPod changes after the backup: a longer song list, a new cover file, a new song.
        fs::write(ipod.join("iPod_Control/iTunes/iTunesDB"), "a newer song list, which is longer").unwrap();
        fs::write(ipod.join("iPod_Control/Artwork/F1_2.ithmb"), "covers for the new song").unwrap();
        fs::write(ipod.join("iPod_Control/Music/F00/NEW.mp3"), "a new song").unwrap();

        let items = restore_plan(&control).unwrap();
        let copied = AtomicU64::new(0);
        let restored = restore(&control, &items, &ipod, &copied).unwrap();
        assert_eq!((restored.files, restored.bytes), (5, 44));
        assert_eq!(copied.load(Ordering::Relaxed), 44);
        assert_eq!(song_list(&ipod), "the song list");
        assert_eq!(fs::read_to_string(ipod.join("iPod_Control/Artwork/ArtworkDB")).unwrap(), "covers");
        // What the backup doesn't have stays, and no temporary files are left.
        assert!(ipod.join("iPod_Control/Artwork/F1_2.ithmb").exists());
        assert!(ipod.join("iPod_Control/Music/F00/NEW.mp3").exists());
        assert!(files_under(&ipod).iter().all(|name| !name.ends_with(".part")), "{:?}", files_under(&ipod));
    }

    #[test]
    fn restores_a_backup_that_was_copied_by_hand() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let by_hand = folder.path().join("by hand");
        fs::rename(backed_up(folder.path(), &ipod), &by_hand).unwrap();
        fs::write(ipod.join("iPod_Control/iTunes/iTunesDB"), "newer").unwrap();

        let control = locate(&by_hand).unwrap();
        let items = restore_plan(&control).unwrap();
        restore(&control, &items, &ipod, &AtomicU64::new(0)).unwrap();
        assert_eq!(song_list(&ipod), "the song list");
    }

    #[test]
    fn removes_what_only_fits_the_newer_song_list() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let control = backed_up(folder.path(), &ipod);
        let on_ipod = |name: &str| ipod.join("iPod_Control/iTunes").join(name);
        // After the backup the iPod was played, and a playlist was made on it.
        fs::write(on_ipod("Play Counts"), "newer plays").unwrap();
        fs::write(on_ipod("OTGPlaylistInfo"), "a playlist made on the iPod").unwrap();

        // The backup has plays of its own, and no playlist: its plays come back, the playlist goes.
        restore(&control, &restore_plan(&control).unwrap(), &ipod, &AtomicU64::new(0)).unwrap();
        assert_eq!(fs::read_to_string(on_ipod("Play Counts")).unwrap(), "plays");
        assert!(!on_ipod("OTGPlaylistInfo").exists());

        // A backup without plays: the iPod's newer ones are of no use to it.
        fs::remove_file(control.join("iTunes/Play Counts")).unwrap();
        fs::write(on_ipod("Play Counts"), "newer plays").unwrap();
        restore(&control, &restore_plan(&control).unwrap(), &ipod, &AtomicU64::new(0)).unwrap();
        assert!(!on_ipod("Play Counts").exists());
        assert_eq!(song_list(&ipod), "the song list", "the song list itself is not in that rule");
    }

    #[test]
    fn a_failed_restore_keeps_the_song_list_and_leaves_no_temporary_files() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let control = backed_up(folder.path(), &ipod);
        fs::write(ipod.join("iPod_Control/iTunes/iTunesDB"), "a newer song list").unwrap();
        let items = restore_plan(&control).unwrap();
        // The backup loses its song list while it is being put back. That is the
        // last file, so the others have been put back by then.
        fs::remove_file(control.join("iTunes/iTunesDB")).unwrap();

        assert!(restore(&control, &items, &ipod, &AtomicU64::new(0)).is_err());
        assert_eq!(song_list(&ipod), "a newer song list");
        assert!(files_under(&ipod).iter().all(|name| !name.ends_with(".part")), "{:?}", files_under(&ipod));
    }

    #[test]
    fn will_not_restore_onto_something_that_is_not_an_ipod() {
        let folder = tempfile::tempdir().unwrap();
        let control = backed_up(folder.path(), &ipod(folder.path()));
        let stick = folder.path().join("stick");
        fs::create_dir(&stick).unwrap();
        let error = restore(&control, &restore_plan(&control).unwrap(), &stick, &AtomicU64::new(0)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(fs::read_dir(&stick).unwrap().count(), 0, "nothing was put on it");
    }

    #[test]
    fn reads_the_ipods_id_from_its_description() {
        let plist = "<?xml version=\"1.0\"?>\n<plist><dict>\n<key>FamilyID</key>\n<integer>4</integer>\n\
                     <key>FireWireGUID</key>\n<string>000a2700deadbeef</string>\n</dict></plist>";
        assert_eq!(find_id(plist).as_deref(), Some("000A2700DEADBEEF"));
        assert_eq!(find_id("ModelNumStr: xB261\nFirewireGuid: 0x000A2700DEADBEEF\n").as_deref(), Some("000A2700DEADBEEF"));
        assert_eq!(find_id("nothing useful"), None);
        assert_eq!(find_id(""), None);
        // A key without a usable value; a later <string> isn't its value.
        assert_eq!(find_id("<key>FireWireGUID</key><string>not hex</string>"), None);
        assert_eq!(find_id("<key>FireWireGUID</key><integer>3</integer><key>X</key><string>ab</string>"), None);
    }

    #[test]
    fn reads_the_id_from_the_files_in_a_folder() {
        let folder = tempfile::tempdir().unwrap();
        let device = folder.path().join("Device");
        fs::create_dir(&device).unwrap();
        assert_eq!(ipod_id(folder.path()), None);
        // Empty, like the SysInfo of a nano that was set up with Oxipod.
        fs::write(device.join("SysInfo"), "").unwrap();
        assert_eq!(ipod_id(folder.path()), None);
        fs::write(device.join("SysInfo"), "FirewireGuid: 0xABCDEF0123456789\n").unwrap();
        assert_eq!(ipod_id(folder.path()).as_deref(), Some("ABCDEF0123456789"));
        // The description wins over the older file.
        fs::write(device.join("SysInfoExtended"), "<key>FireWireGUID</key><string>000A2700DEADBEEF</string>").unwrap();
        assert_eq!(ipod_id(folder.path()).as_deref(), Some("000A2700DEADBEEF"));
    }

    #[test]
    fn works_out_how_much_room_a_restore_needs() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = ipod(folder.path());
        let control = backed_up(folder.path(), &ipod);
        let items = restore_plan(&control).unwrap();
        // Everything is on the iPod at its size already: only the biggest file (15 bytes) twice over.
        assert_eq!(space_needed(&items, &ipod), 15);
        // The song list there is longer now, so putting the backup's back frees room: still nothing to add.
        fs::write(ipod.join("iPod_Control/iTunes/iTunesDB"), "a song list that is a lot longer than before").unwrap();
        assert_eq!(space_needed(&items, &ipod), 15);
        // A file that the iPod doesn't have (any more) needs its whole size.
        fs::remove_file(ipod.join("iPod_Control/Artwork/F1_1.ithmb")).unwrap();
        assert_eq!(space_needed(&items, &ipod), 15 + 15);
    }

    #[test]
    fn names_the_safety_copy_like_a_backup() {
        assert_eq!(safety_folder_name("Nyxnet's iPod", "2026-10-15 1530"), "Nyxnet's iPod before restore 2026-10-15 1530");
        assert_eq!(safety_folder_name("   ", "now"), "iPod before restore now");
        assert_eq!(folder_name("Who: me?", "now"), "Who_ me_ backup now");
    }

    #[test]
    fn the_notes_say_how_to_put_a_backup_back() {
        for text in [note("Nyxnet's iPod", "today"), note_before_restore("Nyxnet's iPod", "today")] {
            assert!(text.contains("Nyxnet's iPod") && text.contains("today"), "{text}");
            assert!(text.contains("Restore from Backup…"), "{text}");
            assert!(!text.contains('{') && !text.contains("\\n"), "{text}");
        }
    }

    // Puts a real backup on a copy of another one, which stands in for the
    // iPod, and reads the result with libgpod. Only temporary files are touched.
    // Run with OXIPOD_TEST_BACKUP (the backup to put back) and OXIPOD_TEST_BASE
    // (another backup of the same iPod, to start from), each a backup folder:
    // OXIPOD_TEST_BACKUP=... OXIPOD_TEST_BASE=... cargo test restores_a_real_backup -- --ignored --nocapture
    #[test]
    #[ignore = "needs two real backups, set OXIPOD_TEST_BACKUP and OXIPOD_TEST_BASE"]
    fn restores_a_real_backup_onto_a_copy() {
        let var = |name: &str| PathBuf::from(std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set")));
        let backup = locate(&var("OXIPOD_TEST_BACKUP")).expect("OXIPOD_TEST_BACKUP is not a backup");
        let base = locate(&var("OXIPOD_TEST_BASE")).expect("OXIPOD_TEST_BASE is not a backup");

        let scratch = tempfile::tempdir().unwrap();
        let ipod = scratch.path().join("ipod");
        fs::create_dir_all(ipod.join("iPod_Control/iTunes")).unwrap();
        restore(&base, &restore_plan(&base).unwrap(), &ipod, &AtomicU64::new(0)).unwrap();
        let control = ipod.join("iPod_Control");
        let before = contents(&control).expect("the stand-in iPod can't be read");
        let wanted = contents(&backup).expect("the backup can't be read");
        println!("the stand-in iPod has {before:?}; putting back {wanted:?}");
        assert_eq!(ipod_id(&backup), ipod_id(&control), "the two backups should be of the same iPod");

        let items = restore_plan(&backup).unwrap();
        let start = std::time::Instant::now();
        let restored = restore(&backup, &items, &ipod, &AtomicU64::new(0)).unwrap();
        println!("put back {} files, {} bytes in {:?}", restored.files, restored.bytes, start.elapsed());

        // The song list reads back as the backup's, covers and all.
        assert_eq!(contents(&control).unwrap(), wanted);
        let database = Database::open(&ipod).unwrap();
        assert_eq!(database.tracks().len(), wanted.songs);
        println!("{} songs, {} with a cover", wanted.songs, database.tracks().iter().filter(|song| song.has_cover).count());
        // Every file of the backup is there, byte for byte.
        for item in &items {
            let original = fs::read(backup.join(&item.relative)).unwrap();
            assert!(original == fs::read(control.join(&item.relative)).unwrap(), "{} differs", item.relative.display());
        }
        assert!(files_under(&ipod).iter().all(|name| !name.ends_with(".part")));
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
