# AGENTS.md

Guidance for AI coding agents (and humans) working in this repository. `CLAUDE.md` just imports this file.

Disk Image Mounter (`dim.exe`) is a tiny Windows 10/11 x64 GUI that mounts disc images as real optical drives. It is written in Rust 1.99, edition 2024, using raw Win32 through `windows-sys`. There is no UI framework and no kernel driver.

## Commands

```sh
cargo build --release            # -> target/release/dim.exe (~340 KB)
cargo test                       # unit tests (convert.rs, store.rs)
cargo test cue_audio_only        # single test by name substring
cargo fmt --check                # formatting (rustfmt.toml forces CRLF)
cargo clippy --release --all-targets -- -D warnings   # must stay at zero warnings
cargo run --release -- "C:\path\image.cue"   # paths on the command line are mounted on start
```

- **A running `dim.exe` locks the binary.** Close it (tray → Quit, or `taskkill /F /IM dim.exe`) before `cargo build --release`, or the link step fails with "Access denied". Killing it detaches every mounted drive.
- **Flags:** `--tray` starts hidden in the tray.
- **Icon:** `assets/app.ico` is generated from `assets/dim-256.svg` with ImageMagick. Sizes 16, 20 and 24 px use a simplified SVG without the eject badge and the thin rings. The 256 px entry must be PNG-compressed, because a BMP 256 entry adds about 260 KB to the exe. `build.rs` embeds `assets/app.ico` through `embed-resource` (`assets/app.rc`) and embeds `assets/app.manifest` with linker args.

## Architecture

**Mounting has no driver.** `vdisk.rs` calls `OpenVirtualDisk` and then `AttachVirtualDisk` with `VIRTUAL_STORAGE_TYPE_DEVICE_ISO`, read-only, and *without* `PERMANENT_LIFETIME`.
- **Lifetime:** the drive exists only while the returned handle is open. `Drop for Mount` closes the handle, which detaches the drive, then deletes any temp ISO. If the process dies, the OS detaches everything, so nothing is left orphaned.
- **Admin rights:** this works without elevation (verified), so the manifest is `asInvoker`.
- **Drive letter:** the mount manager assigns it asynchronously. `attach` polls for about 1 s; after that a `WM_TIMER` in `main.rs` keeps calling `Mount::resolve()` for up to about 60 s. The letter is found by matching `QueryDosDeviceW("X:")` against `\Device\CdRomN`, taken from `GetVirtualDiskPhysicalPath`.

**Only ISO is mountable natively.** `convert.rs` handles every other format.
- **`prepare()`:** decides between `Source::Direct` (the file is already an ISO) and `Source::Extract(Plan)`.
- **Detection by content, not extension:** a raw sector starts with the 12-byte CD sync pattern. The stride is 2352 or 2448 (with subchannel). The mode byte gives the user-data offset: +16 for MODE1, +24 for MODE2 Form 1.
- **CUE:** a minimal parser. It uses the first data track and its `INDEX 01`, and stops at the first index of the next `TRACK` in the same file (sub-indexes of the data track do not end it). A `FILE` naming a network share or device (`\host\share`, `\.\pipe\x`) is rejected.
- **NRG:** the footer (`NER5`/`NERO`) points to `ETNF`/`ETN2` chunks. A chunk larger than the rest of the file is rejected, and audio tracks (mode 7) are skipped.
- **`extract()`:** streams 2048-byte user data into `%TEMP%\dim\<pid>-<n>.iso` using fixed 1 MiB buffers. It runs on a worker thread and supports cancellation.
- **Known limits, kept on purpose:** no audio tracks, no Mode 2 Form 2 data, no compressed formats (CSO, CHD, …), and no relocation of a data track that does not start at LBA 0.

**UI (`main.rs`):** a single window with a single UI thread. All state lives in `thread_local!`s:
- `UI`: a `Cell<Ui>` holding copies of the control handles.
- `CORE`: a `RefCell<Core>` holding mounts, the running job, the mount queue and the recent list.
- **Re-entrancy rule:** never hold a `CORE` borrow across anything that pumps messages: `MessageBoxW`, `TaskDialogIndirect`, `TrackPopupMenu`, `GetOpenFileNameW`, or dropping a `Mount` (its `Drop` may sleep). Take what you need out of the borrow first. Nested window messages would otherwise panic on a `RefCell` double borrow.
- **Mount flow:** `request_mount` either queues the path (while a conversion runs) or calls `start`. `start` runs `prepare`; a direct ISO is attached immediately, otherwise a worker thread converts it. The worker posts `WM_PROGRESS` and `WM_DONE`; the worker stores its `Done` in the `DONE` mutex and `WM_DONE` only signals it (no pointer travels in `lParam`). `finish_mount` attaches, records the recent entry, and calls `refresh()`.
- **`refresh()`:** rebuilds the list view from `CORE`, toggles the visibility of the empty-state hint and the progress bar, and calls `layout()`.
- **Single instance:** a `Local\` mutex. A second launch forwards its paths to the running window through `WM_COPYDATA`, then exits.
- **Closing:** the window close button hides to the tray. Tray → Quit asks for confirmation when something is mounted (`confirm_quit`), then `DestroyWindow`, which drops all mounts.
- **Eject in Explorer:** can leave the letter mapped while the media is gone. `WM_DEVICECHANGE` triggers `revalidate()`, which calls `Mount::alive()`: the mount is dropped when its device lost its letter or its volume reports not-ready after having answered once; a changed letter is followed.
- **Startup cleanup:** at startup the app deletes everything in `%TEMP%\dim`. The single-instance guard guarantees these are orphans.

**Persistence (`store.rs`):** plain text files in `%APPDATA%\dim\`.
- `recent.txt`: the last 10 images, de-duplicated case-insensitively.
- `window.txt`: `x y w h`, with width and height normalized to 96 DPI. The window is restored only if enough of it, title bar included, is inside a monitor work area (`on_screen`); otherwise it is centered on the work area.

## Conventions

- **Language:** UI strings are en-US. The product name shown to users is "Disk Image Mounter" (not "dim").
- **Theme:** the UI is always the light theme. Dark mode was implemented and then removed on purpose, so do not reintroduce it.
- **Native dialogs:** the About box and the quit confirmation use the native `TaskDialogIndirect`. The quit dialog sets custom button captions so it stays in English on non-English Windows.
- **Layout:** sizes are given at 96 DPI and scaled with `GetDpiForWindow` (manifest: PerMonitorV2). The default context-menu item is bold (`SetMenuDefaultItem`) and matches the double-click/Enter action.
- **Errors:** error text shown to the user goes through `err_text` so it stays in English on non-English Windows.
- **Line endings and format:** the whole repo uses CRLF (Windows-only app; `Cargo.lock` is the exception) and is formatted with `cargo fmt`. Preserve CRLF when editing files with tools that normalize line endings.
- **Shortcuts:** deliberate shortcuts carry a `// shortcut: <limit>, <when to upgrade>` comment.
