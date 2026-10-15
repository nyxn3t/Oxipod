//! A safe Rust wrapper around libgpod, the C library that reads and writes
//! the iPod's music database (iTunesDB).
//!
//! All `unsafe` code lives in this module. Outside of it, Oxipod only sees
//! normal Rust types like `Database`, `Track` and `String`.
//!
//! Changes are made in memory; nothing reaches the iPod until `save` is
//! called (except song files, which are copied straight to the iPod).

mod ffi;

use std::ffi::{CStr, CString, OsStr};
use std::fmt;
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gtk::gdk_pixbuf::Pixbuf;
use gtk::glib;

/// Identifies a song. This is libgpod's `dbid`, which stays the same when the
/// database is saved (unlike its `id`, which is renumbered on every save).
pub type TrackId = u64;
pub type PlaylistId = u64;

/// An error reported by libgpod, or by us while talking to it.
#[derive(Debug)]
pub struct Error(String);

impl Error {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Error(message.into())
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// One song on the iPod.
///
/// The fields are copied out of libgpod's memory, so a `Track` stays valid
/// after the `Database` it came from is closed.
#[derive(Debug, Clone, Default)]
pub struct Track {
    pub id: TrackId,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub length: Duration,
    /// Position on the album and the disc it is on (0 when the song doesn't say).
    pub track_number: i32,
    pub disc_number: i32,
    /// When the song was put on the iPod, in seconds since 1970 (0 if unknown).
    pub added: u64,
    /// The artist of the whole album, if it differs from the song's artist
    /// (compilations: "Various Artists").
    pub album_artist: Option<String>,
    /// How many discs the album has (0 when unknown).
    pub disc_count: i32,
    /// The size of the song's file in bytes, and its bit rate in kbit/s.
    pub size: u32,
    pub bitrate: i32,
    /// Whether the iPod has cover art for the song.
    pub has_cover: bool,
}

/// A normal playlist made by the user (not the master playlist, the podcast
/// list or an iTunes smart playlist).
#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: PlaylistId,
    pub name: String,
    pub track_ids: Vec<TrackId>,
}

/// What libgpod knows about the iPod model.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// e.g. "Nano Video (3rd Gen.)"
    pub generation: Option<String>,
    /// e.g. "B261"
    pub model_number: Option<String>,
}

/// The audio formats the iPod plays.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileType {
    #[default]
    Mp3,
    Aac,
    Alac,
}

impl FileType {
    /// The description iTunes stores for this format.
    fn description(self) -> &'static CStr {
        match self {
            FileType::Mp3 => c"MPEG audio file",
            FileType::Aac => c"AAC audio file",
            FileType::Alac => c"Apple Lossless audio file",
        }
    }
}

/// Everything needed to add a song. Numbers are 0 when unknown.
#[derive(Debug, Clone, Default)]
pub struct NewTrack {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: i32,
    pub track_count: i32,
    pub disc_number: i32,
    pub disc_count: i32,
    pub year: i32,
    pub length: Duration,
    pub bitrate_kbps: i32,
    pub sample_rate: u16,
    pub file_type: FileType,
    /// Cover art as JPEG or PNG data.
    pub cover: Option<Vec<u8>>,
}

/// New texts for the tags of some songs (see `Database::set_tags`). A field
/// that is `None` stays as it is; an empty text empties it (except the title,
/// which a song can't be without).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
}

impl Tags {
    /// Whether nothing is to be changed.
    pub fn is_empty(&self) -> bool {
        *self == Tags::default()
    }
}

/// A song that is in the database but whose file still has to be copied to
/// `destination` on the iPod. Finish with `finish_copy` or `cancel_copy`.
#[derive(Debug)]
pub struct PendingCopy {
    pub track: TrackId,
    pub destination: PathBuf,
}

/// An iPod music database, opened from a mounted iPod.
///
/// libgpod owns the memory behind it, which is freed when the `Database` is
/// dropped (goes out of scope).
pub struct Database {
    // NonNull: a raw pointer that is guaranteed not to be null.
    // Raw pointers are not `Send`, so Rust won't let a `Database` move to
    // another thread, which matches libgpod: it isn't thread-safe.
    raw: NonNull<ffi::Itdb_iTunesDB>,
}

impl Database {
    /// Reads the database of the iPod mounted at `mount_point`.
    pub fn open(mount_point: &Path) -> Result<Self, Error> {
        let path = path_to_c(mount_point)?;
        let mut error: *mut ffi::GError = ptr::null_mut();
        // SAFETY: `path` is a valid C string for the duration of the call, and
        // `error` is a valid place for libgpod to store an error.
        let raw = unsafe { ffi::itdb_parse(path.as_ptr(), &mut error) };

        match NonNull::new(raw) {
            Some(raw) => Ok(Database { raw }),
            // SAFETY: itdb_parse returned NULL, so it either set `error` or
            // left it NULL; take_error handles both.
            None => Err(unsafe { take_error(error) }),
        }
    }

    fn raw(&self) -> *mut ffi::Itdb_iTunesDB {
        self.raw.as_ptr()
    }

    /// The iPod's name as set in iTunes, e.g. "dylan's iPod".
    ///
    /// This is the name of the master playlist, the one that holds every song.
    pub fn name(&self) -> Option<String> {
        // SAFETY: the master playlist (if any) is owned by the database.
        unsafe {
            let master = ffi::itdb_playlist_mpl(self.raw());
            if master.is_null() {
                return None;
            }
            string_from_c((*master).name)
        }
    }

    /// Renames the iPod. This is the name iTunes shows, and the one the iPod
    /// shows under Settings > About. It's stored as the name of the master
    /// playlist, the playlist that holds every song. The new name reaches the
    /// iPod with the next `save`.
    pub fn set_name(&mut self, name: &str) -> Result<(), Error> {
        // SAFETY: the master playlist is owned by the database. Its old name
        // was allocated by glib, so it is freed with g_free, and the new one
        // is made with g_strdup (glib_string) for the same reason.
        unsafe {
            let master = ffi::itdb_playlist_mpl(self.raw());
            if master.is_null() {
                return Err(Error::new("the database has no master playlist"));
            }
            ffi::g_free((*master).name.cast());
            (*master).name = glib_string(Some(name));
        }
        Ok(())
    }

