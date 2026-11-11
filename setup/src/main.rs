// Orexa Downloader Setup: a small installer wizard.
//
// - Installs to C:\Program Files\Orexa Downloader (asks Windows for admin approval).
// - Offers a Desktop shortcut.
// - Running a newer setup over an installed copy updates it in place; downloads and
//   settings are kept.
// - The installed copy of this file is the uninstaller (--uninstall).
//
// Scripted use:  Orexa-Downloader-Setup.exe --silent [--dir "D:\Apps\Orexa"] [--no-desktop] [--launch]
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::fs;
use std::io::Read;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use eframe::egui;
use egui::{pos2, vec2, Align, Align2, Color32, FontFamily, FontId, Layout, Margin, RichText, Sense, Stroke};

static APP_GZ: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/app.exe.gz"));
static ICON_PNG: &[u8] = include_bytes!("../../app/assets/orexa-app-icon.png");
static FONT_REGULAR: &[u8] = include_bytes!("../../app/assets/PlusJakartaSans-Regular.ttf");
static FONT_SEMIBOLD: &[u8] = include_bytes!("../../app/assets/PlusJakartaSans-SemiBold.ttf");
static FONT_BOLD: &[u8] = include_bytes!("../../app/assets/PlusJakartaSans-Bold.ttf");

/// The Chrome/Edge/Brave extension, copied next to the app so it can be loaded from there.
static EXTENSION_FILES: &[(&str, &[u8])] = &[
    ("manifest.json", include_bytes!("../../extension/manifest.json")),
    ("background.js", include_bytes!("../../extension/background.js")),
    ("content-youtube.js", include_bytes!("../../extension/content-youtube.js")),
    ("content-media.js", include_bytes!("../../extension/content-media.js")),
    ("popup.html", include_bytes!("../../extension/popup.html")),
    ("popup.css", include_bytes!("../../extension/popup.css")),
    ("popup.js", include_bytes!("../../extension/popup.js")),
    ("dialog.html", include_bytes!("../../extension/dialog.html")),
    ("dialog.css", include_bytes!("../../extension/dialog.css")),
    ("dialog.js", include_bytes!("../../extension/dialog.js")),
    ("dialog-logic.js", include_bytes!("../../extension/dialog-logic.js")),
    ("icons/symbol.svg", include_bytes!("../../extension/icons/symbol.svg")),
    ("fonts/PlusJakartaSans-Regular.ttf", include_bytes!("../../extension/fonts/PlusJakartaSans-Regular.ttf")),
    ("fonts/PlusJakartaSans-Medium.ttf", include_bytes!("../../extension/fonts/PlusJakartaSans-Medium.ttf")),
    ("fonts/PlusJakartaSans-SemiBold.ttf", include_bytes!("../../extension/fonts/PlusJakartaSans-SemiBold.ttf")),
    ("fonts/PlusJakartaSans-Bold.ttf", include_bytes!("../../extension/fonts/PlusJakartaSans-Bold.ttf")),
    ("icons/icon16.png", include_bytes!("../../extension/icons/icon16.png")),
    ("icons/icon32.png", include_bytes!("../../extension/icons/icon32.png")),
    ("icons/icon48.png", include_bytes!("../../extension/icons/icon48.png")),
    ("icons/icon128.png", include_bytes!("../../extension/icons/icon128.png")),
];

const VERSION: &str = env!("APP_VERSION");
const KEY: &str = r"HKLM\Software\Microsoft\Windows\CurrentVersion\Uninstall\OrexaDownloader";
const OLD_USER_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\OrexaDownloader";
const NO_WINDOW: u32 = 0x0800_0000;

// Brand colours (Orexa palette).
const NAVY: Color32 = Color32::from_rgb(0x0B, 0x12, 0x20);
const AQUA: Color32 = Color32::from_rgb(0x22, 0xD3, 0xEE);
const GRAY: Color32 = Color32::from_rgb(0xE2, 0xE8, 0xF0);
const MUTED: Color32 = Color32::from_rgb(0x60, 0x71, 0x7E);
const SUB: Color32 = Color32::from_rgb(0x47, 0x55, 0x69);
const BAD: Color32 = Color32::from_rgb(0xB4, 0x23, 0x18);
const OK: Color32 = Color32::from_rgb(0x06, 0x76, 0x47);

// ---------------------------------------------------------------------------
// System helpers
// ---------------------------------------------------------------------------

