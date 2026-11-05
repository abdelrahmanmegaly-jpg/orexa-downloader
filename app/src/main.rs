// Run as a Windows GUI program so no console (CMD) window opens with the app.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod multipart;
mod native;
mod shell;
mod state;
mod torrent;
mod youtube;

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui;
use egui::{
    pos2, vec2, Align, Align2, Color32, FontFamily, FontId, Layout, Margin, Order, Rect,
    Response, RichText, Sense, Stroke, Ui,
};
use image::{ImageBuffer, Rgba};
use youtube::{is_youtube, SetupEvent, Thumb, Tool, YtInfo, YtOpts};

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Status {
    Queued,
    Downloading,
    Paused,
    Complete,
    Error,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Category {
    All,
    Downloading,
    Finished,
    Unfinished,
    Compressed,
    Documents,
    Music,
    Programs,
    Video,
    Torrents,
}

impl Category {
    const LIBRARY: [Category; 4] = [
        Category::All,
        Category::Downloading,
        Category::Finished,
        Category::Unfinished,
    ];
    const TYPES: [Category; 6] = [
        Category::Compressed,
        Category::Documents,
        Category::Music,
        Category::Programs,
        Category::Video,
        Category::Torrents,
    ];

    fn label(&self) -> &'static str {
        match self {
            Category::All => "All downloads",
            Category::Downloading => "Downloading",
            Category::Finished => "Completed",
            Category::Unfinished => "Unfinished",
            Category::Compressed => "Compressed",
            Category::Documents => "Documents",
            Category::Music => "Music",
            Category::Programs => "Programs",
            Category::Video => "Video",
            Category::Torrents => "Torrents",
        }
    }

    fn icon(&self) -> &'static str {
        match self {
            Category::All => "list",
            Category::Downloading => "download",
            Category::Finished => "check",
            Category::Unfinished => "clock",
            Category::Compressed => "archive",
            Category::Documents => "doc",
            Category::Music => "music",
            Category::Programs => "box",
            Category::Video => "film",
            Category::Torrents => "magnet",
        }
    }

    fn matches(&self, d: &Download) -> bool {
        match self {
            Category::All => true,
            Category::Downloading => d.status == Status::Downloading,
            Category::Finished => d.status == Status::Complete,
            Category::Unfinished => d.status != Status::Complete,
            Category::Torrents => d.is_torrent(),
            other => !d.is_torrent() && ext_cat(&d.ext) == Some(*other),
        }
    }
}

fn ext_cat(ext: &str) -> Option<Category> {
    match ext {
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "iso" => Some(Category::Compressed),
        "pdf" | "doc" | "docx" | "txt" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" => {
            Some(Category::Documents)
        }
        "mp3" | "wav" | "flac" | "ogg" | "m4a" => Some(Category::Music),
        "exe" | "msi" | "dmg" | "apk" | "deb" => Some(Category::Programs),
        "mp4" | "mkv" | "avi" | "mov" | "webm" => Some(Category::Video),
        _ => None,
    }
}

#[derive(Clone)]
struct Download {
    id: u64,
    url: String,
    file_name: String,
    ext: String,
    size: u64,
    downloaded: u64,
    status: Status,
    speed_bps: f64,
    save_dir: String,
    added: String,
    error: Option<String>,
    source: Source,
    thumb: Option<Arc<Thumb>>,
    /// Extra request details when a link comes from the browser.
    extra: Extra,
}

/// Referer, cookies and browser identity sent along with a download.
#[derive(Clone, Default)]
struct Extra {
    referer: Option<String>,
    cookie: Option<String>,
    user_agent: Option<String>,
}

impl Download {
    /// A magnet link or .torrent file, downloaded by the torrent engine.
    fn is_torrent(&self) -> bool {
        matches!(self.source, Source::Torrent)
    }
}

#[derive(Clone, PartialEq)]
enum Source {
    Http,
    Torrent,
    YouTube(YtOpts),
}

impl Download {
    fn progress(&self) -> f32 {
        if self.size == 0 {
            0.0
        } else {
            (self.downloaded as f64 / self.size as f64).clamp(0.0, 1.0) as f32
        }
    }

    fn eta_secs(&self) -> Option<f64> {
        if self.status != Status::Downloading || self.speed_bps < 1.0 || self.size == 0 {
            return None;
        }
        Some(self.size.saturating_sub(self.downloaded) as f64 / self.speed_bps)
    }

    fn path(&self) -> PathBuf {
        let dir = if self.save_dir.is_empty() {
            downloads_dir()
        } else {
            PathBuf::from(&self.save_dir)
        };
        dir.join(&self.file_name)
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn fmt_size(bytes: u64) -> String {
    if bytes == 0 {
        return "Unknown size".into();
    }
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

fn fmt_speed(bps: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    if bps >= MB {
        format!("{:.1} MB/s", bps / MB)
    } else if bps >= KB {
        format!("{:.0} KB/s", bps / KB)
    } else {
        format!("{:.0} B/s", bps)
    }
}

fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{} h {:02} min", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{} min {} s", s / 60, s % 60)
    } else {
        format!("{} s", s)
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok());
                match hex {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                    }
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn clean_name(n: &str) -> String {
    let n = n.rsplit(['/', '\\']).next().unwrap_or(n).trim();
    n.chars()
        .map(|c| if matches!(c, ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect()
}

fn file_name_from_url(url: &str) -> String {
    if torrent::is_magnet(url) {
        return torrent::magnet_name(url).map(|n| clean_name(&n)).unwrap_or_else(|| "Torrent".to_string());
    }
    // Signed links (GitHub, S3, Azure) keep the real name in the query string.
    if let Some((_, q)) = url.split_once('?') {
        let q = q.split('#').next().unwrap_or("");
        for part in q.split('&') {
            if let Some((k, v)) = part.split_once('=') {
                let k = k.to_lowercase();
                let v = percent_decode(v);
                if k == "response-content-disposition" || k == "rscd" || k == "content-disposition" {
                    if let Some(n) = name_from_disposition(&v) {
                        return clean_name(&n);
                    }
                } else if (k == "filename" || k == "file") && !v.trim().is_empty() {
                    return clean_name(&v);
                }
            }
        }
    }
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let name = clean.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() || name.contains(':') {
        "index.html".to_string()
    } else {
        clean_name(&percent_decode(name))
    }
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_lowercase())
        .unwrap_or_default()
}

fn downloads_dir() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Downloads")
}

fn open_path(path: &PathBuf) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

fn reveal_path(path: &PathBuf) {
    if path.exists() {
        let _ = std::process::Command::new("explorer")
            .arg(format!("/select,{}", path.display()))
            .spawn();
    } else if let Some(dir) = path.parent() {
        let _ = std::process::Command::new("explorer").arg(dir).spawn();
    }
}

// ---------------------------------------------------------------------------
// Download engine: background worker threads
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum WorkerCmd {
    Run,
    Pause,
    Stop,
}

enum EngineEvent {
    Progress(u64, u64, u64, f64), // id, downloaded, size, speed
    State(u64, Status, Option<String>),
    FileName(u64, String),
}

struct Engine {
    ev_tx: Sender<EngineEvent>,
    controls: BTreeMap<u64, Sender<WorkerCmd>>,
}

impl Engine {
    fn new() -> (Self, Receiver<EngineEvent>) {
        let (ev_tx, ev_rx) = channel::<EngineEvent>();
        (
            Self {
                ev_tx,
                controls: BTreeMap::new(),
            },
            ev_rx,
        )
    }

    fn start(&mut self, d: Download) {
        self.cancel(d.id);
        let (cmd_tx, cmd_rx) = channel::<WorkerCmd>();
        self.controls.insert(d.id, cmd_tx);
        let ev_tx = self.ev_tx.clone();
        match d.source.clone() {
            Source::Http => {
                thread::spawn(move || download_worker(d, cmd_rx, ev_tx));
            }
            Source::YouTube(o) => {
                thread::spawn(move || youtube::yt_worker(d, o, cmd_rx, ev_tx));
            }
            Source::Torrent => {
                thread::spawn(move || torrent::worker(d, cmd_rx, ev_tx));
            }
        }
    }

    fn pause(&mut self, id: u64) {
        if let Some(tx) = self.controls.get(&id) {
            let _ = tx.send(WorkerCmd::Pause);
        }
    }

    /// Wake a paused worker. Returns false when no live worker exists.
    fn resume(&mut self, id: u64) -> bool {
        match self.controls.get(&id) {
            Some(tx) => tx.send(WorkerCmd::Run).is_ok(),
            None => false,
        }
    }

    fn cancel(&mut self, id: u64) {
        if let Some(tx) = self.controls.remove(&id) {
            let _ = tx.send(WorkerCmd::Stop);
        }
    }
}

fn download_worker(d: Download, cmd_rx: Receiver<WorkerCmd>, ev_tx: Sender<EngineEvent>) {
    let _ = ev_tx.send(EngineEvent::State(d.id, Status::Downloading, None));

    let mut dir = PathBuf::from(&d.save_dir);
    if dir.as_os_str().is_empty() {
        dir = downloads_dir();
    }
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(&d.file_name);

    let client = match reqwest::blocking::Client::builder()
        .user_agent(
            d.extra
                .user_agent
                .clone()
                .unwrap_or_else(|| "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
        )
        .timeout(None)
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
            return;
        }
    };

    // Big files on servers that allow byte ranges: several connections at once.
    if let multipart::Outcome::Handled = multipart::run(&d, &client, &path, &cmd_rx, &ev_tx) {
        return;
    }

    // Resume support: keep what is already on disk and ask for the rest.
    let mut downloaded: u64 = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if d.size > 0 && downloaded >= d.size {
        let _ = ev_tx.send(EngineEvent::State(d.id, Status::Complete, None));
        return;
    }

    let mut req = client.get(&d.url);
    if let Some(r) = &d.extra.referer {
        req = req.header("Referer", r);
    }
    if let Some(c) = &d.extra.cookie {
        req = req.header("Cookie", c);
    }
    if downloaded > 0 {
        req = req.header("Range", format!("bytes={}-", downloaded));
    }

    let mut resp = match req.send() {
        Ok(r) if r.status().is_success() || r.status().as_u16() == 206 => r,
        Ok(r) => {
            let _ = ev_tx.send(EngineEvent::State(
                d.id,
                Status::Error,
                Some(format!("Server answered {}", r.status())),
            ));
            return;
        }
        Err(e) => {
            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
            return;
        }
    };

    let size = if d.size > 0 {
        d.size
    } else {
        resp.content_length().map(|l| l + downloaded).unwrap_or(0)
    };

    let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
            return;
        }
    };
    if downloaded == 0 {
        let _ = file.set_len(0);
    }

    let _ = ev_tx.send(EngineEvent::Progress(d.id, downloaded, size, 0.0));

    let mut buf = [0u8; 64 * 1024];
    let mut last_report = Instant::now();
    let mut window_start = Instant::now();
    let mut window_bytes: u64 = 0;
    let mut speed: f64 = 0.0;
    let mut paused = false;

    loop {
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Pause) => {
                paused = true;
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Paused, None));
            }
            Ok(WorkerCmd::Run) => {
                paused = false;
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Downloading, None));
                window_start = Instant::now();
                window_bytes = 0;
            }
            Ok(WorkerCmd::Stop) => {
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Queued, None));
                return;
            }
            Err(_) => {}
        }

        if paused {
            thread::sleep(Duration::from_millis(120));
            continue;
        }

        match resp.read(&mut buf) {
            Ok(0) => {
                let _ = ev_tx.send(EngineEvent::Progress(d.id, downloaded, size, 0.0));
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Complete, None));
                return;
            }
            Ok(n) => {
                if file.write_all(&buf[..n]).is_err() {
                    let _ = ev_tx.send(EngineEvent::State(
                        d.id,
                        Status::Error,
                        Some("Could not write the file".into()),
                    ));
                    return;
                }
                downloaded += n as u64;
                window_bytes += n as u64;

                let el = window_start.elapsed().as_secs_f64();
                if el > 0.5 {
                    speed = window_bytes as f64 / el;
                    window_start = Instant::now();
                    window_bytes = 0;
                }
                if last_report.elapsed().as_millis() > 200 {
                    last_report = Instant::now();
                    let _ = ev_tx.send(EngineEvent::Progress(d.id, downloaded, size, speed));
                }
            }
            Err(e) => {
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Demo data (only with --demo, used for screenshots)
// ---------------------------------------------------------------------------

fn sample_downloads() -> Vec<Download> {
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
    let dir: String = downloads_dir().to_string_lossy().into();
    let mk = |id: u64, name: &str, size: u64, done: u64, status: Status, speed: f64, err: Option<&str>| {
        Download {
            id,
            url: format!("https://example.com/files/{}", name),
            file_name: name.into(),
            ext: ext_of(name),
            size,
            downloaded: done,
            status,
            speed_bps: speed,
            save_dir: dir.clone(),
            added: now.clone(),
            error: err.map(|s| s.to_string()),
            source: Source::Http,
            thumb: None,
            extra: Extra::default(),
        }
    };
    const GB: u64 = 1024 * 1024 * 1024;
    const MB: u64 = 1024 * 1024;
    vec![
        mk(1, "ubuntu-24.04-desktop-amd64.iso", 5 * GB, 3 * GB + 100 * MB, Status::Downloading, 28.4 * MB as f64, None),
        mk(2, "product-launch-assets.zip", 1600 * MB, 612 * MB, Status::Downloading, 11.2 * MB as f64, None),
        mk(3, "annual-report-2025.txt", 25 * MB, 23 * MB, Status::Downloading, 3.2 * MB as f64, None),
        mk(4, "study-playlist-export.zip", 84 * MB, 38 * MB, Status::Paused, 0.0, None),
        mk(5, "design-assets.zip", 1200 * MB, 0, Status::Queued, 0.0, None),
        mk(6, "backup-photos.zip", 1200 * MB, 640 * MB, Status::Error, 0.0, Some("Server stopped responding")),
        mk(7, "setup-vscode.exe", 98 * MB, 98 * MB, Status::Complete, 0.0, None),
        mk(8, "invoice-october.txt", 412 * 1024, 412 * 1024, Status::Complete, 0.0, None),
        {
            let mut t = mk(9, "Sintel (2010)", 129 * MB, 41 * MB, Status::Downloading, 3.4 * MB as f64, None);
            t.source = Source::Torrent;
            t.url = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel".into();
            t
        },
    ]
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

fn hex(h: u32) -> Color32 {
    Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)
}

#[derive(Clone, Copy)]
struct Pal {
    bg: Color32,
    surface: Color32,
    border: Color32,
    border_strong: Color32,
    text: Color32,
    sub: Color32,
    muted: Color32,
    accent: Color32,
    on_accent: Color32,
    progress: Color32,
    accent_hover: Color32,
    accent_ink: Color32,
    accent_soft: Color32,
    track: Color32,
    tile: Color32,
    ok: Color32,
    ok_soft: Color32,
    bad: Color32,
    paused: Color32,
    scrim: Color32,
    dark: bool,
}

impl Pal {
    // Orexa brand palette: Navy #0B1220, Teal #14B8A6, Aqua #22D3EE,
    // Light Mint #CFFAFE, Gray #E2E8F0. Navy text goes on Aqua and Mint.
    fn light() -> Self {
        Self {
            bg: hex(0xF4F7FA),
            surface: hex(0xFFFFFF),
            border: hex(0xE2E8F0),
            border_strong: hex(0xCBD5E1),
            text: hex(0x0B1220),
            sub: hex(0x475569),
            muted: hex(0x60717E),
            accent: hex(0x22D3EE),
            on_accent: hex(0x0B1220),
            accent_hover: hex(0x1FC0DA),
            accent_ink: hex(0x0B1220),
            accent_soft: hex(0xCFFAFE),
            track: hex(0xE2E8F0),
            tile: hex(0xF1F5F9),
            progress: hex(0x14B8A6),
            ok: hex(0x067647),
            ok_soft: hex(0xECFDF3),
            bad: hex(0xB42318),
            paused: hex(0x94A3B8),
            scrim: Color32::from_rgba_unmultiplied(11, 18, 32, 150),
            dark: false,
        }
    }

    fn dark() -> Self {
        Self {
            bg: hex(0x0B1220),
            surface: hex(0x111B2E),
            border: hex(0x1F2C44),
            border_strong: hex(0x33435F),
            text: hex(0xF1F5F9),
            sub: hex(0xCBD5E1),
            muted: hex(0x94A3B8),
            accent: hex(0x22D3EE),
            on_accent: hex(0x0B1220),
            accent_hover: hex(0x5FE3F5),
            accent_ink: hex(0x22D3EE),
            accent_soft: hex(0x0F2C3F),
            track: hex(0x1F2C44),
            tile: hex(0x17243A),
            progress: hex(0x22D3EE),
            ok: hex(0x6CE9A6),
            ok_soft: hex(0x0F2B20),
            bad: hex(0xF97066),
            paused: hex(0x64748B),
            scrim: Color32::from_rgba_unmultiplied(3, 7, 14, 200),
            dark: true,
        }
    }
}

fn apply_visuals(ctx: &egui::Context, p: &Pal) {
    let mut v = if p.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.bg;
    v.window_fill = p.surface;
    v.extreme_bg_color = p.surface;
    v.faint_bg_color = p.tile;
    v.override_text_color = None;
    v.selection.bg_fill = p.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.5, p.accent);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.border);
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.border_strong);
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.muted);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.rounding = 10.0.into();
    }
    ctx.set_visuals(v);
}

