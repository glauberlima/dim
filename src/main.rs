#![windows_subsystem = "windows"]
#![allow(unsafe_op_in_unsafe_fn)]

mod convert;
mod store;
mod vdisk;

use convert::Source;
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    ffi::OsString,
    mem::{size_of, zeroed},
    os::windows::ffi::OsStringExt,
    path::{Path, PathBuf},
    ptr::{null, null_mut},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering::Relaxed},
    },
};
use vdisk::{Mount, wide};
use windows_sys::Win32::{
    Foundation::*,
    Graphics::Gdi::*,
    Storage::FileSystem::GetVolumeInformationW,
    System::{
        DataExchange::*,
        LibraryLoader::GetModuleHandleW,
        Memory::*,
        Threading::CreateMutexW,
    },
    UI::{
        Controls::{Dialogs::*, *},
        HiDpi::{GetDpiForWindow, GetSystemMetricsForDpi},
        Input::KeyboardAndMouse::*,
        Shell::*,
        WindowsAndMessaging::*,
    },
};

const ID_LIST: i32 = 10;
const ID_MOUNT: i32 = 101;
const ID_UNMOUNT: i32 = 102;
const ID_CANCEL: i32 = 103;
const ID_QUIT: i32 = 104;
const ID_ABOUT: i32 = 105;
const ID_MENU_UNMOUNT: i32 = 300; // 300..400: tray "Desmontar X:" entries
const ID_RECENT: i32 = 400; // 400..410
const ID_CLEAR_RECENT: i32 = 420;
const ID_CTX_OPEN: i32 = 430;
const ID_CTX_SHOW: i32 = 431;
const ID_CTX_COPY: i32 = 432;
const WM_TRAY: u32 = WM_APP + 1;
const WM_PROGRESS: u32 = WM_APP + 2;
const WM_DONE: u32 = WM_APP + 3;
const DBT_DEVNODES_CHANGED: usize = 0x0007;
const DBT_DEVICEREMOVECOMPLETE: usize = 0x8004;
const CF_UNICODETEXT: u32 = 13;
const CLASS: &str = "DimMainWindow";
const BASE_W: i32 = 640; // default client-independent window size at 96 DPI
const BASE_H: i32 = 340;

#[derive(Clone, Copy)]
struct Ui {
    hwnd: HWND,
    list: HWND,
    mount: HWND,
    unmount: HWND,
    cancel: HWND,
    about: HWND,
    status: HWND,
    progress: HWND,
    hint: HWND,
}

impl Ui {
    const NULL: Ui = Ui {
        hwnd: null_mut(),
        list: null_mut(),
        mount: null_mut(),
        unmount: null_mut(),
        cancel: null_mut(),
        about: null_mut(),
        status: null_mut(),
        progress: null_mut(),
        hint: null_mut(),
    };
}

#[derive(Default)]
struct Core {
    mounts: Vec<Mount>,
    job: Option<Arc<AtomicBool>>, // cancel flag of the running conversion
    queue: VecDeque<PathBuf>,
    recent: Vec<PathBuf>,
}

struct Done {
    image: PathBuf,
    temp: PathBuf,
    res: std::io::Result<()>,
}

thread_local! {
    static UI: Cell<Ui> = const { Cell::new(Ui::NULL) };
    static CORE: RefCell<Core> = RefCell::new(Core::default());
    static FONT: Cell<HFONT> = const { Cell::new(null_mut()) };
}
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static TEMP_SEQ: AtomicU32 = AtomicU32::new(0);
static TIMER_TICKS: AtomicU32 = AtomicU32::new(0);

fn ui() -> Ui {
    UI.get()
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn rgb(r: u32, g: u32, b: u32) -> u32 {
    r | g << 8 | b << 16
}

fn copy_wide(dst: &mut [u16], s: &str) {
    let max = dst.len() - 1;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(max)) {
        *d = c;
    }
}

fn msg_box(text: &str, flags: u32) {
    unsafe { MessageBoxW(ui().hwnd, w(text).as_ptr(), w("Disk Image Mounter").as_ptr(), flags) };
}

fn set_status(text: &str) {
    unsafe { SetWindowTextW(ui().status, w(text).as_ptr()) };
}

fn temp_dir() -> PathBuf {
    std::env::temp_dir().join("dim")
}

fn human_size(n: u64) -> String {
    let s = if n >= 1 << 30 { format!("{:.1} GB", n as f64 / (1u64 << 30) as f64) } else if n >= 1 << 20 { format!("{} MB", n >> 20) } else { format!("{} KB", n >> 10) };
    s
}

fn volume_label(letter: char) -> String {
    let root = w(&format!("{letter}:\\"));
    let mut buf = [0u16; 261];
    let ok = unsafe { GetVolumeInformationW(root.as_ptr(), buf.as_mut_ptr(), 261, null_mut(), null_mut(), null_mut(), null_mut(), 0) };
    if ok == 0 { String::new() } else { String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]) }
}

