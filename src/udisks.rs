//! Talking to UDisks2, the system service that mounts drives on Linux
//! desktops. It is what "Safely Remove" in a file manager uses. Going through
//! UDisks2 also works inside the Flatpak sandbox, where the app isn't allowed
//! to unmount drives or open them itself.

use std::path::Path;

use gtk::gio;
use gtk::glib::{self, Variant};

pub const NAME: &str = "org.freedesktop.UDisks2";

/// Calls a method of UDisks2 on the system bus.
pub async fn call(
    bus: &gio::DBusConnection,
    object: &str,
    interface: &str,
    method: &str,
    parameters: Option<&Variant>,
) -> Result<Variant, glib::Error> {
    bus.call_future(
        Some(NAME),
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

/// Everything UDisks2 knows: every object (drive, partition...) with its
/// interfaces and their properties, as `a{oa{sa{sv}}}`.
pub async fn managed_objects(bus: &gio::DBusConnection) -> Result<Variant, glib::Error> {
    Ok(call(bus, "/org/freedesktop/UDisks2", "org.freedesktop.DBus.ObjectManager", "GetManagedObjects", None)
        .await?
        .child_value(0))
}

/// Finds the filesystem mounted at `mount_path`, and the drive it's on.
pub fn find_filesystem(objects: &Variant, mount_path: &Path) -> Option<(String, Option<String>)> {
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

/// The whole disk that the partition `block` is part of. A disk that isn't
/// divided into partitions is its own whole disk.
pub fn whole_disk(objects: &Variant, block: &str) -> Option<String> {
    let interfaces = interfaces_of(objects, block)?;
    let table = lookup(&interfaces, "org.freedesktop.UDisks2.Partition").and_then(|partition| lookup(&partition, "Table"));
    match table {
        Some(table) => table.str().map(str::to_string),
        None => Some(block.to_string()),
    }
}

/// The device file of a block device, like "/dev/sdb".
pub fn device_file(objects: &Variant, block: &str) -> Option<String> {
    let interfaces = interfaces_of(objects, block)?;
    let device = lookup(&interfaces, "org.freedesktop.UDisks2.Block").and_then(|block| lookup(&block, "Device"))?;
    // A byte string that ends with a nul byte.
    let bytes = device.fixed_array::<u8>().ok()?;
    let end = bytes.iter().position(|&byte| byte == 0).unwrap_or(bytes.len());
    String::from_utf8(bytes[..end].to_vec()).ok()
}

/// The interfaces (and their properties) of one object.
fn interfaces_of(objects: &Variant, path: &str) -> Option<Variant> {
    objects
        .iter()
        .find(|entry| entry.child_value(0).str() == Some(path))
        .map(|entry| entry.child_value(1))
}

/// The value for `key` in a D-Bus dictionary (`a{s…}`). Values of type `v`
/// (variant) are unwrapped.
pub fn lookup(dictionary: &Variant, key: &str) -> Option<Variant> {
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

    fn object_path(path: &str) -> Variant {
        glib::variant::ObjectPath::try_from(path).unwrap().to_variant()
    }

    /// A byte string that ends with a nul byte, like UDisks2 sends paths.
    fn byte_string(text: &str) -> Variant {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        bytes.to_variant()
    }

    /// A trimmed-down `GetManagedObjects` answer: a partition mounted elsewhere,
    /// the iPod's disk and the partition on it, which is mounted. And a disk
    /// without partitions.
    fn pretend_objects() -> Variant {
        let mount_point = |path: &str| {
            let points = Variant::array_from_iter_with_type(glib::VariantTy::new("ay").unwrap(), [byte_string(path)]);
            properties(&[("MountPoints", points)])
        };
        let block = |drive: &str, device: &str| {
            properties(&[("Drive", object_path(drive)), ("Device", byte_string(device))])
        };
        let object = |path: &str, interfaces: Variant| Variant::from_dict_entry(&object_path(path), &interfaces);
        Variant::array_from_iter_with_type(
            glib::VariantTy::new("{oa{sa{sv}}}").unwrap(),
            [
                object(
                    "/org/freedesktop/UDisks2/block_devices/sda1",
                    interfaces(&[
                        ("org.freedesktop.UDisks2.Filesystem", mount_point("/mnt/hdd3tb")),
                        ("org.freedesktop.UDisks2.Block", block("/org/freedesktop/UDisks2/drives/Disk", "/dev/sda1")),
                        (
                            "org.freedesktop.UDisks2.Partition",
                            properties(&[("Table", object_path("/org/freedesktop/UDisks2/block_devices/sda"))]),
                        ),
                    ]),
                ),
                object(
                    "/org/freedesktop/UDisks2/block_devices/sdb",
                    interfaces(&[(
                        "org.freedesktop.UDisks2.Block",
                        block("/org/freedesktop/UDisks2/drives/Apple_iPod", "/dev/sdb"),
                    )]),
                ),
                object(
                    "/org/freedesktop/UDisks2/block_devices/sdb1",
                    interfaces(&[
                        ("org.freedesktop.UDisks2.Filesystem", mount_point("/run/media/dylan/IPOD")),
                        ("org.freedesktop.UDisks2.Block", block("/org/freedesktop/UDisks2/drives/Apple_iPod", "/dev/sdb1")),
                        (
                            "org.freedesktop.UDisks2.Partition",
                            properties(&[("Table", object_path("/org/freedesktop/UDisks2/block_devices/sdb"))]),
                        ),
                    ]),
                ),
                // A disk with a file system straight on it, no partitions.
                object(
                    "/org/freedesktop/UDisks2/block_devices/sdc",
                    interfaces(&[
                        ("org.freedesktop.UDisks2.Filesystem", mount_point("/run/media/dylan/STICK")),
                        ("org.freedesktop.UDisks2.Block", block("/org/freedesktop/UDisks2/drives/Stick", "/dev/sdc")),
                    ]),
                ),
            ],
        )
    }

    #[test]
    fn finds_the_filesystem_and_its_drive() {
        let found = find_filesystem(&pretend_objects(), Path::new("/run/media/dylan/IPOD"));
        assert_eq!(
            found,
            Some((
                "/org/freedesktop/UDisks2/block_devices/sdb1".to_string(),
                Some("/org/freedesktop/UDisks2/drives/Apple_iPod".to_string())
            ))
        );
        assert_eq!(find_filesystem(&pretend_objects(), Path::new("/run/media/dylan/OTHER")), None);
    }

    #[test]
    fn finds_the_whole_disk_of_a_partition() {
        let objects = pretend_objects();
        assert_eq!(
            whole_disk(&objects, "/org/freedesktop/UDisks2/block_devices/sdb1").as_deref(),
            Some("/org/freedesktop/UDisks2/block_devices/sdb")
        );
        // Without partitions the disk is its own whole disk.
        assert_eq!(
            whole_disk(&objects, "/org/freedesktop/UDisks2/block_devices/sdc").as_deref(),
            Some("/org/freedesktop/UDisks2/block_devices/sdc")
        );
        assert_eq!(whole_disk(&objects, "/org/freedesktop/UDisks2/block_devices/nope"), None);
    }

    #[test]
    fn tells_the_device_file() {
        let objects = pretend_objects();
        assert_eq!(device_file(&objects, "/org/freedesktop/UDisks2/block_devices/sdb").as_deref(), Some("/dev/sdb"));
        assert_eq!(device_file(&objects, "/org/freedesktop/UDisks2/block_devices/sdb1").as_deref(), Some("/dev/sdb1"));
        assert_eq!(device_file(&objects, "/org/freedesktop/UDisks2/block_devices/nope"), None);
    }

    // Only looks the iPod up; doesn't eject it or open it.
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn finds_the_ipod_in_udisks() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let found = glib::MainContext::default().block_on(async {
            let bus = gio::bus_get_future(gio::BusType::System).await.unwrap();
            let objects = managed_objects(&bus).await.unwrap();
            let (filesystem, drive) = find_filesystem(&objects, Path::new(&mount))?;
            let disk = whole_disk(&objects, &filesystem)?;
            let device = device_file(&objects, &disk);
            Some((filesystem, drive, disk, device))
        });
        println!("{found:?}");
        let (filesystem, drive, disk, device) = found.expect("not found");
        assert!(filesystem.starts_with("/org/freedesktop/UDisks2/block_devices/"));
        assert!(drive.is_some_and(|drive| drive.starts_with("/org/freedesktop/UDisks2/drives/")));
        assert!(disk.starts_with("/org/freedesktop/UDisks2/block_devices/"));
        assert!(device.is_some_and(|device| device.starts_with("/dev/")));
    }
}