// Orexa brand typeface: Plus Jakarta Sans, bundled with the app.
static FONT_REGULAR: &[u8] = include_bytes!("../assets/PlusJakartaSans-Regular.ttf");
static FONT_MEDIUM: &[u8] = include_bytes!("../assets/PlusJakartaSans-Medium.ttf");
static FONT_SEMIBOLD: &[u8] = include_bytes!("../assets/PlusJakartaSans-SemiBold.ttf");
static FONT_BOLD: &[u8] = include_bytes!("../assets/PlusJakartaSans-Bold.ttf");
static SYMBOL_SVG: &[u8] = include_bytes!("../assets/orexa-symbol.svg");

/// The Orexa symbol drawn at exactly the size it is shown, in real screen pixels, so it
/// stays sharp on every display scale (an SVG stretched from a small image looks blurry).
fn logo_texture(ctx: &egui::Context, logical_width: f32) -> Option<egui::TextureHandle> {
    let w = ((logical_width * ctx.pixels_per_point()).round() as u32).clamp(8, 1024);
    let key = egui::Id::new(("orexa-logo", w));
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(key)) {
        return Some(t);
    }
    use resvg::usvg::TreeParsing;
    let tree = resvg::usvg::Tree::from_data(SYMBOL_SVG, &resvg::usvg::Options::default()).ok()?;
    let (sw, sh) = (tree.size.width(), tree.size.height());
    let h = ((w as f32) * sh / sw).round().max(1.0) as u32;
    let mut pix = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::Tree::from_usvg(&tree).render(
        resvg::tiny_skia::Transform::from_scale(w as f32 / sw, h as f32 / sh),
        &mut pix.as_mut(),
    );
    let img = egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], pix.data());
    let tex = ctx.load_texture(format!("orexa-logo-{w}"), img, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(key, tex.clone()));
    Some(tex)
}

fn paint_logo(ui: &Ui, rect: Rect) {
    if let Some(t) = logo_texture(ui.ctx(), rect.width()) {
        ui.painter().image(t.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    }
}
static APP_ICON_PNG: &[u8] = include_bytes!("../assets/orexa-app-icon.png");

fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let faces: [(&str, &[u8]); 4] = [
        ("pjs_regular", FONT_REGULAR),
        ("pjs_medium", FONT_MEDIUM),
        ("pjs_semibold", FONT_SEMIBOLD),
        ("pjs_bold", FONT_BOLD),
    ];
    for (key, bytes) in faces {
        fonts
            .font_data
            .insert(key.into(), egui::FontData::from_static(bytes).into());
    }
    // The brand font has no Arabic letters; use the Windows fonts that do.
    let win = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let mut arabic_regular: Vec<String> = Vec::new();
    let mut arabic_bold: Vec<String> = Vec::new();
    for (key, file, bold) in [
        ("win_tahoma", "tahoma.ttf", false),
        ("win_tahoma_bold", "tahomabd.ttf", true),
        ("win_segoe", "segoeui.ttf", false),
        ("win_arial", "arial.ttf", false),
    ] {
        if let Ok(bytes) = fs::read(win.join(file)) {
            fonts.font_data.insert(key.into(), egui::FontData::from_owned(bytes).into());
            if bold {
                arabic_bold.push(key.to_string());
            } else {
                arabic_regular.push(key.to_string());
            }
        }
    }
    // Keep egui's default fonts as a fallback for glyphs the brand font lacks.
    let fallback = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let family = |first: &str, bold: bool| {
        let mut v = vec![first.to_string()];
        if bold {
            v.extend(arabic_bold.iter().cloned());
        }
        v.extend(arabic_regular.iter().cloned());
        v.extend(fallback.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, family("pjs_regular", false));
    fonts.families.insert(FontFamily::Name("medium".into()), family("pjs_medium", false));
    fonts.families.insert(FontFamily::Name("semibold".into()), family("pjs_semibold", true));
    fonts.families.insert(FontFamily::Name("bold".into()), family("pjs_bold", true));
    ctx.set_fonts(fonts);
}

fn load_app_icon(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(APP_ICON_PNG).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(ctx.load_texture(
        "orexa-app-icon",
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw()),
        egui::TextureOptions::LINEAR,
    ))
}

fn window_icon() -> egui::IconData {
    let img = image::load_from_memory(APP_ICON_PNG)
        .map(|i| image::imageops::resize(&i.to_rgba8(), 256, 256, image::imageops::FilterType::Lanczos3))
        .unwrap_or_else(|_| image::RgbaImage::new(1, 1));
    egui::IconData {
        width: img.width(),
        height: img.height(),
        rgba: img.into_raw(),
    }
}

#[allow(dead_code)]
fn fm(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("medium".into()))
}

fn f(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
fn fs(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("semibold".into()))
}
fn fb(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}
fn rt(s: impl Into<String>, font: FontId, color: Color32) -> RichText {
    RichText::new(shape(&s.into())).font(font).color(color)
}

// ---------------------------------------------------------------------------
// Right-to-left text (Arabic, Persian, Hebrew). The text engine draws letters
// one by one, so Arabic must be joined into its connected letter forms and put
// in visual order before it is drawn.
// ---------------------------------------------------------------------------

fn has_rtl(s: &str) -> bool {
    s.chars()
        .any(|c| matches!(c as u32, 0x0590..=0x08FF | 0xFB1D..=0xFDFF | 0xFE70..=0xFEFF))
}

fn reorder_visual(shaped: &str) -> String {
    // The app reads left to right, so keep that base direction: an Arabic name
    // stays in place and its extension stays at the end.
    let info = unicode_bidi::BidiInfo::new(shaped, Some(unicode_bidi::Level::ltr()));
    let mut out = String::with_capacity(shaped.len());
    for para in &info.paragraphs {
        out.push_str(&info.reorder_line(para, para.range.clone()));
    }
    out
}

/// For display: joined letters, ligatures, visual order.
fn shape(s: &str) -> String {
    if !has_rtl(s) {
        return s.to_string();
    }
    reorder_visual(&ar_reshaper::reshape_line(s))
}

/// For text fields: same look, but one output character per input character so
/// the cursor stays in step (no ligatures that merge two letters into one).
fn shape_editable(s: &str) -> String {
    use ar_reshaper::config::{LigaturesFlags, ReshaperConfig};
    if !has_rtl(s) {
        return s.to_string();
    }
    let mut cfg = ReshaperConfig::new(ar_reshaper::Language::Arabic, LigaturesFlags::none());
    cfg.delete_harakat = false;
    cfg.delete_tatweel = false;
    reorder_visual(&cfg.to_reshaper().reshape(s))
}

fn rtl_layouter(font: FontId, color: Color32) -> impl FnMut(&Ui, &str, f32) -> Arc<egui::Galley> {
    move |ui, text, wrap| {
        let job = egui::text::LayoutJob::simple(shape_editable(text), font.clone(), color, wrap);
        ui.fonts(|fo| fo.layout_job(job))
    }
}

// ---------------------------------------------------------------------------
// Icons (Lucide paths drawn as white SVGs and tinted at paint time)
// ---------------------------------------------------------------------------

macro_rules! svg {
    ($body:expr) => {
        concat!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="0 0 24 24" fill="none" stroke="white" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">"#,
            $body,
            "</svg>"
        )
    };
}