fn main() {
    unsafe {
        let mut start_hidden = false;
        let mut args: Vec<PathBuf> = vec![];
        for a in std::env::args_os().skip(1) {
            match a.to_str() {
                Some("--tray") => start_hidden = true,
                _ => args.push(PathBuf::from(a)),
            }
        }
        let mutex = CreateMutexW(null(), 0, w("Local\\dim-single-instance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            if !(start_hidden && args.is_empty()) {
                forward_to_running(&args);
            }
            return;
        }
        let _keep = mutex;
        // Single instance is guaranteed here, so every leftover is an orphan.
        if let Ok(rd) = std::fs::read_dir(temp_dir()) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        CORE.with_borrow_mut(|c| c.recent = store::load_recent());
        InitCommonControlsEx(&INITCOMMONCONTROLSEX {
            dwSize: size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES | ICC_PROGRESS_CLASS | ICC_STANDARD_CLASSES,
        });
        TASKBAR_CREATED.store(RegisterWindowMessageW(w("TaskbarCreated").as_ptr()), Relaxed);

        let hi = GetModuleHandleW(null());
        let class = w(CLASS);
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hi,
            hIcon: null_mut(),
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hbrBackground: (COLOR_BTNFACE + 1) as usize as HBRUSH,
            lpszMenuName: null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0, class.as_ptr(), w("Disk Image Mounter").as_ptr(), WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT, CW_USEDEFAULT, BASE_W, BASE_H, null_mut(), null_mut(), hi, null(),
        );
        if hwnd.is_null() {
            return;
        }
        place_window(hwnd);
        if !start_hidden {
            ShowWindow(hwnd, SW_SHOW);
        }
        for a in args {
            request_mount(a);
        }

        let accel = CreateAcceleratorTableW(
            &ACCEL { fVirt: (FCONTROL | FVIRTKEY) as u8, key: b'O' as u16, cmd: ID_MOUNT as u16 },
            1,
        );
        let mut m: MSG = zeroed();
        while GetMessageW(&mut m, null_mut(), 0, 0) > 0 {
            if TranslateAcceleratorW(hwnd, accel, &m) == 0 && IsDialogMessageW(hwnd, &mut m) == 0 {
                TranslateMessage(&m);
                DispatchMessageW(&m);
            }
        }
    }
}

/// Restores the saved geometry if it is still on a monitor, otherwise centers on the work area.
/// shortcut: centers on the primary monitor, not the one under the cursor.
unsafe fn place_window(hwnd: HWND) {
    let d = GetDpiForWindow(hwnd).max(96) as i32;
    let saved = store::load_window().and_then(|[x, y, sw, sh]| {
        let r = RECT { left: x, top: y, right: x + sw * d / 96, bottom: y + sh * d / 96 };
        (sw >= 300 && sh >= 200 && !MonitorFromRect(&r, MONITOR_DEFAULTTONULL).is_null()).then_some(r)
    });
    let r = saved.unwrap_or_else(|| {
        let mut wa: RECT = zeroed();
        SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut wa as *mut _ as *mut _, 0);
        let (ww, wh) = (BASE_W * d / 96, BASE_H * d / 96);
        let (x, y) = (wa.left + (wa.right - wa.left - ww) / 2, wa.top + (wa.bottom - wa.top - wh) / 2);
        RECT { left: x, top: y, right: x + ww, bottom: y + wh }
    });
    SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER);
}

unsafe fn save_geometry() {
    let h = ui().hwnd;
    if IsIconic(h) != 0 || IsZoomed(h) != 0 {
        return;
    }
    let (mut r, d): (RECT, i32) = (zeroed(), GetDpiForWindow(h).max(96) as i32);
    GetWindowRect(h, &mut r);
    store::save_window([r.left, r.top, (r.right - r.left) * 96 / d, (r.bottom - r.top) * 96 / d]);
}

unsafe fn forward_to_running(args: &[PathBuf]) {
    let target = FindWindowW(w(CLASS).as_ptr(), null());
    if target.is_null() {
        return;
    }
    let send = |kind: usize, path: &[u16]| {
        let cds = COPYDATASTRUCT { dwData: kind, cbData: (path.len() * 2) as u32, lpData: path.as_ptr() as *mut _ };
        SendMessageW(target, WM_COPYDATA, 0, &cds as *const _ as isize);
    };
    if args.is_empty() {
        send(0, &[0]);
    }
    for a in args {
        let full = std::fs::canonicalize(a).unwrap_or_else(|_| a.clone());
        send(1, &wide(full.as_os_str()));
    }
}

// ---------- mounting ----------

fn request_mount(path: PathBuf) {
    let busy = CORE.with_borrow_mut(|c| {
        if c.job.is_some() {
            c.queue.push_back(path.clone());
        }
        c.job.is_some()
    });
    if busy {
        set_status("Queued — waiting for the current conversion…");
    } else {
        start(path);
    }
}

