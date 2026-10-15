//! Copying songs from the iPod to the computer.
//!
//! The song files on an iPod have names like `F04/libgpod313602.mp3`, which
//! say nothing about the song. They are copied under readable names instead,
//! like "Artist - Title.mp3". Nothing here touches the iPod's database or
//! GTK, so all of it can be tested with plain files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::gpod::Track;

/// The longest the name of a file may get before its extension, in bytes.
/// File systems allow 255 bytes for a whole name; this leaves room for a
/// number like " (2)", the extension and the ".part" of a copy in progress.
const MAX_NAME_BYTES: usize = 200;

/// The same for the name of a folder (an artist, an album).
const MAX_FOLDER_BYTES: usize = 100;

/// How the copied songs are arranged in the folder.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Layout {
    /// Everything in the folder itself: "Artist - Title.mp3".
    #[default]
    Flat,
    /// A folder for each artist and, inside it, one for each album, with the
    /// songs numbered like they are on the album: "Artist/Album/01 Title.mp3".
    Folders,
}

impl Layout {
    /// The name the settings file uses for this layout.
    pub fn id(self) -> &'static str {
        match self {
            Layout::Flat => "flat",
            Layout::Folders => "folders",
        }
    }

    pub fn from_id(id: &str) -> Option<Layout> {
        [Layout::Flat, Layout::Folders].into_iter().find(|layout| layout.id() == id)
    }
}

/// Where a song goes, relative to the folder the user picked.
///
/// A `PathBuf` is a path that is built up from parts, like a `String` is text
/// that is built up from pieces.
pub fn relative_path(layout: Layout, track: &Track, extension: &str) -> PathBuf {
    match layout {
        Layout::Flat => PathBuf::from(file_name(track.artist.as_deref(), track.title.as_deref(), extension)),
        Layout::Folders => {
            // An album with several artists (a compilation) is filed under its
            // album artist, so it stays together in one folder instead of
            // being spread over the folder of every artist on it.
            let artist = folder_name(track.album_artist.as_deref().or(track.artist.as_deref()), "Unknown Artist");
            let album = folder_name(track.album.as_deref(), "Unknown Album");
            let title = Some(clean(track.title.as_deref().unwrap_or(""))).filter(|title| !title.is_empty());
            let name = format!("{}{}", song_number(track), title.as_deref().unwrap_or("Unknown Title"));
            [artist, album, format!("{}.{extension}", shorten(&name, MAX_NAME_BYTES))].iter().collect()
        }
    }
}

/// The number in front of a song's name: "01 ", or "2-03 " on an album with
/// several discs. Songs that don't have a track number get none.
fn song_number(track: &Track) -> String {
    match (track.track_number, track.disc_count > 1 && track.disc_number > 0) {
        (number, _) if number <= 0 => String::new(),
        (number, true) => format!("{}-{number:02} ", track.disc_number),
        (number, false) => format!("{number:02} "),
    }
}

/// The name of an artist's or an album's folder, or `fallback` if there is none.
fn folder_name(text: Option<&str>, fallback: &str) -> String {
    let name = text.map(clean).filter(|name| !name.is_empty());
    name.map(|name| shorten(&name, MAX_FOLDER_BYTES)).unwrap_or_else(|| fallback.to_string())
}

/// What a song's file is called on the computer: "Artist - Title.mp3".
///
/// `Option<&str>` means "maybe some text": a song doesn't always have an
/// artist or a title.
pub fn file_name(artist: Option<&str>, title: Option<&str>, extension: &str) -> String {
    // `map` runs a function on the text if there is some, and `filter` keeps
    // it only if the closure says so: an artist that is empty after cleaning
    // counts as no artist at all.
    let artist = artist.map(clean).filter(|artist| !artist.is_empty());
    let title = title.map(clean).filter(|title| !title.is_empty());
    // Matching on a pair covers all four cases in one place.
    let name = match (artist, title) {
        (Some(artist), Some(title)) => format!("{artist} - {title}"),
        (None, Some(title)) => title,
        (Some(artist), None) => format!("{artist} - Unknown Title"),
        (None, None) => "Unknown Title".to_string(),
    };
    format!("{}.{extension}", shorten(&name, MAX_NAME_BYTES))
}

