//! The first-time setup of an iPod: getting its `SysInfoExtended` file.
//!
//! Newer iPods (the classic, and the nano from the 3rd generation on) only
//! show a song list that carries a checksum, and libgpod can only work that
//! out when it knows the iPod's model and its unique ID (the "FireWire ID").
//! The iPod knows both. It hands them over as a small XML file when it is
//! asked in the right way, with a SCSI command called INQUIRY. Without that
//! file libgpod either writes a song list the iPod ignores, so the iPod looks
//! empty, or refuses to write at all.
//!
//! libgpod comes with a tool for this, `ipod-read-sysinfo-extended`. It asks
//! and saves the answer as `iPod_Control/Device/SysInfoExtended`, but it needs
//! root, because a normal user may not open a drive. This module does the same
//! (it follows `read_sysinfo_extended` in libgpod's tools/ipod-scsi.c), and
//! gets the opened drive from UDisks2. UDisks2 only hands it over once the
//! person at the computer has allowed it: the system shows its usual
//! "Authentication is required" window, with the password or fingerprint of
//! an administrator (the answer is kept for a few minutes). That also works
//! inside the Flatpak sandbox.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::raw::{c_int, c_ulong, c_void};
use std::path::{Path, PathBuf};
use std::ptr;

use gtk::glib::{self, Variant};
use gtk::{gio, prelude::*};

use crate::{gpod, udisks};

/// Where the file goes, relative to the iPod.
const FILE: &str = "iPod_Control/Device/SysInfoExtended";

/// Why the first-time setup failed, and what the person can do instead.
pub struct Failure {
    pub reason: String,
    /// A command to type in a terminal.
    pub command: String,
}

/// Whether the iPod has its `SysInfoExtended` file already.
pub fn is_there(ipod: &Path) -> bool {
    fs::metadata(ipod.join(FILE)).is_ok_and(|file| file.len() > 0)
}

/// Whether this iPod needs the first-time setup: it has no `SysInfoExtended`
/// file yet, and libgpod can't do without. libgpod knows; opening the database
/// reads files from the slow iPod, so it happens off the main thread. (A
/// `Database` can't be passed between threads, so only the answer comes back.)
pub async fn needs_setup(mount_path: &Path) -> bool {
    if is_there(mount_path) {
        return false;
    }
    let path = mount_path.to_path_buf();
    gio::spawn_blocking(move || gpod::Database::open(&path).is_ok_and(|db| db.needs_first_time_setup()))
        .await
        .unwrap_or(false)
}

/// Reads the description from the iPod and saves it on the iPod.
pub async fn set_up(mount_path: &Path) -> Result<(), Failure> {
    fetch_and_save(mount_path).await.map_err(|problem| Failure {
        reason: problem.reason,
        command: manual_command(problem.device.as_deref(), mount_path),
    })
}

/// What went wrong, and, if it's known, which drive the iPod is.
struct Problem {
    reason: String,
    device: Option<String>,
}

impl Problem {
    fn new(reason: impl Into<String>, device: Option<&str>) -> Self {
        Problem { reason: reason.into(), device: device.map(str::to_string) }
    }
}

async fn fetch_and_save(mount_path: &Path) -> Result<(), Problem> {
    let (fd, device) = open_disk(mount_path).await?;
    // Asking the iPod takes a moment (it is a lot of small questions over USB),
    // so it happens off the main thread. A file descriptor can move between threads.
    let xml = gio::spawn_blocking(move || read_sysinfo_extended(fd.as_raw_fd(), &device))
        .await
        .map_err(|_| Problem::new("reading from the iPod crashed", None))??;
    let path = mount_path.to_path_buf();
    save(&path, &xml).map_err(|error| Problem::new(format!("couldn't save the file on the iPod: {error}"), None))
}