    /// The model libgpod detected for this iPod.
    pub fn device_info(&self) -> DeviceInfo {
        // SAFETY: the device is owned by the database, and the info table is a
        // static table inside libgpod; both outlive this call.
        unsafe {
            let info = ffi::itdb_device_get_ipod_info((*self.raw()).device);
            if info.is_null() {
                return DeviceInfo { generation: None, model_number: None };
            }
            DeviceInfo {
                generation: string_from_c(ffi::itdb_info_get_ipod_generation_string(
                    (*info).ipod_generation,
                )),
                model_number: string_from_c((*info).model_number),
            }
        }
    }

    /// Whether this iPod still needs its first-time setup (see `sysinfo.rs`).
    ///
    /// Newer iPods only accept a song list with a checksum, and libgpod can
    /// only work that out when it knows the iPod's model and its unique ID.
    /// So this is true for an iPod libgpod can't identify (it might need a
    /// checksum), and for one that does need a checksum while its ID is
    /// unknown. The iPod's own description (`SysInfoExtended`) fixes both; the
    /// caller checks whether that file is there already.
    pub fn needs_first_time_setup(&self) -> bool {
        // SAFETY: the device is owned by the database, and the info table is a
        // static table inside libgpod. `itdb_device_get_sysinfo` gives a copy
        // of the text (or NULL), which is freed here.
        unsafe {
            let device = (*self.raw()).device;
            let info = ffi::itdb_device_get_ipod_info(device);
            let generation = if info.is_null() {
                ffi::Itdb_IpodGeneration_ITDB_IPOD_GENERATION_UNKNOWN
            } else {
                (*info).ipod_generation
            };
            let id = ffi::itdb_device_get_sysinfo(device, c"FirewireGuid".as_ptr());
            let has_id = !id.is_null();
            ffi::g_free(id.cast());
            setup_needed(generation, has_id)
        }
    }

    /// Every song in the database.
    pub fn tracks(&self) -> Vec<Track> {
        self.track_ptrs()
            .into_iter()
            // SAFETY: the pointers come from the database's own track list.
            .map(|track| unsafe { track_from_raw(track) })
            .collect()
    }

    /// The user's playlists, in the order the iPod shows them.
    pub fn playlists(&self) -> Vec<Playlist> {
        // SAFETY: the playlist list and each playlist's member list are owned
        // by the database.
        unsafe {
            glist_items::<ffi::Itdb_Playlist>((*self.raw()).playlists)
                .into_iter()
                .filter(|&playlist| {
                    ffi::itdb_playlist_is_mpl(playlist) == 0
                        && ffi::itdb_playlist_is_podcasts(playlist) == 0
                        && (*playlist).is_spl == 0
                })
                .map(|playlist| Playlist {
                    id: (*playlist).id,
                    name: string_from_c((*playlist).name).unwrap_or_default(),
                    track_ids: glist_items::<ffi::Itdb_Track>((*playlist).members)
                        .into_iter()
                        .map(|track| (*track).dbid)
                        .collect(),
                })
                .collect()
        }
    }

    /// The song's cover art, scaled to about `size`×`size` pixels.
    pub fn cover(&self, id: TrackId, size: i32) -> Option<Pixbuf> {
        let track = self.find_track(id).ok()?;
        // SAFETY: `track` belongs to the database. itdb_track_get_thumbnail
        // returns a new reference to a GdkPixbuf (or NULL), which
        // from_glib_full takes over, so it is released when the Pixbuf drops.
        unsafe {
            if ffi::itdb_track_has_thumbnails(track) == 0 {
                return None;
            }
            let pixbuf = ffi::itdb_track_get_thumbnail(track, size, size);
            if pixbuf.is_null() {
                return None;
            }
            Some(glib::translate::from_glib_full(
                pixbuf as *mut gtk::gdk_pixbuf::ffi::GdkPixbuf,
            ))
        }
    }

    /// Gives each of these songs the cover `image`, replacing the one it has.
    /// `image` is the bytes of a picture file (JPEG or PNG); libgpod turns it
    /// into the sizes the iPod needs. Returns for how many songs that worked.
    /// The covers reach the iPod with the next `save`.
    pub fn set_cover(&mut self, ids: &[TrackId], image: &[u8]) -> Result<usize, Error> {
        let mut done = 0;
        for &id in ids {
            let track = self.find_track(id)?;
            // SAFETY: `track` belongs to the database. libgpod decodes the
            // picture before returning, so `image` only has to live this long.
            let worked = unsafe {
                ffi::itdb_track_set_thumbnails_from_data(track, image.as_ptr(), image.len() as ffi::gsize)
            };
            if worked != 0 {
                done += 1;
            }
        }
        Ok(done)
    }

    /// Takes the cover away from each of these songs.
    pub fn remove_cover(&mut self, ids: &[TrackId]) -> Result<(), Error> {
        for &id in ids {
            let track = self.find_track(id)?;
            // SAFETY: `track` belongs to the database.
            unsafe { ffi::itdb_track_remove_thumbnails(track) };
        }
        Ok(())
    }

    /// Changes the title, artist, album and album artist of these songs: the
    /// fields `tags` has a value for, the others stay as they are. The changes
    /// reach the iPod with the next `save`. Only the database changes. The tags
    /// inside the song files don't, and the iPod doesn't read them: it shows
    /// what the database says.
    pub fn set_tags(&mut self, ids: &[TrackId], tags: &Tags) -> Result<(), Error> {
        if tags.title.as_deref().is_some_and(|title| title.trim().is_empty()) {
            return Err(Error::new("a song needs a title"));
        }
        // Look every song up before changing any, so that a missing one changes nothing.
        let songs = ids.iter().map(|&id| self.find_track(id)).collect::<Result<Vec<_>, _>>()?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
        for track in songs {
            // SAFETY: `track` belongs to the database, and its text fields are
            // NULL or strings allocated by glib, which is what `replace_text` needs.
            unsafe {
                let track = &mut *track;
                let mut changed = false;
                if let Some(title) = &tags.title {
                    changed |= replace_text(&mut track.title, &mut track.sort_title, title);
                }
                if let Some(artist) = &tags.artist {
                    changed |= replace_text(&mut track.artist, &mut track.sort_artist, artist);
                }
                if let Some(album) = &tags.album {
                    changed |= replace_text(&mut track.album, &mut track.sort_album, album);
                }
                if let Some(album_artist) = &tags.album_artist {
                    changed |= replace_text(&mut track.albumartist, &mut track.sort_albumartist, album_artist);
                }
                if changed {
                    track.time_modified = now as ffi::time_t;
                }
            }
        }
        Ok(())
    }

