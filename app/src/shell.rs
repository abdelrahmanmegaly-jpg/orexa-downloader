// Windows integration: tray icon, "already running" check, toast notifications and
// reading a copied link from the clipboard.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

const TITLE: &str = "Orexa Downloader";
const AUMID: &str = "Orexa.Downloader";

static QUITTING: AtomicBool = AtomicBool::new(false);
/// The window is parked off-screen (in the tray) instead of truly hidden: a hidden window
/// stops the app's update loop, which would freeze notifications, saving and the queue.
static IN_TRAY: AtomicBool = AtomicBool::new(false);
static SAVED_POS: std::sync::Mutex<Option<(i32, i32)>> = std::sync::Mutex::new(None);

/// True once the user chose Quit (so closing the window really exits).
pub fn quitting() -> bool {
    QUITTING.load(Ordering::SeqCst)
}

// ---------------------------------------------------------------------------
// Win32 window helpers (work even while the window is hidden)
// ---------------------------------------------------------------------------

#[repr(C)]
struct Rect32 {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[link(name = "user32")]
extern "system" {
    fn FindWindowW(class: *const u16, title: *const u16) -> isize;
    fn ShowWindow(hwnd: isize, cmd: i32) -> i32;
    fn SetForegroundWindow(hwnd: isize) -> i32;
    fn PostMessageW(hwnd: isize, msg: u32, wparam: usize, lparam: isize) -> i32;
    fn GetWindowRect(hwnd: isize, rect: *mut Rect32) -> i32;
    fn GetSystemMetrics(index: i32) -> i32;
    fn SetWindowPos(hwnd: isize, after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
    fn GetWindowLongPtrW(hwnd: isize, index: i32) -> isize;
    fn SetWindowLongPtrW(hwnd: isize, index: i32, value: isize) -> isize;
}

const GWL_EXSTYLE: i32 = -20;
const WS_EX_TOOLWINDOW: isize = 0x0000_0080;
const WS_EX_APPWINDOW: isize = 0x0004_0000;
const SWP_NOSIZE: u32 = 0x0001;
const SWP_NOZORDER: u32 = 0x0004;
const SWP_NOACTIVATE: u32 = 0x0010;

#[link(name = "kernel32")]
extern "system" {
    fn CreateMutexW(attrs: *const core::ffi::c_void, owner: i32, name: *const u16) -> isize;
    fn CreateEventW(attrs: *const core::ffi::c_void, manual: i32, initial: i32, name: *const u16) -> isize;
    fn OpenEventW(access: u32, inherit: i32, name: *const u16) -> isize;
    fn SetEvent(handle: isize) -> i32;
    fn WaitForSingleObject(handle: isize, millis: u32) -> u32;
    fn GetLastError() -> u32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn hwnd() -> isize {
    unsafe { FindWindowW(std::ptr::null(), wide(TITLE).as_ptr()) }
}

/// Send the window to the tray: no taskbar button, parked off-screen, still running.
pub fn hide_window() {
    let h = hwnd();
    if h == 0 || IN_TRAY.swap(true, Ordering::SeqCst) {
        return;
    }
    unsafe {
        let mut r = Rect32 { left: 0, top: 0, right: 0, bottom: 0 };
        // Remember where it was (unless it started off-screen, as it does when the browser
        // launches us in the background; then it opens centered the first time).
        if GetWindowRect(h, &mut r) != 0 && r.left > -10000 {
            *SAVED_POS.lock().unwrap() = Some((r.left, r.top));
        }
        // A tool window has no taskbar button and is left out of Alt+Tab.
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        ShowWindow(h, 0);
        SetWindowLongPtrW(h, GWL_EXSTYLE, (ex | WS_EX_TOOLWINDOW) & !WS_EX_APPWINDOW);
        SetWindowPos(h, 0, -32000, -32000, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        ShowWindow(h, 4); // SW_SHOWNOACTIVATE: shown (so it keeps running), but off-screen
    }
}

pub fn show_window() {
    let h = hwnd();
    if h == 0 {
        return;
    }
    unsafe {
        if IN_TRAY.swap(false, Ordering::SeqCst) {
            let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
            ShowWindow(h, 0);
            SetWindowLongPtrW(h, GWL_EXSTYLE, (ex & !WS_EX_TOOLWINDOW) | WS_EX_APPWINDOW);
            let saved = SAVED_POS.lock().unwrap().take();
            let (x, y) = saved.unwrap_or_else(|| {
                let mut r = Rect32 { left: 0, top: 0, right: 0, bottom: 0 };
                GetWindowRect(h, &mut r);
                let (w, hgt) = (r.right - r.left, r.bottom - r.top);
                (((GetSystemMetrics(0) - w) / 2).max(0), ((GetSystemMetrics(1) - hgt) / 2).max(0))
            });
            SetWindowPos(h, 0, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
        ShowWindow(h, 9); // SW_RESTORE: shows it and un-minimizes
        SetForegroundWindow(h);
    }
}

/// True while the window is shown to the user (not parked in the tray).
pub fn window_visible() -> bool {
    hwnd() != 0 && !IN_TRAY.load(Ordering::SeqCst)
}

pub fn in_tray() -> bool {
    IN_TRAY.load(Ordering::SeqCst)
}

/// Really exit (used by the tray menu).
pub fn request_quit() {
    QUITTING.store(true, Ordering::SeqCst);
    let h = hwnd();
    if h != 0 {
        unsafe { PostMessageW(h, 0x0010, 0, 0) }; // WM_CLOSE
    } else {
        std::process::exit(0);
    }
}

/// Ask the running copy of Orexa to come to the front.
pub fn signal_running_instance() -> bool {
    let event_name = wide("Local\\OrexaDownloaderShow");
    let ev = unsafe { OpenEventW(0x0002, 0, event_name.as_ptr()) };
    if ev != 0 {
        unsafe { SetEvent(ev) };
        true
    } else {
        false
    }
}

/// If Orexa is already running (for example parked in the tray), ask that copy to come
/// forward and return true so this second copy can exit. The first copy listens for the
/// request itself, because only it knows how to restore its own window.
pub fn already_running(quiet: bool) -> bool {
    let mutex_name = wide("Local\\OrexaDownloaderSingleInstance");
    let event_name = wide("Local\\OrexaDownloaderShow");
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, mutex_name.as_ptr()) };
    let exists = handle != 0 && unsafe { GetLastError() } == 183; // ERROR_ALREADY_EXISTS
    if exists && quiet {
        return true; // started in the background (Windows startup): never bring the window forward
    }
    if exists {
        let ev = unsafe { OpenEventW(0x0002, 0, event_name.as_ptr()) }; // EVENT_MODIFY_STATE
        if ev != 0 {
            unsafe { SetEvent(ev) };
        } else {
            show_window();
        }
        return true;
    }
    // First copy: wait for later launches asking to be shown.
    let ev = unsafe { CreateEventW(std::ptr::null(), 0, 0, event_name.as_ptr()) };
    if ev != 0 {
        thread::spawn(move || loop {
            if unsafe { WaitForSingleObject(ev, 0xFFFF_FFFF) } == 0 {
                show_window();
            } else {
                break;
            }
        });
    }
    // The mutex handle stays open until this process ends.
    false
}

// ---------------------------------------------------------------------------
// Tray icon
// ---------------------------------------------------------------------------

pub fn make_tray() -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let img = image::load_from_memory(APP_ICON_PNG).ok()?.to_rgba8();
    let small = image::imageops::resize(&img, 32, 32, image::imageops::FilterType::Lanczos3);
    let icon = tray_icon::Icon::from_rgba(small.into_raw(), 32, 32).ok()?;

    let open = MenuItem::new("Open Orexa Downloader", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]).ok()?;
    let (open_id, quit_id) = (open.id().clone(), quit.id().clone());

    // Handlers run on the event thread, so they work while the window is hidden.
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == open_id {
            show_window();
        } else if e.id == quit_id {
            request_quit();
        }
    }));
    TrayIconEvent::set_event_handler(Some(|e: TrayIconEvent| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
            show_window();
        }
    }));

    TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .with_tooltip(TITLE)
        .with_icon(icon)
        .build()
        .ok()
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

