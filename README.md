# dim — Disk Image Mounter

Minimal disc image mounter for Windows 10/11 x64. No driver, no admin, ~210 KB, ~11 MB RAM, 0% CPU idle.

Mounts through the native virtdisk API, so images show up as a real optical drive.

- **Use:** run `dim.exe`, click *Mount…* (Ctrl+O), drag a file onto the window, or pass paths on the command line.
  Double-click a row to open the drive; *Unmount* (Del) unmounts. Closing the window keeps dim in the tray; *Quit* (asks first) unmounts everything.
- **Recent images:** the arrow on *Mount…* lists the last 10 mounted images (also in the tray menu). Right-click a row for open / show in Explorer / copy path.
- **Window:** remembers position and size. `dim.exe --tray` starts hidden in the tray; a balloon announces mounts made while hidden.
- **Mounts last while dim runs.** If dim exits or crashes, Windows detaches the drives.
- **Formats:** ISO (CD/DVD/Blu-ray UDF) mounts directly. BIN/IMG (raw 2352/2448), CUE, MDF/MDS, CCD/IMG and NRG
  have their first data track extracted to `%TEMP%\dim\*.iso` (needs free space = image size; removed on unmount).

## Limits
- Audio tracks are not emulated. Mode 2 Form 2 data (VCD/PS1 streams) is wrong.
- Compressed formats (CSO, CHD, ISZ, DAA, ECM) are unsupported.
- "Sparse" ISOs (common from torrents) can't be mounted by Windows; dim shows the `fsutil sparse setflag` fix.
- Unsigned exe: SmartScreen may warn on first run.

Build: `cargo build --release` (Rust 1.99, edition 2024). Tests: `cargo test`.

MIT licensed, (c) 2026 Glauber Lima - https://github.com/glauberlima
