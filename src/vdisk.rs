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
    Foundation::{CloseHandle, ERROR_NOT_READY, GetLastError, HANDLE},
    Storage::FileSystem::{
        FILE_ATTRIBUTE_SPARSE_FILE, GetLogicalDrives, GetVolumeInformationW, QueryDosDeviceW,
    },
    Storage::Vhd::*,
};

pub fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain([0]).collect()
}

/// Decodes a UTF-16 buffer up to its first NUL.
pub fn from_wide(buf: &[u16]) -> String {
    String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)])
}

/// OpenVirtualDisk does not add the extended-length prefix that std adds on its own,
/// so absolute paths of 260+ characters need it spelled out.
fn verbatim(p: &Path) -> PathBuf {
    if p.as_os_str().len() < 260 {
        return p.to_path_buf();
    }
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    let s = abs.to_string_lossy();
    if s.starts_with(r"\\?\") {
        return abs;
    }
    match s.strip_prefix(r"\\") {
        Some(unc) => PathBuf::from(format!(r"\\?\UNC\{unc}")),
        None => PathBuf::from(format!(r"\\?\{s}")),
    }
}

/// An attached image. Dropping it closes the handle, which detaches the drive.
pub struct Mount {
    handle: HANDLE,
    device: String, // e.g. \Device\CdRom1
    pub letter: Option<char>,
    pub image: PathBuf,
    pub size: u64,       // bytes of the mounted ISO
    pub converted: bool, // true when extracted from CUE/BIN/NRG/...
    temp: Option<PathBuf>,
    ready_seen: bool, // the volume answered at least once, so "not ready" later means ejected
}

pub fn attach(iso: &Path, image: PathBuf, temp: Option<PathBuf>) -> io::Result<Mount> {
    let meta = iso.metadata()?;
    if meta.file_attributes() & FILE_ATTRIBUTE_SPARSE_FILE != 0 {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!(
                "The file is \"sparse\" and Windows cannot mount it.\nFix it with:\nfsutil sparse setflag \"{}\" 0",
                iso.display()
            ),
        ));
    }
    let p = wide(verbatim(iso).as_os_str());
    let st = VIRTUAL_STORAGE_TYPE {
        DeviceId: VIRTUAL_STORAGE_TYPE_DEVICE_ISO,
        VendorId: VIRTUAL_STORAGE_TYPE_VENDOR_MICROSOFT,
    };
    let mut h: HANDLE = null_mut();
    unsafe {
        let r = OpenVirtualDisk(
            &st,
            p.as_ptr(),
            VIRTUAL_DISK_ACCESS_READ,
            OPEN_VIRTUAL_DISK_FLAG_NONE,
            null(),
            &mut h,
        );
        if r != 0 {
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        // No PERMANENT_LIFETIME flag: closing the handle (or dying) detaches the drive.
        let r = AttachVirtualDisk(
            h,
            null_mut(),
            ATTACH_VIRTUAL_DISK_FLAG_READ_ONLY,
            0,
            null(),
            null(),
        );
        if r != 0 {
            CloseHandle(h);
            return Err(io::Error::from_raw_os_error(r as i32));
        }
        let mut m = Mount {
            handle: h,
            device: String::new(),
            letter: None,
            image,
            size: meta.len(),
            converted: temp.is_some(),
            temp,
            ready_seen: false,
        };
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
    from_wide(&buf)
        .to_ascii_uppercase()
        .strip_prefix(r"\\.\CDROM")
        .map(|n| format!(r"\Device\CdRom{n}"))
        .unwrap_or_default()
}

fn letter_device(letter: char) -> Option<String> {
    let name: Vec<u16> = format!("{letter}:").encode_utf16().chain([0]).collect();
    let mut buf = [0u16; 260];
    let n = unsafe { QueryDosDeviceW(name.as_ptr(), buf.as_mut_ptr(), buf.len() as u32) };
    (n != 0).then(|| from_wide(&buf))
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

    /// False once the drive vanished behind our back: the device lost its letter, or its media
    /// was ejected (Explorer "Eject" keeps the letter but the volume stops answering).
    /// Follows the device if the letter was changed in Disk Management.
    pub fn alive(&mut self) -> bool {
        if self.letter.is_none() {
            return true;
        }
        let Some(now) = find_letter(&self.device) else {
            return false;
        };
        self.letter = Some(now);
        let root = wide(OsStr::new(&format!("{now}:\\")));
        let ok = unsafe {
            GetVolumeInformationW(
                root.as_ptr(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            )
        } != 0;
        if ok {
            self.ready_seen = true;
            return true;
        }
        // Before the first success a not-ready volume is just still mounting.
        !(self.ready_seen && unsafe { GetLastError() } == ERROR_NOT_READY)
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