/// Makes a piece of text safe to use in a file name.
fn clean(text: &str) -> String {
    // Characters that Windows and FAT drives refuse (and "/", which Linux
    // refuses) become an underscore, so the files also copy to a USB stick.
    // Any kind of white space (a new line, a tab) becomes a plain space.
    let replaced: String = text
        .chars()
        .map(|c| match c {
            c if c.is_whitespace() => ' ',
            c if c.is_control() || "/\\:*?\"<>|".contains(c) => '_',
            c => c,
        })
        .collect();
    // Splitting on white space and joining again trims the ends and squeezes
    // runs of spaces into one.
    let squeezed = replaced.split_whitespace().collect::<Vec<_>>().join(" ");
    // A name can't start with a dot (that would hide the file) or end with a
    // dot or a space (Windows doesn't allow it).
    squeezed.trim_start_matches('.').trim_end_matches(['.', ' ']).to_string()
}

/// Cuts `text` to at most `max` bytes, without cutting a character in half:
/// an "é" is two bytes, a Japanese character three.
fn shorten(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    // The last place at or before `max` where a character starts.
    let end = (0..=max).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
    text[..end].trim_end_matches(['.', ' ']).to_string()
}

/// "Artist - Title.mp3" with a number in it: "Artist - Title (2).mp3".
fn numbered(name: &str, number: u32) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) => format!("{stem} ({number}).{extension}"),
        None => format!("{name} ({number})"),
    }
}

/// What `copy_song` did.
#[derive(Debug)]
pub enum Outcome {
    /// The song was copied.
    Copied,
    /// A file with this name and size was there already, so nothing was
    /// copied. That's what copying the same songs twice looks like.
    AlreadyThere,
}

/// Copies one song file into `folder`, at the place `relative` says (a name,
/// or folders and a name; the folders are made if they're not there yet).
///
/// It never overwrites anything: if a different file is there already, the
/// song gets a number ("Artist - Title (2).mp3"). `impl AsRef<Path>` means
/// "anything that can be used as a path": a `&str`, a `PathBuf`... `io::Result`
/// is a `Result` whose error is a file system error; the `?` after a call
/// returns that error from this function at once.
pub fn copy_song(from: &Path, folder: &Path, relative: impl AsRef<Path>) -> io::Result<Outcome> {
    let relative = relative.as_ref();
    let size = fs::metadata(from)?.len();
    let name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the song has no file name"))?;
    let subfolders = relative.parent().unwrap_or(Path::new(""));

    // Find a name that is free. A file with the wanted name and the same
    // size is taken to be this very song, copied earlier. `while let` keeps
    // looping as long as the file exists (metadata gives `Ok`).
    let mut target = folder.join(relative);
    let mut number = 2;
    while let Ok(existing) = fs::metadata(&target) {
        if existing.len() == size {
            return Ok(Outcome::AlreadyThere);
        }
        target = folder.join(subfolders).join(numbered(name, number));
        number += 1;
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }

    // Copy under a temporary name first and rename it at the end, so a copy
    // that gets interrupted never leaves a half-written song under a real name.
    let mut partial = target.clone().into_os_string();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    let result = fs::copy(from, &partial).and_then(|_| fs::rename(&partial, &target));
    if let Err(error) = result {
        // Best effort: if the leftover can't be removed either, so be it.
        let _ = fs::remove_file(&partial);
        return Err(error);
    }
    Ok(Outcome::Copied)
}