fn hidden(program: &str) -> Command {
    let mut c = Command::new(program);
    c.creation_flags(NO_WINDOW);
    c
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

fn program_files() -> PathBuf {
    env_path("ProgramW6432")
        .or_else(|| env_path("ProgramFiles"))
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files"))
}

fn default_dir() -> PathBuf {
    program_files().join("Orexa Downloader")
}

fn is_elevated() -> bool {
    hidden("fltmc").status().map(|s| s.success()).unwrap_or(false)
}

#[link(name = "shell32")]
extern "system" {
    fn ShellExecuteW(
        hwnd: *mut core::ffi::c_void,
        op: *const u16,
        file: *const u16,
        params: *const u16,
        dir: *const u16,
        show: i32,
    ) -> isize;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Ask Windows to run this program again with administrator rights.
fn relaunch_elevated(args: &[String]) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let params = args
        .iter()
        .map(|a| if a.contains(' ') { format!("\"{}\"", a) } else { a.clone() })
        .collect::<Vec<_>>()
        .join(" ");
    let r = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            wide("runas").as_ptr(),
            wide(&exe.to_string_lossy()).as_ptr(),
            wide(&params).as_ptr(),
            std::ptr::null(),
            1,
        )
    };
    r > 32
}

fn reg_get(key: &str, name: &str) -> Option<String> {
    let out = hidden("reg").args(["query", key, "/v", name]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().find(|l| l.trim_start().starts_with(name))?;
    let rest = line.trim_start().strip_prefix(name)?.trim_start();
    let mut parts = rest.splitn(2, char::is_whitespace);
    let _kind = parts.next()?;
    Some(parts.next()?.trim().to_string())
}

fn reg_set(key: &str, name: &str, kind: &str, value: &str) {
    let _ = hidden("reg").args(["add", key, "/v", name, "/t", kind, "/d", value, "/f"]).status();
}

fn stop_app() {
    let _ = hidden("taskkill").args(["/F", "/IM", "orexa-downloader.exe"]).status();
    thread::sleep(Duration::from_millis(600));
}

fn ps(script: &str) {
    let _ = hidden("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden", "-Command", script])
        .status();
}

fn q(path: &Path) -> String {
    path.to_string_lossy().replace('\'', "''")
}

/// Start a program as the normal user, not as administrator.
fn launch_normally(exe: &Path) {
    let _ = Command::new("explorer.exe").arg(exe).spawn();
}

fn replace_file(tmp: &Path, dest: &Path) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..24 {
        let _ = fs::remove_file(dest);
        match fs::rename(tmp, dest) {
            Ok(()) => return Ok(()),
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_millis(250));
    }
    let _ = fs::remove_file(tmp);
    Err(format!("Could not replace {}: {}", dest.display(), last))
}

// ---------------------------------------------------------------------------
// Install / uninstall work
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Existing {
    dir: PathBuf,
    version: String,
}

fn find_existing() -> Option<Existing> {
    let dir = reg_get(KEY, "InstallLocation")?;
    if dir.is_empty() {
        return None;
    }
    Some(Existing {
        dir: PathBuf::from(dir),
        version: reg_get(KEY, "DisplayVersion").unwrap_or_default(),
    })
}

#[derive(Clone)]
struct InstallOpts {
    dir: PathBuf,
    desktop: bool,
    /// Dry run: no closing apps, no shortcuts, no registry, no launch.
    test: bool,
}

fn remove_shortcuts(per_user_too: bool) {
    let mut script = String::from(
        "Remove-Item -Force -ErrorAction SilentlyContinue \
         ([Environment]::GetFolderPath('CommonDesktopDirectory') + '\\Orexa Downloader.lnk'), \
         ([Environment]::GetFolderPath('CommonPrograms') + '\\Orexa Downloader.lnk')",
    );
    if per_user_too {
        script.push_str(
            ", ([Environment]::GetFolderPath('Desktop') + '\\Orexa Downloader.lnk'), \
             ([Environment]::GetFolderPath('Programs') + '\\Orexa Downloader.lnk')",
        );
    }
    ps(&script);
}