fn start(path: PathBuf) {
    let key = |p: &Path| p.to_string_lossy().to_lowercase();
    let dup = CORE.with_borrow(|c| c.mounts.iter().position(|m| key(&m.image) == key(&path)));
    if let Some(i) = dup {
        select(i);
        set_status("That image is already mounted.");
        return next_in_queue();
    }
    match convert::prepare(&path) {
        Err(e) => {
            msg_box(&format!("Could not read the image:\n{}\n\n{e}", path.display()), MB_ICONERROR);
            next_in_queue();
        }
        Ok(Source::Direct(p)) => {
            finish_mount(path, p, None);
            next_in_queue();
        }
        Ok(Source::Extract(plan)) => {
            let dir = temp_dir();
            let _ = std::fs::create_dir_all(&dir);
            let need = plan.sectors * 2048;
            if convert::free_space(&dir) < need + (16 << 20) {
                msg_box(&format!("Not enough free space in {}\nto convert the image ({} MB needed).", dir.display(), need >> 20), MB_ICONERROR);
                return next_in_queue();
            }
            let temp = dir.join(format!("{}-{}.iso", std::process::id(), TEMP_SEQ.fetch_add(1, Relaxed)));
            let cancel = Arc::new(AtomicBool::new(false));
            CORE.with_borrow_mut(|c| c.job = Some(cancel.clone()));
            refresh();
            set_status("Converting… 0%");
            let hwnd = ui().hwnd as usize;
            std::thread::spawn(move || {
                let (mut last, h) = (255u8, hwnd as HWND);
                let res = convert::extract(&plan, &temp, &cancel, &mut |p| {
                    if p != last {
                        last = p;
                        unsafe { PostMessageW(h, WM_PROGRESS, p as usize, 0) };
                    }
                });
                if res.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                let done = Box::into_raw(Box::new(Done { image: path, temp, res }));
                unsafe { PostMessageW(h, WM_DONE, 0, done as isize) };
            });
        }
    }
}

fn next_in_queue() {
    if let Some(p) = CORE.with_borrow_mut(|c| c.queue.pop_front()) {
        start(p);
    }
}

fn add_recent(image: &Path) {
    let abs = std::path::absolute(image).unwrap_or_else(|_| image.to_path_buf());
    CORE.with_borrow_mut(|c| {
        store::push_recent(&mut c.recent, abs);
        store::save_recent(&c.recent);
    });
}

fn mounted_notice(letter: char) {
    unsafe {
        let h = ui().hwnd;
        if IsWindowVisible(h) == 0 || IsIconic(h) != 0 {
            balloon(&format!("Mounted at {letter}:"));
        }
    }
}

fn finish_mount(image: PathBuf, iso: PathBuf, temp: Option<PathBuf>) {
    match vdisk::attach(&iso, image.clone(), temp.clone()) {
        Ok(m) => {
            if let Some(l) = m.letter {
                mounted_notice(l);
            } else {
                unsafe { SetTimer(ui().hwnd, 1, 500, None) }; // keep looking for the letter
                TIMER_TICKS.store(0, Relaxed);
            }
            add_recent(&image);
            let i = CORE.with_borrow_mut(|c| {
                c.mounts.push(m);
                c.mounts.len() - 1
            });
            refresh();
            select(i);
            set_status("Ready."); // the list already shows the new drive
        }
        Err(e) => {
            if let Some(t) = temp {
                let _ = std::fs::remove_file(t);
            }
            set_status("Mount failed.");
            msg_box(&format!("Could not mount:\n{}\n\n{e}", image.display()), MB_ICONERROR);
        }
    }
}

fn selected() -> Option<usize> {
    let i = unsafe { SendMessageW(ui().list, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as isize) };
    if i >= 0 { Some(i as usize) } else { CORE.with_borrow(|c| (c.mounts.len() == 1).then_some(0)) }
}

fn select(i: usize) {
    unsafe {
        let mut it: LVITEMW = zeroed();
        it.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
        it.state = LVIS_SELECTED | LVIS_FOCUSED;
        SendMessageW(ui().list, LVM_SETITEMSTATE, i, &it as *const _ as isize);
    }
}

fn unmount(i: usize) {
    let m = CORE.with_borrow_mut(|c| (i < c.mounts.len()).then(|| c.mounts.remove(i)));
    let Some(m) = m else { return };
    drop(m); // detaches; outside the borrow because it may sleep
    refresh();
    set_status("Ready."); // the row disappearing is the feedback
}

/// Drops mounts whose drive disappeared (Explorer "Eject", device removal).
fn revalidate() {
    let dead: Vec<Mount> = CORE.with_borrow_mut(|c| {
        let (alive, dead) = std::mem::take(&mut c.mounts).into_iter().partition(|m| m.alive());
        c.mounts = alive;
        dead
    });
    if !dead.is_empty() {
        drop(dead);
        refresh();
        set_status("A drive was ejected.");
    }
}

fn open_drive(i: usize) {
    if let Some(l) = CORE.with_borrow(|c| c.mounts.get(i).and_then(|m| m.letter)) {
        unsafe { ShellExecuteW(ui().hwnd, w("open").as_ptr(), w(&format!("{l}:\\")).as_ptr(), null(), null(), SW_SHOWNORMAL) };
    }
}

