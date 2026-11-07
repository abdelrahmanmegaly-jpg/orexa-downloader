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

/// The tray menu asked to bring a download's window back.
pub static REOPEN_DOWNLOAD: AtomicBool = AtomicBool::new(false);

pub fn make_tray() -> Option<tray_icon::TrayIcon> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let img = image::load_from_memory(APP_ICON_PNG).ok()?.to_rgba8();
    let small = image::imageops::resize(&img, 32, 32, image::imageops::FilterType::Lanczos3);
    let icon = tray_icon::Icon::from_rgba(small.into_raw(), 32, 32).ok()?;

    let open = MenuItem::new("Open Orexa Downloader", true, None);
    let current = MenuItem::new("Show the current download", true, None);
    let quit = MenuItem::new("Quit", true, None);
    let menu = Menu::new();
    menu.append_items(&[&open, &current, &PredefinedMenuItem::separator(), &quit]).ok()?;
    let (open_id, quit_id, current_id) = (open.id().clone(), quit.id().clone(), current.id().clone());

    // Handlers run on the event thread, so they work while the window is hidden.
    MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
        if e.id == open_id {
            show_window();
        } else if e.id == current_id {
            REOPEN_DOWNLOAD.store(true, Ordering::SeqCst);
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
    // Windows draws Arabic itself, so the notification gets plain translated text.
    let title = crate::i18n::translate(&title).unwrap_or(title);
    let body = crate::i18n::translate(&body).unwrap_or(body);
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

/// A "download complete" notification with Open file and Show in folder buttons.
/// The thread stays alive for a while so the buttons keep working.
pub fn toast_file(title: String, body: String, file: std::path::PathBuf) {
    let tr = |s: &str| crate::i18n::translate(s).unwrap_or_else(|| s.to_string());
    let title = tr(&title);
    let (open_label, folder_label) = (tr("Open file"), tr("Show in folder"));
    thread::spawn(move || {
        use tauri_winrt_notification::{IconCrop, Sound, Toast};
        let icon = icon_path();
        let target = file.clone();
        let mut t = Toast::new(AUMID)
            .title(&title)
            .text1(&body)
            .sound(Some(Sound::Default))
            .add_button(&open_label, "open")
            .add_button(&folder_label, "folder")
            .on_activated(move |action| {
                match action.as_deref() {
                    Some("folder") => {
                        if target.exists() {
                            let _ = Command::new("explorer").arg(format!("/select,{}", target.display())).spawn();
                        } else if let Some(dir) = target.parent() {
                            let _ = Command::new("explorer").arg(dir).spawn();
                        }
                    }
                    // The button or a click on the notification itself.
                    _ => {
                        let _ = Command::new("explorer").arg(&target).spawn();
                    }
                }
                Ok(())
            });
        if icon.exists() {
            t = t.icon(&icon, IconCrop::Square, TITLE);
        }
        if t.show().is_ok() {
            // Keep the buttons working while the notification can still be clicked.
            thread::sleep(std::time::Duration::from_secs(15 * 60));
        }
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

/// Free bytes on the drive that holds `path` (or its nearest existing parent).
pub fn free_space(path: &std::path::Path) -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
    }
    let mut dir = path.to_path_buf();
    while !dir.exists() {
        if !dir.pop() {
            return None;
        }
    }
    let w = wide(&dir.to_string_lossy());
    let (mut free, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut free, &mut total, &mut total_free) };
    (ok != 0).then_some(free)
}

fn reg(args: &[&str]) -> (bool, String) {
    let mut c = Command::new("reg");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000);
    }
    match c.args(args).output() {
        Ok(o) => (o.status.success(), String::from_utf8_lossy(&o.stdout).into_owned()),
        Err(_) => (false, String::new()),
    }
}

fn tell_windows_associations_changed() {
    #[link(name = "shell32")]
    extern "system" {
        fn SHChangeNotify(event: i32, flags: u32, item1: *const std::ffi::c_void, item2: *const std::ffi::c_void);
    }
    unsafe { SHChangeNotify(0x0800_0000, 0, std::ptr::null(), std::ptr::null()) }; // SHCNE_ASSOCCHANGED
}