/// An earlier version installed itself per user; clean that up so there is only one copy.
fn migrate_old_user_install() {
    if let Some(dir) = reg_get(OLD_USER_KEY, "InstallLocation") {
        if !dir.is_empty() {
            let _ = fs::remove_dir_all(&dir);
        }
        let _ = hidden("reg").args(["delete", OLD_USER_KEY, "/f"]).status();
        ps(
            "Remove-Item -Force -ErrorAction SilentlyContinue \
             ([Environment]::GetFolderPath('Desktop') + '\\Orexa Downloader.lnk'), \
             ([Environment]::GetFolderPath('Programs') + '\\Orexa Downloader.lnk')",
        );
    }
}

fn install(opts: &InstallOpts, report: &dyn Fn(f32, &str)) -> Result<(), String> {
    let dir = &opts.dir;
    if !opts.test {
        report(0.05, "Closing Orexa Downloader...");
        stop_app();
        report(0.12, "Looking for an older copy...");
        migrate_old_user_install();
    }

    report(0.2, "Unpacking the app...");
    let mut exe_bytes = Vec::new();
    flate2::read::GzDecoder::new(APP_GZ)
        .read_to_end(&mut exe_bytes)
        .map_err(|e| format!("Could not unpack the app: {}", e))?;

    report(0.4, "Copying files...");
    fs::create_dir_all(dir).map_err(|e| format!("Could not create {}: {}", dir.display(), e))?;
    let exe = dir.join("orexa-downloader.exe");
    let tmp = dir.join("orexa-downloader.exe.new");
    fs::write(&tmp, &exe_bytes).map_err(|e| format!("Could not write the app: {}", e))?;
    replace_file(&tmp, &exe)?;

    report(0.5, "Adding the browser extension...");
    let ext_dir = dir.join("extension");
    for (name, bytes) in EXTENSION_FILES {
        let target = ext_dir.join(name);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&target, bytes).map_err(|e| format!("Could not write {}: {}", target.display(), e))?;
    }

    // This file becomes the uninstaller (and the updater: run a newer setup over it).
    report(0.6, "Adding the uninstaller...");
    if let Ok(me) = std::env::current_exe() {
        let un_tmp = dir.join("uninstall.exe.new");
        if fs::copy(&me, &un_tmp).is_ok() {
            let _ = replace_file(&un_tmp, &dir.join("uninstall.exe"));
        }
    }

    if opts.test {
        report(1.0, "Done");
        return Ok(());
    }

    report(0.75, "Creating shortcuts...");
    let desktop = if opts.desktop {
        format!(
            "$l = $s.CreateShortcut((Join-Path ([Environment]::GetFolderPath('CommonDesktopDirectory')) 'Orexa Downloader.lnk')); \
             $l.TargetPath = '{exe}'; $l.WorkingDirectory = '{dir}'; $l.IconLocation = '{exe},0'; \
             $l.Description = 'Orexa Downloader'; $l.Save();",
            exe = q(&exe),
            dir = q(dir)
        )
    } else {
        "Remove-Item -Force -ErrorAction SilentlyContinue \
         (Join-Path ([Environment]::GetFolderPath('CommonDesktopDirectory')) 'Orexa Downloader.lnk');"
            .to_string()
    };
    ps(&format!(
        "$s = New-Object -ComObject WScript.Shell; {desktop} \
         $m = $s.CreateShortcut((Join-Path ([Environment]::GetFolderPath('CommonPrograms')) 'Orexa Downloader.lnk')); \
         $m.TargetPath = '{exe}'; $m.WorkingDirectory = '{dir}'; $m.IconLocation = '{exe},0'; \
         $m.Description = 'Orexa Downloader'; $m.Save()",
        desktop = desktop,
        exe = q(&exe),
        dir = q(dir)
    ));

    // Offer the browser extension: Chrome, Brave and Edge ask the person, the next time they
    // start, whether to turn it on.
    report(0.85, "Offering the browser extension...");
    for root in [
        r"HKCU\Software\Google\Chrome\Extensions",
        r"HKCU\Software\BraveSoftware\Brave-Browser\Extensions",
        r"HKCU\Software\Microsoft\Edge\Extensions",
    ] {
        let _ = hidden("reg")
            .args([
                "add",
                &format!(r"{}\lhlfkhkcodflljkapdcpcecapmncjfeh", root),
                "/v",
                "update_url",
                "/t",
                "REG_SZ",
                "/d",
                "https://clients2.google.com/service/update2/crx",
                "/f",
            ])
            .status();
    }

    report(0.9, "Registering with Windows...");
    let uninstall_cmd = format!("\"{}\" --uninstall", dir.join("uninstall.exe").display());
    let icon = format!("{},0", exe.display());
    let loc = dir.display().to_string();
    let size_kb = ((exe_bytes.len() + 20_000_000) / 1024).to_string();
    for (name, kind, value) in [
        ("DisplayName", "REG_SZ", "Orexa Downloader"),
        ("DisplayVersion", "REG_SZ", VERSION),
        ("Publisher", "REG_SZ", "Orexa"),
        ("InstallLocation", "REG_SZ", loc.as_str()),
        ("DisplayIcon", "REG_SZ", icon.as_str()),
        ("UninstallString", "REG_SZ", uninstall_cmd.as_str()),
        ("EstimatedSize", "REG_DWORD", size_kb.as_str()),
        ("NoModify", "REG_DWORD", "1"),
        ("NoRepair", "REG_DWORD", "1"),
    ] {
        reg_set(KEY, name, kind, value);
    }
    // Torrents open a network port. Allowing the app in the firewall now means Windows
    // never has to ask the person later (failing here is harmless).
    if !opts.test {
        let _ = hidden("netsh").args(["advfirewall", "firewall", "delete", "rule", "name=Orexa Downloader"]).status();
        let _ = hidden("netsh")
            .args([
                "advfirewall",
                "firewall",
                "add",
                "rule",
                "name=Orexa Downloader",
                "dir=in",
                "action=allow",
                &format!("program={}", exe.display()),
                "enable=yes",
                "profile=any",
            ])
            .status();
    }
    report(1.0, "Done");
    Ok(())
}