fn image_of(i: usize) -> Option<PathBuf> {
    CORE.with_borrow(|c| c.mounts.get(i).map(|m| m.image.clone()))
}

unsafe fn show_in_explorer(path: &Path) {
    let args = w(&format!("/select,\"{}\"", path.display()));
    ShellExecuteW(ui().hwnd, w("open").as_ptr(), w("explorer.exe").as_ptr(), args.as_ptr(), null(), SW_SHOWNORMAL);
}

unsafe fn copy_text(s: &str) {
    let v = w(s);
    if OpenClipboard(ui().hwnd) == 0 {
        return;
    }
    EmptyClipboard();
    let h = GlobalAlloc(GMEM_MOVEABLE, v.len() * 2);
    if !h.is_null() {
        std::ptr::copy_nonoverlapping(v.as_ptr(), GlobalLock(h) as *mut u16, v.len());
        GlobalUnlock(h);
        if SetClipboardData(CF_UNICODETEXT, h as HANDLE).is_null() {
            GlobalFree(h);
        }
    }
    CloseClipboard();
}

fn pick_file() {
    unsafe {
        let mut buf = vec![0u16; 32768];
        let filter: Vec<u16> = "Disc images\0*.iso;*.cue;*.bin;*.img;*.mdf;*.mds;*.nrg;*.ccd\0All files\0*.*\0\0".encode_utf16().collect();
        let mut ofn: OPENFILENAMEW = zeroed();
        ofn.lStructSize = size_of::<OPENFILENAMEW>() as u32;
        ofn.hwndOwner = ui().hwnd;
        ofn.lpstrFilter = filter.as_ptr();
        ofn.lpstrFile = buf.as_mut_ptr();
        ofn.nMaxFile = buf.len() as u32;
        ofn.Flags = OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY;
        if GetOpenFileNameW(&mut ofn) != 0 {
            let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
            request_mount(PathBuf::from(OsString::from_wide(&buf[..n])));
        }
    }
}

fn refresh() {
    unsafe {
        let u = ui();
        SendMessageW(u.list, LVM_DELETEALLITEMS, 0, 0);
        let rows: Vec<[String; 5]> = CORE.with_borrow(|c| {
            c.mounts
                .iter()
                .map(|m| {
                    let ext = m.image.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or("?".into());
                    [
                        m.letter.map(|l| format!("{l}:")).unwrap_or("(no letter)".into()),
                        m.letter.map(volume_label).unwrap_or_default(),
                        if m.converted { format!("{ext}→ISO") } else { ext },
                        human_size(m.size),
                        m.image.display().to_string(),
                    ]
                })
                .collect()
        });
        for (i, row) in rows.iter().enumerate() {
            for (col, text) in row.iter().enumerate() {
                let mut it: LVITEMW = zeroed();
                it.mask = LVIF_TEXT;
                it.iItem = i as i32;
                it.iSubItem = col as i32;
                let mut tw = w(text);
                it.pszText = tw.as_mut_ptr();
                let msg = if col == 0 { LVM_INSERTITEMW } else { LVM_SETITEMTEXTW };
                SendMessageW(u.list, msg, if col == 0 { 0 } else { i }, &it as *const _ as isize);
            }
        }
        let busy = CORE.with_borrow(|c| c.job.is_some());
        EnableWindow(u.unmount, (!rows.is_empty()) as i32);
        EnableWindow(u.cancel, busy as i32);
        ShowWindow(u.progress, if busy { SW_SHOW } else { SW_HIDE });
        ShowWindow(u.hint, if rows.is_empty() { SW_SHOW } else { SW_HIDE });
        layout();
    }
}

// ---------- tray ----------

/// App icon (resource 1) at the size Windows wants for this DPI.
unsafe fn icon(metric: SYSTEM_METRICS_INDEX) -> HICON {
    let px = GetSystemMetricsForDpi(metric, GetDpiForWindow(ui().hwnd).max(96));
    LoadImageW(GetModuleHandleW(null()), 1 as usize as *const u16, IMAGE_ICON, px, px, LR_DEFAULTCOLOR) as HICON
}

unsafe extern "system" fn about_cb(_: HWND, n: u32, _: WPARAM, lp: LPARAM, _: isize) -> i32 {
    if n == TDN_HYPERLINK_CLICKED as u32 {
        ShellExecuteW(null_mut(), w("open").as_ptr(), lp as *const u16, null(), null(), SW_SHOWNORMAL);
    }
    0
}