    /// Adds a song to the database and the master playlist, and picks where
    /// on the iPod its file goes. The caller copies `file` there (this can
    /// take a while, so it's left to the caller to do it off the main thread)
    /// and then calls `finish_copy` or `cancel_copy`.
    pub fn add_track(&mut self, new: &NewTrack, file: &Path) -> Result<PendingCopy, Error> {
        let size = std::fs::metadata(file)
            .map_err(|error| Error::new(format!("can't read {}: {error}", file.display())))?
            .len();
        let size = u32::try_from(size).map_err(|_| Error::new("the file is larger than 4 GB"))?;
        let c_file = path_to_c(file)?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

        // SAFETY: itdb_track_new returns a new, empty track that we fill in and
        // then hand over to the database with itdb_track_add. Strings are
        // allocated with g_strdup because libgpod frees them with g_free.
        unsafe {
            let track = ffi::itdb_track_new();
            (*track).title = glib_string(Some(&new.title));
            (*track).artist = glib_string(new.artist.as_deref());
            (*track).album = glib_string(new.album.as_deref());
            (*track).albumartist = glib_string(new.album_artist.as_deref());
            (*track).genre = glib_string(new.genre.as_deref());
            (*track).filetype = ffi::g_strdup(new.file_type.description().as_ptr());
            (*track).track_nr = new.track_number;
            (*track).tracks = new.track_count;
            (*track).cd_nr = new.disc_number;
            (*track).cds = new.disc_count;
            (*track).year = new.year;
            (*track).tracklen = i32::try_from(new.length.as_millis()).unwrap_or(i32::MAX);
            (*track).bitrate = new.bitrate_kbps;
            (*track).samplerate = new.sample_rate;
            (*track).size = size;
            (*track).mediatype = ffi::Itdb_Mediatype_ITDB_MEDIATYPE_AUDIO;
            (*track).time_added = now as ffi::time_t;
            (*track).time_modified = now as ffi::time_t;

            // From here on the database owns the track.
            ffi::itdb_track_add(self.raw(), track, -1);
            if (*track).dbid == 0 {
                (*track).dbid = self.unused_track_id();
            }
            ffi::itdb_playlist_add_track(ffi::itdb_playlist_mpl(self.raw()), track, -1);

            if let Some(cover) = &new.cover {
                // A song without cover art is fine, so a failure is ignored.
                // `as` converts between number types; usize to u64 is lossless
                // on 64-bit Linux.
                ffi::itdb_track_set_thumbnails_from_data(track, cover.as_ptr(), cover.len() as ffi::gsize);
            }

            let mut error = ptr::null_mut();
            let destination =
                ffi::itdb_cp_get_dest_filename(track, ptr::null(), c_file.as_ptr(), &mut error);
            if destination.is_null() {
                let error = take_error(error);
                self.unlink_and_free(track);
                return Err(error);
            }
            let destination_path = path_from_c(destination);
            ffi::g_free(destination.cast());

            Ok(PendingCopy { track: (*track).dbid, destination: destination_path })
        }
    }

    /// Records that the song's file was copied to `pending.destination`.
    pub fn finish_copy(&mut self, pending: &PendingCopy) -> Result<(), Error> {
        let track = self.find_track(pending.track)?;
        let destination = path_to_c(&pending.destination)?;
        let mut error = ptr::null_mut();
        // SAFETY: `track` belongs to the database and `destination` is valid.
        let result = unsafe {
            ffi::itdb_cp_finalize(track, ptr::null(), destination.as_ptr(), &mut error)
        };
        if result.is_null() {
            // SAFETY: itdb_cp_finalize failed, so `error` is set or NULL.
            return Err(unsafe { take_error(error) });
        }
        Ok(())
    }

    /// Undoes `add_track` after the copy failed, removing any partial file.
    pub fn cancel_copy(&mut self, pending: &PendingCopy) {
        let _ = std::fs::remove_file(&pending.destination);
        if let Ok(track) = self.find_track(pending.track) {
            // SAFETY: `track` belongs to the database.
            unsafe { self.unlink_and_free(track) };
        }
    }

