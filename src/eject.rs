//! Safely removing an iPod through UDisks2 (see `udisks.rs`), the same way
//! "Safely Remove" in a file manager does it.

use std::path::Path;

use gtk::gio;
use gtk::glib::{self, Variant};

use crate::udisks;

/// Unmounts the drive mounted at `mount_path` and powers it off, so it can be
/// unplugged.
pub async fn eject(mount_path: &Path) -> Result<(), glib::Error> {
    let bus = gio::bus_get_future(gio::BusType::System).await?;
    let objects = udisks::managed_objects(&bus).await?;

    let (filesystem, drive) = udisks::find_filesystem(&objects, mount_path).ok_or_else(|| {
        glib::Error::new(gio::IOErrorEnum::NotFound, "UDisks2 doesn't know this drive")
    })?;

    // Both calls take a dictionary of options; none are needed.
    let no_options = Variant::tuple_from_iter([glib::VariantDict::new(None).end()]);
    udisks::call(&bus, &filesystem, "org.freedesktop.UDisks2.Filesystem", "Unmount", Some(&no_options)).await?;
    if let Some(drive) = drive {
        // Not every drive can be powered off; it's unmounted either way, so
        // a failure here isn't worth reporting.
        let _ = udisks::call(&bus, &drive, "org.freedesktop.UDisks2.Drive", "PowerOff", Some(&no_options)).await;
    }
    Ok(())
}