unsafe fn about() {
    let content = w(&format!(
        "Version {}\n\nMounts disc images (ISO, CUE/BIN, NRG, MDF…) as real Windows drives, with no drivers and no administrator rights.\n\n© 2026 Glauber Lima · MIT License\n<a href=\"https://github.com/glauberlima\">github.com/glauberlima</a>",
        env!("CARGO_PKG_VERSION")
    ));
    let (title, head) = (w("About Disk Image Mounter"), w("Disk Image Mounter"));
    let more = w("Built with Rust on the native Windows virtdisk API. Drives disappear when the app closes.");
    let mut cfg: TASKDIALOGCONFIG = zeroed();
    cfg.cbSize = size_of::<TASKDIALOGCONFIG>() as u32;
    cfg.hwndParent = ui().hwnd;
    cfg.dwFlags = TDF_ENABLE_HYPERLINKS | TDF_USE_HICON_MAIN | TDF_ALLOW_DIALOG_CANCELLATION | TDF_EXPAND_FOOTER_AREA;
    cfg.dwCommonButtons = TDCBF_OK_BUTTON;
    cfg.pszWindowTitle = title.as_ptr();
    cfg.pszMainInstruction = head.as_ptr();
    cfg.pszContent = content.as_ptr();
    cfg.pszExpandedInformation = more.as_ptr();
    cfg.Anonymous1.hMainIcon = icon(SM_CXICON);
    cfg.pfCallback = Some(about_cb);
    TaskDialogIndirect(&cfg, null_mut(), null_mut(), null_mut());
}

unsafe fn nid() -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = zeroed();
    n.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = ui().hwnd;
    n.uID = 1;
    n
}

unsafe fn tray(op: u32) {
    let mut n = nid();
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = icon(SM_CXSMICON);
    copy_wide(&mut n.szTip, "Disk Image Mounter");
    Shell_NotifyIconW(op, &n);
}

unsafe fn balloon(text: &str) {
    let mut n = nid();
    n.uFlags = NIF_INFO;
    n.dwInfoFlags = NIIF_INFO;
    copy_wide(&mut n.szInfoTitle, "Disk Image Mounter");
    copy_wide(&mut n.szInfo, text);
    Shell_NotifyIconW(NIM_MODIFY, &n);
}

unsafe fn show_window() {
    let h = ui().hwnd;
    ShowWindow(h, SW_SHOW);
    ShowWindow(h, SW_RESTORE);
    SetForegroundWindow(h);
}

/// "Mount recent" entries (ID_RECENT + n). `header` adds the greyed title used by the split button.
unsafe fn recent_menu(header: bool) -> HMENU {
    let m = CreatePopupMenu();
    let items: Vec<PathBuf> = CORE.with_borrow(|c| c.recent.clone());
    if header {
        AppendMenuW(m, MF_STRING | MF_GRAYED, 0, w("Montar recentes").as_ptr());
        AppendMenuW(m, MF_SEPARATOR, 0, null());
    }
    if items.is_empty() {
        AppendMenuW(m, MF_STRING | MF_GRAYED, 0, w("(no recent images)").as_ptr());
        return m;
    }
    for (i, p) in items.iter().enumerate() {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let dir = p.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let key = if i < 9 { format!("&{}", i + 1) } else { "1&0".into() };
        let flags = MF_STRING | if p.exists() { 0 } else { MF_GRAYED };
        AppendMenuW(m, flags, ID_RECENT as usize + i, w(&format!("{key}  {name}  —  {dir}")).as_ptr());
    }
    AppendMenuW(m, MF_SEPARATOR, 0, null());
    AppendMenuW(m, MF_STRING, ID_CLEAR_RECENT as usize, w("Clear list").as_ptr());
    m
}

/// Shows `menu` at (x, y) and runs the chosen command. Destroys the menu.
unsafe fn run_menu(menu: HMENU, x: i32, y: i32) {
    SetForegroundWindow(ui().hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, x, y, 0, ui().hwnd, null());
    PostMessageW(ui().hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
    if cmd != 0 {
        command(cmd);
    }
}

unsafe fn tray_menu() {
    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, ID_MOUNT as usize, w("Mount image…").as_ptr());
    AppendMenuW(menu, MF_POPUP, recent_menu(false) as usize, w("Mount recent").as_ptr());
    let items: Vec<String> = CORE.with_borrow(|c| {
        c.mounts
            .iter()
            .map(|m| {
                let name = m.image.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                format!("Unmount {} ({name})", m.letter.map(|l| format!("{l}:")).unwrap_or("?".into()))
            })
            .collect()
    });
    if !items.is_empty() {
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
    }
    for (i, t) in items.iter().enumerate() {
        AppendMenuW(menu, MF_STRING, (ID_MENU_UNMOUNT as usize) + i, w(t).as_ptr());
    }
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_ABOUT as usize, w("About Disk Image Mounter…").as_ptr());
    AppendMenuW(menu, MF_STRING, ID_QUIT as usize, w("Quit").as_ptr());
    let mut pt = POINT { x: 0, y: 0 };
    GetCursorPos(&mut pt);
    run_menu(menu, pt.x, pt.y);
}