/// Asks UDisks2 for the iPod's whole disk, opened for reading, and returns it
/// with the disk's device file ("/dev/sdb"). The person at the computer may do
/// this without a password; it is what a normal user lacks for opening the
/// drive directly.
async fn open_disk(mount_path: &Path) -> Result<(OwnedFd, String), Problem> {
    let bus = gio::bus_get_future(gio::BusType::System)
        .await
        .map_err(|error| Problem::new(format!("can't reach the system's drive service: {error}"), None))?;
    let objects = udisks::managed_objects(&bus)
        .await
        .map_err(|error| Problem::new(format!("can't ask the system's drive service: {error}"), None))?;
    let (filesystem, _) = udisks::find_filesystem(&objects, mount_path)
        .ok_or_else(|| Problem::new("the system's drive service doesn't know this iPod", None))?;
    let disk = udisks::whole_disk(&objects, &filesystem)
        .ok_or_else(|| Problem::new("the system's drive service doesn't know this iPod's disk", None))?;
    let device = udisks::device_file(&objects, &disk).unwrap_or_else(|| "/dev/sdX".to_string());

    // OpenDevice(mode, options) gives back a file descriptor, which D-Bus
    // sends along in a list next to the reply.
    let parameters = Variant::tuple_from_iter(["r".to_variant(), glib::VariantDict::new(None).end()]);
    let (reply, fds) = bus
        .call_with_unix_fd_list_future(
            Some(udisks::NAME),
            &disk,
            "org.freedesktop.UDisks2.Block",
            "OpenDevice",
            Some(&parameters),
            None,
            gio::DBusCallFlags::NONE,
            10_000,
            gio::UnixFDList::NONE,
        )
        .await
        .map_err(|error| Problem::new(format!("the system wouldn't open the iPod's disk: {error}"), Some(&device)))?;
    let index = reply
        .child_value(0)
        .get::<glib::variant::Handle>()
        .ok_or_else(|| Problem::new("the system's answer was not understood", Some(&device)))?;
    let fds = fds.ok_or_else(|| Problem::new("the system's answer had no disk in it", Some(&device)))?;
    // The descriptor is ours now, and `OwnedFd` closes it when it's dropped.
    let fd = fds
        .get(index.0)
        .map_err(|error| Problem::new(format!("the system's answer had no disk in it: {error}"), Some(&device)))?;
    Ok((fd, device))
}

/// Asks the iPod for its description and checks that it looks like one.
fn read_sysinfo_extended(fd: RawFd, device: &str) -> Result<String, Problem> {
    assemble(|page| inquiry(fd, page).ok())
        .ok_or_else(|| Problem::new("the iPod did not answer the question for its description", Some(device)))
}

/// Puts the pages together. `read_page` asks the iPod for one page of
/// information and gives back what is in it.
///
/// Page 0xC0 is a list of the other pages (one byte each, ending with a
/// zero); together those pages are the XML text.
fn assemble(mut read_page: impl FnMut(u8) -> Option<Vec<u8>>) -> Option<String> {
    const PAGE_LIST: u8 = 0xC0;
    let list = read_page(PAGE_LIST)?;
    let mut xml = Vec::new();
    for page in list.into_iter().take_while(|&page| page != 0) {
        let data = read_page(page)?;
        // A page can be padded with zero bytes.
        let end = data.iter().position(|&byte| byte == 0).unwrap_or(data.len());
        xml.extend_from_slice(&data[..end]);
    }
    String::from_utf8(xml).ok().filter(|xml| looks_like_a_description(xml))
}

/// Whether the text is the XML property list that libgpod expects.
fn looks_like_a_description(xml: &str) -> bool {
    let xml = xml.trim();
    xml.starts_with("<?xml") && xml.contains("<plist") && xml.ends_with("</plist>")
}