fn uninstall(dir: &Path, purge: bool, test: bool) {
    if !test {
        stop_app();
        remove_shortcuts(true);
        let _ = hidden("reg").args(["delete", KEY, "/f"]).status();
        let _ = hidden("reg").args(["delete", OLD_USER_KEY, "/f"]).status();
        let _ = hidden("netsh").args(["advfirewall", "firewall", "delete", "rule", "name=Orexa Downloader"]).status();
        // No longer open magnet links and .torrent files.
        for key in [
            r"HKCU\Software\Classes\Orexa.Magnet",
            r"HKCU\Software\Classes\Orexa.Torrent",
            r"HKCU\Software\Orexa",
        ] {
            let _ = hidden("reg").args(["delete", key, "/f"]).status();
        }
        let _ = hidden("reg")
            .args(["delete", r"HKCU\Software\RegisteredApplications", "/v", "Orexa Downloader", "/f"])
            .status();
        let _ = hidden("reg")
            .args(["delete", r"HKCU\Software\Classes\.torrent\OpenWithProgids", "/v", "Orexa.Torrent", "/f"])
            .status();
        // The direct handlers Orexa may have set (only if they are Orexa's).
        let direct_magnet = hidden("reg")
            .args(["query", r"HKCU\Software\Classes\magnet\shell\open\command", "/ve"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("orexa-downloader"))
            .unwrap_or(false);
        if direct_magnet {
            let _ = hidden("reg").args(["delete", r"HKCU\Software\Classes\magnet", "/f"]).status();
        }
        let direct_torrent = hidden("reg")
            .args(["query", r"HKCU\Software\Classes\.torrent", "/ve"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains("Orexa.Torrent"))
            .unwrap_or(false);
        if direct_torrent {
            let _ = hidden("reg").args(["delete", r"HKCU\Software\Classes\.torrent", "/ve", "/f"]).status();
        }
        // No longer start with Windows.
        let _ = hidden("reg")
            .args(["delete", r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run", "/v", "Orexa Downloader", "/f"])
            .status();
        // The browser connector registration the app added for Chrome, Edge and Brave.
        for key in [
            r"HKCU\Software\Google\Chrome\NativeMessagingHosts\com.orexa.downloader",
            r"HKCU\Software\Microsoft\Edge\NativeMessagingHosts\com.orexa.downloader",
            r"HKCU\Software\BraveSoftware\Brave-Browser\NativeMessagingHosts\com.orexa.downloader",
        ] {
            let _ = hidden("reg").args(["delete", key, "/f"]).status();
        }
        // The offer to add the store extension to Chrome, Brave and Edge.
        for root in [
            r"HKCU\Software\Google\Chrome\Extensions",
            r"HKCU\Software\BraveSoftware\Brave-Browser\Extensions",
            r"HKCU\Software\Microsoft\Edge\Extensions",
        ] {
            let _ = hidden("reg")
                .args(["delete", &format!(r"{}\lhlfkhkcodflljkapdcpcecapmncjfeh", root), "/f"])
                .status();
        }
        if let Some(local) = env_path("LOCALAPPDATA") {
            let orexa = local.join("Orexa");
            let _ = fs::remove_file(orexa.join("extension-offered"));
            // The helper tools are always removed; history and settings only on request.
            let _ = fs::remove_dir_all(orexa.join("tools"));
            // The saved YouTube sign-in is private, so it never outlives the app.
            let _ = fs::remove_dir_all(orexa.join("cookies"));
            if purge {
                let _ = fs::remove_dir_all(&orexa);
            }
        }
    }
    // This program lives in the folder being removed, so finish from a helper process.
    let cmd = format!("/c ping 127.0.0.1 -n 3 >nul & rmdir /s /q \"{}\"", dir.display());
    let _ = hidden("cmd.exe").raw_arg(cmd).spawn();
}

// ---------------------------------------------------------------------------
// The wizard window
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Progress {
    frac: f32,
    text: String,
    result: Option<Result<(), String>>,
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Welcome,
    Working,
    Finished,
    ConfirmRemove,
    Removed,
}

struct Wizard {
    page: Page,
    dir: String,
    desktop: bool,
    purge: bool,
    launch: bool,
    existing: Option<Existing>,
    progress: Arc<Mutex<Progress>>,
    test: bool,
    icon: Option<egui::TextureHandle>,
    install_dir_for_remove: PathBuf,
    shot: Option<String>,
    frames: u32,
}

fn rt(s: impl Into<String>, size: f32, family: &str, color: Color32) -> RichText {
    let font = match family {
        "semibold" => FontId::new(size, FontFamily::Name("semibold".into())),
        "bold" => FontId::new(size, FontFamily::Name("bold".into())),
        _ => FontId::new(size, FontFamily::Proportional),
    };
    RichText::new(s.into()).font(font).color(color)
}

fn setup_style(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    for (key, bytes) in [("reg", FONT_REGULAR), ("semi", FONT_SEMIBOLD), ("bold", FONT_BOLD)] {
        fonts.font_data.insert(key.into(), egui::FontData::from_static(bytes).into());
    }
    let fallback = fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let fam = |first: &str| {
        let mut v = vec![first.to_string()];
        v.extend(fallback.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, fam("reg"));
    fonts.families.insert(FontFamily::Name("semibold".into()), fam("semi"));
    fonts.families.insert(FontFamily::Name("bold".into()), fam("bold"));
    ctx.set_fonts(fonts);

    let mut v = egui::Visuals::light();
    v.panel_fill = Color32::WHITE;
    v.extreme_bg_color = Color32::WHITE;
    v.selection.bg_fill = AQUA.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.5, AQUA);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(0xCB, 0xD5, 0xE1));
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, MUTED);
    for w in [&mut v.widgets.noninteractive, &mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.rounding = 10.0.into();
    }
    ctx.set_visuals(v);
}

fn button(ui: &mut egui::Ui, label: &str, primary: bool, enabled: bool) -> egui::Response {
    let font = FontId::new(14.0, FontFamily::Name("semibold".into()));
    let tw = ui.fonts(|f| f.layout_no_wrap(label.to_string(), font.clone(), Color32::WHITE).size().x);
    let (rect, resp) = ui.allocate_exact_size(vec2(tw + 40.0, 40.0), if enabled { Sense::click() } else { Sense::hover() });
    let hov = resp.hovered() && enabled;
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let border = Color32::from_rgb(0xCB, 0xD5, 0xE1);
    let (bg, fg, stroke) = if primary {
        if enabled {
            (if hov { Color32::from_rgb(0x1F, 0xC0, 0xDA) } else { AQUA }, NAVY, Stroke::NONE)
        } else {
            (GRAY, MUTED, Stroke::NONE)
        }
    } else {
        (
            if hov { Color32::from_rgb(0xF1, 0xF5, 0xF9) } else { Color32::WHITE },
            if enabled { NAVY } else { MUTED },
            Stroke::new(1.0, border),
        )
    };
    ui.painter().rect_filled(rect, 10.0, bg);
    if stroke != Stroke::NONE {
        ui.painter().rect_stroke(rect, 10.0, stroke);
    }
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font, fg);
    resp
}

impl Wizard {
    fn new(
        cc: &eframe::CreationContext<'_>,
        uninstall_mode: bool,
        dir: Option<PathBuf>,
        test: bool,
        shot: Option<String>,
        preview: Option<String>,
    ) -> Self {
        setup_style(&cc.egui_ctx);
        let icon = image::load_from_memory(ICON_PNG).ok().map(|img| {
            let img = img.to_rgba8();
            let (w, h) = img.dimensions();
            cc.egui_ctx.load_texture(
                "icon",
                egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw()),
                egui::TextureOptions::LINEAR,
            )
        });
        let existing = if test || shot.is_some() { None } else { find_existing() };
        let dir = dir
            .or_else(|| existing.as_ref().map(|e| e.dir.clone()))
            .unwrap_or_else(default_dir);
        let here = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| dir.clone());
        let mut w = Self {
            page: if uninstall_mode { Page::ConfirmRemove } else { Page::Welcome },
            dir: dir.display().to_string(),
            desktop: true,
            purge: false,
            launch: true,
            existing,
            progress: Arc::new(Mutex::new(Progress::default())),
            test,
            icon,
            install_dir_for_remove: here,
            shot,
            frames: 0,
        };
        // Layout previews for screenshots.
        match preview.as_deref() {
            Some("update") => w.existing = Some(Existing { dir: default_dir(), version: "0.0.9".into() }),
            Some("working") => {
                w.page = Page::Working;
                *w.progress.lock().unwrap() =
                    Progress { frac: 0.55, text: "Copying files...".into(), result: None };
            }
            Some("done") => {
                w.page = Page::Finished;
                w.progress.lock().unwrap().result = Some(Ok(()));
            }
            Some("failed") => {
                w.page = Page::Finished;
                w.progress.lock().unwrap().result =
                    Some(Err("Could not replace the app: access denied".into()));
            }
            Some("removed") => w.page = Page::Removed,
            _ => {}
        }
        w
    }

    fn start_install(&mut self) {
        self.page = Page::Working;
        let opts = InstallOpts { dir: PathBuf::from(self.dir.trim()), desktop: self.desktop, test: self.test };
        let prog = self.progress.clone();
        thread::spawn(move || {
            let report = |f: f32, t: &str| {
                let mut p = prog.lock().unwrap();
                p.frac = f;
                p.text = t.to_string();
            };
            let res = install(&opts, &report);
            prog.lock().unwrap().result = Some(res);
        });
    }

    fn header(&self, ui: &mut egui::Ui, title: &str, subtitle: &str) {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(64.0, 64.0), Sense::hover());
            if let Some(t) = &self.icon {
                ui.painter().image(
                    t.id(),
                    r,
                    egui::Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                    Color32::WHITE,
                );
            }
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.add_space(6.0);
                ui.label(rt(title, 24.0, "bold", NAVY));
                ui.label(rt(subtitle, 14.0, "reg", MUTED));
            });
        });
    }
}