fn icon_svg(name: &str) -> &'static str {
    match name {
        "download" => svg!(r#"<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" x2="12" y1="15" y2="3"/>"#),
        "plus" => svg!(r#"<path d="M5 12h14"/><path d="M12 5v14"/>"#),
        "minus" => svg!(r#"<path d="M5 12h14"/>"#),
        "search" => svg!(r#"<circle cx="11" cy="11" r="8"/><path d="m21 21-4.3-4.3"/>"#),
        "pause" => svg!(r#"<rect x="14" y="4" width="4" height="16" rx="1"/><rect x="6" y="4" width="4" height="16" rx="1"/>"#),
        "play" => svg!(r#"<polygon points="6 3 20 12 6 21 6 3"/>"#),
        "folder" => svg!(r#"<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>"#),
        "check" => svg!(r#"<path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><path d="m9 11 3 3L22 4"/>"#),
        "clock" => svg!(r#"<circle cx="12" cy="12" r="10"/><polyline points="12 6 12 12 16 14"/>"#),
        "magnet" => svg!(r#"<path d="m6 15-4-4 6.75-6.77a7.79 7.79 0 0 1 11 11L13 22l-4-4 6.39-6.36a2.14 2.14 0 0 0-3-3L6 15"/><path d="m5 8 4 4"/><path d="m12 15 4 4"/>"#),
        "heart" => svg!(r#"<path fill="white" d="M19 14c1.49-1.46 3-3.21 3-5.5A5.5 5.5 0 0 0 16.5 3c-1.76 0-3 .5-4.5 2-1.5-1.5-2.74-2-4.5-2A5.5 5.5 0 0 0 2 8.5c0 2.3 1.5 4.05 3 5.5l7 7Z"/>"#),
        "help" => svg!(r#"<circle cx="12" cy="12" r="10"/><path d="M9.09 9a3 3 0 0 1 5.83 1c0 2-3 3-3 3"/><path d="M12 17h.01"/>"#),
        "settings" => svg!(r#"<path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z"/><circle cx="12" cy="12" r="3"/>"#),
        "doc" => svg!(r#"<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/>"#),
        "film" => svg!(r#"<rect width="18" height="18" x="3" y="3" rx="2"/><path d="M7 3v18"/><path d="M3 7.5h4"/><path d="M3 12h18"/><path d="M3 16.5h4"/><path d="M17 3v18"/><path d="M17 7.5h4"/><path d="M17 16.5h4"/>"#),
        "music" => svg!(r#"<path d="M9 18V5l12-2v13"/><circle cx="6" cy="18" r="3"/><circle cx="18" cy="16" r="3"/>"#),
        "archive" => svg!(r#"<rect width="20" height="5" x="2" y="3" rx="1"/><path d="M4 8v11a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8"/><path d="M10 12h4"/>"#),
        "box" => svg!(r#"<path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16Z"/><path d="m3.3 7 8.7 5 8.7-5"/><path d="M12 22V12"/>"#),
        "trash" => svg!(r#"<path d="M3 6h18"/><path d="M19 6v14c0 1-1 2-2 2H7c-1 0-2-1-2-2V6"/><path d="M8 6V4c0-1 1-2 2-2h4c1 0 2 1 2 2v2"/>"#),
        "list" => svg!(r#"<line x1="8" x2="21" y1="6" y2="6"/><line x1="8" x2="21" y1="12" y2="12"/><line x1="8" x2="21" y1="18" y2="18"/><line x1="3" x2="3.01" y1="6" y2="6"/><line x1="3" x2="3.01" y1="12" y2="12"/><line x1="3" x2="3.01" y1="18" y2="18"/>"#),
        "x" => svg!(r#"<path d="M18 6 6 18"/><path d="m6 6 12 12"/>"#),
        "link" => svg!(r#"<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>"#),
        "open" => svg!(r#"<path d="M15 3h6v6"/><path d="M10 14 21 3"/><path d="M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6"/>"#),
        "retry" => svg!(r#"<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>"#),
        "alert" => svg!(r#"<circle cx="12" cy="12" r="10"/><line x1="12" x2="12" y1="8" y2="12"/><line x1="12" x2="12.01" y1="16" y2="16"/>"#),
        "square" => svg!(r#"<rect width="16" height="16" x="4" y="4" rx="2"/>"#),
        "copy" => svg!(r#"<rect width="13" height="13" x="9" y="9" rx="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/>"#),
        _ => svg!(r#"<circle cx="12" cy="12" r="10"/>"#),
    }
}

fn paint_icon(ui: &Ui, rect: Rect, name: &str, color: Color32) {
    egui::Image::from_bytes(format!("bytes://icon_{}.svg", name), icon_svg(name).as_bytes())
        .tint(color)
        .paint_at(ui, rect);
}

/// The About / License panel on the Settings page.
fn about_panel(ui: &mut Ui, p: &Pal, width: f32) {
    egui::Frame::none()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .rounding(16.0)
        .inner_margin(Margin::same(22.0))
        .show(ui, |ui| {
            ui.set_width(width - 44.0);
            ui.spacing_mut().item_spacing.y = 6.0;

            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(40.0, 37.0), Sense::hover());
                paint_logo(ui, r);
                ui.add_space(6.0);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    ui.label(rt("Orexa Downloader", fb(18.0), p.text));
                    ui.label(rt(format!("Version {}", env!("CARGO_PKG_VERSION")), f(13.0), p.muted));
                });
            });
            ui.add_space(10.0);
            ui.label(rt("About", fs(14.0), p.text));
            ui.label(rt(
                "A fast, simple download manager. Save files, web videos, YouTube and torrents, with one click from your browser.",
                f(13.0),
                p.sub,
            ));

            ui.add_space(12.0);
            ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, p.border));
            ui.add_space(12.0);
            ui.label(rt("License", fs(14.0), p.text));
            egui::Frame::none()
                .fill(p.accent_soft)
                .rounding(8.0)
                .inner_margin(Margin::symmetric(10.0, 4.0))
                .show(ui, |ui| {
                    ui.label(rt("Free to use", fs(13.0), p.accent_ink));
                });
            ui.label(rt("Orexa Downloader is free to use.", f(13.0), p.sub));

            ui.add_space(12.0);
            ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, p.border));
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                ui.label(rt("Made with", f(13.0), p.sub));
                let (r, _) = ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
                paint_icon(ui, r, "heart", Color32::from_rgb(0xE5, 0x48, 0x4D));
            });
            ui.label(rt("by Orexa Software from Egypt", f(13.0), p.sub));
        });
}

// ---------------------------------------------------------------------------
// System file icons (the same icons Explorer shows)
// ---------------------------------------------------------------------------

#[derive(Default)]
struct IconCache {
    map: HashMap<String, Option<egui::TextureHandle>>,
}

impl IconCache {
    fn ensure(&mut self, ctx: &egui::Context, key: &str, path: &std::path::Path) {
        if self.map.contains_key(key) {
            return;
        }
        let tex = file_icon_provider::get_file_icon(path, 64).ok().and_then(|ic| {
            if ic.width == 0 || ic.height == 0 {
                return None;
            }
            let img = egui::ColorImage::from_rgba_unmultiplied(
                [ic.width as usize, ic.height as usize],
                &ic.pixels,
            );
            Some(ctx.load_texture(format!("icon:{}", key), img, egui::TextureOptions::LINEAR))
        });
        self.map.insert(key.to_string(), tex);
    }

    fn ensure_rgba(&mut self, ctx: &egui::Context, key: &str, w: u32, h: u32, rgba: &[u8]) {
        if self.map.contains_key(key) {
            return;
        }
        let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba);
        let tex = ctx.load_texture(format!("icon:{}", key), img, egui::TextureOptions::LINEAR);
        self.map.insert(key.to_string(), Some(tex));
    }

    fn get(&self, key: &str) -> Option<&egui::TextureHandle> {
        self.map.get(key).and_then(|t| t.as_ref())
    }
}

/// Icon for a file type, before any file exists: ask the shell about an empty
/// placeholder that carries only the extension.
fn type_icon_key(ext: &str) -> (String, PathBuf) {
    let ext = if ext.is_empty() { "file" } else { ext };
    let dir = std::env::temp_dir().join("orexa_icons");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join(format!("type.{}", ext));
    if !path.exists() {
        let _ = fs::write(&path, b"");
    }
    (format!("type:{}", ext), path)
}

/// A finished download uses its own icon (the real app icon for an .exe).
fn download_icon_key(d: &Download) -> (String, PathBuf) {
    if d.status == Status::Complete {
        let p = d.path();
        if p.exists() {
            return (p.to_string_lossy().into_owned(), p);
        }
    }
    type_icon_key(&d.ext)
}

// ---------------------------------------------------------------------------
// Small widgets
// ---------------------------------------------------------------------------

fn text_w(ui: &Ui, s: &str, font: FontId) -> f32 {
    ui.fonts(|fo| fo.layout_no_wrap(shape(s), font, Color32::WHITE).size().x)
}

fn trunc(ui: &Ui, s: &str, font: FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(shape(s), font, color, max_w.max(10.0));
    job.wrap.max_rows = 1;
    job.wrap.break_anywhere = true;
    job.wrap.overflow_character = Some('…');
    ui.fonts(|fo| fo.layout_job(job))
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Primary,
    Secondary,
}

fn button(ui: &mut Ui, p: &Pal, label: &str, icon: Option<&str>, kind: Kind, h: f32, enabled: bool) -> Response {
    let font = fs(14.0);
    let tw = text_w(ui, label, font.clone());
    let iw = if icon.is_some() { 26.0 } else { 0.0 };
    let w = tw + iw + 36.0;
    let (rect, resp) = ui.allocate_exact_size(
        vec2(w, h),
        if enabled { Sense::click() } else { Sense::hover() },
    );
    let hov = resp.hovered() && enabled;
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let (bg, fg, stroke) = match (kind, enabled) {
        (Kind::Primary, true) => (if hov { p.accent_hover } else { p.accent }, p.on_accent, Stroke::NONE),
        (Kind::Primary, false) => (p.track, p.muted, Stroke::NONE),
        (Kind::Secondary, true) => (if hov { p.tile } else { p.surface }, p.text, Stroke::new(1.0, p.border_strong)),
        (Kind::Secondary, false) => (p.surface, p.muted, Stroke::new(1.0, p.border)),
    };
    let pt = ui.painter();
    pt.rect_filled(rect, 10.0, bg);
    if stroke != Stroke::NONE {
        pt.rect_stroke(rect, 10.0, stroke);
    }
    let mut x = rect.left() + 18.0;
    if let Some(name) = icon {
        paint_icon(ui, Rect::from_min_size(pos2(x, rect.center().y - 9.0), vec2(18.0, 18.0)), name, fg);
        x += 26.0;
    }
    ui.painter().text(pos2(x, rect.center().y), Align2::LEFT_CENTER, label, font, fg);
    resp
}

fn icon_button_at(ui: &Ui, p: &Pal, rect: Rect, icon: &str, id: egui::Id, tip: &str) -> bool {
    let r = ui.interact(rect, id, Sense::click());
    if r.hovered() {
        ui.painter().rect_filled(rect, 8.0, p.tile);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    paint_icon(ui, Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, p.sub);
    r.on_hover_text(tip).clicked()
}

fn text_button_at(ui: &Ui, p: &Pal, rect: Rect, label: &str, icon: &str, id: egui::Id) -> bool {
    let r = ui.interact(rect, id, Sense::click());
    let hov = r.hovered();
    if hov {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    ui.painter().rect_filled(rect, 8.0, if hov { p.tile } else { p.surface });
    ui.painter().rect_stroke(rect, 8.0, Stroke::new(1.0, p.border_strong));
    let tw = text_w(ui, label, fs(14.0));
    let total = 16.0 + 6.0 + tw;
    let x0 = rect.center().x - total / 2.0;
    paint_icon(ui, Rect::from_min_size(pos2(x0, rect.center().y - 8.0), vec2(16.0, 16.0)), icon, p.sub);
    ui.painter().text(pos2(x0 + 22.0, rect.center().y), Align2::LEFT_CENTER, label, fs(14.0), p.text);
    r.clicked()
}

fn nav_item(ui: &mut Ui, p: &Pal, icon: &str, label: &str, count: Option<usize>, selected: bool) -> bool {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::click());
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let pt = ui.painter();
    if selected {
        pt.rect_filled(rect, 8.0, p.accent_soft);
    } else if resp.hovered() {
        pt.rect_filled(rect, 8.0, p.tile);
    }
    let fg = if selected { p.accent_ink } else { p.sub };
    paint_icon(ui, Rect::from_min_size(pos2(rect.left() + 12.0, rect.center().y - 10.0), vec2(20.0, 20.0)), icon, fg);
    ui.painter().text(
        pos2(rect.left() + 44.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        if selected { fs(14.0) } else { f(14.0) },
        if selected { p.accent_ink } else { p.text },
    );
    if let Some(c) = count {
        ui.painter().text(
            pos2(rect.right() - 12.0, rect.center().y),
            Align2::RIGHT_CENTER,
            c.to_string(),
            f(13.0),
            if selected { p.accent_ink } else { p.muted },
        );
    }
    resp.clicked()
}

fn toggle(ui: &mut Ui, p: &Pal, on: &mut bool) -> Response {
    let (rect, mut r) = ui.allocate_exact_size(vec2(44.0, 24.0), Sense::click());
    if r.clicked() {
        *on = !*on;
        r.mark_changed();
    }
    ui.ctx().set_cursor_icon(if r.hovered() { egui::CursorIcon::PointingHand } else { egui::CursorIcon::Default });
    let t = ui.ctx().animate_bool(r.id, *on);
    ui.painter().rect_filled(rect, 12.0, p.border_strong.lerp_to_gamma(p.accent, t));
    let cx = egui::lerp((rect.left() + 12.0)..=(rect.right() - 12.0), t);
    ui.painter().circle_filled(pos2(cx, rect.center().y), 10.0, Color32::WHITE);
    r
}

/// A centered dialog. It is placed from the window size and a typical height, snapped to
/// whole screen pixels. (Centering on its own measured height made it jump by a pixel
/// back and forth whenever the centre fell exactly between two pixels.)
fn modal(ctx: &egui::Context, p: &Pal, id: &str, width: f32, typical_height: f32, add: impl FnOnce(&mut Ui)) {
    egui::Area::new(egui::Id::new((id, "scrim")))
        .order(Order::Middle)
        .fixed_pos(egui::Pos2::ZERO)
        .show(ctx, |ui| {
            let r = ctx.screen_rect();
            ui.allocate_rect(r, Sense::click_and_drag());
            ui.painter().rect_filled(r, 0.0, p.scrim);
        });
    let ppp = ctx.pixels_per_point();
    let snap = |v: f32| (v * ppp).floor() / ppp;
    let screen = ctx.screen_rect();
    let full_w = width + 56.0; // the dialog's inner margin on both sides
    let pos = pos2(
        snap(((screen.width() - full_w) / 2.0).max(8.0)),
        snap(((screen.height() - typical_height) / 2.0).max(24.0)),
    );
    let dialog = egui::Area::new(egui::Id::new((id, "dialog")))
        .order(Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::none()
                .fill(p.surface)
                .rounding(20.0)
                .stroke(Stroke::new(1.0, p.border))
                .shadow(egui::Shadow {
                    offset: vec2(0.0, 12.0),
                    blur: 40.0,
                    spread: 0.0,
                    color: Color32::from_black_alpha(80),
                })
                .inner_margin(Margin::same(28.0))
                .show(ui, |ui| {
                    ui.set_width(width);
                    add(ui);
                });
        });
    // Developer aid: log the dialog's rectangle every frame to spot layout jitter.
    if let Some(path) = std::env::var_os("OREXA_DEBUG_LAYOUT") {
        let r = dialog.response.rect;
        let line = format!("{} x={:.1} y={:.1} w={:.1} h={:.1}\n", id, r.min.x, r.min.y, r.width(), r.height());
        let _ = fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(line.as_bytes()));
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

enum Act {
    Select(u64),
    Start(u64),
    Pause(u64),
    Open(u64),
    Folder(u64),
    Remove(u64),
    AddDialog,
}

fn draw_row(ui: &mut Ui, p: &Pal, icons: &IconCache, d: &Download, selected: bool, last: bool, acts: &mut Vec<Act>) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(vec2(w, 84.0), Sense::click());
    if resp.clicked() {
        acts.push(Act::Select(d.id));
    }
    let pt = ui.painter().clone();
    if selected {
        pt.rect_filled(rect.shrink2(vec2(6.0, 4.0)), 10.0, p.accent_soft);
    }
    if !last {
        pt.hline(rect.x_range(), rect.bottom() - 0.5, Stroke::new(1.0, p.border));
    }
    let cy = rect.center().y;

    // File type tile
    let tile = Rect::from_min_size(pos2(rect.left() + 20.0, cy - 22.0), vec2(44.0, 44.0));
    pt.rect_filled(tile, 10.0, p.tile);
    let (ikey, _) = download_icon_key(d);
    if let Some(tex) = icons.get(&format!("thumb:{}", d.id)) {
        // YouTube thumbnail, cropped to a square from the middle.
        let a = tex.size_vec2().x / tex.size_vec2().y;
        let u0 = ((1.0 - 1.0 / a) / 2.0).clamp(0.0, 0.49);
        pt.image(
            tex.id(),
            tile.shrink(2.0),
            Rect::from_min_max(pos2(u0, 0.0), pos2(1.0 - u0, 1.0)),
            Color32::WHITE,
        );
    } else if d.is_torrent() {
        // Torrents have their own magnet icon.
        pt.rect_filled(tile, 10.0, p.accent_soft);
        paint_icon(ui, Rect::from_center_size(tile.center(), vec2(24.0, 24.0)), "magnet", p.accent_ink);
    } else if let Some(tex) = icons.get(&ikey) {
        pt.image(
            tex.id(),
            Rect::from_center_size(tile.center(), vec2(28.0, 28.0)),
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
    } else {
        let icon = ext_cat(&d.ext).map(|c| c.icon()).unwrap_or("doc");
        paint_icon(ui, Rect::from_center_size(tile.center(), vec2(22.0, 22.0)), icon, p.sub);
    }

    // Geometry from the right edge
    let big = matches!(d.status, Status::Complete | Status::Error);
    let rr = rect.right() - 20.0;
    let trash = Rect::from_min_size(pos2(rr - 36.0, cy - 18.0), vec2(36.0, 36.0));
    let folder = trash.translate(vec2(-40.0, 0.0));
    // Every row reserves the same action slot so columns line up.
    let slot_left = folder.left() - 8.0 - 92.0;
    let main = if big {
        Rect::from_min_size(pos2(slot_left, cy - 18.0), vec2(92.0, 36.0))
    } else {
        folder.translate(vec2(-40.0, 0.0))
    };
    let col_w = 170.0;
    let col_x = slot_left - 16.0 - col_w;
    let mid_x = tile.right() + 16.0;
    let mid_w = (col_x - 16.0 - mid_x).max(120.0);

    // Name, progress, meta
    let cat_name = ext_cat(&d.ext).map(|c| c.label()).unwrap_or("Other");
    let time = d.added.split(' ').nth(1).unwrap_or("");
    let meta = if d.status == Status::Complete {
        format!("{}, {}, {}", fmt_size(d.size), cat_name, time)
    } else if d.size == 0 {
        "Size unknown".to_string()
    } else {
        let got = if d.downloaded == 0 { "0 MB".to_string() } else { fmt_size(d.downloaded) };
        format!("{} of {}, {:.0}%", got, fmt_size(d.size), d.progress() * 100.0)
    };
    let show_bar = d.status != Status::Complete;
    let block_h = if show_bar { 17.0 + 8.0 + 6.0 + 8.0 + 16.0 } else { 17.0 + 4.0 + 16.0 };
    let top = cy - block_h / 2.0;
    let g = trunc(ui, &d.file_name, fs(14.0), p.text, mid_w);
    pt.galley(pos2(mid_x, top), g, p.text);
    let mut y = top + 17.0 + if show_bar { 8.0 } else { 4.0 };
    if show_bar {
        let track = Rect::from_min_size(pos2(mid_x, y), vec2(mid_w, 6.0));
        pt.rect_filled(track, 3.0, p.track);
        let prog = d.progress();
        if prog > 0.0 {
            let fill = match d.status {
                Status::Paused => p.paused,
                Status::Error => p.bad,
                _ => p.progress,
            };
            pt.rect_filled(
                Rect::from_min_size(track.min, vec2((mid_w * prog).max(6.0), 6.0)),
                3.0,
                fill,
            );
        }
        y += 6.0 + 8.0;
    }
    let g = trunc(ui, &meta, f(13.0), p.muted, mid_w);
    pt.galley(pos2(mid_x, y), g, p.muted);

    // Status column
    let r1y = cy - 19.0;
    let (r1, r1c, r2): (String, Color32, String) = match d.status {
        Status::Downloading => (
            fmt_speed(d.speed_bps),
            p.text,
            d.eta_secs().map(|s| format!("{} left", fmt_duration(s))).unwrap_or_else(|| "Starting".into()),
        ),
        Status::Paused => ("Paused".into(), p.text, "Resume any time".into()),
        Status::Queued => ("Queued".into(), p.text, "Waiting for a free slot".into()),
        Status::Error => ("Failed".into(), p.bad, d.error.clone().unwrap_or_else(|| "Something went wrong".into())),
        Status::Complete => (String::new(), p.text, String::new()),
    };
    if d.status == Status::Complete {
        let pill = Rect::from_min_size(pos2(col_x, cy - 12.0), vec2(112.0, 24.0));
        pt.rect_filled(pill, 12.0, p.ok_soft);
        paint_icon(ui, Rect::from_min_size(pos2(pill.left() + 10.0, cy - 7.0), vec2(14.0, 14.0)), "check", p.ok);
        pt.text(pos2(pill.left() + 30.0, cy), Align2::LEFT_CENTER, "Completed", fs(12.0), p.ok);
    } else {
        let g = trunc(ui, &r1, fs(14.0), r1c, col_w);
        pt.galley(pos2(col_x, r1y), g, r1c);
        let g = trunc(ui, &r2, f(13.0), p.muted, col_w);
        pt.galley(pos2(col_x, r1y + 19.0), g, p.muted);
    }

    // Actions
    let id = egui::Id::new(("row", d.id));
    match d.status {
        Status::Complete => {
            if text_button_at(ui, p, main, "Open", "open", id.with("open")) {
                acts.push(Act::Open(d.id));
            }
        }
        Status::Error => {
            if text_button_at(ui, p, main, "Retry", "retry", id.with("retry")) {
                acts.push(Act::Start(d.id));
            }
        }
        Status::Downloading => {
            if icon_button_at(ui, p, main, "pause", id.with("pause"), "Pause") {
                acts.push(Act::Pause(d.id));
            }
        }
        Status::Paused | Status::Queued => {
            let tip = if d.status == Status::Paused { "Resume" } else { "Start now" };
            if icon_button_at(ui, p, main, "play", id.with("play"), tip) {
                acts.push(Act::Start(d.id));
            }
        }
    }
    if icon_button_at(ui, p, folder, "folder", id.with("folder"), "Show in folder") {
        acts.push(Act::Folder(d.id));
    }
    if icon_button_at(ui, p, trash, "trash", id.with("trash"), "Remove from list") {
        acts.push(Act::Remove(d.id));
    }
}

fn draw_group(ui: &mut Ui, p: &Pal, icons: &IconCache, title: &str, rows: &[&Download], selected: Option<u64>, acts: &mut Vec<Act>) {
    ui.horizontal(|ui| {
        ui.label(rt(title, fs(14.0), p.text));
        ui.label(rt(rows.len().to_string(), f(13.0), p.muted));
    });
    ui.add_space(8.0);
    egui::Frame::none()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .rounding(14.0)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, d) in rows.iter().enumerate() {
                draw_row(ui, p, icons, d, selected == Some(d.id), i + 1 == rows.len(), acts);
            }
        });
    ui.add_space(24.0);
}

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Link probing: find the size and real file name before the download starts
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq)]
enum ProbeState {
    Idle,
    Checking,
    Ready(u64, Option<String>),
    YtReady(Box<YtInfo>),
    /// A YouTube link while yt-dlp is not installed yet.
    NeedsSetup,
    Failed(String),
}

enum ProbeOk {
    Http(u64, Option<String>),
    Yt(Box<YtInfo>),
}

type ProbeResult = (String, Result<ProbeOk, String>);

#[derive(Clone, PartialEq)]
enum ToolStatus {
    Missing,
    Installing(f32),
    Ready,
    Failed(String),
}

struct Tools {
    ytdlp: ToolStatus,
    ffmpeg: ToolStatus,
}

impl Tools {
    fn detect() -> Self {
        let st = |found: bool| if found { ToolStatus::Ready } else { ToolStatus::Missing };
        Self {
            ytdlp: st(youtube::ytdlp_path().is_some()),
            ffmpeg: st(youtube::ffmpeg_path().is_some()),
        }
    }
    fn get(&self, t: Tool) -> &ToolStatus {
        match t {
            Tool::YtDlp => &self.ytdlp,
            Tool::Ffmpeg => &self.ffmpeg,
        }
    }
    fn set(&mut self, t: Tool, s: ToolStatus) {
        match t {
            Tool::YtDlp => self.ytdlp = s,
            Tool::Ffmpeg => self.ffmpeg = s,
        }
    }
}

fn name_from_disposition(v: &str) -> Option<String> {
    let lower = v.to_lowercase();
    let i = lower.find("filename=")?;
    let rest = v[i + 9..].trim();
    let name = rest.split(';').next()?.trim().trim_matches('"').trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

fn probe_link(url: &str) -> Result<(u64, Option<String>), String> {
    use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, RANGE};
    let client = reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .timeout(Duration::from_secs(12))
        .build()
        .map_err(|e| e.to_string())?;

    let len_of = |r: &reqwest::blocking::Response| -> Option<u64> {
        r.headers()
            .get(CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
    };
    let name_of = |r: &reqwest::blocking::Response| -> Option<String> {
        if let Some(n) = r
            .headers()
            .get(CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .and_then(name_from_disposition)
        {
            return Some(n);
        }
        let n = file_name_from_url(r.url().as_str());
        if n != "index.html" && n.contains('.') {
            Some(n)
        } else {
            None
        }
    };

    // First choice: a HEAD request.
    if let Ok(r) = client.head(url).send() {
        if r.status().is_success() {
            if let Some(len) = len_of(&r).filter(|l| *l > 0) {
                return Ok((len, name_of(&r)));
            }
        }
    }
    // Fallback: ask for one byte and read the total from Content-Range.
    let r = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(format!("Server answered {}", r.status()));
    }
    let total = r
        .headers()
        .get(CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit('/').next())
        .and_then(|v| v.parse::<u64>().ok())
        .or_else(|| if r.status().as_u16() == 200 { len_of(&r) } else { None })
        .unwrap_or(0);
    Ok((total, name_of(&r)))
}

#[derive(Clone, Copy, PartialEq)]
enum Page {
    Downloads,
    Settings,
}

struct NewDownloadDialog {
    open: bool,
    url: String,
    save_dir: String,
    file_name: String,
    start_now: bool,
    focus: bool,
    probe: ProbeState,
    last_edit: Option<Instant>,
    yt_sel: usize,
    extra: Extra,
}

impl Default for NewDownloadDialog {
    fn default() -> Self {
        Self {
            open: false,
            url: String::new(),
            save_dir: downloads_dir().to_string_lossy().into(),
            file_name: String::new(),
            start_now: true,
            focus: false,
            probe: ProbeState::Idle,
            last_edit: None,
            yt_sel: 0,
            extra: Extra::default(),
        }
    }
}

struct App {
    downloads: Vec<Download>,
    engine: Engine,
    ev_rx: Receiver<EngineEvent>,
    probe_tx: Sender<ProbeResult>,
    probe_rx: Receiver<ProbeResult>,
    tools: Tools,
    /// Set up yt-dlp and ffmpeg by itself, without any click.
    auto_provision: bool,
    /// A quiet yt-dlp refresh is running; the working copy stays in use meanwhile.
    refreshing_ytdlp: bool,
    setup_tx: Sender<SetupEvent>,
    setup_rx: Receiver<SetupEvent>,
    icons: IconCache,
    app_icon: Option<egui::TextureHandle>,
    next_id: u64,
    page: Page,
    cat: Category,
    selected_row: Option<u64>,
    search: String,
    dialog: NewDownloadDialog,
    confirm_delete: Option<u64>,
    /// The "How to add the Chrome extension" window, and which browser it explains.
    help_open: bool,
    help_browser: usize,
    dark: bool,
    themed: Option<bool>,
    default_dir: String,
    max_parallel: usize,
    speed_hist: Vec<f32>,
    last_sample: Instant,
    demo: bool,
    demo_phase: f32,
    screenshot_path: Option<String>,
    frames: u32,
    shot_frame: u32,
    auto_add: bool,
    /// Remember downloads and settings between runs.
    persist: bool,
    last_save: Instant,
    last_json: String,
    win: Option<state::SavedWindow>,
    /// Windows notification when a download finishes.
    notify_done: bool,
    /// Start in the tray when Windows starts (on by default).
    autostart: bool,
    tray_hint_shown: bool,
    _tray: Option<tray_icon::TrayIcon>,
    /// Links sent by the browser: ask first (show the Add dialog) or start right away.
    browser_ask: bool,
    thumb_tx: Sender<(u64, Thumb)>,
    thumb_rx: Receiver<(u64, Thumb)>,
    last_inbox: Instant,
    recent_browser: Vec<(String, Instant)>,
    /// Link that was filled in from the clipboard, and one the user closed without using.
    prefilled: Option<String>,
    dismissed_clip: Option<String>,
    // Developer options.
    heartbeat: Option<String>,
    hide_after: Option<u64>,
    started: Instant,
    last_hb: Instant,
    background_pending: bool,
}

struct Options {
    screenshot: Option<String>,
    demo: bool,
    dark: bool,
    open: Option<String>,
    cat: Option<String>,
    url: Option<String>,
    // Developer options for testing.
    setup: Vec<Tool>,
    no_auto_setup: bool,
    auto_add: bool,
    wait_frames: u32,
    persist: bool,
    heartbeat: Option<String>,
    hide_after: Option<u64>,
    /// Started by the browser connector: go straight to the tray.
    background: bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, o: Options) -> Self {
        egui_extras::install_image_loaders(&cc.egui_ctx);
        // Developer aid: try other screen scales (OREXA_SCALE=1.5).
        if let Some(v) = std::env::var("OREXA_SCALE").ok().and_then(|v| v.parse::<f32>().ok()) {
            cc.egui_ctx.set_pixels_per_point(v);
        }
        setup_fonts(&cc.egui_ctx);
        let (engine, ev_rx) = Engine::new();
        let (probe_tx, probe_rx) = channel::<ProbeResult>();
        let (setup_tx, setup_rx) = channel::<SetupEvent>();
        let (thumb_tx, thumb_rx) = channel::<(u64, Thumb)>();
        let mut app = Self {
            downloads: if o.demo { sample_downloads() } else { Vec::new() },
            engine,
            ev_rx,
            probe_tx,
            probe_rx,
            tools: Tools::detect(),
            auto_provision: o.screenshot.is_none() && !o.no_auto_setup,
            refreshing_ytdlp: false,
            setup_tx,
            setup_rx,
            icons: IconCache::default(),
            app_icon: load_app_icon(&cc.egui_ctx),
            next_id: 100,
            page: Page::Downloads,
            cat: Category::All,
            selected_row: None,
            search: String::new(),
            dialog: NewDownloadDialog::default(),
            confirm_delete: None,
            help_open: false,
            help_browser: 0,
            dark: o.dark,
            themed: None,
            default_dir: downloads_dir().to_string_lossy().into(),
            max_parallel: 1,
            speed_hist: vec![0.0; 20],
            last_sample: Instant::now(),
            demo: o.demo,
            demo_phase: 0.0,
            screenshot_path: o.screenshot,
            frames: 0,
            shot_frame: o.wait_frames,
            auto_add: o.auto_add,
            persist: o.persist,
            last_save: Instant::now(),
            last_json: String::new(),
            win: None,
            notify_done: true,
            autostart: true,
            tray_hint_shown: false,
            browser_ask: false,
            thumb_tx,
            thumb_rx,
            last_inbox: Instant::now(),
            recent_browser: Vec::new(),
            _tray: None,
            prefilled: None,
            dismissed_clip: None,
            heartbeat: o.heartbeat.clone(),
            hide_after: o.hide_after,
            started: Instant::now(),
            last_hb: Instant::now(),
            background_pending: o.background && o.persist,
        };
        if app.persist {
            app._tray = shell::make_tray();
            shell::register_notifications();
            shell::register_native_host();
            youtube::clean_stale_cookie_copies();
        }
        if app.persist {
            app.load_state();
            // Start in the tray with Windows (what the settings say, on by default).
            shell::set_autostart(app.autostart);
        }
        for t in &o.setup {
            app.start_setup(*t);
        }
        // Keep yt-dlp fresh (YouTube changes often): a quiet refresh once it is a week old.
        if app.auto_provision && app.tools.ytdlp == ToolStatus::Ready {
            let old = youtube::ytdlp_path()
                .and_then(|p| fs::metadata(p).ok())
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .map(|e| e > Duration::from_secs(7 * 24 * 3600))
                .unwrap_or(false);
            if old {
                app.refreshing_ytdlp = true;
                youtube::setup_tool(Tool::YtDlp, app.setup_tx.clone());
            }
        }
        // First launch: quietly fetch what YouTube downloads need.
        if app.auto_provision {
            if app.tools.ytdlp == ToolStatus::Missing {
                app.start_setup(Tool::YtDlp);
            } else if app.tools.ffmpeg == ToolStatus::Missing {
                app.start_setup(Tool::Ffmpeg);
            }
        }
        match o.open.as_deref() {
            Some("add") => {
                app.open_add();
                // Developer shortcut: fill an example link unless one was already picked up
                // from the clipboard.
                if app.dialog.url.is_empty() {
                    app.dialog.url = o
                        .url
                        .clone()
                        .unwrap_or_else(|| "https://releases.example.com/app-v3.2.1-setup.exe".into());
                    app.dialog.file_name = file_name_from_url(&app.dialog.url);
                    app.dialog.last_edit = Some(Instant::now());
                }
            }
            Some("settings") => app.page = Page::Settings,
            Some("help") => app.help_open = true,
            // Previews of the waiting states for a YouTube link (no network).
            Some("yt-checking") | Some("yt-idle") => {
                app.open_add();
                app.tools.ytdlp = ToolStatus::Ready;
                app.dialog.url = "https://www.youtube.com/watch?v=Q2Opf1BCZAk".into();
                app.dialog.probe = if o.open.as_deref() == Some("yt-checking") { ProbeState::Checking } else { ProbeState::Idle };
            }
            // Preview of a YouTube link that YouTube refused to describe (no network).
            Some("yt-fail") => {
                app.open_add();
                app.tools.ytdlp = ToolStatus::Ready;
                app.dialog.url = "https://www.youtube.com/watch?v=Q2Opf1BCZAk".into();
                app.dialog.probe = ProbeState::Failed(
                    "[youtube] Q2Opf1BCZAk: Sign in to confirm you\u{2019}re not a bot. Use --cookies-from-browser or --cookies for the authentication. See  https://github.com/yt-dlp/yt-dlp/wiki/FAQ#how-do-i-pass-cookies-to-yt-dlp  for how to manually pass cookies. Also see  https://github.com/yt-dlp/yt-dlp/wiki/Extractors#exporting-youtube-cookies  for tips on effectively exporting YouTube cookies".into(),
                );
            }
            // Preview of right-to-left text in the list (no network).
            Some("arabic-demo") => {
                let now = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
                let mk = |id: u64, name: &str, mb: u64, pct: u64, st: Status| Download {
                    id,
                    url: "https://example.com/x".into(),
                    file_name: name.into(),
                    ext: ext_of(name),
                    size: mb * 1024 * 1024,
                    downloaded: mb * 1024 * 1024 * pct / 100,
                    status: st,
                    speed_bps: if st == Status::Downloading { 3.0 * 1024.0 * 1024.0 } else { 0.0 },
                    save_dir: String::new(),
                    added: now.clone(),
                    error: None,
                    source: Source::Http,
                    thumb: None,
                    extra: Extra::default(),
                };
                app.downloads = vec![
                    mk(1, "الله لا إله إلا هو الحي القيوم.mp4", 120, 40, Status::Downloading),
                    mk(2, "محاضرة عن البرمجة - Rust Programming.mp4", 300, 100, Status::Complete),
                    mk(3, "كتاب التفسير.pdf", 12, 10, Status::Paused),
                ];
                app.demo = true;
            }
            // Preview of the quiet first-launch setup status (no network).
            Some("setup-demo") => app.tools.ffmpeg = ToolStatus::Installing(0.42),
            // Layout preview of the YouTube card with made-up video details (no network).
            Some("yt-demo") => {
                app.open_add();
                app.tools.ytdlp = ToolStatus::Ready;
                app.dialog.url = "https://www.youtube.com/watch?v=demo".into();
                let mut sizes = HashMap::new();
                for (h, mb) in [(1080u32, 96u64), (720, 52), (480, 28), (360, 18)] {
                    sizes.insert(h, mb * 1024 * 1024);
                }
                app.dialog.file_name = "Sample video title.mp4".into();
                app.dialog.probe = ProbeState::YtReady(Box::new(YtInfo {
                    id: "demo".into(),
                    title: "Sample video title".into(),
                    channel: "Sample Channel".into(),
                    duration: 212,
                    thumb: None,
                    thumb_url: String::new(),
                    heights: vec![1080, 720, 480, 360],
                    video_sizes: sizes,
                    audio_size: 4 * 1024 * 1024,
                    std_height: 360,
                    std_size: 20 * 1024 * 1024,
                }));
            }
            _ => {}
        }
        if let Some(c) = o.cat.as_deref() {
            for cat in Category::LIBRARY.iter().chain(Category::TYPES.iter()) {
                if cat.label().eq_ignore_ascii_case(c) {
                    app.cat = *cat;
                }
            }
        }
        app
    }

    /// Read links the browser extension dropped into the inbox folder.
    fn poll_inbox(&mut self, ctx: &egui::Context) {
        if !self.persist || self.last_inbox.elapsed() < Duration::from_millis(700) {
            return;
        }
        self.last_inbox = Instant::now();
        let Ok(rd) = fs::read_dir(native::inbox_dir()) else { return };
        let mut files: Vec<PathBuf> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect();
        files.sort();
        for f in files {
            let text = fs::read_to_string(&f);
            let _ = fs::remove_file(&f);
            if let Ok(v) = text.map_err(|_| ()).and_then(|t| serde_json::from_str::<serde_json::Value>(&t).map_err(|_| ())) {
                self.handle_browser_link(ctx, &v);
            }
        }
    }

    fn handle_browser_link(&mut self, ctx: &egui::Context, v: &serde_json::Value) {
        let text = |k: &str| {
            v[k].as_str()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(String::from)
        };
        let Some(url) = text("url") else { return };
        if !(url.starts_with("http://") || url.starts_with("https://") || torrent::is_magnet(&url)) {
            return;
        }
        // The same link twice within a few seconds is one click.
        self.recent_browser.retain(|(_, t)| t.elapsed() < Duration::from_secs(10));
        if self.recent_browser.iter().any(|(u, _)| *u == url) {
            return;
        }
        self.recent_browser.push((url.clone(), Instant::now()));

        let extra = Extra { referer: text("referrer"), cookie: text("cookie"), user_agent: text("userAgent") };
        let suggested = text("filename").map(|n| clean_name(&n));
        // "now" or "queue" means the browser dialog already asked the user.
        let mode = text("mode");
        let save_dir = text("saveDir").unwrap_or_else(|| self.default_dir.clone());

        // A magnet link or .torrent file: the torrent engine takes it.
        if v.get("yt").is_none() && torrent::is_torrent_link(&url, suggested.as_deref().unwrap_or("")) {
            let queue = mode.as_deref() == Some("queue");
            let file_name = suggested
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| torrent::magnet_name(&url).map(|n| clean_name(&n)))
                .unwrap_or_else(|| file_name_from_url(&url));
            let d = Download {
                id: self.next_id,
                url,
                ext: ext_of(&file_name),
                file_name,
                size: 0,
                downloaded: 0,
                status: if queue { Status::Queued } else { Status::Downloading },
                speed_bps: 0.0,
                save_dir,
                added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
                error: None,
                source: Source::Torrent,
                thumb: None,
                extra,
            };
            self.next_id += 1;
            self.accept_browser_download(d, queue);
            return;
        }

        // A YouTube choice made in the browser dialog.
        if let Some(y) = v.get("yt").filter(|y| y.is_object()) {
            let audio = y["audio"].as_bool().unwrap_or(false);
            let id = y["id"].as_str().unwrap_or("").to_string();
            let height = y["height"].as_u64().unwrap_or(0) as u32;
            // The video title is the name (titles can contain dots, so never split them).
            let stem = text("title")
                .map(|t| clean_name(&t))
                .or_else(|| {
                    suggested
                        .clone()
                        .map(|n| n.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(n))
                })
                .unwrap_or_else(|| "YouTube video".to_string());
            let stem: String = stem.chars().take(120).collect(); // long captions would break the file name
            let file_name = format!("{}.{}", stem.trim_end_matches(['.', ' ']), if audio { "m4a" } else { "mp4" });
            let queue = mode.as_deref() == Some("queue");
            let url_for_extra = url.clone();
            let d = Download {
                id: self.next_id,
                url,
                ext: ext_of(&file_name),
                file_name,
                size: v["size"].as_u64().unwrap_or(0),
                downloaded: 0,
                status: if queue { Status::Queued } else { Status::Downloading },
                speed_bps: 0.0,
                save_dir,
                added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
                error: None,
                source: Source::YouTube(YtOpts { id, height, audio }),
                thumb: None,
                // A stream (not YouTube) keeps the page's referrer, cookies and browser identity.
                extra: if is_youtube(&url_for_extra) { Extra::default() } else { extra.clone() },
            };
            self.next_id += 1;
            // The thumbnail picture arrives a moment later.
            if let Some(turl) = text("thumbUrl") {
                let (tx, id) = (self.thumb_tx.clone(), d.id);
                thread::spawn(move || {
                    if let Some(t) = youtube::fetch_thumb(&turl) {
                        let _ = tx.send((id, t));
                    }
                });
            }
            self.accept_browser_download(d, queue);
            return;
        }

        // A file the browser dialog already confirmed.
        if let Some(m) = mode.as_deref() {
            let queue = m == "queue";
            let file_name = suggested.unwrap_or_else(|| file_name_from_url(&url));
            let d = Download {
                id: self.next_id,
                url,
                ext: ext_of(&file_name),
                file_name,
                size: v["size"].as_u64().unwrap_or(0),
                downloaded: 0,
                status: if queue { Status::Queued } else { Status::Downloading },
                speed_bps: 0.0,
                save_dir,
                added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
                error: None,
                source: Source::Http,
                thumb: None,
                extra,
            };
            self.next_id += 1;
            self.accept_browser_download(d, queue);
            return;
        }

        // Older extension versions send a bare link: YouTube always shows the Add dialog.
        if youtube::uses_ytdlp(&url) || self.browser_ask {
            self.open_add();
            self.dialog.file_name = suggested.unwrap_or_else(|| file_name_from_url(&url));
            self.dialog.url = url;
            self.dialog.extra = extra;
            self.dialog.last_edit = Some(Instant::now() - Duration::from_secs(1));
            shell::show_window();
            ctx.request_repaint();
            return;
        }
        let file_name = suggested.unwrap_or_else(|| file_name_from_url(&url));
        let d = Download {
            id: self.next_id,
            url,
            ext: ext_of(&file_name),
            file_name,
            size: v["size"].as_u64().unwrap_or(0),
            downloaded: 0,
            status: Status::Downloading,
            speed_bps: 0.0,
            save_dir,
            added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            error: None,
            source: Source::Http,
            thumb: None,
            extra,
        };
        self.next_id += 1;
        self.accept_browser_download(d, false);
    }

    /// Add a browser download to the list: start it now, or leave it queued.
    fn accept_browser_download(&mut self, d: Download, queue: bool) {
        if self.notify_done {
            shell::toast(
                if queue { "Added to the queue" } else { "Download started" }.into(),
                d.file_name.clone(),
            );
        }
        if !queue {
            self.engine.start(d.clone());
        }
        self.downloads.push(d);
    }

    /// Close the window but keep the app (and its downloads) running in the tray.
    fn hide_to_tray(&mut self) {
        shell::hide_window();
        if self.persist && !self.tray_hint_shown {
            self.tray_hint_shown = true;
            shell::toast(
                "Orexa Downloader is still running".into(),
                "It is in the tray. Click its icon to open it, or choose Quit there.".into(),
            );
        }
    }

    /// The Add dialog always opens empty, except that a link you just copied is filled in
    /// and checked straight away.
    fn open_add(&mut self) {
        self.dialog.open = true;
        self.dialog.save_dir = self.default_dir.clone();
        self.dialog.focus = true;
        self.clear_dialog_fields();
        if self.persist {
            if let Some(url) = shell::clipboard_url() {
                let used = self.downloads.iter().any(|d| d.url == url);
                if !used && self.dismissed_clip.as_deref() != Some(url.as_str()) {
                    self.dialog.file_name = file_name_from_url(&url);
                    self.dialog.url = url.clone();
                    self.dialog.last_edit = Some(Instant::now() - Duration::from_secs(1));
                    self.prefilled = Some(url);
                }
            }
        }
    }

    fn clear_dialog_fields(&mut self) {
        self.dialog.url.clear();
        self.dialog.file_name.clear();
        self.dialog.probe = ProbeState::Idle;
        self.dialog.last_edit = None;
        self.dialog.yt_sel = 0;
        self.dialog.extra = Extra::default();
        self.prefilled = None;
    }

    /// Close the Add dialog without adding: nothing typed is kept.
    fn close_dialog(&mut self) {
        // A copied link that was dismissed is not offered again until the clipboard changes.
        if let Some(p) = self.prefilled.take() {
            if self.dialog.url.trim() == p {
                self.dismissed_clip = Some(p);
            }
        }
        self.dialog.open = false;
        self.clear_dialog_fields();
    }

    // Collect finished link checks and start a new one once typing pauses.
    fn start_setup(&mut self, tool: Tool) {
        if matches!(self.tools.get(tool), ToolStatus::Installing(_)) {
            return;
        }
        self.tools.set(tool, ToolStatus::Installing(0.0));
        youtube::setup_tool(tool, self.setup_tx.clone());
    }

    fn poll_setup(&mut self) {
        while let Ok(ev) = self.setup_rx.try_recv() {
            match ev {
                SetupEvent::Progress(t, done, total) => {
                    if t == Tool::YtDlp && self.refreshing_ytdlp {
                        continue; // refreshing in the background: nothing to show
                    }
                    let f = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                    self.tools.set(t, ToolStatus::Installing(f.clamp(0.0, 1.0)));
                }
                SetupEvent::Done(t) => {
                    if t == Tool::YtDlp {
                        self.refreshing_ytdlp = false;
                    }
                    self.tools.set(t, ToolStatus::Ready);
                    if t == Tool::YtDlp && self.auto_provision && self.tools.ffmpeg == ToolStatus::Missing {
                        self.start_setup(Tool::Ffmpeg);
                    }
                    // The video that was waiting for yt-dlp can be checked now.
                    if t == Tool::YtDlp && self.dialog.probe == ProbeState::NeedsSetup {
                        self.dialog.last_edit = Some(Instant::now() - Duration::from_secs(1));
                    }
                }
                SetupEvent::Failed(t, e) => {
                    if t == Tool::YtDlp && self.refreshing_ytdlp {
                        // The refresh did not work; the old copy still does.
                        self.refreshing_ytdlp = false;
                    } else {
                        self.tools.set(t, ToolStatus::Failed(e));
                    }
                }
            }
        }
    }

    // Collect finished link checks and start a new one once typing pauses.
    fn poll_probe(&mut self) {
        self.poll_setup();
        while let Ok((url, res)) = self.probe_rx.try_recv() {
            if url != self.dialog.url.trim() {
                continue;
            }
            match res {
                Ok(ProbeOk::Http(size, name)) => {
                    if let Some(n) = &name {
                        if self.dialog.file_name == file_name_from_url(&url) {
                            self.dialog.file_name = n.clone();
                        }
                    }
                    self.dialog.probe = ProbeState::Ready(size, name);
                }
                Ok(ProbeOk::Yt(info)) => {
                    let list = youtube::choices(&info, self.tools.ffmpeg == ToolStatus::Ready);
                    self.dialog.yt_sel = youtube::default_choice(&list);
                    let title: String = clean_name(&info.title).chars().take(120).collect();
                    self.dialog.file_name = format!("{}.mp4", title.trim_end_matches(['.', ' ']));
                    self.dialog.probe = ProbeState::YtReady(info);
                }
                Err(e) => self.dialog.probe = ProbeState::Failed(e),
            }
        }
        if let Some(t) = self.dialog.last_edit {
            if t.elapsed() >= Duration::from_millis(350) {
                self.dialog.last_edit = None;
                if self.url_ok() {
                    let url = self.dialog.url.trim().to_string();
                    if youtube::uses_ytdlp(&url) {
                        if self.tools.ytdlp != ToolStatus::Ready {
                            self.dialog.probe = ProbeState::NeedsSetup;
                            if self.auto_provision && self.tools.ytdlp == ToolStatus::Missing {
                                self.start_setup(Tool::YtDlp);
                            }
                            return;
                        }
                        self.dialog.probe = ProbeState::Checking;
                        let tx = self.probe_tx.clone();
                        thread::spawn(move || {
                            let r = youtube::probe_youtube(&url).map(|i| ProbeOk::Yt(Box::new(i)));
                            let _ = tx.send((url, r));
                        });
                    } else if torrent::is_magnet(&url) {
                        // Nothing to check before adding: the size shows once it starts.
                        self.dialog.probe = ProbeState::Ready(0, None);
                    } else {
                        self.dialog.probe = ProbeState::Checking;
                        let tx = self.probe_tx.clone();
                        thread::spawn(move || {
                            let r = probe_link(&url).map(|(s, n)| ProbeOk::Http(s, n));
                            let _ = tx.send((url, r));
                        });
                    }
                }
            }
        }
    }

    /// A YouTube link can only be added once its info has been read.
    fn can_submit(&self) -> bool {
        if !self.url_ok() {
            return false;
        }
        if youtube::uses_ytdlp(self.dialog.url.trim()) {
            matches!(self.dialog.probe, ProbeState::YtReady(_))
        } else {
            true
        }
    }

    fn url_ok(&self) -> bool {
        let u = self.dialog.url.trim();
        u.starts_with("http://") || u.starts_with("https://") || torrent::is_magnet(u)
    }

    fn add_download(&mut self, start_now: bool) {
        if !self.can_submit() {
            return;
        }
        let url = self.dialog.url.trim().to_string();
        let mut file_name = if self.dialog.file_name.trim().is_empty() {
            file_name_from_url(&url)
        } else {
            clean_name(self.dialog.file_name.trim())
        };
        let mut size = match &self.dialog.probe {
            ProbeState::Ready(sz, _) => *sz,
            _ => 0,
        };
        let mut source = Source::Http;
        let mut thumb = None;
        if let ProbeState::YtReady(info) = &self.dialog.probe {
            let list = youtube::choices(info, self.tools.ffmpeg == ToolStatus::Ready);
            let pick = list[self.dialog.yt_sel.min(list.len() - 1)].clone();
            let stem = file_name.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(file_name.clone());
            file_name = format!("{}.{}", stem, if pick.audio { "m4a" } else { "mp4" });
            size = pick.size;
            thumb = info.thumb.clone();
            source = Source::YouTube(YtOpts { id: info.id.clone(), height: pick.height, audio: pick.audio });
        }
        if matches!(source, Source::Http) && torrent::is_torrent_link(&url, &file_name) {
            source = Source::Torrent;
        }
        let d = Download {
            id: self.next_id,
            url,
            file_name: file_name.clone(),
            ext: ext_of(&file_name),
            size,
            downloaded: 0,
            status: if start_now { Status::Downloading } else { Status::Queued },
            speed_bps: 0.0,
            save_dir: self.dialog.save_dir.clone(),
            added: chrono::Local::now().format("%Y-%m-%d %H:%M").to_string(),
            error: None,
            source,
            thumb,
            extra: std::mem::take(&mut self.dialog.extra),
        };
        self.next_id += 1;
        if start_now {
            self.engine.start(d.clone());
        }
        self.downloads.push(d);
        self.dialog.open = false;
        self.dialog.url.clear();
        self.dialog.file_name.clear();
        self.dialog.probe = ProbeState::Idle;
        self.dialog.last_edit = None;
    }

    fn start_id(&mut self, id: u64) {
        let Some(d) = self.downloads.iter().find(|x| x.id == id).cloned() else { return };
        if !matches!(d.status, Status::Queued | Status::Paused | Status::Error) {
            return;
        }
        let resumed = d.status == Status::Paused && self.engine.resume(id);
        if !resumed {
            self.engine.start(d.clone());
        }
        if let Some(m) = self.downloads.iter_mut().find(|x| x.id == id) {
            m.status = Status::Downloading;
            m.error = None;
        }
    }

    fn pause_all(&mut self) {
        let ids: Vec<u64> = self
            .downloads
            .iter()
            .filter(|d| d.status == Status::Downloading)
            .map(|d| d.id)
            .collect();
        for id in ids {
            self.engine.pause(id);
        }
    }

    fn delete_confirmed(&mut self) {
        if let Some(id) = self.confirm_delete.take() {
            self.engine.cancel(id);
            self.downloads.retain(|d| d.id != id);
            if self.selected_row == Some(id) {
                self.selected_row = None;
            }
        }
    }

    fn poll_events(&mut self) {
        while let Ok(ev) = self.ev_rx.try_recv() {
            match ev {
                EngineEvent::Progress(id, done, size, speed) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        d.downloaded = done;
                        if size > 0 {
                            d.size = size;
                        }
                        d.speed_bps = speed;
                    }
                }
                EngineEvent::FileName(id, name) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        d.ext = ext_of(&name);
                        d.file_name = name;
                    }
                }
                EngineEvent::State(id, status, err) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        d.status = status;
                        if status != Status::Downloading {
                            d.speed_bps = 0.0;
                        }
                        if status == Status::Complete && d.size > 0 {
                            d.downloaded = d.size;
                        }
                        if status == Status::Complete && self.notify_done && self.persist {
                            shell::toast("Download complete".into(), d.file_name.clone());
                        }
                        if let Some(e) = err {
                            d.error = Some(e);
                        }
                    }
                }
            }
        }
    }

    // Start waiting downloads when fewer than max_parallel are running.
    fn pump_queue(&mut self) {
        if self.demo {
            return;
        }
        let mut active = self.downloads.iter().filter(|d| d.status == Status::Downloading).count();
        let ids: Vec<u64> = self
            .downloads
            .iter()
            .filter(|d| d.status == Status::Queued)
            .map(|d| d.id)
            .collect();
        for id in ids {
            if active >= self.max_parallel {
                break;
            }
            self.start_id(id);
            active += 1;
        }
    }

    fn tick_demo(&mut self) {
        if !self.demo {
            return;
        }
        self.demo_phase += 0.02;
        for d in &mut self.downloads {
            if d.status == Status::Downloading && d.downloaded < d.size {
                d.downloaded = (d.downloaded + 4_096).min(d.size);
            }
        }
    }

    fn sample_speed(&mut self) {
        if self.last_sample.elapsed() >= Duration::from_millis(500) {
            self.last_sample = Instant::now();
            let total: f64 = self
                .downloads
                .iter()
                .filter(|d| d.status == Status::Downloading)
                .map(|d| d.speed_bps)
                .sum();
            self.speed_hist.remove(0);
            self.speed_hist.push(total as f32);
        }
    }

    fn apply(&mut self, acts: Vec<Act>) {
        for a in acts {
            match a {
                Act::Select(id) => self.selected_row = Some(id),
                Act::Start(id) => {
                    self.selected_row = Some(id);
                    self.start_id(id);
                }
                Act::Pause(id) => self.engine.pause(id),
                Act::Open(id) => {
                    if let Some(d) = self.downloads.iter().find(|x| x.id == id) {
                        open_path(&d.path());
                    }
                }
                Act::Folder(id) => {
                    if let Some(d) = self.downloads.iter().find(|x| x.id == id) {
                        reveal_path(&d.path());
                    }
                }
                Act::Remove(id) => self.confirm_delete = Some(id),
                Act::AddDialog => self.open_add(),
            }
        }
    }

    // ---- Custom title bar -------------------------------------------------
    fn ui_titlebar(&mut self, ctx: &egui::Context, p: &Pal) {
        let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
        egui::TopBottomPanel::top("titlebar")
            .exact_height(40.0)
            .frame(egui::Frame::none().fill(p.surface))
            .show(ctx, |ui| {
                let bar = ui.max_rect();
                ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, p.border));

                // Drag the window by the bar; double click toggles maximize.
                let drag = ui.interact(bar, egui::Id::new("titlebar_drag"), Sense::click_and_drag());
                if drag.drag_started() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }

                // Brand: small symbol + app name.
                let sym = Rect::from_min_size(pos2(bar.left() + 16.0, bar.center().y - 8.0), vec2(18.0, 16.6));
                paint_logo(ui, sym);
                ui.painter().text(
                    pos2(sym.right() + 10.0, bar.center().y),
                    Align2::LEFT_CENTER,
                    "Orexa Downloader",
                    fs(13.0),
                    p.sub,
                );

                // "How to add the Chrome extension?", just before the window buttons.
                {
                    let label = "How to add the extension?";
                    let show_label = bar.width() > 780.0;
                    let tw = if show_label { text_w(ui, label, f(12.5)) } else { 0.0 };
                    let w = if show_label { 12.0 + 16.0 + 6.0 + tw + 12.0 } else { 34.0 };
                    let r = Rect::from_min_size(pos2(bar.right() - 3.0 * 46.0 - 10.0 - w, bar.center().y - 14.0), vec2(w, 28.0));
                    let resp = ui.interact(r, egui::Id::new("help_btn"), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(r, 8.0, p.tile);
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let icon_x = if show_label { r.left() + 12.0 + 8.0 } else { r.center().x };
                    paint_icon(ui, Rect::from_center_size(pos2(icon_x, r.center().y), vec2(16.0, 16.0)), "help", p.sub);
                    if show_label {
                        ui.painter().text(pos2(icon_x + 14.0, r.center().y), Align2::LEFT_CENTER, label, f(12.5), p.sub);
                    }
                    if resp.on_hover_text("How to add the Chrome extension?").clicked() {
                        self.help_open = true;
                    }
                }

                // Window buttons, right aligned.
                let bw = 46.0;
                let mut x = bar.right();
                let mut btn = |icon: &str, id: &str, tip: &str, danger: bool| -> bool {
                    x -= bw;
                    let r = Rect::from_min_size(pos2(x, bar.top()), vec2(bw, bar.height() - 1.0));
                    let resp = ui.interact(r, egui::Id::new(id), Sense::click());
                    let hov = resp.hovered();
                    if hov {
                        ui.painter().rect_filled(r, 0.0, if danger { p.bad } else { p.tile });
                    }
                    let fg = if hov && danger { Color32::WHITE } else { p.sub };
                    paint_icon(ui, Rect::from_center_size(r.center(), vec2(16.0, 16.0)), icon, fg);
                    resp.on_hover_text(tip).clicked()
                };
                if btn("x", "win_close", "Close to the tray (downloads keep running)", true) {
                    self.hide_to_tray();
                }
                if btn(if maximized { "copy" } else { "square" }, "win_max", if maximized { "Restore" } else { "Maximize" }, false) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                if btn("minus", "win_min", "Minimize", false) {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                }
            });
    }

    // ---- Sidebar ----------------------------------------------------------
    fn ui_sidebar(&mut self, ctx: &egui::Context, p: &Pal) {
        egui::SidePanel::left("nav")
            .exact_width(248.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(p.surface).inner_margin(Margin::symmetric(16.0, 20.0)))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().vline(
                    full.right() + 16.0 - 0.5,
                    (full.top() - 20.0)..=(full.bottom() + 20.0),
                    Stroke::new(1.0, p.border),
                );
                ui.spacing_mut().item_spacing.y = 4.0;
                let group = |ui: &mut Ui, s: &str| {
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        ui.add_space(12.0);
                        ui.label(rt(s, fs(12.0), p.muted));
                    });
                    ui.add_space(2.0);
                };
                let counts = |c: Category, dl: &Vec<Download>| dl.iter().filter(|d| c.matches(d)).count();

                group(ui, "Library");
                for c in Category::LIBRARY {
                    let n = counts(c, &self.downloads);
                    if nav_item(ui, p, c.icon(), c.label(), Some(n), self.page == Page::Downloads && self.cat == c) {
                        self.page = Page::Downloads;
                        self.cat = c;
                    }
                }
                group(ui, "Categories");
                for c in Category::TYPES {
                    let n = counts(c, &self.downloads);
                    if nav_item(ui, p, c.icon(), c.label(), Some(n), self.page == Page::Downloads && self.cat == c) {
                        self.page = Page::Downloads;
                        self.cat = c;
                    }
                }
                ui.with_layout(Layout::bottom_up(Align::Min), |ui| {
                    if nav_item(ui, p, "settings", "Settings", None, self.page == Page::Settings) {
                        self.page = Page::Settings;
                    }
                });
            });
    }

    // ---- Toolbar ------------------------------------------------------------
    fn ui_toolbar(&mut self, ctx: &egui::Context, p: &Pal) {
        if self.page == Page::Settings {
            return; // Settings has nothing to search or add
        }
        egui::TopBottomPanel::top("toolbar")
            .exact_height(72.0)
            .frame(egui::Frame::none().fill(p.surface).inner_margin(Margin::symmetric(32.0, 0.0)))
            .show(ctx, |ui| {
                let r = ui.max_rect();
                ui.painter().hline(
                    (r.left() - 32.0)..=(r.right() + 32.0),
                    r.bottom() - 0.5,
                    Stroke::new(1.0, p.border),
                );
                ui.horizontal_centered(|ui| {
                    egui::Frame::none()
                        .fill(p.bg)
                        .stroke(Stroke::new(1.0, p.border))
                        .rounding(10.0)
                        .inner_margin(Margin::symmetric(12.0, 9.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                                paint_icon(ui, r, "search", p.muted);
                                let mut lay = rtl_layouter(f(14.0), p.text);
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.search)
                                        .layouter(&mut lay)
                                        .frame(false)
                                        .hint_text(rt("Search downloads", f(14.0), p.muted))
                                        .font(f(14.0))
                                        .text_color(p.text)
                                        .desired_width(340.0),
                                );
                            });
                        });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if button(ui, p, "Add URL", Some("plus"), Kind::Primary, 40.0, true).clicked() {
                            self.open_add();
                        }
                        ui.add_space(4.0);
                        let any = self.downloads.iter().any(|d| d.status == Status::Downloading);
                        if button(ui, p, "Pause all", Some("pause"), Kind::Secondary, 40.0, any).clicked() {
                            self.pause_all();
                        }

                    });
                });
            });
    }

    // ---- Downloads page -----------------------------------------------------
    fn ui_downloads(&mut self, ui: &mut Ui, p: &Pal) {
        let mut acts: Vec<Act> = Vec::new();
        let q = self.search.trim().to_lowercase();
        let rows: Vec<&Download> = self
            .downloads
            .iter()
            .filter(|d| self.cat.matches(d) && (q.is_empty() || d.file_name.to_lowercase().contains(&q)))
            .collect();
        let selected = self.selected_row;
        let cat = self.cat;
        let ctx = ui.ctx().clone();
        for d in &rows {
            let (k, path) = download_icon_key(d);
            self.icons.ensure(&ctx, &k, &path);
            if let Some(t) = &d.thumb {
                self.icons.ensure_rgba(&ctx, &format!("thumb:{}", d.id), t.w, t.h, &t.rgba);
            }
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::none().inner_margin(Margin::symmetric(32.0, 28.0)).show(ui, |ui| {
                if self.downloads.is_empty() {
                    let top = ((ui.available_height() - 380.0) / 2.0).max(24.0);
                    ui.add_space(top);
                    ui.vertical_centered(|ui| {
                        let (halo, _) = ui.allocate_exact_size(vec2(104.0, 104.0), Sense::hover());
                        if let Some(tex) = &self.app_icon {
                            ui.painter().image(
                                tex.id(),
                                halo,
                                Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }
                        ui.add_space(20.0);
                        ui.label(rt("No downloads yet", fb(36.0), p.text));
                        ui.add_space(8.0);
                        ui.scope(|ui| {
                            ui.set_max_width(560.0);
                            ui.label(rt(
                                "Paste a link to save your first file. Orexa Downloader keeps it organized and picks up where it left off if your connection drops.",
                                f(16.0),
                                p.sub,
                            ));
                        });
                        ui.add_space(20.0);
                        let w = text_w(ui, "Add URL", fs(14.0)) + 36.0 + 26.0;
                        ui.add_space(0.0);
                        let _ = w;
                        if button(ui, p, "Add URL", Some("plus"), Kind::Primary, 48.0, true).clicked() {
                            acts.push(Act::AddDialog);
                        }
                        ui.add_space(14.0);
                        ui.label(rt("Tip: you can also start from the Add URL button at any time.", f(13.0), p.muted));
                    });
                    return;
                }

                if cat == Category::All {
                    self.ui_summary(ui, p);
                } else {
                    ui.label(rt(cat.label(), fb(32.0), p.text));
                    let n = self.downloads.iter().filter(|d| cat.matches(d)).count();
                    ui.label(rt(format!("{} file{}", n, if n == 1 { "" } else { "s" }), f(14.0), p.muted));
                    ui.add_space(20.0);
                }

                if rows.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        ui.label(rt("Nothing here yet", fs(20.0), p.text));
                        ui.add_space(4.0);
                        let msg = if q.is_empty() { "Downloads in this view will show up here." } else { "No downloads match your search." };
                        ui.label(rt(msg, f(14.0), p.muted));
                    });
                    return;
                }

                let mut groups: Vec<(String, Vec<&Download>)> = Vec::new();
                if cat == Category::Finished {
                    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
                    let (t, e): (Vec<&Download>, Vec<&Download>) =
                        rows.iter().copied().partition(|d| d.added.starts_with(&today));
                    groups.push(("Today".into(), t));
                    groups.push(("Earlier".into(), e));
                } else {
                    let run: Vec<&Download> = rows.iter().copied().filter(|d| d.status == Status::Downloading).collect();
                    let wait: Vec<&Download> = rows
                        .iter()
                        .copied()
                        .filter(|d| matches!(d.status, Status::Paused | Status::Queued | Status::Error))
                        .collect();
                    let done: Vec<&Download> = rows.iter().copied().filter(|d| d.status == Status::Complete).collect();
                    groups.push(("Downloading".into(), run));
                    groups.push(("Paused, queued and failed".into(), wait));
                    groups.push(("Completed".into(), done));
                }
                for (title, list) in groups {
                    if !list.is_empty() {
                        draw_group(ui, p, &self.icons, &title, &list, selected, &mut acts);
                    }
                }
            });
        });
        self.apply(acts);
    }

    fn ui_summary(&self, ui: &mut Ui, p: &Pal) {
        let speed: f64 = self.downloads.iter().filter(|d| d.status == Status::Downloading).map(|d| d.speed_bps).sum();
        let active = self.downloads.iter().filter(|d| d.status == Status::Downloading).count();
        let longest = self.downloads.iter().filter_map(|d| d.eta_secs()).fold(0.0_f64, f64::max);
        let done = self.downloads.iter().filter(|d| d.status == Status::Complete).count();
        egui::Frame::none()
            .fill(p.surface)
            .stroke(Stroke::new(1.0, p.border))
            .rounding(16.0)
            .inner_margin(Margin::symmetric(28.0, 22.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(rt("Current speed", fs(13.0), p.muted));
                        let big = if speed < 1.0 { "0 MB/s".to_string() } else { fmt_speed(speed) };
                        ui.label(rt(big, fb(44.0), p.text));
                    });
                    ui.add_space(40.0);
                    let (r, _) = ui.allocate_exact_size(vec2(196.0, 52.0), Sense::hover());
                    let peak = self.speed_hist.iter().cloned().fold(1.0_f32, f32::max);
                    let n = self.speed_hist.len();
                    for (i, v) in self.speed_hist.iter().enumerate() {
                        let h = (v / peak * 48.0).max(4.0);
                        let x = r.left() + i as f32 * 10.0;
                        let col = if i + 5 >= n { p.accent } else { p.accent.gamma_multiply(0.28) };
                        ui.painter().rect_filled(
                            Rect::from_min_max(pos2(x, r.bottom() - h), pos2(x + 6.0, r.bottom())),
                            3.0,
                            col,
                        );
                    }
                    let (area, _) = ui.allocate_exact_size(vec2(ui.available_width().max(0.0), 64.0), Sense::hover());
                    let stats = [
                        (done.to_string(), "Completed"),
                        (if longest > 0.0 { fmt_duration(longest) } else { "None".into() }, "Longest time left"),
                        (active.to_string(), "Downloading"),
                    ];
                    let mut right = area.right();
                    for (v, l) in stats {
                        let w = text_w(ui, &v, fs(20.0)).max(text_w(ui, l, f(13.0)));
                        let x = right - w;
                        ui.painter().text(pos2(x, area.center().y - 22.0), Align2::LEFT_TOP, &v, fs(20.0), p.text);
                        ui.painter().text(pos2(x, area.center().y + 6.0), Align2::LEFT_TOP, l, f(13.0), p.muted);
                        right = x - 40.0;
                    }
                });
            });
        ui.add_space(24.0);
    }

    // ---- Settings page ----------------------------------------------------------
    fn ui_settings(&mut self, ui: &mut Ui, p: &Pal) {
        let mut open_ext = false;
        let mut open_help = false;
        let mut pick_default_dir = false;
        let ext_found = extension_dir().is_some();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Frame::none().inner_margin(Margin::symmetric(48.0, 40.0)).show(ui, |ui| {
                ui.label(rt("General", fb(32.0), p.text));
                ui.label(rt("Changes apply right away.", f(14.0), p.muted));
                ui.add_space(24.0);

                // Wide enough: the About panel sits on the right, the cards get what is left.
                const SIDE_W: f32 = 290.0;
                let avail = ui.available_width();
                let side = avail >= 540.0 + 24.0 + SIDE_W;
                let cw = if side { (avail - 24.0 - SIDE_W).min(760.0) } else { avail.min(760.0) };
                if side {
                    let r = Rect::from_min_size(pos2(ui.cursor().left() + cw + 24.0, ui.cursor().top()), vec2(SIDE_W, 600.0));
                    let mut right = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(Layout::top_down(Align::Min)));
                    about_panel(&mut right, p, SIDE_W);
                }

                let card = |ui: &mut Ui, rows: &mut dyn FnMut(&mut Ui)| {
                    egui::Frame::none()
                        .fill(p.surface)
                        .stroke(Stroke::new(1.0, p.border))
                        .rounding(16.0)
                        .show(ui, |ui| {
                            ui.set_width(cw);
                            ui.spacing_mut().item_spacing.y = 0.0;
                            rows(ui);
                        });
                    ui.add_space(20.0);
                };
                let line = |ui: &mut Ui, title: &str, desc: &str, last: bool, ctl: &mut dyn FnMut(&mut Ui)| {
                    egui::Frame::none().inner_margin(Margin::symmetric(24.0, 18.0)).show(ui, |ui| {
                        ui.set_width(cw - 48.0);
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                ui.label(rt(title, fs(14.0), p.text));
                                ui.label(rt(desc, f(13.0), p.muted));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ctl(ui));
                        });
                    });
                    if !last {
                        let y = ui.cursor().top();
                        ui.painter().hline(ui.min_rect().x_range(), y, Stroke::new(1.0, p.border));
                    }
                };

                let default_dir = &mut self.default_dir;
                let max_parallel = &mut self.max_parallel;
                card(ui, &mut |ui| {
                    line(ui, "Default save folder", "New downloads go here.", false, &mut |ui| {
                        if button(ui, p, "Change", Some("folder"), Kind::Secondary, 40.0, true).clicked() {
                            pick_default_dir = true;
                        }
                        ui.add_space(8.0);
                        let mut lay = rtl_layouter(f(14.0), p.text);
                        ui.add(
                            egui::TextEdit::singleline(default_dir)
                                .layouter(&mut lay)
                                .font(f(14.0))
                                .text_color(p.text)
                                .margin(Margin::symmetric(12.0, 10.0))
                                .desired_width((cw - 48.0 - 250.0 - 110.0).clamp(120.0, 300.0)),
                        );
                    });
                    line(ui, "Downloads at the same time", "Waiting downloads start when a slot is free.", true, &mut |ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(124.0, 40.0), Sense::hover());
                        ui.painter().rect_filled(r, 10.0, p.surface);
                        ui.painter().rect_stroke(r, 10.0, Stroke::new(1.0, p.border_strong));
                        let minus = Rect::from_min_size(r.min, vec2(36.0, 40.0));
                        let plus = Rect::from_min_size(pos2(r.right() - 36.0, r.top()), vec2(36.0, 40.0));
                        if icon_button_at(ui, p, minus, "minus", egui::Id::new("par_minus"), "Fewer") && *max_parallel > 1 {
                            *max_parallel -= 1;
                        }
                        if icon_button_at(ui, p, plus, "plus", egui::Id::new("par_plus"), "More") && *max_parallel < 8 {
                            *max_parallel += 1;
                        }
                        ui.painter().text(r.center(), Align2::CENTER_CENTER, max_parallel.to_string(), fs(14.0), p.text);
                    });
                });

                let notify_done = &mut self.notify_done;
                let autostart = &mut self.autostart;
                let autostart_before = *autostart;
                card(ui, &mut |ui| {
                    line(
                        ui,
                        "Start with Windows",
                        "Open Orexa in the tray when Windows starts.",
                        false,
                        &mut |ui| {
                            toggle(ui, p, autostart);
                        },
                    );
                    line(
                        ui,
                        "Notifications",
                        "Show a Windows notification when a download finishes.",
                        true,
                        &mut |ui| {
                            toggle(ui, p, notify_done);
                        },
                    );
                });
                if self.autostart != autostart_before {
                    shell::set_autostart(self.autostart);
                }

                card(ui, &mut |ui| {
                    line(
                        ui,
                        "Chrome extension",
                        if ext_found {
                            "Add it to Chrome, Brave or Edge."
                        } else {
                            "Extension folder not found next to the app."
                        },
                        true,
                        &mut |ui| {
                            if button(ui, p, "Open folder", Some("folder"), Kind::Secondary, 36.0, ext_found).clicked() {
                                open_ext = true;
                            }
                            if button(ui, p, "How to add", Some("help"), Kind::Secondary, 36.0, true).clicked() {
                                open_help = true;
                            }
                        },
                    );
                });

                let dark = &mut self.dark;
                card(ui, &mut |ui| {
                    line(ui, "Theme", "Choose how Orexa Downloader looks.", true, &mut |ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(166.0, 40.0), Sense::hover());
                        ui.painter().rect_filled(r, 10.0, p.tile);
                        for (i, (label, is_dark)) in [("Light", false), ("Dark", true)].iter().enumerate() {
                            let o = Rect::from_min_size(pos2(r.left() + 3.0 + i as f32 * 80.0, r.top() + 3.0), vec2(80.0, 34.0));
                            let sel = *dark == *is_dark;
                            let resp = ui.interact(o, egui::Id::new(("theme", i)), Sense::click());
                            if sel {
                                ui.painter().rect_filled(o, 8.0, if p.dark { p.border_strong } else { p.surface });
                            }
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            ui.painter().text(
                                o.center(),
                                Align2::CENTER_CENTER,
                                *label,
                                if sel { fs(14.0) } else { f(14.0) },
                                if sel { p.text } else { p.sub },
                            );
                            if resp.clicked() {
                                *dark = *is_dark;
                            }
                        }
                    });
                });

                if !side {
                    about_panel(ui, p, cw.max(300.0));
                }
            });
        });
        if open_ext {
            if let Some(dir) = extension_dir() {
                open_path(&dir);
            }
        }
        if open_help {
            self.help_open = true;
        }
        if pick_default_dir {
            let mut dialog = rfd::FileDialog::new().set_title("Choose the default download folder");
            let current = PathBuf::from(self.default_dir.trim());
            if current.is_dir() {
                dialog = dialog.set_directory(current);
            }
            if let Some(dir) = dialog.pick_folder() {
                self.default_dir = dir.to_string_lossy().into_owned();
            }
        }
    }

    // ---- Add dialog -----------------------------------------------------------------
    fn ui_add_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        if !self.dialog.open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.close_dialog();
            return;
        }
        let ok = self.can_submit();
        let url_valid = self.url_ok();
        let tools_yt = self.tools.ytdlp.clone();
        let tools_ff = self.tools.ffmpeg.clone();
        let mut setup_req: Option<Tool> = None;
        let mut close = false;
        let mut submit = false;
        let mut submit_queue = false;
        let busy = self.downloads.iter().any(|d| d.status == Status::Downloading);
        let mut url_changed = false;
        let focus = std::mem::take(&mut self.dialog.focus);

        let (ikey, ipath) = type_icon_key(&ext_of(&self.dialog.file_name));
        self.icons.ensure(ctx, &ikey, &ipath);
        let icon_tex = self.icons.get(&ikey).cloned();

        let yt_thumb = match &self.dialog.probe {
            ProbeState::YtReady(info) => info.thumb.as_ref().and_then(|t| {
                let key = format!("yt:{}", info.id);
                self.icons.ensure_rgba(ctx, &key, t.w, t.h, &t.rgba);
                self.icons.get(&key).cloned()
            }),
            _ => None,
        };

        let dlg = &mut self.dialog;
        let mut marks: Vec<(&str, f32)> = Vec::new();
        modal(ctx, p, "add", 504.0, 560.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(rt("Add a download", fb(22.0), p.text));
                    ui.add_space(4.0);
                    ui.label(rt("Paste a link and choose where to save it.", f(14.0), p.muted));
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                    if icon_button_at(ui, p, r, "x", egui::Id::new("dlg_close"), "Close") {
                        close = true;
                    }
                });
            });
            marks.push(("after_header", ui.cursor().top()));
            ui.add_space(20.0);
            ui.label(rt("Download link", fs(13.0), p.sub));
            ui.add_space(6.0);
            let x0 = ui.cursor().left();
            let te = ui.add(
                egui::TextEdit::singleline(&mut dlg.url)
                    .hint_text(rt("https://", f(14.0), p.muted))
                    .font(f(14.0))
                    .text_color(p.text)
                    .margin(Margin { left: 42.0, right: 12.0, top: 11.0, bottom: 11.0 })
                    .desired_width(f32::INFINITY),
            );
            paint_icon(
                ui,
                Rect::from_center_size(pos2(x0 + 22.0, te.rect.center().y), vec2(18.0, 18.0)),
                "link",
                p.muted,
            );
            if focus {
                te.request_focus();
            }
            if te.changed() {
                url_changed = true;
            }
            if te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            if url_changed {
                dlg.file_name = file_name_from_url(&dlg.url);
                dlg.probe = ProbeState::Idle;
                dlg.last_edit = Some(Instant::now());
            }
            ui.add_space(16.0);

            marks.push(("after_url", ui.cursor().top()));
            if youtube::uses_ytdlp(&dlg.url) {
                egui::Frame::none()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.border))
                    .rounding(12.0)
                    .inner_margin(Margin::symmetric(16.0, 14.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 6.0;
                        let setup_row = |ui: &mut Ui, st: &ToolStatus, tool: Tool, req: &mut Option<Tool>| {
                            match st {
                                // Installing quietly: nothing extra to show (no bar, no numbers).
                                ToolStatus::Installing(_) | ToolStatus::Ready => {}
                                ToolStatus::Failed(e) => {
                                    ui.label(rt(format!("Setup failed: {}", e), f(13.0), p.bad));
                                    if button(ui, p, "Try again", None, Kind::Secondary, 36.0, true).clicked() {
                                        *req = Some(tool);
                                    }
                                }
                                _ => {
                                    let label = if tool == Tool::Ffmpeg { "Install ffmpeg" } else { "Set up now" };
                                    if button(ui, p, label, None, Kind::Primary, 36.0, true).clicked() {
                                        *req = Some(tool);
                                    }
                                }
                            }
                        };
                        match dlg.probe.clone() {
                            ProbeState::YtReady(info) => {
                                ui.horizontal(|ui| {
                                    ui.spacing_mut().item_spacing.x = 12.0;
                                    let (t, _) = ui.allocate_exact_size(vec2(96.0, 54.0), Sense::hover());
                                    ui.painter().rect_filled(t, 8.0, p.tile);
                                    if let Some(tex) = &yt_thumb {
                                        ui.painter().image(
                                            tex.id(),
                                            t,
                                            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                            Color32::WHITE,
                                        );
                                    }
                                    ui.vertical(|ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        let mut lay = rtl_layouter(fs(14.0), p.text);
                                        ui.add(
                                            egui::TextEdit::singleline(&mut dlg.file_name)
                                                .layouter(&mut lay)
                                                .frame(false)
                                                .font(fs(14.0))
                                                .text_color(p.text)
                                                .desired_width(ui.available_width()),
                                        );
                                        let who = if info.channel.is_empty() {
                                            "YouTube".to_string()
                                        } else {
                                            format!("{}, {}", info.channel, youtube::fmt_clock(info.duration))
                                        };
                                        ui.label(rt(who, f(13.0), p.muted));
                                    });
                                });
                                ui.add_space(8.0);
                                let list = youtube::choices(&info, tools_ff == ToolStatus::Ready);
                                dlg.yt_sel = dlg.yt_sel.min(list.len() - 1);
                                ui.label(rt("Quality", fs(13.0), p.sub));
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                                    for (i, c) in list.iter().enumerate() {
                                        let w = text_w(ui, &c.label, fs(13.0)) + 28.0;
                                        let (r, resp) = ui.allocate_exact_size(vec2(w, 34.0), Sense::click());
                                        let sel = dlg.yt_sel == i;
                                        if resp.hovered() {
                                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                        }
                                        ui.painter().rect_filled(
                                            r,
                                            8.0,
                                            if sel { p.accent_soft } else if resp.hovered() { p.tile } else { p.surface },
                                        );
                                        ui.painter().rect_stroke(
                                            r,
                                            8.0,
                                            Stroke::new(1.0, if sel { p.accent } else { p.border_strong }),
                                        );
                                        ui.painter().text(
                                            r.center(),
                                            Align2::CENTER_CENTER,
                                            &c.label,
                                            if sel { fs(13.0) } else { f(13.0) },
                                            if sel { p.accent_ink } else { p.text },
                                        );
                                        if resp.clicked() {
                                            dlg.yt_sel = i;
                                        }
                                    }
                                });
                                let pick = &list[dlg.yt_sel];
                                let size_line = if pick.size > 0 {
                                    format!("About {}", fmt_size(pick.size))
                                } else {
                                    "Size will show once it starts".to_string()
                                };
                                ui.label(rt(size_line, f(13.0), p.muted));
                            }
                            ProbeState::Failed(msg) => {
                                let bot = msg.contains("Sign in to confirm")
                                    || msg.contains("not a bot")
                                    || msg.contains("--cookies");
                                if bot {
                                    ui.label(rt("YouTube wants to confirm you are not a bot", fs(14.0), p.bad));
                                    ui.label(rt(
                                        "Open YouTube in Chrome (the Orexa extension shares your sign-in), then press Try again.",
                                        f(13.0),
                                        p.muted,
                                    ));
                                } else {
                                    ui.label(rt("Could not read this video", fs(14.0), p.bad));
                                    ui.label(rt(msg, f(13.0), p.muted));
                                }
                                ui.add_space(4.0);
                                if button(ui, p, "Try again", Some("retry"), Kind::Secondary, 36.0, true).clicked() {
                                    dlg.probe = ProbeState::Idle;
                                    dlg.last_edit = Some(Instant::now() - Duration::from_secs(1));
                                }
                            }
                            ProbeState::NeedsSetup => {
                                ui.label(rt("Video link detected", fs(14.0), p.text));
                                if tools_yt == ToolStatus::Ready {
                                    ui.label(rt("Reading video details... this can take a little while.", f(13.0), p.muted));
                                } else {
                                    let msg = if matches!(tools_yt, ToolStatus::Installing(_)) {
                                        "Getting video support ready. This happens once and continues on its own."
                                    } else {
                                        "Getting video support ready..."
                                    };
                                    ui.label(rt(msg, f(13.0), p.muted));
                                    setup_row(ui, &tools_yt, Tool::YtDlp, &mut setup_req);
                                }
                            }
                            _ => {
                                ui.label(rt("Reading video details... this can take a little while.", f(13.0), p.muted));
                            }
                        }
                    });
                ui.add_space(16.0);
            } else if !dlg.url.trim().is_empty() {
                egui::Frame::none()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.border))
                    .rounding(12.0)
                    .inner_margin(Margin::symmetric(16.0, 14.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let (t, _) = ui.allocate_exact_size(vec2(44.0, 44.0), Sense::hover());
                            ui.painter().rect_filled(t, 10.0, p.surface);
                            ui.painter().rect_stroke(t, 10.0, Stroke::new(1.0, p.border));
                            let ext = ext_of(&dlg.file_name);
                            let c = ext_cat(&ext);
                            if torrent::is_torrent_link(&dlg.url, &dlg.file_name) {
                                ui.painter().rect_filled(t, 10.0, p.accent_soft);
                                paint_icon(ui, Rect::from_center_size(t.center(), vec2(24.0, 24.0)), "magnet", p.accent_ink);
                            } else if let Some(tex) = &icon_tex {
                                ui.painter().image(
                                    tex.id(),
                                    Rect::from_center_size(t.center(), vec2(28.0, 28.0)),
                                    Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            } else {
                                paint_icon(ui, Rect::from_center_size(t.center(), vec2(22.0, 22.0)), c.map(|c| c.icon()).unwrap_or("doc"), p.sub);
                            }
                            ui.add_space(6.0);
                            ui.vertical(|ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                let mut lay = rtl_layouter(fs(14.0), p.text);
                                ui.add(
                                    egui::TextEdit::singleline(&mut dlg.file_name)
                                        .layouter(&mut lay)
                                        .frame(false)
                                        .font(fs(14.0))
                                        .text_color(p.text)
                                        .desired_width(240.0),
                                );
                                let is_torrent = torrent::is_torrent_link(&dlg.url, &dlg.file_name);
                                let kind = if is_torrent { "Torrent" } else { c.map(|c| c.label()).unwrap_or("Other") };
                                let line = match &dlg.probe {
                                    ProbeState::Ready(..) if is_torrent => format!("{}, size shows once it starts", kind),
                                    ProbeState::Ready(sz, _) if *sz > 0 => format!("{}, {}", kind, fmt_size(*sz)),
                                    ProbeState::Ready(..) | ProbeState::Failed(_) => format!("{}, size unknown", kind),
                                    _ => format!("{}, checking size...", kind),
                                };
                                ui.label(rt(line, f(13.0), p.muted));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                let (txt, col, bg) = if !url_valid {
                                    ("Use http or https", p.bad, p.bad.gamma_multiply(0.14))
                                } else {
                                    match &dlg.probe {
                                        ProbeState::Ready(..) => ("Link is ready", p.ok, p.ok_soft),
                                        ProbeState::Failed(msg) => {
                                            let blocked = ["401", "403", "404", "410"].iter().any(|c| msg.contains(c));
                                            (
                                                if blocked { "Link expired or blocked" } else { "Could not check link" },
                                                p.bad,
                                                p.bad.gamma_multiply(0.14),
                                            )
                                        }
                                        _ => ("Checking...", p.sub, p.track),
                                    }
                                };
                                let w = text_w(ui, txt, fs(12.0)) + 24.0;
                                let (r, _) = ui.allocate_exact_size(vec2(w, 24.0), Sense::hover());
                                ui.painter().rect_filled(r, 12.0, bg);
                                ui.painter().text(r.center(), Align2::CENTER_CENTER, txt, fs(12.0), col);
                            });
                        });
                    });
                ui.add_space(16.0);
            }

            marks.push(("after_card", ui.cursor().top()));
            ui.label(rt("Save to", fs(13.0), p.sub));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 12.0;
                let browse_w = text_w(ui, "Browse", fs(14.0)) + 36.0;
                let x0 = ui.cursor().left();
                let mut lay = rtl_layouter(f(14.0), p.text);
                let te = ui.add(
                    egui::TextEdit::singleline(&mut dlg.save_dir)
                        .layouter(&mut lay)
                        .font(f(14.0))
                        .text_color(p.text)
                        .margin(Margin { left: 42.0, right: 12.0, top: 11.0, bottom: 11.0 })
                        .desired_width((ui.available_width() - browse_w - 12.0 - 54.0).max(120.0)),
                );
                paint_icon(
                    ui,
                    Rect::from_center_size(pos2(x0 + 22.0, te.rect.center().y), vec2(18.0, 18.0)),
                    "folder",
                    p.muted,
                );
                if button(ui, p, "Browse", None, Kind::Secondary, te.rect.height().max(44.0), true).clicked() {
                    let mut dialog = rfd::FileDialog::new().set_title("Choose a download folder");
                    let current = PathBuf::from(dlg.save_dir.trim());
                    if current.is_dir() {
                        dialog = dialog.set_directory(current);
                    }
                    if let Some(dir) = dialog.pick_folder() {
                        dlg.save_dir = dir.to_string_lossy().into_owned();
                    }
                }
            });
            marks.push(("after_save", ui.cursor().top()));
            ui.add_space(18.0);
            if busy {
                // Something is already downloading: let the user choose.
                ui.label(rt(
                    "A download is running. Add this one to the queue to start after it, or download it now.",
                    f(13.0),
                    p.muted,
                ));
            } else {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(rt("Start downloading now", fs(14.0), p.text));
                        ui.label(rt("Turn off to add it to the queue instead.", f(13.0), p.muted));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        toggle(ui, p, &mut dlg.start_now);
                    });
                });
            }
            marks.push(("after_toggle", ui.cursor().top()));
            ui.add_space(24.0);
            // Fixed-height footer so the dialog shrinks back when content goes away.
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, 44.0), Layout::right_to_left(Align::Center), |ui| {
                if busy {
                    if button(ui, p, "Download now", Some("download"), Kind::Primary, 44.0, ok).clicked() {
                        submit = true;
                    }
                    ui.add_space(4.0);
                    if button(ui, p, "Add to queue", Some("clock"), Kind::Secondary, 44.0, ok).clicked() {
                        submit_queue = true;
                    }
                } else if button(ui, p, "Start download", Some("download"), Kind::Primary, 44.0, ok).clicked() {
                    submit = true;
                }
                ui.add_space(4.0);
                if button(ui, p, "Cancel", None, Kind::Secondary, 44.0, true).clicked() {
                    close = true;
                }
            });
        });
        if let Some(path) = std::env::var_os("OREXA_DEBUG_LAYOUT") {
            let line = marks.iter().map(|(n, y)| format!("{}={:.2}", n, y)).collect::<Vec<_>>().join(" ");
            let _ = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(format!("{}.marks", path.to_string_lossy()))
                .and_then(|mut f| f.write_all(format!("{}\n", line).as_bytes()));
        }
        if let Some(t) = setup_req {
            self.start_setup(t);
        }
        if close {
            self.close_dialog();
        } else if ok && submit_queue {
            self.add_download(false);
        } else if ok && submit {
            let start = busy || self.dialog.start_now;
            self.add_download(start);
        }
    }

    fn ui_delete_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some(id) = self.confirm_delete else { return };
        let name = self
            .downloads
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.file_name.clone())
            .unwrap_or_default();
        let mut yes = false;
        let mut no = false;
        modal(ctx, p, "delete", 400.0, 220.0, |ui| {
            ui.label(rt("Remove this download?", fb(20.0), p.text));
            ui.add_space(8.0);
            ui.label(rt(
                format!("{} will leave the list. The file on your disk is not deleted.", name),
                f(14.0),
                p.sub,
            ));
            ui.add_space(24.0);
            let fw = ui.available_width();
            ui.allocate_ui_with_layout(vec2(fw, 44.0), Layout::right_to_left(Align::Center), |ui| {
                // Destructive action wears the destructive color, never the primary one.
                let w = text_w(ui, "Remove", fs(14.0)) + 36.0;
                let (r, resp) = ui.allocate_exact_size(vec2(w, 44.0), Sense::click());
                ui.painter().rect_filled(r, 10.0, if resp.hovered() { p.bad.gamma_multiply(0.85) } else { p.bad });
                ui.painter().text(r.center(), Align2::CENTER_CENTER, "Remove", fs(14.0), if p.dark { hex(0x1A0B09) } else { Color32::WHITE });
                if resp.clicked() {
                    yes = true;
                }
                ui.add_space(4.0);
                if button(ui, p, "Keep", None, Kind::Secondary, 44.0, true).clicked() {
                    no = true;
                }
            });
        });
        if yes {
            self.delete_confirmed();
        } else if no || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.confirm_delete = None;
        }
    }
}