fn icon_path() -> PathBuf {
    state::data_dir().join("icon.png")
}

/// Teach Windows our app name and icon so toasts say "Orexa Downloader".
pub fn register_notifications() {
    thread::spawn(|| {
        let icon = icon_path();
        let _ = fs::create_dir_all(state::data_dir());
        if !icon.exists() {
            let _ = fs::write(&icon, APP_ICON_PNG);
        }
        let key = format!(r"HKCU\Software\Classes\AppUserModelId\{}", AUMID);
        let reg = |name: &str, value: &str| {
            let mut c = Command::new("reg");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000);
            }
            let _ = c.args(["add", &key, "/v", name, "/t", "REG_SZ", "/d", value, "/f"]).status();
        };
        reg("DisplayName", TITLE);
        reg("IconUri", &icon.to_string_lossy());
    });
}

/// Show a Windows notification. Never blocks the app.
pub fn toast(title: String, body: String) {
    thread::spawn(move || {
        use tauri_winrt_notification::{IconCrop, Sound, Toast};
        let icon = icon_path();
        let mut t = Toast::new(AUMID).title(&title).text1(&body).sound(Some(Sound::Default));
        if icon.exists() {
            t = t.icon(&icon, IconCrop::Square, TITLE);
        }
        let _ = t.show();
    });
}

// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// A web link on the clipboard, if there is one.
pub fn clipboard_url() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let text = cb.get_text().ok()?;
    let first = text.trim().lines().next()?.trim();
    let ok = (first.starts_with("http://") || first.starts_with("https://")) && !first.contains(char::is_whitespace);
    ok.then(|| first.to_string())
}

// ---------------------------------------------------------------------------
// Browser connector registration
// ---------------------------------------------------------------------------

/// Start Orexa in the tray when Windows starts (or stop doing so). It uses Windows' own per-user
/// startup list, so no administrator rights are needed. Development builds never register.
pub fn set_autostart(enable: bool) {
    if std::env::var_os("OREXA_NO_AUTOSTART").is_some() {
        return;
    }
    thread::spawn(move || {
        let Ok(exe) = std::env::current_exe() else { return };
        if enable && exe.to_string_lossy().to_lowercase().contains("\\target\\") {
            return; // a build folder, not an installed copy
        }
        let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let mut c = Command::new("reg");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000);
        }
        if enable {
            let cmd = format!("\"{}\" --background", exe.display());
            c.args(["add", key, "/v", "Orexa Downloader", "/t", "REG_SZ", "/d", &cmd, "/f"]);
        } else {
            c.args(["delete", key, "/v", "Orexa Downloader", "/f"]);
        }
        let _ = c.status();
    });
}

/// Tell Chrome, Edge and Brave where the connector is. Runs on every start, so it also
/// follows the app if it is moved or updated.
pub fn register_native_host() {
    thread::spawn(|| {
        let Ok(exe) = std::env::current_exe() else { return };
        let manifest = state::data_dir().join("native-host.json");
        let json = serde_json::json!({
            "name": native::HOST_NAME,
            "description": "Orexa Downloader",
            "path": exe.to_string_lossy(),
            "type": "stdio",
            "allowed_origins": native::EXTENSION_IDS
                .iter()
                .map(|id| format!("chrome-extension://{}/", id))
                .collect::<Vec<_>>(),
        });
        let _ = fs::create_dir_all(state::data_dir());
        if fs::write(&manifest, serde_json::to_string_pretty(&json).unwrap_or_default()).is_err() {
            return;
        }
        for base in [
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts",
            r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts",
            r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts",
        ] {
            let key = format!(r"{}\{}", base, native::HOST_NAME);
            let mut c = Command::new("reg");
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                c.creation_flags(0x0800_0000);
            }
            let _ = c
                .args(["add", &key, "/ve", "/t", "REG_SZ", "/d", &manifest.to_string_lossy(), "/f"])
                .status();
        }
    });
}