/// Saves the description on the iPod. It's written under another name first
/// and renamed at the end, so a file that is half written never gets the real
/// name.
fn save(ipod: &Path, xml: &str) -> io::Result<()> {
    let target = ipod.join(FILE);
    if let Some(folder) = target.parent() {
        // Not every iPod has a Device folder.
        fs::create_dir_all(folder)?;
    }
    let mut partial = target.clone().into_os_string();
    partial.push(".part");
    let partial = PathBuf::from(partial);
    let result = fs::write(&partial, xml).and_then(|()| fs::rename(&partial, &target));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result
}

/// The command a person can run instead, when Oxipod can't do it itself.
pub fn manual_command(device: Option<&str>, mount_path: &Path) -> String {
    format!(
        "sudo ipod-read-sysinfo-extended {} {}",
        device.unwrap_or("/dev/sdX"),
        shell_quote(&mount_path.to_string_lossy())
    )
}

/// Puts quotes around text for a terminal, if it needs them. A mount point
/// like "/run/media/dylan/DYLAN_S IPO" has a space in it.
fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty() && text.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+=,:@%".contains(c));
    if plain {
        text.to_string()
    } else {
        // Inside single quotes nothing is special, except the quote itself.
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

// What follows talks to the iPod. Linux has one function for sending a
// command to a device, `ioctl`, and it is declared here by hand because that
// is simpler than another crate. `SgIoHdr` is the form in which the command is
// handed over (`sg_io_hdr_t` in <scsi/sg.h>), field for field.

unsafe extern "C" {
    fn ioctl(fd: c_int, request: c_ulong, ...) -> c_int;
}

/// The request "send this SCSI command" (`SG_IO`).
const SG_IO: c_ulong = 0x2285;
/// The command reads from the device (`SG_DXFER_FROM_DEV`).
const FROM_DEVICE: c_int = -3;

#[repr(C)]
struct SgIoHdr {
    interface_id: c_int,
    dxfer_direction: c_int,
    cmd_len: u8,
    mx_sb_len: u8,
    iovec_count: u16,
    dxfer_len: u32,
    dxferp: *mut c_void,
    cmdp: *mut u8,
    sbp: *mut u8,
    timeout: u32,
    flags: u32,
    pack_id: c_int,
    usr_ptr: *mut c_void,
    status: u8,
    masked_status: u8,
    msg_status: u8,
    sb_len_wr: u8,
    host_status: u16,
    driver_status: u16,
    resid: c_int,
    duration: u32,
    info: u32,
}

// If the layout is not the one the kernel expects, this fails to compile.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<SgIoHdr>() == 88);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(size_of::<SgIoHdr>() == 64);

/// Sends INQUIRY to the device, asking for page `page` of its "vital product
/// data", and returns what is in the page.
fn inquiry(fd: RawFd, page: u8) -> io::Result<Vec<u8>> {
    // The answer has room for 252 bytes; the first four are a header, and the
    // fourth of those says how long the page is.
    let mut data = [0u8; 252];
    let mut sense = [0u8; 32];
    // The command is six bytes: INQUIRY (0x12), "give me a page" (a 1), which
    // page, the room we have (as two bytes), and a 0.
    let mut command = [0x12, 0x01, page, 0x00, data.len() as u8, 0x00];
    let mut header = SgIoHdr {
        interface_id: c_int::from(b'S'),
        dxfer_direction: FROM_DEVICE,
        cmd_len: command.len() as u8,
        mx_sb_len: sense.len() as u8,
        iovec_count: 0,
        dxfer_len: data.len() as u32,
        dxferp: data.as_mut_ptr().cast(),
        cmdp: command.as_mut_ptr(),
        sbp: sense.as_mut_ptr(),
        timeout: 5_000,
        flags: 0,
        pack_id: 0,
        usr_ptr: ptr::null_mut(),
        status: 0,
        masked_status: 0,
        msg_status: 0,
        sb_len_wr: 0,
        host_status: 0,
        driver_status: 0,
        resid: 0,
        duration: 0,
        info: 0,
    };
    // SAFETY: `header` points at `data`, `command` and `sense`, which all stay
    // alive until the call returns and are as long as the header says. The
    // kernel reads the command and writes into the other two. `fd` is an open
    // file descriptor, and `SG_IO` takes a pointer to an `SgIoHdr`.
    let result = unsafe { ioctl(fd, SG_IO, &mut header as *mut SgIoHdr) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if header.status != 0 || header.host_status != 0 || header.driver_status != 0 {
        return Err(io::Error::other(format!("the device refused page {page:#04x}")));
    }
    let length = usize::from(data[3]).min(data.len() - 4);
    Ok(data[4..4 + length].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pretend iPod: page 0xC0 lists the pages 0xC2 and 0xC3, which hold the text.
    fn pretend_ipod(page: u8) -> Option<Vec<u8>> {
        match page {
            0xC0 => Some(vec![0xC2, 0xC3, 0]),
            0xC2 => Some(b"<?xml version=\"1.0\"?><plist><dict>".to_vec()),
            // Padded with zero bytes, like a page can be.
            0xC3 => Some(b"<key>FireWireGUID</key></dict></plist>\0\0\0".to_vec()),
            _ => None,
        }
    }

    #[test]
    fn puts_the_pages_together() {
        let xml = assemble(pretend_ipod).unwrap();
        assert_eq!(xml, "<?xml version=\"1.0\"?><plist><dict><key>FireWireGUID</key></dict></plist>");
    }

    #[test]
    fn gives_up_when_the_ipod_does_not_answer() {
        // No list of pages at all, as an old iPod might say.
        assert_eq!(assemble(|_| None), None);
        // A page from the list is missing.
        let broken = |page| if page == 0xC3 { None } else { pretend_ipod(page) };
        assert_eq!(assemble(broken), None);
        // An empty list gives no text, and that is no description.
        assert_eq!(assemble(|page| (page == 0xC0).then(|| vec![0])), None);
    }

    #[test]
    fn only_accepts_something_that_looks_like_a_description() {
        let not_xml = |page| match page {
            0xC0 => Some(vec![0xC2, 0]),
            _ => Some(b"hello, I am not a property list".to_vec()),
        };
        assert_eq!(assemble(not_xml), None);
        // Bytes that are not text.
        let garbage = |page| match page {
            0xC0 => Some(vec![0xC2, 0]),
            _ => Some(vec![0xff, 0xfe, 0xfd]),
        };
        assert_eq!(assemble(garbage), None);
    }

    #[test]
    fn saves_the_file_in_the_device_folder_and_leaves_nothing_else() {
        let folder = tempfile::tempdir().unwrap();
        let ipod = folder.path();
        assert!(!is_there(ipod));

        // The Device folder doesn't exist yet and is made.
        save(ipod, "<?xml?><plist></plist>").unwrap();
        assert!(is_there(ipod));
        assert_eq!(fs::read_to_string(ipod.join(FILE)).unwrap(), "<?xml?><plist></plist>");
        let names: Vec<_> = fs::read_dir(ipod.join("iPod_Control/Device")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names, ["SysInfoExtended"]);

        // A second time replaces it.
        save(ipod, "<?xml?><plist>new</plist>").unwrap();
        assert_eq!(fs::read_to_string(ipod.join(FILE)).unwrap(), "<?xml?><plist>new</plist>");
    }

    #[test]
    fn an_empty_file_does_not_count() {
        let folder = tempfile::tempdir().unwrap();
        fs::create_dir_all(folder.path().join("iPod_Control/Device")).unwrap();
        fs::write(folder.path().join(FILE), "").unwrap();
        assert!(!is_there(folder.path()));
    }

    #[test]
    fn writes_the_command_a_person_can_type() {
        assert_eq!(
            manual_command(Some("/dev/sdb"), Path::new("/run/media/dylan/DYLAN_S IPO")),
            "sudo ipod-read-sysinfo-extended /dev/sdb '/run/media/dylan/DYLAN_S IPO'"
        );
        assert_eq!(
            manual_command(None, Path::new("/mnt/ipod")),
            "sudo ipod-read-sysinfo-extended /dev/sdX /mnt/ipod"
        );
        assert_eq!(shell_quote("it's here"), "'it'\\''s here'");
    }

    // Asks a real iPod and compares the answer with the file libgpod's own
    // tool saved on it earlier. Only reads from the iPod. Run with:
    // OXIPOD_TEST_IPOD=/run/media/... cargo test reads_what_libgpods_tool_saved -- --ignored --nocapture
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn reads_what_libgpods_tool_saved() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let mount = Path::new(&mount);
        let saved = fs::read_to_string(mount.join(FILE)).expect("the iPod needs a SysInfoExtended file made by libgpod's tool to compare with");

        let start = std::time::Instant::now();
        let (fd, device) = match glib::MainContext::default().block_on(open_disk(mount)) {
            Ok(opened) => opened,
            Err(problem) => panic!("could not open the disk: {}", problem.reason),
        };
        println!("opened {device} after {:?}", start.elapsed());
        let start = std::time::Instant::now();
        let xml = match read_sysinfo_extended(fd.as_raw_fd(), &device) {
            Ok(xml) => xml,
            Err(problem) => panic!("could not read the description: {}", problem.reason),
        };
        println!("{} bytes read in {:?}, {} bytes saved by libgpod's tool", xml.len(), start.elapsed(), saved.len());
        assert_eq!(xml.len(), saved.len(), "not the same length as what libgpod's tool saved");
        assert!(
            without_rbsync(&xml) == without_rbsync(&saved),
            "different from what libgpod's tool saved (apart from rbsync)"
        );
    }

    // Says what Oxipod makes of a mounted iPod. Changes nothing, and doesn't
    // open the disk, so the system doesn't ask for anything. Run with:
    // OXIPOD_TEST_IPOD=/run/media/... cargo test what_oxipod_thinks -- --ignored --nocapture
    #[test]
    #[ignore = "needs a mounted iPod, set OXIPOD_TEST_IPOD to its mount point"]
    fn what_oxipod_thinks_of_a_mounted_ipod() {
        let mount = std::env::var("OXIPOD_TEST_IPOD").expect("OXIPOD_TEST_IPOD is not set");
        let mount = Path::new(&mount);
        let db = gpod::Database::open(mount).expect("could not open the database");
        println!("the description file is there: {}", is_there(mount));
        println!("what libgpod makes of the model: {:?}", db.device_info());
        println!("libgpod needs the setup: {}", db.needs_first_time_setup());
        println!("so needs_setup() says: {}", glib::MainContext::default().block_on(needs_setup(mount)));
    }

    /// The iPod makes up a new value for `rbsync` every time it is asked, so
    /// that one is left out when two answers are compared.
    fn without_rbsync(xml: &str) -> String {
        let mut skipping = false;
        let mut kept = String::new();
        for line in xml.lines() {
            skipping |= line.contains("<key>rbsync</key>");
            if !skipping {
                kept.push_str(line);
                kept.push('\n');
            }
            if skipping && line.contains("</data>") {
                skipping = false;
            }
        }
        kept
    }

    #[test]
    fn leaves_out_the_part_that_changes_every_time() {
        let one = "<plist>\n<key>a</key>\n<key>rbsync</key>\n<data>AAAA\nBBBB==</data>\n<key>b</key>\n</plist>";
        let two = "<plist>\n<key>a</key>\n<key>rbsync</key>\n<data>CCCC\nDDDD==</data>\n<key>b</key>\n</plist>";
        assert_eq!(without_rbsync(one), without_rbsync(two));
        assert_eq!(without_rbsync(one), "<plist>\n<key>a</key>\n<key>b</key>\n</plist>\n");
    }
}