    /// Deletes a song: its file on the iPod and its place in every playlist.
    pub fn remove_track(&mut self, id: TrackId) -> Result<(), Error> {
        let track = self.find_track(id)?;
        // SAFETY: `track` belongs to the database. itdb_filename_on_ipod
        // returns a new string (or NULL) that we free with g_free.
        unsafe {
            let file = ffi::itdb_filename_on_ipod(track);
            if !file.is_null() {
                let path = path_from_c(file);
                ffi::g_free(file.cast());
                match std::fs::remove_file(&path) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(Error::new(format!("can't delete {}: {error}", path.display())));
                    }
                }
            }
            self.unlink_and_free(track);
        }
        Ok(())
    }

    /// Creates an empty playlist at the end of the list.
    pub fn create_playlist(&mut self, name: &str) -> Result<PlaylistId, Error> {
        let name = CString::new(name).map_err(|_| Error::new("the name contains a nul byte"))?;
        // SAFETY: itdb_playlist_new copies the name; itdb_playlist_add hands
        // the new playlist over to the database.
        unsafe {
            let playlist = ffi::itdb_playlist_new(name.as_ptr(), 0);
            ffi::itdb_playlist_add(self.raw(), playlist, -1);
            if (*playlist).id == 0 {
                (*playlist).id = self.unused_playlist_id();
            }
            Ok((*playlist).id)
        }
    }

    /// Deletes a playlist. The songs in it stay on the iPod.
    pub fn remove_playlist(&mut self, id: PlaylistId) -> Result<(), Error> {
        let playlist = self.find_playlist(id)?;
        // SAFETY: `playlist` belongs to the database and is a user playlist
        // (find_playlist refuses the master playlist).
        unsafe { ffi::itdb_playlist_remove(playlist) };
        Ok(())
    }

    /// Adds a song to the end of a playlist, unless it's already in it.
    pub fn add_to_playlist(&mut self, playlist: PlaylistId, track: TrackId) -> Result<(), Error> {
        let playlist = self.find_playlist(playlist)?;
        let track = self.find_track(track)?;
        // SAFETY: both belong to the database.
        unsafe {
            if ffi::itdb_playlist_contains_track(playlist, track) == 0 {
                ffi::itdb_playlist_add_track(playlist, track, -1);
            }
        }
        Ok(())
    }

    /// Takes a song out of a playlist. The song stays on the iPod.
    pub fn remove_from_playlist(&mut self, playlist: PlaylistId, track: TrackId) -> Result<(), Error> {
        let playlist = self.find_playlist(playlist)?;
        let track = self.find_track(track)?;
        // SAFETY: both belong to the database.
        unsafe { ffi::itdb_playlist_remove_track(playlist, track) };
        Ok(())
    }

    /// Writes all changes to the iPod (iTunesDB and the cover art database).
    pub fn save(&mut self) -> Result<(), Error> {
        let mut error = ptr::null_mut();
        // SAFETY: self.raw is a valid database.
        if unsafe { ffi::itdb_write(self.raw(), &mut error) } == 0 {
            // SAFETY: itdb_write failed, so `error` is set or NULL.
            return Err(unsafe { take_error(error) });
        }
        Ok(())
    }

    /// Where a song's file is on the iPod, or None if the database has no such
    /// song. The file's name there is made up by libgpod (like
    /// `F04/libgpod313602.mp3`) and says nothing about the song.
    pub fn track_file(&self, id: TrackId) -> Option<PathBuf> {
        let track = self.find_track(id).ok()?;
        // SAFETY: `track` belongs to the database.
        unsafe { filename_on_ipod(track) }
    }

    /// Song files in the iPod's music folders that no song in the database
    /// points to, with their sizes. They take up space but the iPod can't play
    /// them; they're usually left over from an interrupted sync.
    pub fn orphaned_files(&self) -> Vec<(PathBuf, u64)> {
        let Some(music_dir) = self.music_dir() else {
            return Vec::new();
        };
        // A HashSet answers "is this path in here?" quickly, however big it is.
        let used: std::collections::HashSet<PathBuf> = self
            .track_ptrs()
            .into_iter()
            // SAFETY: the tracks belong to the database.
            .filter_map(|track| unsafe { filename_on_ipod(track) })
            .collect();

        // The music is spread over folders F00, F01, …; anything else in the
        // music folder is left alone.
        let mut orphans = Vec::new();
        let Ok(folders) = std::fs::read_dir(&music_dir) else {
            return orphans;
        };
        for folder in folders.filter_map(|entry| entry.ok()) {
            let name = folder.file_name();
            let is_music_folder = name.to_str().is_some_and(|name| {
                name.len() == 3 && name.starts_with(['F', 'f']) && name[1..].chars().all(|c| c.is_ascii_digit())
            });
            if !is_music_folder {
                continue;
            }
            let Ok(files) = std::fs::read_dir(folder.path()) else {
                continue;
            };
            for file in files.filter_map(|entry| entry.ok()) {
                let Ok(metadata) = file.metadata() else {
                    continue;
                };
                if metadata.is_file() && !used.contains(&file.path()) {
                    orphans.push((file.path(), metadata.len()));
                }
            }
        }
        orphans.sort();
        orphans
    }

    /// iPod_Control/Music, with the capitalisation used on this iPod.
    fn music_dir(&self) -> Option<PathBuf> {
        // SAFETY: the mount point is owned by the database. itdb_get_music_dir
        // returns a new string (or NULL) that we free with g_free.
        unsafe {
            let mount_point = ffi::itdb_get_mountpoint(self.raw());
            if mount_point.is_null() {
                return None;
            }
            let dir = ffi::itdb_get_music_dir(mount_point);
            if dir.is_null() {
                return None;
            }
            let path = path_from_c(dir);
            ffi::g_free(dir.cast());
            Some(path)
        }
    }

    fn track_ptrs(&self) -> Vec<*mut ffi::Itdb_Track> {
        // SAFETY: the track list is owned by the database.
        unsafe { glist_items((*self.raw()).tracks) }
    }

    fn find_track(&self, id: TrackId) -> Result<*mut ffi::Itdb_Track, Error> {
        self.track_ptrs()
            .into_iter()
            // SAFETY: the pointers come from the database's own track list.
            .find(|&track| unsafe { (*track).dbid } == id)
            .ok_or_else(|| Error::new("the song isn't on the iPod anymore"))
    }

    fn find_playlist(&self, id: PlaylistId) -> Result<*mut ffi::Itdb_Playlist, Error> {
        // SAFETY: self.raw is a valid database; the result is NULL or one of
        // its playlists.
        unsafe {
            let playlist = ffi::itdb_playlist_by_id(self.raw(), id);
            if playlist.is_null() || ffi::itdb_playlist_is_mpl(playlist) != 0 {
                return Err(Error::new("the playlist isn't on the iPod anymore"));
            }
            Ok(playlist)
        }
    }

    /// Takes a track out of every playlist, then out of the database, and
    /// frees it.
    ///
    /// # Safety
    /// `track` must belong to this database. It is freed, so it must not be
    /// used afterwards.
    unsafe fn unlink_and_free(&mut self, track: *mut ffi::Itdb_Track) {
        // SAFETY: guaranteed by the caller; the playlists belong to the
        // database. itdb_track_remove doesn't touch playlists itself, so a
        // playlist would otherwise keep a dangling pointer to the track.
        unsafe {
            for playlist in glist_items::<ffi::Itdb_Playlist>((*self.raw()).playlists) {
                while ffi::itdb_playlist_contains_track(playlist, track) != 0 {
                    ffi::itdb_playlist_remove_track(playlist, track);
                }
            }
            ffi::itdb_track_remove(track);
        }
    }

    fn unused_track_id(&self) -> TrackId {
        let used: Vec<TrackId> = self.tracks().iter().map(|track| track.id).collect();
        random_id(|id| used.contains(&id))
    }

    fn unused_playlist_id(&self) -> PlaylistId {
        random_id(|id| self.find_playlist(id).is_ok())
    }
}