/// Right-click (or Menu key) on the mount list.
unsafe fn list_menu(lp: LPARAM) {
    let list = ui().list;
    let (mut x, mut y) = (lp as i16 as i32, (lp >> 16) as i16 as i32);
    if x == -1 && y == -1 {
        let mut r: RECT = zeroed();
        GetWindowRect(list, &mut r);
        (x, y) = (r.left + 20, r.top + 40);
    } else {
        let mut hit: LVHITTESTINFO = zeroed();
        hit.pt = POINT { x, y };
        ScreenToClient(list, &mut hit.pt);
        if SendMessageW(list, LVM_SUBITEMHITTEST, 0, &mut hit as *mut _ as isize) >= 0 {
            select(hit.iItem as usize);
        }
    }
    if selected().is_none() {
        return;
    }
    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, ID_CTX_OPEN as usize, w("Open drive").as_ptr());
    AppendMenuW(menu, MF_STRING, ID_CTX_SHOW as usize, w("Show image in Explorer").as_ptr());
    AppendMenuW(menu, MF_STRING, ID_CTX_COPY as usize, w("Copy path").as_ptr());
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, ID_UNMOUNT as usize, w("Unmount").as_ptr());
    SetMenuDefaultItem(menu, ID_CTX_OPEN as u32, 0); // bold = what double-click/Enter does
    run_menu(menu, x, y);
}

// ---------- window ----------

/// Quitting detaches every drive, so ask first when something is mounted or converting.
unsafe fn confirm_quit() -> bool {
    let (n, converting) = CORE.with_borrow(|c| (c.mounts.len(), c.job.is_some()));
    if n == 0 && !converting {
        return true;
    }
    let mut text = String::new();
    if n > 0 {
        let (what, drives) = if n == 1 { ("1 mounted image".to_string(), "its drive") } else { (format!("{n} mounted images"), "their drives") };
        text += &format!("{what} will be unmounted and {drives} will disappear.
");
    }
    if converting {
        text += "A conversion is in progress and will be canceled.";
    }
    // Custom button captions keep the dialog in English regardless of the Windows language.
    const QUIT: i32 = 1001;
    const KEEP: i32 = 1002;
    let (quit, keep) = (w("Quit"), w("Cancel"));
    let buttons = [TASKDIALOG_BUTTON { nButtonID: QUIT, pszButtonText: quit.as_ptr() }, TASKDIALOG_BUTTON { nButtonID: KEEP, pszButtonText: keep.as_ptr() }];
    let (title, head, body) = (w("Disk Image Mounter"), w("Quit Disk Image Mounter?"), w(&text));
    let mut cfg: TASKDIALOGCONFIG = zeroed();
    cfg.cbSize = size_of::<TASKDIALOGCONFIG>() as u32;
    cfg.hwndParent = ui().hwnd;
    cfg.dwFlags = TDF_ALLOW_DIALOG_CANCELLATION;
    cfg.pszWindowTitle = title.as_ptr();
    cfg.pszMainInstruction = head.as_ptr();
    cfg.pszContent = body.as_ptr();
    cfg.Anonymous1.pszMainIcon = 0xFFFF as *const u16; // TD_WARNING_ICON
    cfg.cButtons = buttons.len() as u32;
    cfg.pButtons = buttons.as_ptr();
    cfg.nDefaultButton = KEEP;
    let mut chosen = 0;
    TaskDialogIndirect(&cfg, &mut chosen, null_mut(), null_mut());
    chosen == QUIT
}

unsafe fn command(id: i32) {
    match id {
        ID_MOUNT => pick_file(),
        ID_UNMOUNT => match selected() {
            Some(i) => unmount(i),
            None => set_status("Select a drive in the list."),
        },
        ID_CANCEL => CORE.with_borrow(|c| {
            if let Some(f) = &c.job {
                f.store(true, Relaxed);
            }
        }),
        ID_ABOUT => about(),
        ID_QUIT => {
            if confirm_quit() {
                DestroyWindow(ui().hwnd);
            }
        }
        ID_CTX_OPEN => selected().into_iter().for_each(open_drive),
        ID_CTX_SHOW => {
            if let Some(p) = selected().and_then(image_of) {
                show_in_explorer(&p);
            }
        }
        ID_CTX_COPY => {
            if let Some(p) = selected().and_then(image_of) {
                copy_text(&p.display().to_string());
                set_status("Path copied.");
            }
        }
        ID_CLEAR_RECENT => CORE.with_borrow_mut(|c| {
            c.recent.clear();
            store::save_recent(&c.recent);
        }),
        r if (ID_RECENT..ID_RECENT + store::MAX_RECENT as i32).contains(&r) => {
            if let Some(p) = CORE.with_borrow(|c| c.recent.get((r - ID_RECENT) as usize).cloned()) {
                request_mount(p);
            }
        }
        u if (ID_MENU_UNMOUNT..ID_MENU_UNMOUNT + 100).contains(&u) => unmount((u - ID_MENU_UNMOUNT) as usize),
        _ => {}
    }
}

unsafe fn create_children(hwnd: HWND) {
    let hi = GetModuleHandleW(null());
    let mk = |class: &str, text: &str, style: u32, id: i32| {
        CreateWindowExW(
            if class == "SysListView32" { WS_EX_CLIENTEDGE } else { 0 },
            w(class).as_ptr(), w(text).as_ptr(), WS_CHILD | WS_VISIBLE | style,
            0, 0, 10, 10, hwnd, id as usize as HMENU, hi, null(),
        )
    };
    let btn = WS_TABSTOP | BS_PUSHBUTTON as u32;
    let u = Ui {
        hwnd,
        list: mk("SysListView32", "", WS_TABSTOP | LVS_REPORT | LVS_SINGLESEL | LVS_SHOWSELALWAYS, ID_LIST),
        hint: mk("STATIC", "Drag an image here\nor click Mount…", 1 /* SS_CENTER */, 0),
        mount: mk("BUTTON", "Mount…", WS_TABSTOP | BS_SPLITBUTTON as u32, ID_MOUNT),
        unmount: mk("BUTTON", "Unmount", btn, ID_UNMOUNT),
        cancel: mk("BUTTON", "Cancel", btn, ID_CANCEL),
        about: mk("BUTTON", "About", btn, ID_ABOUT),
        status: mk("STATIC", "Ready.", 0, 0),
        progress: mk("msctls_progress32", "", PBS_SMOOTH as u32, 0),
    };
    UI.set(u);
    SendMessageW(u.list, LVM_SETEXTENDEDLISTVIEWSTYLE, 0, LVS_EX_FULLROWSELECT as isize);
    for (i, t) in ["Drive", "Label", "Type", "Size", "Image"].iter().enumerate() {
        let mut col: LVCOLUMNW = zeroed();
        col.mask = LVCF_TEXT | LVCF_WIDTH;
        col.cx = 100;
        let mut tw = w(t);
        col.pszText = tw.as_mut_ptr();
        SendMessageW(u.list, LVM_INSERTCOLUMNW, i, &col as *const _ as isize);
    }
    SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, icon(SM_CXICON) as isize);
    SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, icon(SM_CXSMICON) as isize);
    apply_font();
    DragAcceptFiles(hwnd, 1);
    tray(NIM_ADD);
    refresh();
}

