//! Attach/detach ISO files as virtual optical drives via virtdisk (no driver, no admin).
use std::{
    ffi::OsStr,
    io,
    os::windows::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    thread::sleep,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE},
    Storage::FileSystem::{GetLogicalDrives, QueryDosDeviceW},
    Storage::Vhd::*,
};

const FILE_ATTRIBUTE_SPARSE_FILE: u32 = 0x200;

pub fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

/// An attached image. Dropping it closes the handle, which detaches the drive.
pub struct Mount {
    handle: HANDLE,
    device: String, // e.g. \Device\CdRom1
    pub letter: Option<char>,
    pub image: PathBuf,
    pub size: u64,        // bytes of the mounted ISO
    pub converted: bool, // true when extracted from CUE/BIN/NRG/...
    temp: Option<PathBuf>,
}

pub fn attach(iso: &Path, image: PathBuf, temp: Option<PathBuf>) -> io::Result<Mount> {
    if iso.metadata()?.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "The file is \"sparse\" and Windows cannot mount it.\nFix it with:\nfsutil sparse setflag \"{}\" 0",
                iso.display()
            ),
        ));
    }
    let p = wide(iso.as_os_str());
    let st = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_ISO,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    };
    let mut h: HANDLE = null_mut();
    unsafe {
        let r = OpenVirtualDisk(&st, p.as_ptr(), VIRTUAL_DISK_ACCESS_READ, OPEN_VIRTUAL_DISK_FLAG_NONE, null(), &mut h);
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        // No PERMANENT_LIFETIME flag: closing the handle (or dying) detaches the drive.
        let r = AttachVirtualDisk(h, null_mut(), ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY, 0, null(), null());
        if r != 0 {
            CloseHandle(h);
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let size = iso.metadata().map(|m| m.len()).unwrap_or(0);
        let mut m = Mount { handle: h, device: String::new(), letter: None, image, size, converted: temp.is_some(), temp };
        // mountmgr assigns the letter asynchronously; slower discs are picked up by resolve().
        for _ in 0..10 {
            if m.resolve() {
                break;
            }
            sleep(Duration::from_millis(100));
        }
        Ok(m)
    }
}

fn physical_device(h: HANDLE) -> String {
    let mut buf = [0u16; 260];
    let mut size = std::mem::size_of_val(&buf) as u32;
    if unsafe { GetVirtualDiskPhysicalPath(h, &mut size, buf.as_mut_ptr()) } != 0 {
        return String::new();
    }
    let s = String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]);
    s.to_ascii_uppercase().strip_prefix(r"\\.\CDROM").map(|n| format!(r"\Device\CdRom{n}")).unwrap_or_default()
}

fn letter_device(letter: char) -> Option<String> {
    let name: Vec<u16> = format!("{letter}:").encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 260];
    let n = unsafe { QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (n != 0).then(|| String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]))
}

fn find_letter(device: &str) -> Option<char> {
    let mask = unsafe { GetLogicalDrives() };
    ('A'..='Z')
        .filter(|c| mask & (1 << (*c as u8 - b'A')) != 0)
        .find(|&c| letter_device(c).is_some_and(|d| d.eq_ignore_ascii_case(device)))
}

impl Mount {
    /// Looks up the drive letter if still unknown. True once there is one.
    pub fn resolve(&mut self) -> bool {
        if self.letter.is_none() {
            if self.device.is_empty() {
                self.device = physical_device(self.handle);
            }
            if !self.device.is_empty() {
                self.letter = find_letter(&self.device);
            }
        }
        self.letter.is_some()
    }

    /// False once the drive vanished behind our back (e.g. "Eject" in Explorer).
    pub fn alive(&self) -> bool {
        match self.letter {
            Some(c) => letter_device(c).is_some_and(|d| d.eq_ignore_ascii_case(&self.device)),
            None => true,
        }
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.handle) };
        if let Some(t) = &self.temp {
            for _ in 0..5 {
                match std::fs::remove_file(t) {
                    Ok(()) => break,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                    Err(_) => sleep(Duration::from_millis(100)),
                }
            }
        }
    }
}