// `Drop` is Rust's destructor: it runs automatically when a `Database` goes
// out of scope, so the C memory can never leak or be freed twice.
impl Drop for Database {
    fn drop(&mut self) {
        // SAFETY: self.raw came from itdb_parse and is freed exactly once here.
        unsafe { ffi::itdb_free(self.raw()) }
    }
}

/// A random non-zero 64-bit id for which `taken` returns false.
fn random_id(taken: impl Fn(u64) -> bool) -> u64 {
    loop {
        let id = (u64::from(glib::random_int()) << 32) | u64::from(glib::random_int());
        if id != 0 && !taken(id) {
            return id;
        }
    }
}

/// The rule behind `Database::needs_first_time_setup`.
fn setup_needed(generation: ffi::Itdb_IpodGeneration, has_firewire_id: bool) -> bool {
    use ffi::*;

    // The iPods that want a checksum in the song list. (libgpod works with
    // the classic and the nano up to the 4th generation; the newer ones are
    // here because they need the same thing, whether or not they work.)
    let wants_checksum = [
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_1,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_2,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_3,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_3,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_4,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_5,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_6,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_TOUCH_1,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_TOUCH_2,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_TOUCH_3,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_TOUCH_4,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_IPHONE_1,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_IPHONE_2,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_IPHONE_3,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_IPHONE_4,
        Itdb_IpodGeneration_ITDB_IPOD_GENERATION_IPAD_1,
    ]
    .contains(&generation);
    generation == Itdb_IpodGeneration_ITDB_IPOD_GENERATION_UNKNOWN || (wants_checksum && !has_firewire_id)
}

/// Copies the fields we show out of a libgpod track.
///
/// # Safety
/// The track's string fields must be NULL or valid C strings.
unsafe fn track_from_raw(track_ptr: *mut ffi::Itdb_Track) -> Track {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let track = &*track_ptr;
        Track {
            id: track.dbid,
            title: string_from_c(track.title),
            artist: string_from_c(track.artist),
            album: string_from_c(track.album),
            length: Duration::from_millis(track.tracklen.max(0) as u64),
            track_number: track.track_nr,
            disc_number: track.cd_nr,
            added: track.time_added.max(0) as u64,
            album_artist: string_from_c(track.albumartist),
            disc_count: track.cds,
            size: track.size,
            bitrate: track.bitrate,
            has_cover: ffi::itdb_track_has_thumbnails(track_ptr) != 0,
        }
    }
}

/// Where libgpod says a song's file is on the iPod.
///
/// # Safety
/// `track` must be a valid track of an open database.
unsafe fn filename_on_ipod(track: *mut ffi::Itdb_Track) -> Option<PathBuf> {
    // SAFETY: guaranteed by the caller. itdb_filename_on_ipod returns a new
    // string (or NULL), which is freed with g_free once it's been copied into
    // a PathBuf.
    unsafe {
        let file = ffi::itdb_filename_on_ipod(track);
        if file.is_null() {
            return None;
        }
        let path = path_from_c(file);
        ffi::g_free(file.cast());
        Some(path)
    }
}

/// Collects the `data` pointers of a GList into a Vec.
///
/// # Safety
/// `node` must be NULL or a valid GList whose items are `T`s.
unsafe fn glist_items<T>(mut node: *mut ffi::GList) -> Vec<*mut T> {
    let mut items = Vec::new();
    while !node.is_null() {
        // SAFETY: guaranteed by the caller.
        unsafe {
            items.push((*node).data as *mut T);
            node = (*node).next;
        }
    }
    items
}

/// Copies a C string into an owned Rust `String`. NULL becomes `None`.
/// Invalid UTF-8 is replaced with "�" instead of failing.
///
/// # Safety
/// `ptr` must be NULL or point to a valid nul-terminated string.
unsafe fn string_from_c(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: guaranteed by the caller.
    let text = unsafe { CStr::from_ptr(ptr) };
    Some(text.to_string_lossy().into_owned())
}

/// A copy of `text` allocated by glib (NULL for `None`), for fields that
/// libgpod frees with g_free. Nul bytes, which C strings can't hold, are
/// dropped.
fn glib_string(text: Option<&str>) -> *mut c_char {
    let Some(text) = text else {
        return ptr::null_mut();
    };
    let text = CString::new(text.replace('\0', "")).expect("nul bytes were removed");
    // SAFETY: `text` is a valid C string; g_strdup copies it.
    unsafe { ffi::g_strdup(text.as_ptr()) }
}

/// Puts `text` (without the spaces around it) in one of a track's text fields,
/// and empties the field's sort name: that was made for the old text, and the
/// iPod would go on sorting by it. Empty text means no text (NULL). Says
/// whether anything changed; a field that has this text already is left alone,
/// sort name included.
///
/// # Safety
/// Both fields must be NULL or strings allocated by glib (libgpod frees them
/// with g_free).
unsafe fn replace_text(field: &mut *mut c_char, sort_field: &mut *mut c_char, text: &str) -> bool {
    let text = text.trim();
    // SAFETY: guaranteed by the caller.
    unsafe {
        if string_from_c(*field).as_deref().unwrap_or_default() == text {
            return false;
        }
        ffi::g_free((*field).cast());
        ffi::g_free((*sort_field).cast());
    }
    *field = glib_string(Some(text).filter(|text| !text.is_empty()));
    *sort_field = ptr::null_mut();
    true
}

fn path_to_c(path: &Path) -> Result<CString, Error> {
    // C wants a nul-terminated string; a path with a nul byte in it can't be
    // passed to C at all.
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| Error::new(format!("{} contains a nul byte", path.display())))
}