/// Let Orexa open `magnet:` links and `.torrent` files when you click them in any program (or
/// stop doing so). It appears in Windows "Default apps"; if nothing else handles these yet it
/// becomes the handler right away. Development builds never register.
pub fn register_file_types(enable: bool) {
    if std::env::var_os("OREXA_NO_AUTOSTART").is_some() || std::env::var_os("OREXA_NO_REGISTRY").is_some() {
        return;
    }
    thread::spawn(move || {
        let Ok(exe) = std::env::current_exe() else { return };
        let exe_s = exe.to_string_lossy().to_string();
        if enable && exe_s.to_lowercase().contains("\\target\\") {
            return; // a build folder, not an installed copy
        }
        const CLASSES: &str = r"HKCU\Software\Classes";
        let open_cmd = format!("\"{}\" \"%1\"", exe_s);
        let icon = format!("\"{}\",0", exe_s);

        if enable {
            for (id, desc) in [("Orexa.Magnet", "URL:Magnet Link"), ("Orexa.Torrent", "BitTorrent file")] {
                let k = format!(r"{}\{}", CLASSES, id);
                reg(&["add", &k, "/ve", "/d", desc, "/f"]);
                if id == "Orexa.Magnet" {
                    reg(&["add", &k, "/v", "URL Protocol", "/d", "", "/f"]);
                }
                reg(&["add", &format!(r"{}\DefaultIcon", k), "/ve", "/d", &icon, "/f"]);
                reg(&["add", &format!(r"{}\shell\open\command", k), "/ve", "/d", &open_cmd, "/f"]);
            }
            reg(&["add", &format!(r"{}\.torrent\OpenWithProgids", CLASSES), "/v", "Orexa.Torrent", "/t", "REG_NONE", "/f"]);
            // So it is listed in Windows Settings > Default apps.
            let cap = r"HKCU\Software\Orexa\Capabilities";
            reg(&["add", cap, "/v", "ApplicationName", "/d", "Orexa Downloader", "/f"]);
            reg(&["add", cap, "/v", "ApplicationDescription", "/d", "Fast downloads, videos and torrents", "/f"]);
            reg(&["add", &format!(r"{}\URLAssociations", cap), "/v", "magnet", "/d", "Orexa.Magnet", "/f"]);
            reg(&["add", &format!(r"{}\FileAssociations", cap), "/v", ".torrent", "/d", "Orexa.Torrent", "/f"]);
            reg(&["add", r"HKCU\Software\RegisteredApplications", "/v", "Orexa Downloader", "/d", r"Software\Orexa\Capabilities", "/f"]);

            // If nothing handles magnet links yet (or an older Orexa does), be the handler now.
            let (found, text) = reg(&["query", r"HKCR\magnet\shell\open\command", "/ve"]);
            if !found || text.to_lowercase().contains("orexa-downloader") {
                let k = format!(r"{}\magnet", CLASSES);
                reg(&["add", &k, "/ve", "/d", "URL:Magnet Link", "/f"]);
                reg(&["add", &k, "/v", "URL Protocol", "/d", "", "/f"]);
                reg(&["add", &format!(r"{}\DefaultIcon", k), "/ve", "/d", &icon, "/f"]);
                reg(&["add", &format!(r"{}\shell\open\command", k), "/ve", "/d", &open_cmd, "/f"]);
            }
            // Same for .torrent files.
            let (_, text) = reg(&["query", r"HKCR\.torrent", "/ve"]);
            let has_default = text.lines().any(|l| l.contains("REG_SZ") && l.split("REG_SZ").nth(1).map(|v| !v.trim().is_empty() && !v.contains("value not set")).unwrap_or(false));
            if !has_default {
                reg(&["add", &format!(r"{}\.torrent", CLASSES), "/ve", "/d", "Orexa.Torrent", "/f"]);
            }
        } else {
            for id in ["Orexa.Magnet", "Orexa.Torrent"] {
                reg(&["delete", &format!(r"{}\{}", CLASSES, id), "/f"]);
            }
            reg(&["delete", &format!(r"{}\.torrent\OpenWithProgids", CLASSES), "/v", "Orexa.Torrent", "/f"]);
            reg(&["delete", r"HKCU\Software\Orexa", "/f"]);
            reg(&["delete", r"HKCU\Software\RegisteredApplications", "/v", "Orexa Downloader", "/f"]);
            // The direct handlers, but only if they are ours.
            let (_, text) = reg(&["query", &format!(r"{}\magnet\shell\open\command", CLASSES), "/ve"]);
            if text.to_lowercase().contains("orexa-downloader") {
                reg(&["delete", &format!(r"{}\magnet", CLASSES), "/f"]);
            }
            let (_, text) = reg(&["query", &format!(r"{}\.torrent", CLASSES), "/ve"]);
            if text.contains("Orexa.Torrent") {
                reg(&["delete", &format!(r"{}\.torrent", CLASSES), "/ve", "/f"]);
            }
        }
        tell_windows_associations_changed();
    });
}

/// Open Windows Settings > Default apps (to choose Orexa when another app handles these).
pub fn open_default_apps() {
    let _ = Command::new("explorer").arg("ms-settings:defaultapps").spawn();
}

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

/// True when Windows itself is set to Arabic (used to pick the language on first run).
pub fn system_language_is_arabic() -> bool {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
    }
    // The low ten bits are the primary language; Arabic is 0x01.
    unsafe { GetUserDefaultUILanguage() & 0x3FF == 0x01 }
}

/// Put the computer to sleep.
pub fn sleep_computer() {
    let _ = Command::new("rundll32.exe").args(["powrprof.dll,SetSuspendState", "0,1,0"]).spawn();
}

/// The size of the main screen in pixels.
pub fn screen_size() -> Option<(f32, f32)> {
    #[link(name = "user32")]
    extern "system" {
        fn GetSystemMetrics(index: i32) -> i32;
    }
    let (w, h) = unsafe { (GetSystemMetrics(0), GetSystemMetrics(1)) };
    (w > 0 && h > 0).then(|| (w as f32, h as f32))
}