/// How a copy job went, for the message at the end.
#[derive(Default)]
pub struct Report {
    pub copied: usize,
    pub already_there: usize,
    /// One line for each song that couldn't be copied: "name: reason".
    pub failed: Vec<String>,
    /// Why the job stopped early (cancelled, iPod unplugged, disk full).
    pub stopped: Option<String>,
}

impl Report {
    /// The message for when the job is over. `folder` is the name of the
    /// folder the songs went to.
    pub fn message(&self, folder: &str) -> String {
        let songs = |count: usize| if count == 1 { "1 song".to_string() } else { format!("{count} songs") };

        // Nothing new and nothing wrong: it was all there from an earlier copy.
        if self.copied == 0 && self.already_there > 0 && self.failed.is_empty() && self.stopped.is_none() {
            return if self.already_there == 1 {
                format!("That song is already in “{folder}”")
            } else {
                format!("All {} are already in “{folder}”", songs(self.already_there))
            };
        }

        let mut text = match &self.stopped {
            Some(reason) => format!("Copied {}. {reason}", songs(self.copied)),
            None => format!("Copied {} to “{folder}”", songs(self.copied)),
        };
        let mut notes = Vec::new();
        if self.already_there > 0 {
            notes.push(format!("{} already there", self.already_there));
        }
        if let Some(first) = self.failed.first() {
            let more = if self.failed.len() > 1 { format!(" and {} more", self.failed.len() - 1) } else { String::new() };
            notes.push(format!("couldn't copy {first}{more}"));
        }
        if !notes.is_empty() {
            text.push_str(&format!(" ({})", notes.join(", ")));
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The names of the files in a folder, sorted.
    fn names_in(folder: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(folder)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn names_a_song_after_its_artist_and_title() {
        assert_eq!(
            file_name(Some("Lorna Shore"), Some("Pain Remains I"), "mp3"),
            "Lorna Shore - Pain Remains I.mp3"
        );
    }

    #[test]
    fn copes_with_a_missing_artist_or_title() {
        assert_eq!(file_name(None, Some("Dive"), "mp3"), "Dive.mp3");
        assert_eq!(file_name(Some("Knitter"), None, "m4a"), "Knitter - Unknown Title.m4a");
        assert_eq!(file_name(None, None, "mp3"), "Unknown Title.mp3");
        // Text that is empty once cleaned counts as missing.
        assert_eq!(file_name(Some("  ... "), Some("Dive"), "mp3"), "Dive.mp3");
    }

    #[test]
    fn replaces_characters_a_file_name_can_not_have() {
        assert_eq!(file_name(Some("AC/DC"), Some("Who Made Who?"), "mp3"), "AC_DC - Who Made Who_.mp3");
        assert_eq!(file_name(None, Some("What: \"Now\" <1>"), "mp3"), "What_ _Now_ _1_.mp3");
        assert_eq!(file_name(None, Some("one\ntwo\tthree"), "mp3"), "one two three.mp3");
    }

    #[test]
    fn tidies_dots_and_spaces() {
        assert_eq!(file_name(Some("  Artist.. "), Some(" .Title. "), "mp3"), "Artist - Title.mp3");
        assert_eq!(file_name(None, Some("two   spaces"), "mp3"), "two spaces.mp3");
    }

    #[test]
    fn cuts_long_names_without_breaking_a_character() {
        let long = file_name(None, Some(&"a".repeat(300)), "mp3");
        assert_eq!(long.len(), MAX_NAME_BYTES + ".mp3".len());

        // 101 times a two-byte character is 202 bytes: the cut has to fall
        // between characters, so only 100 of them fit.
        let accents = file_name(None, Some(&"é".repeat(101)), "mp3");
        assert_eq!(accents, format!("{}.mp3", "é".repeat(100)));
    }

    /// A song with the facts the folder layout uses.
    fn song(artist: &str, album: &str, title: &str, number: i32) -> Track {
        let text = |text: &str| (!text.is_empty()).then(|| text.to_string());
        Track {
            artist: text(artist),
            album: text(album),
            title: text(title),
            track_number: number,
            ..Default::default()
        }
    }

    #[test]
    fn the_folders_layout_files_a_song_under_artist_and_album() {
        let track = song("Lorna Shore", "Pain Remains", "Pain Remains I: After All I've Done", 1);
        assert_eq!(
            relative_path(Layout::Folders, &track, "mp3"),
            Path::new("Lorna Shore/Pain Remains/01 Pain Remains I_ After All I've Done.mp3")
        );
        // The flat layout is what it always was.
        assert_eq!(relative_path(Layout::Flat, &song("A", "B", "C", 1), "mp3"), Path::new("A - C.mp3"));
    }

    #[test]
    fn a_compilation_is_filed_under_its_album_artist() {
        let mut track = song("Presser", "Scarred Summer - Sunset Slammers", "Hot Day", 7);
        track.album_artist = Some("Various Artists".into());
        assert_eq!(
            relative_path(Layout::Folders, &track, "mp3"),
            Path::new("Various Artists/Scarred Summer - Sunset Slammers/07 Hot Day.mp3")
        );
    }

    #[test]
    fn numbers_songs_on_albums_with_several_discs() {
        let mut track = song("A", "B", "C", 3);
        track.disc_count = 2;
        track.disc_number = 2;
        assert_eq!(relative_path(Layout::Folders, &track, "mp3"), Path::new("A/B/2-03 C.mp3"));
        // One disc: no disc number. No track number: no number at all.
        track.disc_count = 1;
        assert_eq!(relative_path(Layout::Folders, &track, "mp3"), Path::new("A/B/03 C.mp3"));
        track.track_number = 0;
        assert_eq!(relative_path(Layout::Folders, &track, "mp3"), Path::new("A/B/C.mp3"));
    }

    #[test]
    fn the_folders_layout_copes_with_missing_and_awkward_names() {
        assert_eq!(
            relative_path(Layout::Folders, &song("", "", "", 0), "mp3"),
            Path::new("Unknown Artist/Unknown Album/Unknown Title.mp3")
        );
        // A slash in a name must not make an extra folder, and ".." must not
        // lead out of the folder the user picked.
        assert_eq!(
            relative_path(Layout::Folders, &song("AC/DC", "..", "Back In Black", 1), "mp3"),
            Path::new("AC_DC/Unknown Album/01 Back In Black.mp3")
        );
    }

    #[test]
    fn layouts_have_names_for_the_settings_file() {
        for layout in [Layout::Flat, Layout::Folders] {
            assert_eq!(Layout::from_id(layout.id()), Some(layout));
        }
        assert_eq!(Layout::from_id("something else"), None);
    }

    #[test]
    fn copies_into_folders_and_numbers_inside_them() {
        let ipod = tempfile::tempdir().unwrap();
        let computer = tempfile::tempdir().unwrap();
        let first = ipod.path().join("libgpod1.mp3");
        let second = ipod.path().join("libgpod2.mp3");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, b"another one").unwrap();

        let outcome = copy_song(&first, computer.path(), "A/B/01 C.mp3").unwrap();
        assert!(matches!(outcome, Outcome::Copied));
        assert_eq!(fs::read(computer.path().join("A/B/01 C.mp3")).unwrap(), b"one");
        // A different song that wants the same place gets a number, in the same folder.
        copy_song(&second, computer.path(), "A/B/01 C.mp3").unwrap();
        assert_eq!(names_in(&computer.path().join("A/B")), ["01 C (2).mp3", "01 C.mp3"]);
        // Copying the first one again does nothing.
        assert!(matches!(copy_song(&first, computer.path(), "A/B/01 C.mp3").unwrap(), Outcome::AlreadyThere));
    }

    #[test]
    fn numbers_a_name_before_the_extension() {
        assert_eq!(numbered("Artist - Title.mp3", 2), "Artist - Title (2).mp3");
        assert_eq!(numbered("no extension", 3), "no extension (3)");
    }

    #[test]
    fn copies_a_song_under_its_new_name() {
        let ipod = tempfile::tempdir().unwrap();
        let computer = tempfile::tempdir().unwrap();
        let source = ipod.path().join("libgpod313602.mp3");
        fs::write(&source, b"song data").unwrap();

        let outcome = copy_song(&source, computer.path(), "Artist - Title.mp3").unwrap();
        assert!(matches!(outcome, Outcome::Copied));
        assert_eq!(fs::read(computer.path().join("Artist - Title.mp3")).unwrap(), b"song data");
        // Only the finished file is left, no ".part".
        assert_eq!(names_in(computer.path()), ["Artist - Title.mp3"]);
        // And the iPod's own file is untouched.
        assert_eq!(fs::read(&source).unwrap(), b"song data");
    }

    #[test]
    fn copying_the_same_song_twice_does_nothing_the_second_time() {
        let ipod = tempfile::tempdir().unwrap();
        let computer = tempfile::tempdir().unwrap();
        let source = ipod.path().join("libgpod1.mp3");
        fs::write(&source, b"song data").unwrap();

        copy_song(&source, computer.path(), "A - B.mp3").unwrap();
        let again = copy_song(&source, computer.path(), "A - B.mp3").unwrap();
        assert!(matches!(again, Outcome::AlreadyThere));
        assert_eq!(names_in(computer.path()), ["A - B.mp3"]);
    }

    #[test]
    fn never_overwrites_a_different_file_with_the_same_name() {
        let ipod = tempfile::tempdir().unwrap();
        let computer = tempfile::tempdir().unwrap();
        fs::write(computer.path().join("A - B.mp3"), b"my own file").unwrap();
        let first = ipod.path().join("libgpod1.mp3");
        let second = ipod.path().join("libgpod2.mp3");
        fs::write(&first, b"another song entirely").unwrap();
        fs::write(&second, b"a third, longer song than the others").unwrap();

        copy_song(&first, computer.path(), "A - B.mp3").unwrap();
        copy_song(&second, computer.path(), "A - B.mp3").unwrap();

        assert_eq!(names_in(computer.path()), ["A - B (2).mp3", "A - B (3).mp3", "A - B.mp3"]);
        // The file that was already there is exactly as it was.
        assert_eq!(fs::read(computer.path().join("A - B.mp3")).unwrap(), b"my own file");
        assert_eq!(fs::read(computer.path().join("A - B (2).mp3")).unwrap(), b"another song entirely");
    }

    #[test]
    fn a_failed_copy_leaves_nothing_behind() {
        let ipod = tempfile::tempdir().unwrap();
        let computer = tempfile::tempdir().unwrap();
        let missing = ipod.path().join("not there.mp3");

        assert!(copy_song(&missing, computer.path(), "A - B.mp3").is_err());
        assert!(names_in(computer.path()).is_empty());
    }

    #[test]
    fn tells_how_the_job_went() {
        let report = |copied, already_there, failed: &[&str], stopped: Option<&str>| Report {
            copied,
            already_there,
            failed: failed.iter().map(|line| line.to_string()).collect(),
            stopped: stopped.map(str::to_string),
        };
        assert_eq!(report(12, 0, &[], None).message("Music"), "Copied 12 songs to “Music”");
        assert_eq!(report(1, 0, &[], None).message("Music"), "Copied 1 song to “Music”");
        assert_eq!(report(10, 2, &[], None).message("Music"), "Copied 10 songs to “Music” (2 already there)");
        assert_eq!(report(0, 12, &[], None).message("Music"), "All 12 songs are already in “Music”");
        assert_eq!(report(0, 1, &[], None).message("Music"), "That song is already in “Music”");
        assert_eq!(
            report(3, 0, &["A.mp3: Permission denied", "B.mp3: no"], None).message("Music"),
            "Copied 3 songs to “Music” (couldn't copy A.mp3: Permission denied and 1 more)"
        );
        assert_eq!(report(5, 0, &[], Some("Cancelled.")).message("Music"), "Copied 5 songs. Cancelled.");
    }

    // Run with: OXIPOD_TEST_IPOD=/run/media/... cargo test -- --ignored --nocapture
    // Only reads from the iPod.
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn every_song_on_a_mounted_ipod_has_a_file_and_a_name() {
        use crate::gpod::Database;

        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let db = Database::open(Path::new(&mount)).expect("could not open the database");
        let tracks = db.tracks();
        let mut names = std::collections::HashMap::new();
        for track in &tracks {
            let file = db.track_file(track.id).unwrap_or_else(|| panic!("no file for {:?}", track.title));
            assert!(file.is_file(), "{} is missing", file.display());
            let extension = file.extension().and_then(|e| e.to_str()).unwrap_or("mp3").to_lowercase();
            let name = file_name(track.artist.as_deref(), track.title.as_deref(), &extension);
            assert!(name.len() <= 255, "{name} is too long");
            *names.entry(name).or_insert(0) += 1;
        }
        let repeated: Vec<_> = names.iter().filter(|(_, count)| **count > 1).collect();
        println!("{} songs, {} different names, used more than once: {repeated:?}", tracks.len(), names.len());
    }

    // Plans where every song of a mounted iPod would go in both layouts, and
    // checks that all the paths are sensible. Only reads from the iPod.
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn plans_sensible_paths_for_every_song_on_a_mounted_ipod() {
        use crate::gpod::Database;

        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let db = Database::open(Path::new(&mount)).expect("could not open the database");
        let tracks = db.tracks();
        for layout in [Layout::Flat, Layout::Folders] {
            let mut seen = std::collections::HashMap::new();
            let mut longest = 0;
            for track in &tracks {
                let path = relative_path(layout, track, "mp3");
                assert!(path.is_relative(), "{} is not relative", path.display());
                for part in path.components() {
                    let part = part.as_os_str().to_string_lossy();
                    assert!(!part.is_empty() && part != ".." && part != ".", "bad part {part:?} in {}", path.display());
                    assert!(part.len() <= 255, "{part} is too long");
                }
                longest = longest.max(path.as_os_str().len());
                *seen.entry(path).or_insert(0) += 1;
            }
            let collisions: Vec<_> = seen.iter().filter(|(_, count)| **count > 1).collect();
            let artists: std::collections::HashSet<_> =
                seen.keys().filter_map(|path| path.components().next()).collect();
            println!(
                "{layout:?}: {} songs, {} different paths, {} paths used twice {:?}, longest path {longest} bytes, {} top-level entries",
                tracks.len(),
                seen.len(),
                collisions.len(),
                collisions.iter().map(|(path, _)| path.display().to_string()).collect::<Vec<_>>(),
                artists.len()
            );
        }
    }

    // Copies the first few songs of a mounted iPod into a temporary folder,
    // once in each layout.
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn copies_songs_from_a_mounted_ipod() {
        use crate::gpod::Database;

        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let db = Database::open(Path::new(&mount)).expect("could not open the database");
        for layout in [Layout::Flat, Layout::Folders] {
            let computer = tempfile::tempdir().unwrap();
            for track in db.tracks().iter().take(5) {
                let source = db.track_file(track.id).expect("the song has a file");
                let relative = relative_path(layout, track, "mp3");
                let Outcome::Copied = copy_song(&source, computer.path(), &relative).unwrap() else {
                    panic!("{} was already there", relative.display());
                };
                let path = computer.path().join(&relative);
                assert_eq!(fs::metadata(&path).unwrap().len(), fs::metadata(&source).unwrap().len());
                println!("{layout:?}: {}", relative.display());
            }
        }
    }
}