unsafe fn apply_font() {
    let mut ncm: NONCLIENTMETRICSW = zeroed();
    ncm.cbSize = size_of::<NONCLIENTMETRICSW>() as u32;
    SystemParametersInfoW(SPI_GETNONCLIENTMETRICS, ncm.cbSize, &mut ncm as *mut _ as *mut _, 0);
    let old = FONT.get();
    let f = CreateFontIndirectW(&ncm.lfMessageFont);
    FONT.set(f);
    let u = ui();
    for h in [u.list, u.mount, u.unmount, u.cancel, u.about, u.status, u.hint] {
        SendMessageW(h, WM_SETFONT, f as usize, 1);
    }
    if !old.is_null() {
        DeleteObject(old);
    }
}

unsafe fn layout() {
    let u = ui();
    if u.list.is_null() {
        return;
    }
    let mut rc: RECT = zeroed();
    GetClientRect(u.hwnd, &mut rc);
    let d = GetDpiForWindow(u.hwnd).max(96) as i32;
    let p = |v: i32| v * d / 96;
    let (m, bh, bw, sh) = (p(8), p(28), p(100), p(20));
    let (cw, ch) = (rc.right, rc.bottom);
    let by = ch - m - sh - m / 2 - bh;
    let (lw, lh) = (cw - 2 * m, (by - 2 * m).max(p(40)));
    MoveWindow(u.list, m, m, lw, lh, 1);
    let fixed = [p(60), p(120), p(70), p(75)];
    for (i, c) in fixed.iter().enumerate() {
        SendMessageW(u.list, LVM_SETCOLUMNWIDTH, i, *c as isize);
    }
    SendMessageW(u.list, LVM_SETCOLUMNWIDTH, 4, (lw - fixed.iter().sum::<i32>() - p(28)).max(p(60)) as isize);
    let hh = p(44);
    MoveWindow(u.hint, m, m + (lh - hh) / 2 + p(10), lw, hh, 1);
    MoveWindow(u.mount, m, by, bw, bh, 1);
    MoveWindow(u.unmount, m + bw + m, by, bw, bh, 1);
    MoveWindow(u.cancel, m + 2 * (bw + m), by, bw, bh, 1);
    MoveWindow(u.about, cw - m - bw, by, bw, bh, 1);
    let busy = CORE.with_borrow(|c| c.job.is_some());
    let sw = if busy { lw * 55 / 100 } else { lw };
    MoveWindow(u.status, m, ch - m - sh, sw, sh, 1);
    MoveWindow(u.progress, m + sw + m, ch - m - sh, (lw - sw - m).max(0), sh, 1);
}