impl eframe::App for Wizard {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        // Move on when the background work ends.
        if self.page == Page::Working {
            ctx.request_repaint_after(Duration::from_millis(60));
            if self.progress.lock().unwrap().result.is_some() {
                self.page = Page::Finished;
            }
        }

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(Color32::WHITE).inner_margin(Margin::symmetric(32.0, 28.0)))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                match self.page {
                    Page::Welcome => self.page_welcome(ui),
                    Page::Working => self.page_working(ui),
                    Page::Finished => self.page_finished(ui, ctx),
                    Page::ConfirmRemove => self.page_confirm_remove(ui, ctx),
                    Page::Removed => self.page_removed(ui, ctx),
                }
            });

        // Screenshot mode (for checking the layout).
        if let Some(path) = self.shot.clone() {
            self.frames += 1;
            if self.frames == 20 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            } else if self.frames > 20 {
                let img = ctx.input(|i| {
                    i.raw.events.iter().find_map(|e| match e {
                        egui::Event::Screenshot { image, .. } => Some(image.clone()),
                        _ => None,
                    })
                });
                if let Some(img) = img {
                    let (w, h) = (img.width() as u32, img.height() as u32);
                    let mut buf = image::RgbaImage::new(w, h);
                    for y in 0..h as usize {
                        for x in 0..w as usize {
                            let c = img[(x, y)];
                            buf.put_pixel(x as u32, y as u32, image::Rgba([c.r(), c.g(), c.b(), c.a()]));
                        }
                    }
                    let _ = buf.save(path);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
            ctx.request_repaint();
        }
    }
}