/// # Safety
/// `ptr` must point to a valid nul-terminated string.
unsafe fn path_from_c(ptr: *const c_char) -> PathBuf {
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { CStr::from_ptr(ptr) }.to_bytes();
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// Turns a GError from libgpod into our `Error` and frees it.
///
/// # Safety
/// `error` must be NULL or a GError that nothing else will use or free.
unsafe fn take_error(error: *mut ffi::GError) -> Error {
    if error.is_null() {
        return Error::new("libgpod failed without saying why");
    }
    // SAFETY: guaranteed by the caller.
    unsafe {
        let message = string_from_c((*error).message).unwrap_or_default();
        ffi::g_error_free(error);
        Error(message)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn works_out_which_ipods_need_the_first_time_setup() {
        use super::ffi::*;
        use super::setup_needed;

        // An iPod libgpod can't identify might want a checksum: ask it.
        assert!(setup_needed(Itdb_IpodGeneration_ITDB_IPOD_GENERATION_UNKNOWN, false));
        assert!(setup_needed(Itdb_IpodGeneration_ITDB_IPOD_GENERATION_UNKNOWN, true));
        // The ones that want a checksum need to know their ID.
        for generation in [
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_1,
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_CLASSIC_3,
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_3,
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_4,
        ] {
            assert!(setup_needed(generation, false), "generation {generation}");
            assert!(!setup_needed(generation, true), "generation {generation}");
        }
        // The older ones take any song list.
        for generation in [
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_MINI_2,
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_1,
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_NANO_2,
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_VIDEO_2,
            Itdb_IpodGeneration_ITDB_IPOD_GENERATION_SHUFFLE_2,
        ] {
            assert!(!setup_needed(generation, false), "generation {generation}");
        }
    }

    use super::*;

    // Run with: OXIPOD_TEST_IPOD=/run/media/... cargo test -- --ignored --nocapture
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn reads_a_mounted_ipod() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let db = Database::open(Path::new(&mount)).expect("could not open the database");

        let tracks = db.tracks();
        println!("name: {:?}", db.name());
        println!("model: {:?}", db.device_info());
        println!("{} tracks, first: {:?}", tracks.len(), tracks.first());
        println!("playlists: {:?}", db.playlists());
        let with_cover = tracks.iter().filter(|track| db.cover(track.id, 64).is_some()).count();
        println!("{with_cover} tracks have cover art");
        assert!(!tracks.is_empty());
    }

    // Checks that the new facts in `Track` agree with libgpod's own answers.
    // Run with OXIPOD_TEST_IPOD set. Only reads from the iPod.
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn track_facts_match_what_libgpod_says() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let db = Database::open(Path::new(&mount)).expect("could not open the database");
        let tracks = db.tracks();
        for track in &tracks {
            assert_eq!(track.has_cover, db.cover(track.id, 40).is_some(), "{:?}", track.title);
            let file = db.track_file(track.id).expect("the song has a file");
            assert_eq!(u64::from(track.size), std::fs::metadata(&file).unwrap().len(), "{:?}", track.title);
        }
        let with_cover = tracks.iter().filter(|track| track.has_cover).count();
        let with_album_artist = tracks.iter().filter(|track| track.album_artist.is_some()).count();
        let multi_disc = tracks.iter().filter(|track| track.disc_count > 1).count();
        let bitrates: std::collections::BTreeSet<i32> = tracks.iter().map(|track| track.bitrate).collect();
        println!(
            "{} songs: {with_cover} with a cover, {} without; {with_album_artist} with an album artist; {multi_disc} on multi-disc albums; bit rates {bitrates:?}",
            tracks.len(),
            tracks.len() - with_cover
        );
    }

    // Gives a song without a cover a test picture, checks it after reopening the
    // database, and takes it away again. Run with OXIPOD_TEST_IPOD set. Writes
    // to the iPod, but leaves it as it was (the song had no cover).
    #[test]
    #[ignore = "writes to a mounted iPod"]
    fn sets_and_removes_a_cover() {
        use gtk::gdk_pixbuf::Colorspace;

        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let path = Path::new(&mount);
        let mut db = Database::open(path).expect("could not open the database");
        let song = db.tracks().into_iter().find(|track| !track.has_cover).expect("a song without a cover");
        let with_cover_before = db.tracks().iter().filter(|track| track.has_cover).count();

        let picture = Pixbuf::new(Colorspace::Rgb, false, 8, 256, 256).unwrap();
        picture.fill(0xed8150ff);
        let jpeg = picture.save_to_bufferv("jpeg", &[("quality", "90")]).unwrap();

        assert_eq!(db.set_cover(&[song.id], &jpeg).unwrap(), 1);
        db.save().unwrap();
        drop(db);
        let mut db = Database::open(path).expect("could not reopen the database");
        assert!(db.cover(song.id, 40).is_some(), "the cover should be there after reopening");
        assert!(db.tracks().iter().any(|track| track.id == song.id && track.has_cover));
        assert_eq!(db.tracks().iter().filter(|track| track.has_cover).count(), with_cover_before + 1);

        db.remove_cover(&[song.id]).unwrap();
        db.save().unwrap();
        drop(db);
        let db = Database::open(path).expect("could not reopen the database");
        assert!(db.cover(song.id, 40).is_none(), "the cover should be gone again");
        assert_eq!(db.tracks().iter().filter(|track| track.has_cover).count(), with_cover_before);
    }

    // The same for the biggest album without a cover: all its songs at once.
    // Run with OXIPOD_TEST_IPOD set. Writes to the iPod, but leaves it as it was.
    #[test]
    #[ignore = "writes to a mounted iPod"]
    fn sets_and_removes_the_cover_of_a_whole_album() {
        use gtk::gdk_pixbuf::Colorspace;

        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let path = Path::new(&mount);
        let mut db = Database::open(path).expect("could not open the database");
        let tracks = db.tracks();
        let with_cover_before = tracks.iter().filter(|track| track.has_cover).count();

        let mut albums: std::collections::HashMap<&str, Vec<TrackId>> = std::collections::HashMap::new();
        for track in tracks.iter().filter(|track| !track.has_cover) {
            if let Some(album) = track.album.as_deref() {
                albums.entry(album).or_default().push(track.id);
            }
        }
        let (album, ids) = albums.into_iter().max_by_key(|(_, ids)| ids.len()).expect("an album without a cover");
        assert!(ids.len() >= 3, "the biggest album without a cover has only {} songs", ids.len());
        println!("giving “{album}” ({} songs) a cover", ids.len());

        let picture = Pixbuf::new(Colorspace::Rgb, false, 8, 320, 320).unwrap();
        picture.fill(0x4080c0ff);
        let jpeg = picture.save_to_bufferv("jpeg", &[("quality", "90")]).unwrap();

        assert_eq!(db.set_cover(&ids, &jpeg).unwrap(), ids.len());
        db.save().unwrap();
        drop(db);
        let mut db = Database::open(path).expect("could not reopen the database");
        for id in &ids {
            assert!(db.cover(*id, 40).is_some(), "song {id} should have its cover after reopening");
        }
        assert_eq!(db.tracks().iter().filter(|track| track.has_cover).count(), with_cover_before + ids.len());

        db.remove_cover(&ids).unwrap();
        db.save().unwrap();
        drop(db);
        let db = Database::open(path).expect("could not reopen the database");
        assert!(ids.iter().all(|id| db.cover(*id, 40).is_none()), "the covers should be gone again");
        assert_eq!(db.tracks().iter().filter(|track| track.has_cover).count(), with_cover_before);
    }

    // Renames the iPod, checks the new name after reopening the database, and
    // puts the old name back. Run with OXIPOD_TEST_IPOD set.
    #[test]
    #[ignore = "writes to a mounted iPod"]
    fn renames_an_ipod_and_back() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let path = Path::new(&mount);
        let mut db = Database::open(path).expect("could not open the database");
        let original = db.name().expect("the iPod has a name");
        let tracks = db.tracks().len();

        db.set_name("Oxipod rename test").unwrap();
        db.save().unwrap();
        drop(db);
        let mut db = Database::open(path).expect("could not reopen the database");
        assert_eq!(db.name().as_deref(), Some("Oxipod rename test"));
        assert_eq!(db.tracks().len(), tracks);

        db.set_name(&original).unwrap();
        db.save().unwrap();
        drop(db);
        let db = Database::open(path).expect("could not reopen the database");
        assert_eq!(db.name().as_deref(), Some(original.as_str()));
        assert_eq!(db.tracks().len(), tracks);
    }

    // Adds a song and a playlist, saves, reopens, then removes both again.
    // Run with OXIPOD_TEST_IPOD, OXIPOD_TEST_MP3 and OXIPOD_TEST_COVER set.
    #[test]
    #[ignore = "writes to a mounted iPod"]
    fn adds_and_removes_a_song_and_a_playlist() {
        let var = |name| std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set"));
        let mount = PathBuf::from(var("OXIPOD_TEST_IPOD"));
        let mp3 = PathBuf::from(var("OXIPOD_TEST_MP3"));
        let cover = std::fs::read(var("OXIPOD_TEST_COVER")).unwrap();

        let mut db = Database::open(&mount).unwrap();
        let before = db.tracks().len();
        let new = NewTrack {
            title: "Oxipod Test Song".into(),
            artist: Some("Oxipod".into()),
            length: Duration::from_secs(110),
            bitrate_kbps: 320,
            sample_rate: 44100,
            cover: Some(cover),
            ..NewTrack::default()
        };
        let pending = db.add_track(&new, &mp3).unwrap();
        std::fs::copy(&mp3, &pending.destination).unwrap();
        db.finish_copy(&pending).unwrap();
        let playlist = db.create_playlist("Oxipod Test Playlist").unwrap();
        db.add_to_playlist(playlist, pending.track).unwrap();
        db.save().unwrap();
        drop(db);

        let mut db = Database::open(&mount).unwrap();
        assert_eq!(db.tracks().len(), before + 1);
        let saved = db.playlists().into_iter().find(|p| p.id == playlist).expect("playlist missing");
        assert_eq!(saved.track_ids, vec![pending.track]);
        assert!(db.cover(pending.track, 64).is_some(), "cover missing after save");
        assert!(pending.destination.exists());

        db.remove_playlist(playlist).unwrap();
        db.remove_track(pending.track).unwrap();
        db.save().unwrap();
        drop(db);

        let db = Database::open(&mount).unwrap();
        assert_eq!(db.tracks().len(), before);
        assert!(db.playlists().iter().all(|p| p.id != playlist));
        assert!(!pending.destination.exists(), "song file was not deleted");
    }

    // Puts a fake leftover file on the iPod, checks it's found, removes it.
    #[test]
    #[ignore = "writes to a mounted iPod"]
    fn finds_leftover_files() {
        let mount = PathBuf::from(std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set"));
        let db = Database::open(&mount).unwrap();
        assert!(db.orphaned_files().is_empty(), "the iPod already has leftovers");

        let fake = db.music_dir().unwrap().join("F00").join("OXIPODTEST.mp3");
        std::fs::write(&fake, b"not really music").unwrap();
        let found = db.orphaned_files();
        std::fs::remove_file(&fake).unwrap();
        assert_eq!(found, vec![(fake, 16)]);
    }

    /// A database that isn't on any iPod, with three songs in it (ids 1 to 3).
    /// Every song has the sort name "old sort name" for its artist.
    fn pretend_database() -> Database {
        // SAFETY: itdb_new makes an empty database that only we own. The tracks
        // are handed over to it with itdb_track_add, their strings made by
        // g_strdup (glib_string) as libgpod wants.
        unsafe {
            let raw = NonNull::new(ffi::itdb_new()).expect("itdb_new works");
            let db = Database { raw };
            for (id, (title, artist, album)) in
                [("Song One", "Artist A", "Album X"), ("Song Two", "Artist A", "Album X"), ("Song Three", "Artist B", "")]
                    .into_iter()
                    .enumerate()
            {
                let track = ffi::itdb_track_new();
                (*track).title = glib_string(Some(title));
                (*track).artist = glib_string(Some(artist));
                (*track).album = glib_string(Some(album).filter(|album| !album.is_empty()));
                (*track).sort_artist = glib_string(Some("old sort name"));
                ffi::itdb_track_add(db.raw(), track, -1);
                (*track).dbid = id as u64 + 1;
            }
            db
        }
    }

    /// Whether a song's artist has a sort name.
    fn has_sort_artist(db: &Database, id: TrackId) -> bool {
        // SAFETY: the track belongs to the database.
        unsafe { !(*db.find_track(id).unwrap()).sort_artist.is_null() }
    }

    fn track(db: &Database, id: TrackId) -> Track {
        db.tracks().into_iter().find(|track| track.id == id).unwrap()
    }

    #[test]
    fn changes_only_the_tags_it_is_given() {
        let mut db = pretend_database();
        let tags = Tags { artist: Some("New Artist".into()), album: Some("New Album".into()), ..Tags::default() };
        db.set_tags(&[1, 2], &tags).unwrap();

        for id in [1, 2] {
            let song = track(&db, id);
            assert_eq!(song.artist.as_deref(), Some("New Artist"));
            assert_eq!(song.album.as_deref(), Some("New Album"));
            assert!(!has_sort_artist(&db, id), "the sort name belonged to the old artist");
        }
        assert_eq!(track(&db, 1).title.as_deref(), Some("Song One"), "the title wasn't given");
        // The song that wasn't named stays as it was.
        assert_eq!(track(&db, 3).artist.as_deref(), Some("Artist B"));
        assert!(has_sort_artist(&db, 3));
    }

    #[test]
    fn empty_text_empties_a_tag_and_spaces_are_trimmed() {
        let mut db = pretend_database();
        let tags = Tags { title: Some("  Renamed  ".into()), album: Some(String::new()), ..Tags::default() };
        db.set_tags(&[1], &tags).unwrap();
        let song = track(&db, 1);
        assert_eq!(song.title.as_deref(), Some("Renamed"));
        assert_eq!(song.album, None);
        assert_eq!(song.artist.as_deref(), Some("Artist A"));
    }

    #[test]
    fn the_same_text_keeps_the_sort_name() {
        let mut db = pretend_database();
        let tags = Tags { artist: Some("Artist A".into()), ..Tags::default() };
        db.set_tags(&[1], &tags).unwrap();
        assert!(has_sort_artist(&db, 1), "nothing changed, so the sort name is still right");
    }

    #[test]
    fn a_song_cannot_lose_its_title_or_be_missing() {
        let mut db = pretend_database();
        let no_title = Tags { title: Some("   ".into()), artist: Some("Changed".into()), ..Tags::default() };
        assert!(db.set_tags(&[1], &no_title).is_err());
        assert_eq!(track(&db, 1).artist.as_deref(), Some("Artist A"), "a refused change changes nothing");

        // One of the songs doesn't exist: the one that does is left alone too.
        let tags = Tags { artist: Some("Changed".into()), ..Tags::default() };
        assert!(db.set_tags(&[1, 99], &tags).is_err());
        assert_eq!(track(&db, 1).artist.as_deref(), Some("Artist A"));
    }

    #[test]
    fn knows_when_there_is_nothing_to_change() {
        assert!(Tags::default().is_empty());
        assert!(!Tags { album: Some(String::new()), ..Tags::default() }.is_empty());
    }

    // Changes the tags of one song, checks them after reopening the database,
    // and puts the old ones back. Run with OXIPOD_TEST_IPOD set. Writes to the
    // iPod, but leaves it as it was (the song has no sort names to lose).
    #[test]
    #[ignore = "writes to a mounted iPod"]
    fn edits_the_tags_of_a_song_and_back() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let path = Path::new(&mount);
        let mut db = Database::open(path).expect("could not open the database");
        let before = db.tracks();
        // A song without sort names, so putting its text back leaves it exactly as it was.
        let song = before
            .iter()
            .find(|song| {
                // SAFETY: the track belongs to the database.
                unsafe {
                    let raw = &*db.find_track(song.id).unwrap();
                    [raw.sort_title, raw.sort_artist, raw.sort_album, raw.sort_albumartist].iter().all(|sort| sort.is_null())
                }
            })
            .expect("a song without sort names")
            .clone();

        let edited = Tags {
            title: Some("Oxipod tag test".into()),
            artist: Some("Oxipod Test Artist".into()),
            album: Some(String::new()),
            album_artist: Some("Oxipod Test Album Artist".into()),
        };
        db.set_tags(&[song.id], &edited).unwrap();
        db.save().unwrap();
        drop(db);

        let mut db = Database::open(path).expect("could not reopen the database");
        let saved = track(&db, song.id);
        assert_eq!(saved.title.as_deref(), Some("Oxipod tag test"));
        assert_eq!(saved.artist.as_deref(), Some("Oxipod Test Artist"));
        assert_eq!(saved.album, None);
        assert_eq!(saved.album_artist.as_deref(), Some("Oxipod Test Album Artist"));
        assert_eq!(db.tracks().len(), before.len());

        let original = Tags {
            title: song.title.clone(),
            artist: Some(song.artist.clone().unwrap_or_default()),
            album: Some(song.album.clone().unwrap_or_default()),
            album_artist: Some(song.album_artist.clone().unwrap_or_default()),
        };
        db.set_tags(&[song.id], &original).unwrap();
        db.save().unwrap();
        drop(db);

        let db = Database::open(path).expect("could not reopen the database");
        let restored = track(&db, song.id);
        let same = |a: &Option<String>, b: &Option<String>| a.as_deref().map(str::trim) == b.as_deref().map(str::trim);
        assert!(same(&restored.title, &song.title) && same(&restored.artist, &song.artist), "{restored:?}");
        assert!(same(&restored.album, &song.album) && same(&restored.album_artist, &song.album_artist), "{restored:?}");
        assert_eq!(db.tracks().len(), before.len());
    }

    #[test]
    fn opening_a_folder_without_an_ipod_fails() {
        let result = Database::open(Path::new("/nonexistent"));
        assert!(result.is_err());
    }
}