/// The tutorial pictures for "How to add the extension" (decoded once).
const HELP_PICS: [&[u8]; 5] = [
    include_bytes!("../assets/tutorial/step1_open.png"),
    include_bytes!("../assets/tutorial/step2_developer.png"),
    include_bytes!("../assets/tutorial/step3_load.png"),
    include_bytes!("../assets/tutorial/step4_folder.png"),
    include_bytes!("../assets/tutorial/step5_done.png"),
];

/// A tutorial picture, resized to the exact pixels it is shown at (sharp on any screen scale).
fn help_picture(ctx: &egui::Context, index: usize, logical_width: f32) -> Option<(egui::TextureHandle, egui::Vec2)> {
    static DECODED: std::sync::OnceLock<Vec<image::RgbaImage>> = std::sync::OnceLock::new();
    let all = DECODED.get_or_init(|| {
        HELP_PICS
            .iter()
            .map(|b| image::load_from_memory(b).map(|i| i.to_rgba8()).unwrap_or_else(|_| image::RgbaImage::new(1, 1)))
            .collect()
    });
    let src = all.get(index)?;
    let px_w = ((logical_width * ctx.pixels_per_point()).round() as u32).clamp(16, 2400);
    let px_h = ((px_w as f32) * src.height() as f32 / src.width() as f32).round().max(1.0) as u32;
    let key = egui::Id::new(("help-pic", index, px_w));
    let size = vec2(px_w as f32 / ctx.pixels_per_point(), px_h as f32 / ctx.pixels_per_point());
    if let Some(t) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(key)) {
        return Some((t, size));
    }
    let small = image::imageops::resize(src, px_w, px_h, image::imageops::FilterType::Lanczos3);
    let img = egui::ColorImage::from_rgba_unmultiplied([px_w as usize, px_h as usize], small.as_raw());
    let tex = ctx.load_texture(format!("help-{index}-{px_w}"), img, egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(key, tex.clone()));
    Some((tex, size))
}

