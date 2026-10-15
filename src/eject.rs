//! Safely removing an iPod through UDisks2, the system service that mounts
//! drives on Linux desktops (it's what "Safely Remove" in a file manager
//! uses). Going through UDisks2 also works inside the Flatpak sandbox, where
//! the app isn't allowed to unmount anything itself.

use std::path::Path;

use gtk::gio;
use gtk::glib::{self, Variant};

const UDISKS: &str = "org.freedesktop.UDisks2";

/// Unmounts the drive mounted at `mount_path` and powers it off, so it can be
/// unplugged.
pub async fn eject(mount_path: &Path) -> Result<(), glib::Error> {
    let bus = gio::bus_get_future(gio::BusType::System).await?;

    // Ask UDisks2 for everything it knows: every object (drive, partition…)
    // with its interfaces and their properties, as a{oa{sa{sv}}}.
    let objects = call(&bus, "/org/freedesktop/UDisks2", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", None)
        .await?
        .child_value(0);

    let (filesystem, drive) = find_filesystem(&objects, mount_path).ok_or_else(|| {
        glib::Error::new(gio::IOErrorEnum::NotFound, "UDisks2 doesn't know this drive")
    })?;

    // Both calls take a dictionary of options; none are needed.
    let no_options = Variant::tuple_from_iter([glib::VariantDict::new(None).end()]);
    call(&bus, &filesystem, "org.freedesktop.UDisks2.Filesystem", "Unmount", Some(&no_options)).await?;
    if let Some(drive) = drive {
        // Not every drive can be powered off; it's unmounted either way, so
        // a failure here isn't worth reporting.
        let _ = call(&bus, &drive, "org.freedesktop.UDisks2.Drive", "PowerOff", Some(&no_options)).await;
    }
    Ok(())
}

async fn call(
    bus: &gio::DBusConnection,
    object: &str,
    interface: &str,
    method: &str,
    parameters: Option<&Variant>,
) -> Result<Variant, glib::Error> {
    bus.call_future(
        Some(UDISKS),
        object,
        interface,
        method,
        parameters,
        None,
        gio::DBusCallFlags::NONE,
        // Unmounting waits for pending writes, which can take a while.
        60_000,
    )
    .await
}

/// Finds the filesystem mounted at `mount_path`, and the drive it's on.
fn find_filesystem(objects: &Variant, mount_path: &Path) -> Option<(String, Option<String>)> {
    use std::os::unix::ffi::OsStrExt;

    // UDisks2 stores mount points as nul-terminated byte strings.
    let mut wanted = mount_path.as_os_str().as_bytes().to_vec();
    wanted.push(0);

    for entry in objects.iter() {
        let object_path = entry.child_value(0).str()?.to_string();
        let interfaces = entry.child_value(1);
        let filesystem = lookup(&interfaces, "org.freedesktop.UDisks2.Filesystem");
        let Some(mount_points) = filesystem.and_then(|fs| lookup(&fs, "MountPoints")) else {
            continue;
        };
        let mounted_here = mount_points
            .iter()
            .any(|point| point.fixed_array::<u8>().is_ok_and(|bytes| bytes == wanted.as_slice()));
        if !mounted_here {
            continue;
        }

        let drive = lookup(&interfaces, "org.freedesktop.UDisks2.Block")
            .and_then(|block| lookup(&block, "Drive"))
            .and_then(|drive| drive.str().map(str::to_string))
            .filter(|drive| drive != "/");
        return Some((object_path, drive));
    }
    None
}

/// The value for `key` in a D-Bus dictionary (`a{s…}`). Values of type `v`
/// (variant) are unwrapped.
fn lookup(dictionary: &Variant, key: &str) -> Option<Variant> {
    dictionary
        .iter()
        .find(|entry| entry.child_value(0).str() == Some(key))
        .map(|entry| {
            let value = entry.child_value(1);
            // Only a `v` can be unwrapped. Asking any other value to do it
            // makes GLib print a CRITICAL warning, even though the answer
            // (the value itself) would have been right.
            if value.is_type(glib::VariantTy::VARIANT) {
                value.as_variant().unwrap_or(value)
            } else {
                value
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glib::prelude::ToVariant;

    /// A D-Bus dictionary `a{sv}`: every value wrapped in a variant (`v`), as UDisks2 sends it.
    fn properties(entries: &[(&str, Variant)]) -> Variant {
        Variant::array_from_iter_with_type(
            glib::VariantTy::new("{sv}").unwrap(),
            entries
                .iter()
                .map(|(key, value)| Variant::from_dict_entry(&key.to_variant(), &Variant::from_variant(value))),
        )
    }

    /// A dictionary `a{sa{sv}}` from interface names to their properties. The
    /// values are *not* variants here, which is what `lookup` must cope with.
    fn interfaces(entries: &[(&str, Variant)]) -> Variant {
        Variant::array_from_iter_with_type(
            glib::VariantTy::new("{sa{sv}}").unwrap(),
            entries.iter().map(|(key, value)| Variant::from_dict_entry(&key.to_variant(), value)),
        )
    }

    /// A trimmed-down `GetManagedObjects` answer: a partition mounted elsewhere
    /// and the iPod's partition with the drive it is on.
    fn managed_objects() -> Variant {
        let mount_point = |path: &str| {
            let mut bytes = path.as_bytes().to_vec();
            bytes.push(0);
            let points = Variant::array_from_iter_with_type(glib::VariantTy::new("ay").unwrap(), [bytes.to_variant()]);
            properties(&[("MountPoints", points)])
        };
        let block = |drive: &str| {
            let drive = glib::variant::ObjectPath::try_from(drive).unwrap().to_variant();
            properties(&[("Drive", drive)])
        };
        let object = |path: &str, interfaces: Variant| {
            let path = glib::variant::ObjectPath::try_from(path).unwrap().to_variant();
            Variant::from_dict_entry(&path, &interfaces)
        };
        Variant::array_from_iter_with_type(
            glib::VariantTy::new("{oa{sa{sv}}}").unwrap(),
            [
                object(
                    "/org/freedesktop/UDisks2/block_devices/sda1",
                    interfaces(&[
                        ("org.freedesktop.UDisks2.Filesystem", mount_point("/mnt/hdd3tb")),
                        ("org.freedesktop.UDisks2.Block", block("/org/freedesktop/UDisks2/drives/Disk")),
                    ]),
                ),
                object(
                    "/org/freedesktop/UDisks2/block_devices/sdb1",
                    interfaces(&[
                        ("org.freedesktop.UDisks2.Filesystem", mount_point("/run/media/dylan/IPOD")),
                        ("org.freedesktop.UDisks2.Block", block("/org/freedesktop/UDisks2/drives/Apple_iPod")),
                    ]),
                ),
            ],
        )
    }

    #[test]
    fn finds_the_filesystem_and_its_drive() {
        let found = find_filesystem(&managed_objects(), Path::new("/run/media/dylan/IPOD"));
        assert_eq!(
            found,
            Some((
                "/org/freedesktop/UDisks2/block_devices/sdb1".to_string(),
                Some("/org/freedesktop/UDisks2/drives/Apple_iPod".to_string())
            ))
        );
        assert_eq!(find_filesystem(&managed_objects(), Path::new("/run/media/dylan/OTHER")), None);
    }

    // Only looks the iPod up; doesn't eject it.
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn finds_the_ipod_in_udisks() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let found = glib::MainContext::default().block_on(async {
            let bus = gio::bus_get_future(gio::BusType::System).await.unwrap();
            let objects = call(&bus, "/org/freedesktop/UDisks2", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", None)
                .await
                .unwrap()
                .child_value(0);
            find_filesystem(&objects, Path::new(&mount))
        });
        println!("{found:?}");
        let (filesystem, drive) = found.expect("not found");
        assert!(filesystem.starts_with("/org/freedesktop/UDisks2/block_devices/"));
        assert!(drive.is_some_and(|drive| drive.starts_with("/org/freedesktop/UDisks2/drives/")));
    }
}
