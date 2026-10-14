//! A safe Rust wrapper around libgpod, the C library that reads and writes
//! the iPod's music database (iTunesDB).
//!
//! All `unsafe` code lives in this module. Outside of it, Oxipod only sees
//! normal Rust types like `Database`, `Track` and `String`.

mod ffi;

use std::ffi::{CStr, CString};
use std::fmt;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::time::Duration;

/// An error reported by libgpod.
#[derive(Debug)]
pub struct Error(String);

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
#[derive(Debug, Clone)]
pub struct Track {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub length: Duration,
}

/// What libgpod knows about the iPod model.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// e.g. "Nano Video (3rd Gen.)"
    pub generation: Option<String>,
    /// e.g. "B261"
    pub model_number: Option<String>,
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
        // C wants a nul-terminated string; a path with a nul byte in it can't
        // be passed to C at all.
        let path = CString::new(mount_point.as_os_str().as_bytes())
            .map_err(|_| Error("the mount path contains a nul byte".into()))?;

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

    /// The iPod's name as set in iTunes, e.g. "dylan's iPod".
    ///
    /// This is the name of the master playlist, the one that holds every song.
    pub fn name(&self) -> Option<String> {
        // SAFETY: self.raw is a valid database; the master playlist (if any)
        // is owned by it and lives as long as it does.
        unsafe {
            let master = ffi::itdb_playlist_mpl(self.raw.as_ptr());
            if master.is_null() {
                return None;
            }
            string_from_c((*master).name)
        }
    }

    /// Every song in the database.
    pub fn tracks(&self) -> Vec<Track> {
        let mut tracks = Vec::new();
        // SAFETY: `tracks` is a GList owned by the database; each node's
        // `data` points to an Itdb_Track that is also owned by the database.
        unsafe {
            let mut node = (*self.raw.as_ptr()).tracks;
            while !node.is_null() {
                let track = &*((*node).data as *const ffi::Itdb_Track);
                tracks.push(Track {
                    title: string_from_c(track.title),
                    artist: string_from_c(track.artist),
                    album: string_from_c(track.album),
                    length: Duration::from_millis(track.tracklen.max(0) as u64),
                });
                node = (*node).next;
            }
        }
        tracks
    }

    /// The model libgpod detected for this iPod.
    pub fn device_info(&self) -> DeviceInfo {
        // SAFETY: the device is owned by the database, and the info table is a
        // static table inside libgpod; both outlive this call.
        unsafe {
            let info = ffi::itdb_device_get_ipod_info((*self.raw.as_ptr()).device);
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
}

// `Drop` is Rust's destructor: it runs automatically when a `Database` goes
// out of scope, so the C memory can never leak or be freed twice.
impl Drop for Database {
    fn drop(&mut self) {
        // SAFETY: self.raw came from itdb_parse and is freed exactly once here.
        unsafe { ffi::itdb_free(self.raw.as_ptr()) }
    }
}

/// Copies a C string into an owned Rust `String`. NULL becomes `None`.
/// Invalid UTF-8 is replaced with "�" instead of failing.
///
/// # Safety
/// `ptr` must be NULL or point to a valid nul-terminated string.
unsafe fn string_from_c(ptr: *const std::os::raw::c_char) -> Option<String> {
    if ptr.is_null() {
        return None;
    }
    // SAFETY: guaranteed by the caller.
    let text = unsafe { CStr::from_ptr(ptr) };
    Some(text.to_string_lossy().into_owned())
}

/// Turns a GError from libgpod into our `Error` and frees it.
///
/// # Safety
/// `error` must be NULL or a GError that nothing else will use or free.
unsafe fn take_error(error: *mut ffi::GError) -> Error {
    if error.is_null() {
        return Error("libgpod failed without saying why".into());
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
        assert!(!tracks.is_empty());
    }

    #[test]
    fn opening_a_folder_without_an_ipod_fails() {
        let result = Database::open(Path::new("/nonexistent"));
        assert!(result.is_err());
    }
}