impl Wizard {
    fn page_welcome(&mut self, ui: &mut egui::Ui) {
        let updating = self.existing.is_some();
        self.header(
            ui,
            if updating { "Update Orexa Downloader" } else { "Install Orexa Downloader" },
            &format!("Version {}", VERSION),
        );
        ui.add_space(14.0);
        let intro = match &self.existing {
            Some(e) if !e.version.is_empty() => format!(
                "Version {} is installed. This updates it to {} and replaces the old files. Your downloads and settings are kept.",
                e.version, VERSION
            ),
            Some(_) => "Orexa Downloader is installed. This updates it and replaces the old files. Your downloads and settings are kept.".to_string(),
            None => "Orexa Downloader will be installed on this computer. Video download support is set up automatically the first time you open it.".to_string(),
        };
        ui.add(egui::Label::new(rt(intro, 14.0, "reg", SUB)).wrap());
        ui.add_space(14.0);

        ui.label(rt("Install location", 13.0, "semibold", SUB));
        ui.horizontal(|ui| {
            let browse_w = 96.0;
            let w = (ui.available_width() - browse_w - 10.0 - 24.0).max(120.0);
            ui.add_enabled(
                !updating,
                egui::TextEdit::singleline(&mut self.dir)
                    .font(FontId::new(14.0, FontFamily::Proportional))
                    .margin(Margin::symmetric(12.0, 10.0))
                    .desired_width(w),
            );
            if button(ui, "Browse", false, !updating).clicked() {
                if let Some(p) = rfd::FileDialog::new().set_title("Choose where to install").pick_folder() {
                    // Always keep it in its own folder.
                    let named = p
                        .file_name()
                        .map(|n| n.to_string_lossy().contains("Orexa"))
                        .unwrap_or(false);
                    self.dir = if named { p } else { p.join("Orexa Downloader") }.display().to_string();
                }
            }
        });
        if updating {
            ui.label(rt("The existing install location is kept.", 12.0, "reg", MUTED));
        }
        ui.add_space(6.0);
        ui.checkbox(&mut self.desktop, rt("Show Orexa Downloader on the Desktop", 14.0, "reg", NAVY));
        ui.add_space(2.0);
        ui.label(rt("Needs about 40 MB of disk space.", 12.0, "reg", MUTED));

        ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let ready = !self.dir.trim().is_empty();
                    if button(ui, if updating { "Update" } else { "Install" }, true, ready).clicked() {
                        self.start_install();
                    }
                    ui.add_space(4.0);
                    if button(ui, "Cancel", false, true).clicked() {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }

    fn page_working(&mut self, ui: &mut egui::Ui) {
        self.header(ui, "Installing...", "Please wait a moment");
        ui.add_space(30.0);
        let (frac, text) = {
            let p = self.progress.lock().unwrap();
            (p.frac, p.text.clone())
        };
        ui.label(rt(text, 14.0, "reg", SUB));
        ui.add_space(4.0);
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
        ui.painter().rect_filled(r, 4.0, GRAY);
        ui.painter()
            .rect_filled(egui::Rect::from_min_size(r.min, vec2((r.width() * frac).max(8.0), 8.0)), 4.0, AQUA);
    }

    fn page_finished(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let result = self.progress.lock().unwrap().result.clone();
        match result {
            Some(Ok(())) => {
                self.header(ui, "All set", &format!("Orexa Downloader {} is installed", VERSION));
                ui.add_space(24.0);
                ui.label(rt("You can open it from the Start menu or the Desktop.", 14.0, "reg", SUB));
                ui.add_space(6.0);
                ui.add(
                    egui::Label::new(rt(
                        "Chrome, Brave or Edge will ask you to turn on the Orexa extension the next time you open them.",
                        13.0,
                        "reg",
                        MUTED,
                    ))
                    .wrap(),
                );
                ui.add_space(8.0);
                ui.checkbox(&mut self.launch, rt("Open Orexa Downloader now", 14.0, "reg", NAVY));
                ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
                    if button(ui, "Finish", true, true).clicked() {
                        if self.launch && !self.test {
                            launch_normally(&PathBuf::from(self.dir.trim()).join("orexa-downloader.exe"));
                        }
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            }
            Some(Err(e)) => {
                self.header(ui, "Setup could not finish", "Please try again");
                ui.add_space(24.0);
                ui.add(egui::Label::new(rt(e, 14.0, "reg", BAD)).wrap());
                ui.add_space(6.0);
                ui.label(rt("Close other programs that use Orexa Downloader and run setup again.", 13.0, "reg", MUTED));
                ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
                    if button(ui, "Close", true, true).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            }
            None => {}
        }
    }

    fn page_confirm_remove(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.header(ui, "Remove Orexa Downloader", &format!("Version {}", VERSION));
        ui.add_space(16.0);
        ui.add(
            egui::Label::new(rt(
                "This removes the app, its shortcuts and the YouTube helper tools from this computer. Files you downloaded are not touched.",
                14.0,
                "reg",
                SUB,
            ))
            .wrap(),
        );
        ui.add_space(10.0);
        ui.checkbox(&mut self.purge, rt("Also delete my download list and settings", 14.0, "reg", NAVY));
        ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
            ui.horizontal(|ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (rect, resp) = ui.allocate_exact_size(vec2(100.0, 40.0), Sense::click());
                    if resp.hovered() {
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    ui.painter()
                        .rect_filled(rect, 10.0, if resp.hovered() { BAD.gamma_multiply(0.85) } else { BAD });
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "Remove",
                        FontId::new(14.0, FontFamily::Name("semibold".into())),
                        Color32::WHITE,
                    );
                    if resp.clicked() {
                        uninstall(&self.install_dir_for_remove, self.purge, self.test);
                        self.page = Page::Removed;
                    }
                    ui.add_space(4.0);
                    if button(ui, "Cancel", false, true).clicked() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                });
            });
        });
    }

    fn page_removed(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        self.header(ui, "Removed", "Orexa Downloader is no longer installed");
        ui.add_space(24.0);
        ui.label(rt("Thank you for using Orexa Downloader.", 14.0, "reg", OK));
        ui.with_layout(Layout::bottom_up(Align::Max), |ui| {
            if button(ui, "Close", true, true).clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let value = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();

    let test = flag("--test");
    let uninstall_mode = flag("--uninstall");
    let silent = flag("--silent") || flag("/S");
    let dir_arg = value("--dir").map(PathBuf::from);
    let shot = value("--screenshot");
    let preview = value("--preview");

    // Program Files needs administrator rights; ask Windows once, then carry on.
    if !test && shot.is_none() && !is_elevated() {
        let _ = relaunch_elevated(&args);
        return;
    }

    if silent && uninstall_mode {
        let here = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(default_dir);
        uninstall(&here, flag("--purge"), test);
        return;
    }

    if silent && !uninstall_mode {
        let dir = dir_arg
            .or_else(|| find_existing().map(|e| e.dir))
            .unwrap_or_else(default_dir);
        let opts = InstallOpts { dir, desktop: !flag("--no-desktop"), test };
        // --launch: open Orexa again when done (used by the in-app updater), even if the
        // install failed, so the person is never left without the app.
        let app = opts.dir.join("orexa-downloader.exe");
        if let Err(e) = install(&opts, &|_, _| {}) {
            let _ = fs::write(std::env::temp_dir().join("orexa-install-error.txt"), e);
            if flag("--launch") && !test {
                launch_normally(&app);
            }
            std::process::exit(1);
        }
        if flag("--launch") && !test {
            launch_normally(&app);
        }
        return;
    }

    // The Orexa logo for the window's title bar and the taskbar.
    fn window_icon() -> egui::IconData {
        let img = image::load_from_memory(ICON_PNG)
            .map(|i| image::imageops::resize(&i.to_rgba8(), 256, 256, image::imageops::FilterType::Lanczos3))
            .unwrap_or_else(|_| image::RgbaImage::new(1, 1));
        egui::IconData { width: img.width(), height: img.height(), rgba: img.into_raw() }
    }

    let options = eframe::NativeOptions {
        centered: true,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 440.0])
            .with_resizable(false)
            .with_maximize_button(false)
            .with_icon(window_icon())
            .with_title(if uninstall_mode { "Orexa Downloader Uninstall" } else { "Orexa Downloader Setup" }),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Orexa Downloader Setup",
        options,
        Box::new(move |cc| Ok(Box::new(Wizard::new(cc, uninstall_mode, dir_arg, test, shot, preview)))),
    );
}
