//! The few things Oxipod remembers between runs, kept in a small text file.
//!
//! The file lives in the user's config folder, `~/.config/oxipod/settings.ini`
//! (inside the app's own folder when it runs as a Flatpak), and looks like
//! this:
//!
//! ```text
//! [copy]
//! layout=folders
//! ```
//!
//! GLib's `KeyFile` reads and writes this kind of file. A missing or broken
//! file is not an error: every setting has a default.

use std::fs;
use std::path::PathBuf;

use gtk::glib;

use crate::export::Layout;

pub struct Settings {
    file: glib::KeyFile,
    path: PathBuf,
}

impl Settings {
    /// Reads the settings file of the current user.
    pub fn load() -> Self {
        Self::at(glib::user_config_dir().join("oxipod").join("settings.ini"))
    }

    fn at(path: PathBuf) -> Self {
        let file = glib::KeyFile::new();
        // No file yet, or one we can't read: stay with the defaults.
        let _ = file.load_from_file(&path, glib::KeyFileFlags::NONE);
        Settings { file, path }
    }

    /// How songs are arranged when they are copied to the computer.
    pub fn copy_layout(&self) -> Layout {
        // `ok()` turns a Result into an Option, dropping the error;
        // `and_then` only continues when there is a value.
        self.file
            .string("copy", "layout")
            .ok()
            .and_then(|id| Layout::from_id(&id))
            .unwrap_or_default()
    }

    pub fn set_copy_layout(&self, layout: Layout) {
        self.file.set_string("copy", "layout", layout.id());
        self.save();
    }

    fn save(&self) {
        if let Some(folder) = self.path.parent() {
            let _ = fs::create_dir_all(folder);
        }
        if let Err(error) = fs::write(&self.path, self.file.to_data()) {
            eprintln!("Couldn't save the settings: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_the_defaults() {
        let folder = tempfile::tempdir().unwrap();
        let settings = Settings::at(folder.path().join("oxipod").join("settings.ini"));
        assert_eq!(settings.copy_layout(), Layout::Flat);
    }

    #[test]
    fn remembers_the_layout_in_a_new_folder_and_a_new_run() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("oxipod").join("settings.ini");

        Settings::at(path.clone()).set_copy_layout(Layout::Folders);
        // A new `Settings` stands for the next time the app starts.
        assert_eq!(Settings::at(path.clone()).copy_layout(), Layout::Folders);

        Settings::at(path.clone()).set_copy_layout(Layout::Flat);
        assert_eq!(Settings::at(path).copy_layout(), Layout::Flat);
    }

    #[test]
    fn a_broken_file_means_the_defaults() {
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("settings.ini");
        fs::write(&path, "this is [not\nan ini file at all =").unwrap();
        assert_eq!(Settings::at(path.clone()).copy_layout(), Layout::Flat);
        // ...and a nonsense value is ignored too.
        fs::write(&path, "[copy]\nlayout=spiral\n").unwrap();
        assert_eq!(Settings::at(path).copy_layout(), Layout::Flat);
    }
}