impl App {
    /// "How to add the Chrome extension": the folder, a button to open it, and picture steps.
    fn ui_help_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        if !self.help_open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.help_open = false;
            return;
        }
        let ext = extension_dir();
        let browsers: [(&str, &str); 3] = [("Chrome", "chrome://extensions"), ("Brave", "brave://extensions"), ("Edge", "edge://extensions")];
        let sel = self.help_browser.min(2);
        let addr = browsers[sel].1;
        let mut close = false;
        let mut open_folder = false;
        let mut copy_addr = false;
        let mut pick = sel;
        let width = 700.0_f32.min(ctx.screen_rect().width() - 96.0).max(420.0);
        let scroll_h = (ctx.screen_rect().height() - 330.0).clamp(200.0, 560.0);

        let steps: [(&str, String); 5] = [
            ("Open the extensions page", format!("Type {} in the address bar and press Enter.", addr)),
            ("Turn on Developer mode", "Use the switch in the top-right corner of the page.".to_string()),
            ("Click Load unpacked", "A window for choosing a folder opens.".to_string()),
            (
                "Choose the extension folder",
                "Select the folder named \"extension\" (the Open folder button above shows you where it is), then press Select Folder.".to_string(),
            ),
            (
                "Done",
                "Orexa Downloader now appears in your list. From now on, downloads, videos and magnet links open Orexa's window. Tip: pin it with the puzzle-piece icon.".to_string(),
            ),
        ];

        let screen_h = ctx.screen_rect().height();
        modal(ctx, p, "help", width, 640.0, |ui| {
            ui.set_max_height(screen_h); // the pictures list below needs room to grow
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(rt("Add the Orexa extension to your browser", fb(21.0), p.text));
                    ui.add_space(4.0);
                    ui.label(rt("It takes about a minute and you only do it once.", f(14.0), p.muted));
                });
                ui.with_layout(Layout::right_to_left(Align::Min), |ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                    if icon_button_at(ui, p, r, "x", egui::Id::new("help_close"), "Close") {
                        close = true;
                    }
                });
            });
            ui.add_space(16.0);

            // Where the extension is, with a button to open it.
            egui::Frame::none()
                .fill(p.bg)
                .stroke(Stroke::new(1.0, p.border))
                .rounding(12.0)
                .inner_margin(Margin::symmetric(16.0, 12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(rt("Extension folder", fs(13.0), p.sub));
                            let path = ext
                                .as_ref()
                                .map(|d| d.display().to_string())
                                .unwrap_or_else(|| "Not found next to the app".to_string());
                            let g = trunc(ui, &path, f(13.0), p.muted, (width - 250.0).max(120.0));
                            ui.label(g);
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if button(ui, p, "Open folder", Some("folder"), Kind::Primary, 38.0, ext.is_some()).clicked() {
                                open_folder = true;
                            }
                        });
                    });
                });
            ui.add_space(14.0);

            // Which browser, and the address to type.
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for (i, (name, _)) in browsers.iter().enumerate() {
                    let kind = if i == sel { Kind::Primary } else { Kind::Secondary };
                    if button(ui, p, name, None, kind, 36.0, true).clicked() {
                        pick = i;
                    }
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if button(ui, p, "Copy address", None, Kind::Secondary, 36.0, true).clicked() {
                        copy_addr = true;
                    }
                    ui.label(rt(addr, fs(14.0), p.text));
                });
            });
            ui.add_space(12.0);

            egui::ScrollArea::vertical().max_height(scroll_h).auto_shrink([false, true]).show(ui, |ui| {
                let img_w = ui.available_width() - 14.0;
                for (i, (title, body)) in steps.iter().enumerate() {
                    ui.horizontal_top(|ui| {
                        let (c, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                        ui.painter().circle_filled(c.center(), 14.0, p.accent);
                        ui.painter().text(c.center(), Align2::CENTER_CENTER, (i + 1).to_string(), fs(14.0), p.accent_ink);
                        ui.add_space(8.0);
                        ui.vertical(|ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            ui.label(rt(*title, fs(15.0), p.text));
                            ui.label(rt(body.as_str(), f(13.0), p.sub));
                        });
                    });
                    ui.add_space(8.0);
                    if let Some((tex, size)) = help_picture(ui.ctx(), i, img_w) {
                        let (r, _) = ui.allocate_exact_size(size, Sense::hover());
                        ui.painter().image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        ui.painter().rect_stroke(r, 8.0, Stroke::new(1.0, p.border_strong));
                    }
                    ui.add_space(20.0);
                }
                ui.label(rt("Using Brave or Edge? The steps are the same; only the address at the start differs.", f(12.0), p.muted));
                ui.add_space(4.0);
            });
        });

        self.help_browser = pick;
        if copy_addr {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(addr.to_string());
            }
        }
        if open_folder {
            if let Some(d) = &ext {
                open_path(d);
            }
        }
        if close {
            self.help_open = false;
        }
    }
}