unsafe fn text_from_copydata(c: &COPYDATASTRUCT) -> PathBuf {
    let s = std::slice::from_raw_parts(c.lpData as *const u16, c.cbData as usize / 2);
    let n = s.iter().position(|&x| x == 0).unwrap_or(s.len());
    PathBuf::from(OsString::from_wide(&s[..n]))
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            create_children(hwnd);
            return 0;
        }
        WM_SIZE => layout(),
        WM_GETMINMAXINFO => {
            let d = GetDpiForWindow(hwnd).max(96) as i32;
            (*(lp as *mut MINMAXINFO)).ptMinTrackSize = POINT { x: 480 * d / 96, y: 280 * d / 96 };
            return 0;
        }
        WM_DPICHANGED => {
            let r = &*(lp as *const RECT);
            SetWindowPos(hwnd, null_mut(), r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            apply_font();
            layout();
            return 0;
        }
        WM_CTLCOLORSTATIC if lp as HWND == ui().hint => {
            SetTextColor(wp as HDC, rgb(0x70, 0x70, 0x70));
            SetBkMode(wp as HDC, TRANSPARENT as i32);
            return GetSysColorBrush(COLOR_WINDOW) as LRESULT;
        }
        WM_CONTEXTMENU if wp as HWND == ui().list => {
            list_menu(lp);
            return 0;
        }
        WM_COMMAND => command((wp & 0xFFFF) as i32),
        WM_NOTIFY => {
            let h = &*(lp as *const NMHDR);
            if h.idFrom == ID_LIST as usize {
                if h.code == LVN_ITEMACTIVATE {
                    let a = &*(lp as *const NMITEMACTIVATE);
                    if a.iItem >= 0 {
                        open_drive(a.iItem as usize);
                    }
                } else if h.code == LVN_KEYDOWN && (&*(lp as *const NMLVKEYDOWN)).wVKey == VK_DELETE {
                    command(ID_UNMOUNT);
                }
            } else if h.idFrom == ID_MOUNT as usize && h.code == BCN_DROPDOWN {
                let mut pt = POINT { x: (&*(lp as *const NMBCDROPDOWN)).rcButton.left, y: (&*(lp as *const NMBCDROPDOWN)).rcButton.bottom };
                ClientToScreen(ui().mount, &mut pt);
                run_menu(recent_menu(true), pt.x, pt.y);
                return 1;
            }
        }
        WM_DROPFILES => {
            let hd = wp as HDROP;
            let n = DragQueryFileW(hd, u32::MAX, null_mut(), 0);
            let files: Vec<PathBuf> = (0..n)
                .map(|i| {
                    let mut b = vec![0u16; DragQueryFileW(hd, i, null_mut(), 0) as usize + 1];
                    let len = DragQueryFileW(hd, i, b.as_mut_ptr(), b.len() as u32) as usize;
                    PathBuf::from(OsString::from_wide(&b[..len]))
                })
                .collect();
            DragFinish(hd);
            files.into_iter().for_each(request_mount);
            return 0;
        }
        WM_COPYDATA => {
            let c = &*(lp as *const COPYDATASTRUCT);
            show_window();
            if c.dwData == 1 {
                request_mount(text_from_copydata(c));
            }
            return 1;
        }
        WM_TIMER => {
            let (found, pending) = CORE.with_borrow_mut(|c| {
                let mut found = None;
                for m in c.mounts.iter_mut().filter(|m| m.letter.is_none()) {
                    if m.resolve() {
                        found = m.letter;
                    }
                }
                (found, c.mounts.iter().any(|m| m.letter.is_none()))
            });
            if let Some(l) = found {
                refresh();
                mounted_notice(l);
            }
            // Give up after ~60 s; the row stays "(sem letra)".
            if !pending || TIMER_TICKS.fetch_add(1, Relaxed) > 120 {
                KillTimer(hwnd, 1);
            }
            return 0;
        }
        WM_PROGRESS => {
            SendMessageW(ui().progress, PBM_SETPOS, wp, 0);
            set_status(&format!("Converting… {wp}%"));
        }
        WM_DONE => {
            let d = Box::from_raw(lp as *mut Done);
            CORE.with_borrow_mut(|c| c.job = None);
            refresh();
            match d.res {
                Ok(()) => {
                    finish_mount(d.image, d.temp.clone(), Some(d.temp));
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => set_status("Conversion canceled."),
                Err(e) => {
                    set_status("Conversion failed.");
                    msg_box(&format!("Error converting the image:\n{}\n\n{e}", d.image.display()), MB_ICONERROR);
                }
            }
            next_in_queue();
            return 0;
        }
        WM_DEVICECHANGE if wp == DBT_DEVICEREMOVECOMPLETE || wp == DBT_DEVNODES_CHANGED => revalidate(),
        WM_TRAY => match (lp & 0xFFFF) as u32 {
            WM_LBUTTONUP => show_window(),
            WM_RBUTTONUP | WM_CONTEXTMENU => tray_menu(),
            _ => {}
        },
        WM_CLOSE => {
            save_geometry();
            ShowWindow(hwnd, SW_HIDE); // keep mounts alive in the tray
            return 0;
        }
        WM_ENDSESSION if wp != 0 => {
            DestroyWindow(hwnd);
        }
        WM_DESTROY => {
            save_geometry();
            CORE.with_borrow(|c| {
                if let Some(f) = &c.job {
                    f.store(true, Relaxed);
                }
            });
            tray(NIM_DELETE);
            let mounts = CORE.with_borrow_mut(|c| std::mem::take(&mut c.mounts));
            drop(mounts);
            PostQuitMessage(0);
            return 0;
        }
        m if m != 0 && m == TASKBAR_CREATED.load(Relaxed) => tray(NIM_ADD),
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wp, lp)
}