fn extension_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent()?
        .ancestors()
        .take(6)
        .map(|a| a.join("extension"))
        .find(|c| c.join("manifest.json").exists())
}

/// Thin window border plus edge/corner resizing (the native frame is gone).
fn window_frame_and_resize(ctx: &egui::Context, p: &Pal) {
    use egui::{CursorIcon, ResizeDirection as D};
    let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
    if maximized {
        return;
    }
    let screen = ctx.screen_rect();
    ctx.layer_painter(egui::LayerId::new(Order::Tooltip, egui::Id::new("win_border")))
        .rect_stroke(screen.shrink(0.5), 0.0, Stroke::new(1.0, p.border_strong));

    let Some(pos) = ctx.input(|i| i.pointer.hover_pos()) else { return };
    let m = 5.0;
    let left = pos.x <= screen.left() + m;
    let right = pos.x >= screen.right() - m;
    let top = pos.y <= screen.top() + m;
    let bottom = pos.y >= screen.bottom() - m;
    let dir = match (left, right, top, bottom) {
        (true, _, true, _) => Some((D::NorthWest, CursorIcon::ResizeNwSe)),
        (_, true, true, _) => Some((D::NorthEast, CursorIcon::ResizeNeSw)),
        (true, _, _, true) => Some((D::SouthWest, CursorIcon::ResizeNeSw)),
        (_, true, _, true) => Some((D::SouthEast, CursorIcon::ResizeNwSe)),
        (true, ..) => Some((D::West, CursorIcon::ResizeHorizontal)),
        (_, true, ..) => Some((D::East, CursorIcon::ResizeHorizontal)),
        (_, _, true, _) => Some((D::North, CursorIcon::ResizeVertical)),
        (_, _, _, true) => Some((D::South, CursorIcon::ResizeVertical)),
        _ => None,
    };
    if let Some((d, cursor)) = dir {
        ctx.set_cursor_icon(cursor);
        if ctx.input(|i| i.pointer.primary_pressed()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(d));
        }
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_state(true);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let p = if self.dark { Pal::dark() } else { Pal::light() };
        if self.themed != Some(self.dark) {
            apply_visuals(ctx, &p);
            self.themed = Some(self.dark);
        }

        // Closing the window (Alt+F4, taskbar) hides to the tray unless Quit was chosen.
        if self.persist
            && self.screenshot_path.is_none()
            && !shell::quitting()
            && ctx.input(|i| i.viewport().close_requested())
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide_to_tray();
        }
        if let Some(path) = &self.heartbeat {
            if self.last_hb.elapsed() >= Duration::from_secs(1) {
                self.last_hb = Instant::now();
                let line = format!("{} visible={}\n", self.started.elapsed().as_secs(), shell::window_visible());
                let _ = fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(line.as_bytes()));
            }
        }
        if let Some(n) = self.hide_after {
            if self.started.elapsed().as_secs() >= n {
                self.hide_after = None;
                shell::hide_window();
            }
        }
        if self.background_pending && self.started.elapsed() >= Duration::from_millis(600) {
            self.background_pending = false;
            shell::hide_window();
        }
        self.poll_inbox(ctx);
        while let Ok((id, t)) = self.thumb_rx.try_recv() {
            if let Some(d) = self.downloads.iter_mut().find(|d| d.id == id) {
                d.thumb = Some(Arc::new(t));
            }
        }
        self.poll_events();
        self.track_window(ctx);
        self.save_state(false);
        self.poll_probe();
        if self.auto_add
            && self.dialog.open
            && matches!(self.dialog.probe, ProbeState::Ready(..) | ProbeState::YtReady(_))
        {
            self.auto_add = false;
            self.add_download(true);
        }
        self.pump_queue();
        self.tick_demo();
        self.sample_speed();
        // While parked in the tray, tick about once a second: enough to keep downloads,
        // notifications and saving going, and gentle on the CPU.
        ctx.request_repaint_after(Duration::from_millis(if shell::in_tray() { 1000 } else { 120 }));

        self.ui_titlebar(ctx, &p);
        self.ui_sidebar(ctx, &p);
        self.ui_toolbar(ctx, &p);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(p.bg))
            .show(ctx, |ui| match self.page {
                Page::Downloads => self.ui_downloads(ui, &p),
                Page::Settings => self.ui_settings(ui, &p),
            });
        self.ui_add_dialog(ctx, &p);
        self.ui_delete_dialog(ctx, &p);
        self.ui_help_dialog(ctx, &p);
        window_frame_and_resize(ctx, &p);

        // ---- Screenshot mode ------------------------------------------------------
        if let Some(path) = self.screenshot_path.clone() {
            self.frames += 1;
            if self.frames == self.shot_frame {
                ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
            } else if self.frames > self.shot_frame {
                let image = ctx.input(|i| {
                    i.raw.events.iter().find_map(|e| {
                        if let egui::Event::Screenshot { image, .. } = e {
                            Some(image.clone())
                        } else {
                            None
                        }
                    })
                });
                if let Some(img) = image {
                    write_png(&path, &img);
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Write a ColorImage to a PNG file.
// ---------------------------------------------------------------------------

fn write_png(path: &str, img: &egui::ColorImage) {
    let (w, h) = (img.width() as u32, img.height() as u32);
    let mut buf: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::new(w, h);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let c = img[(x, y)];
            buf.put_pixel(x as u32, y as u32, Rgba([c.r(), c.g(), c.b(), c.a()]));
        }
    }
    if let Some(parent) = std::path::Path::new(path).parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = buf.save(path);
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> eframe::Result<()> {
    // Browser connector: Chrome, Edge and Brave start us with the extension's address as
    // the first argument (or with --native-host when run by hand).
    if let Some(first) = std::env::args().nth(1) {
        if first == "--native-host" || first.starts_with("chrome-extension://") {
            let allowed = first == "--native-host"
                || native::EXTENSION_IDS.iter().any(|id| first.contains(id));
            if allowed {
                native::run();
            }
            return Ok(());
        }
    }
    let mut o = Options {
        screenshot: None,
        demo: false,
        dark: false,
        open: None,
        cat: None,
        url: None,
        setup: Vec::new(),
        no_auto_setup: false,
        auto_add: false,
        wait_frames: 120,
        persist: false,
        heartbeat: None,
        hide_after: None,
        background: false,
    };
    let mut force_persist = false; // developer option: keep state even in screenshot runs
    let mut multi = false; // developer option: allow a second copy
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--screenshot" => o.screenshot = args.next(),
            "--demo" => o.demo = true,
            "--dark" => o.dark = true,
            "--open" => o.open = args.next(),
            "--view" => o.cat = args.next(),
            "--url" => o.url = args.next(),
            "--setup-ytdlp" => o.setup.push(Tool::YtDlp),
            "--setup-ffmpeg" => o.setup.push(Tool::Ffmpeg),
            "--auto-add" => o.auto_add = true,
            "--no-auto-setup" => o.no_auto_setup = true,
            "--persist" => force_persist = true,
            "--multi" => multi = true,
            "--background" => o.background = true,
            "--heartbeat" => o.heartbeat = args.next(),
            "--hide-after" => o.hide_after = args.next().and_then(|v| v.parse().ok()),
            "--wait" => {
                o.wait_frames = args.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(15) * 8;
            }
            _ => {}
        }
    }

    // Only a normal launch keeps state; screenshot and demo runs must not touch it.
    o.persist = force_persist || (o.screenshot.is_none() && !o.demo && o.open.is_none());
    // One copy only: a second launch just brings the running one (maybe in the tray) forward.
    if o.persist && !multi && shell::already_running(o.background) {
        return Ok(());
    }
    let saved_win = if o.persist {
        state::load().and_then(|s| s.window).unwrap_or_default()
    } else {
        state::SavedWindow::default()
    };
    let background = o.background && o.persist;
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([saved_win.w.max(960.0), saved_win.h.max(600.0)])
        .with_maximized(saved_win.maximized && !background)
        .with_min_inner_size([960.0, 600.0])
        .with_decorations(false)
        .with_icon(window_icon())
        .with_title("Orexa Downloader");
    if background {
        // Started by the browser: begin out of sight, then settle in the tray.
        viewport = viewport.with_position([-32000.0, -32000.0]);
    }
    let native_options = eframe::NativeOptions {
        // Open in the middle of the screen.
        centered: !background,
        viewport,
        ..Default::default()
    };

    eframe::run_native(
        "Orexa Downloader",
        native_options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, o)))),
    )
}
