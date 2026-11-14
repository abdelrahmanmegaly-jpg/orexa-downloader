// Run as a Windows GUI program so no console (CMD) window opens with the app.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod checksum;
mod cleanup;
mod i18n;
mod multipart;
mod native;
mod relink;
mod shell;
mod schedule;
mod state;
mod throttle;
mod torrent;
mod update;
mod web;
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
    /// A torrent: which files to download (numbers in the torrent's own order). None = all.
    files: Option<Vec<usize>>,
    /// A finished torrent that is still sharing: (bytes uploaded, upload speed).
    seeding: Option<(u64, f64)>,
    /// What is happening right now (not saved): connections, speed history, event log.
    live: LiveInfo,
}

/// Live details of a download, shown in its details window.
#[derive(Clone, Default)]
struct LiveInfo {
    /// Connections in use right now.
    conns: usize,
    /// Speed samples, one every half second.
    hist: Vec<f32>,
    /// Timestamped events: starts, retries, pauses, failures.
    log: Vec<String>,
    /// The server is not answering; the next try is at this moment.
    retry_at: Option<Instant>,
}

impl LiveInfo {
    fn note(&mut self, msg: impl AsRef<str>) {
        let t = chrono::Local::now().format("%H:%M:%S");
        self.log.push(format!("{}  {}", t, msg.as_ref()));
        if self.log.len() > 80 {
            self.log.remove(0);
        }
    }
}

/// Referer, cookies and browser identity sent along with a download.
#[derive(Clone, Default)]
struct Extra {
    referer: Option<String>,
    cookie: Option<String>,
    user_agent: Option<String>,
    /// The link came from the browser, which can download it itself if the site refuses Orexa.
    from_browser: bool,
}

impl Download {
    /// A magnet link or .torrent file, downloaded by the torrent engine.
    fn is_torrent(&self) -> bool {
        matches!(self.source, Source::Torrent)
    }
}

/// Answers from the update threads.
enum UpdateMsg {
    Checked(Result<Option<update::Release>, String>),
    Downloaded(Result<PathBuf, String>),
}

/// What to do when every download has finished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum AfterFinish {
    Nothing,
    Folder,
    Sleep,
}

impl AfterFinish {
    fn code(self) -> &'static str {
        match self {
            AfterFinish::Nothing => "",
            AfterFinish::Folder => "folder",
            AfterFinish::Sleep => "sleep",
        }
    }
    fn from_code(c: &str) -> Self {
        match c {
            "folder" => AfterFinish::Folder,
            "sleep" => AfterFinish::Sleep,
            _ => AfterFinish::Nothing,
        }
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
    if torrent::is_torrent_file(url) {
        return std::path::Path::new(url.trim())
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Torrent".to_string());
    }
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

/// `name`, or "name (1).ext", "name (2).ext"... when something with that name is already there.
fn unique_name(dir: &str, name: &str, in_use: &dyn Fn(&str) -> bool) -> String {
    let dir = if dir.trim().is_empty() { downloads_dir() } else { PathBuf::from(dir) };
    let taken = |n: &str| in_use(n) || dir.join(n).exists();
    if !taken(name) {
        return name.to_string();
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() && e.len() <= 8 && !e.contains(' ') => (s.to_string(), format!(".{}", e)),
        _ => (name.to_string(), String::new()),
    };
    for i in 1..1000 {
        let candidate = format!("{} ({}){}", stem, i, ext);
        if !taken(&candidate) {
            return candidate;
        }
    }
    name.to_string()
}

/// The folder name for a kind of file, used when "sort into folders" is on.
fn type_folder(file_name: &str, is_torrent: bool) -> &'static str {
    if is_torrent {
        return "Torrents";
    }
    match ext_cat(&ext_of(file_name)) {
        Some(Category::Compressed) => "Compressed",
        Some(Category::Documents) => "Documents",
        Some(Category::Music) => "Music",
        Some(Category::Programs) => "Programs",
        Some(Category::Video) => "Videos",
        _ => "Other",
    }
}

/// The same link, ignoring a trailing slash, a #fragment and upper/lower case of the address.
fn same_url(a: &str, b: &str) -> bool {
    let norm = |u: &str| u.trim().split('#').next().unwrap_or("").trim_end_matches('/').to_lowercase();
    !a.trim().is_empty() && norm(a) == norm(b)
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
    /// A finished torrent that is still sharing: (bytes uploaded, upload speed); None = stopped.
    Seeding(u64, Option<(u64, f64)>),
    Progress(u64, u64, u64, f64), // id, downloaded, size, speed
    State(u64, Status, Option<String>),
    FileName(u64, String),
    /// A line for the download's event log.
    Log(u64, String),
    /// How many connections the download uses right now.
    Conns(u64, usize),
    /// The site refused Orexa but would let the browser download it: give it back.
    HandBack(u64),
    /// The server is not answering: the next try is in this many seconds (None = answering).
    Waiting(u64, Option<u64>),
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
        if let Some(msg) = space_problem(&d) {
            let _ = self.ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(msg)));
            return;
        }
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

/// Not enough free space for what is left to download? Say so before starting.
fn space_problem(d: &Download) -> Option<String> {
    if d.size == 0 || d.is_torrent() {
        return None;
    }
    let dir = if d.save_dir.is_empty() { downloads_dir() } else { PathBuf::from(&d.save_dir) };
    let have = fs::metadata(dir.join(&d.file_name)).map(|m| m.len()).unwrap_or(0).min(d.size);
    let need = d.size - have;
    let free = shell::free_space(&dir)?;
    if free >= need {
        return None;
    }
    let drive = dir.to_string_lossy().chars().take(2).collect::<String>();
    Some(format!(
        "Not enough free space on {}: it needs {} and only {} is free.",
        drive,
        fmt_size(need),
        fmt_size(free)
    ))
}

/// What a wait between tries ended with.
enum Waited {
    Again,
    /// Stopped (back to the queue): the worker ends.
    Stopped,
}

/// The server is not answering: say when the next try is, then wait for it. Pause holds the
/// wait until Resume; Stop ends it.
fn wait_for_server(d: &Download, secs: u64, cmd_rx: &Receiver<WorkerCmd>, ev_tx: &Sender<EngineEvent>) -> Waited {
    let _ = ev_tx.send(EngineEvent::Waiting(d.id, Some(secs)));
    let _ = ev_tx.send(EngineEvent::Log(d.id, format!("The server is not answering, trying again in {} s", secs)));
    let end = Instant::now() + Duration::from_secs(secs);
    let mut paused = false;
    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(WorkerCmd::Stop) => {
                let _ = ev_tx.send(EngineEvent::Waiting(d.id, None));
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Queued, None));
                return Waited::Stopped;
            }
            Ok(WorkerCmd::Pause) => {
                paused = true;
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Paused, None));
            }
            Ok(WorkerCmd::Run) => {
                // Resume tries right away.
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Downloading, None));
                return Waited::Again;
            }
            Err(_) => {}
        }
        if !paused && Instant::now() >= end {
            return Waited::Again;
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

    // A server that goes silent for this long counts as not answering (each read, not the
    // whole download).
    let client = match reqwest::blocking::Client::builder()
        .user_agent(
            d.extra
                .user_agent
                .clone()
                .unwrap_or_else(|| web::DEFAULT_UA.to_string()),
        )
        .timeout(Duration::from_secs(45))
        .connect_timeout(Duration::from_secs(20))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
            return;
        }
    };

    // Waits so far while the server was not answering (reset once data flows again).
    let mut waits = 0usize;
    let wait = |waits: &mut usize, ev_tx: &Sender<EngineEvent>| -> Option<Waited> {
        let secs = *web::PATIENT_WAITS.get(*waits)?;
        *waits += 1;
        Some(wait_for_server(&d, secs, &cmd_rx, ev_tx))
    };

    // Big files on servers that allow byte ranges: several connections at once.
    loop {
        match multipart::run(&d, &client, &path, &cmd_rx, &ev_tx) {
            multipart::Outcome::Handled => return,
            multipart::Outcome::Fallback => break,
            multipart::Outcome::Unreachable => match wait(&mut waits, &ev_tx) {
                Some(Waited::Again) => continue,
                Some(Waited::Stopped) => return,
                None => {
                    let _ = ev_tx.send(EngineEvent::Waiting(d.id, None));
                    let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(web::NOT_ANSWERING.into())));
                    return;
                }
            },
        }
    }

    let mut first = true;
    'connect: loop {
        // Resume support: keep what is already on disk and ask for the rest.
        let mut downloaded: u64 = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if d.size > 0 && downloaded >= d.size {
            let _ = ev_tx.send(EngineEvent::Waiting(d.id, None));
            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Complete, None));
            return;
        }

        // Asked like a browser; a refusal is tried again with another Referer.
        let range = (downloaded > 0).then(|| format!("bytes={}-", downloaded));
        let sent = web::get_tolerant(&client, &d.url, d.extra.referer.as_deref(), d.extra.cookie.as_deref(), range.as_deref())
            .map(|(r, _)| r);

        // Not answering, or answering "busy, try later": wait and try again.
        let trouble = match &sent {
            Err(e) => web::unreachable(e),
            Ok(r) => matches!(r.status().as_u16(), 500 | 502..=504),
        };
        if trouble {
            match wait(&mut waits, &ev_tx) {
                Some(Waited::Again) => continue 'connect,
                Some(Waited::Stopped) => return,
                None => {
                    let _ = ev_tx.send(EngineEvent::Waiting(d.id, None));
                    let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(web::NOT_ANSWERING.into())));
                    return;
                }
            }
        }

        let mut resp = match sent {
            Ok(r) if r.status().is_success() || r.status().as_u16() == 206 => r,
            Ok(r) => {
                // Refused before anything arrived, for a link the browser gave us: the browser,
                // with its own sign-in, can still download it.
                if matches!(r.status().as_u16(), 401 | 403) && downloaded == 0 && d.extra.from_browser {
                    let _ = ev_tx.send(EngineEvent::HandBack(d.id));
                    return;
                }
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(web::status_message(r.status()))));
                return;
            }
            Err(e) => {
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
                return;
            }
        };
        if waits > 0 {
            waits = 0;
            let _ = ev_tx.send(EngineEvent::Waiting(d.id, None));
            let _ = ev_tx.send(EngineEvent::Log(d.id, "The server is answering again".into()));
        }

        // Asked for the rest but the server sends the whole file: start over.
        if downloaded > 0 && resp.status().as_u16() != 206 {
            downloaded = 0;
        }
        let size = if d.size > 0 {
            d.size
        } else {
            resp.content_length().map(|l| l + downloaded).unwrap_or(0)
        };

        // Starting from the beginning: empty the file first. (A file opened for appending
        // cannot be shortened on Windows, so it is created anew.)
        if downloaded == 0 {
            if let Err(e) = fs::File::create(&path) {
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
                return;
            }
        }
        let mut file = match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(f) => f,
            Err(e) => {
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e.to_string())));
                return;
            }
        };

        let _ = ev_tx.send(EngineEvent::Progress(d.id, downloaded, size, 0.0));
        let _ = ev_tx.send(EngineEvent::Conns(d.id, 1));
        if first {
            first = false;
            let _ = ev_tx.send(EngineEvent::Log(
                d.id,
                if downloaded > 0 {
                    format!("Continued with one connection from {}", fmt_size(downloaded))
                } else {
                    "Started with one connection".to_string()
                },
            ));
        }

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
                    // Ended early: the connection was cut. Continue from here.
                    if size > 0 && downloaded < size {
                        let _ = ev_tx.send(EngineEvent::Log(d.id, "The connection was cut, continuing".into()));
                        continue 'connect;
                    }
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
                    throttle::take(n);

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
                    let _ = ev_tx.send(EngineEvent::Progress(d.id, downloaded, size, 0.0));
                    // Cut off or silent: wait for the server, then continue from here.
                    let _ = ev_tx.send(EngineEvent::Log(d.id, format!("The connection was interrupted: {}", e)));
                    match wait(&mut waits, &ev_tx) {
                        Some(Waited::Again) => continue 'connect,
                        Some(Waited::Stopped) => return,
                        None => {
                            let _ = ev_tx.send(EngineEvent::Waiting(d.id, None));
                            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(web::NOT_ANSWERING.into())));
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Demo data (only with --demo, used for screenshots)
// ---------------------------------------------------------------------------

/// Give the demo downloads some history so their details windows have something to show.
fn demo_with_details(mut v: Vec<Download>) -> Vec<Download> {
    for d in v.iter_mut().filter(|d| !d.is_torrent()) {
        d.live.note("Started with 8 connections, the file is split into 48 parts");
        if d.status == Status::Downloading {
            d.live.conns = 8;
            d.live.note("Part 12 was interrupted, trying again (1 of 4)");
            d.live.note("Part 31 was interrupted, trying again (1 of 4)");
            for i in 0..80 {
                let w = ((i as f32) * 0.35).sin() * 0.25 + 0.7 + ((i * 7 % 11) as f32) * 0.012;
                d.live.hist.push(d.speed_bps as f32 * w);
            }
        }
    }
    v
}

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
            files: None,
            seeding: None,
            live: Default::default(),
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
        {
            let mut t = mk(10, "Big Buck Bunny", 276 * MB, 276 * MB, Status::Complete, 0.0, None);
            t.source = Source::Torrent;
            t.url = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny".into();
            t.seeding = Some((340 * MB, 1.2 * MB as f64));
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
#[cfg(has_thmanyah)]
static AR_REGULAR: &[u8] = include_bytes!("../assets/fonts/thmanyahsans-Regular.otf");
#[cfg(has_thmanyah)]
static AR_MEDIUM: &[u8] = include_bytes!("../assets/fonts/thmanyahsans-Medium.otf");
#[cfg(has_thmanyah)]
static AR_BOLD: &[u8] = include_bytes!("../assets/fonts/thmanyahsans-Bold.otf");
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
    // The brand font has no Arabic letters. Arabic is drawn in Thmanyah; the
    // Windows fonts that have Arabic come after it as a safety net.
    #[allow(unused_mut)]
    let mut thmanyah: [Option<&str>; 3] = [None, None, None]; // regular, medium, bold
    #[cfg(has_thmanyah)]
    {
        for (key, bytes, slot) in [("th_regular", AR_REGULAR, 0), ("th_medium", AR_MEDIUM, 1), ("th_bold", AR_BOLD, 2)] {
            fonts.font_data.insert(key.into(), egui::FontData::from_static(bytes).into());
            thmanyah[slot] = Some(key);
        }
    }
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
    let family = |first: &str, bold: bool, th: usize| {
        let mut v = vec![first.to_string()];
        if let Some(k) = thmanyah[th] {
            v.push(k.to_string());
        }
        if bold {
            v.extend(arabic_bold.iter().cloned());
        }
        v.extend(arabic_regular.iter().cloned());
        v.extend(fallback.iter().cloned());
        v
    };
    fonts.families.insert(FontFamily::Proportional, family("pjs_regular", false, 0));
    fonts.families.insert(FontFamily::Name("medium".into()), family("pjs_medium", false, 1));
    fonts.families.insert(FontFamily::Name("semibold".into()), family("pjs_semibold", true, 2));
    fonts.families.insert(FontFamily::Name("bold".into()), family("pjs_bold", true, 2));
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
    RichText::new(tr(&s.into())).font(font).color(color)
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

/// The joined-letter forms for letters that stand alone (and the lam-alef
/// ligatures) are missing from the Thmanyah font. A standalone letter is drawn
/// correctly by its ordinary code point, and lam-alef by joining lam to alef.
fn forms_for_font(s: &str) -> String {
    fn base(c: u32) -> &'static [u32] {
        match c {
        0xFE70 => &[0x064B],
        0xFE71 => &[0x0640, 0x064B],
        0xFE72 => &[0x064C],
        0xFE74 => &[0x064D],
        0xFE76 => &[0x064E],
        0xFE77 => &[0x0640, 0x064E],
        0xFE78 => &[0x064F],
        0xFE79 => &[0x0640, 0x064F],
        0xFE7A => &[0x0650],
        0xFE7B => &[0x0640, 0x0650],
        0xFE7C => &[0x0651],
        0xFE7D => &[0x0640, 0x0651],
        0xFE7E => &[0x0652],
        0xFE7F => &[0x0640, 0x0652],
        0xFE80 => &[0x0621],
        0xFE81 => &[0x0622],
        0xFE83 => &[0x0623],
        0xFE85 => &[0x0624],
        0xFE87 => &[0x0625],
        0xFE89 => &[0x0626],
        0xFE8D => &[0x0627],
        0xFE8F => &[0x0628],
        0xFE93 => &[0x0629],
        0xFE95 => &[0x062A],
        0xFE99 => &[0x062B],
        0xFE9D => &[0x062C],
        0xFEA1 => &[0x062D],
        0xFEA5 => &[0x062E],
        0xFEA9 => &[0x062F],
        0xFEAB => &[0x0630],
        0xFEAD => &[0x0631],
        0xFEAF => &[0x0632],
        0xFEB1 => &[0x0633],
        0xFEB5 => &[0x0634],
        0xFEB9 => &[0x0635],
        0xFEBD => &[0x0636],
        0xFEC1 => &[0x0637],
        0xFEC5 => &[0x0638],
        0xFEC9 => &[0x0639],
        0xFECD => &[0x063A],
        0xFED1 => &[0x0641],
        0xFED5 => &[0x0642],
        0xFED9 => &[0x0643],
        0xFEDD => &[0x0644],
        0xFEE1 => &[0x0645],
        0xFEE5 => &[0x0646],
        0xFEE9 => &[0x0647],
        0xFEED => &[0x0648],
        0xFEEF => &[0x0649],
        0xFEF1 => &[0x064A],
        0xFEF5 => &[0xFEDF, 0xFE82],
        0xFEF6 => &[0xFEE0, 0xFE82],
        0xFEF7 => &[0xFEDF, 0xFE84],
        0xFEF8 => &[0xFEE0, 0xFE84],
        0xFEF9 => &[0xFEDF, 0xFE88],
        0xFEFA => &[0xFEE0, 0xFE88],
        0xFEFB => &[0xFEDF, 0xFE8E],
        0xFEFC => &[0xFEE0, 0xFE8E],
            _ => &[],
        }
    }
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let b = base(c as u32);
        if b.is_empty() {
            out.push(c);
        } else {
            out.extend(b.iter().filter_map(|&x| char::from_u32(x)));
        }
    }
    out
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
    reorder_visual(&forms_for_font(&ar_reshaper::reshape_line(s)))
}

/// For text written in the interface language: same joining, but the sentence
/// reads right to left, so Latin words inside it stay where they belong.
fn shape_rtl(s: &str) -> String {
    // Split into right-to-left stretches (Arabic) and left-to-right stretches
    // (numbers, units, Latin names). A space or punctuation mark between two
    // left-to-right pieces belongs to them ("3.1 GB", "02:23"); anywhere else it
    // follows the sentence direction.
    let chars: Vec<char> = s.chars().collect();
    // 0 = Arabic/right-to-left, 1 = left-to-right, 2 = neutral
    let mut kind: Vec<u8> = chars
        .iter()
        .map(|&c| {
            if has_rtl(&c.to_string()) {
                0
            } else if c.is_alphanumeric() || c == '%' {
                1
            } else {
                2
            }
        })
        .collect();
    let mut i = 0;
    while i < kind.len() {
        if kind[i] != 2 {
            i += 1;
            continue;
        }
        let start = i;
        while i < kind.len() && kind[i] == 2 {
            i += 1;
        }
        let left = if start > 0 { Some(kind[start - 1]) } else { None };
        let right = if i < kind.len() { Some(kind[i]) } else { None };
        let k = if left == Some(1) && right == Some(1) { 1 } else { 0 };
        for x in &mut kind[start..i] {
            *x = k;
        }
    }
    let mut segs: Vec<(u8, String)> = Vec::new();
    for (c, k) in chars.iter().zip(kind.iter()) {
        match segs.last_mut() {
            Some((sk, text)) if sk == k => text.push(*c),
            _ => segs.push((*k, c.to_string())),
        }
    }
    let mut out = String::with_capacity(s.len());
    for (k, text) in segs.into_iter().rev() {
        if k == 1 {
            out.push_str(&text);
        } else {
            let joined = forms_for_font(&ar_reshaper::reshape_line(&text));
            out.extend(joined.chars().rev().map(|c| match c {
                '(' => ')',
                ')' => '(',
                '[' => ']',
                ']' => '[',
                '«' => '»',
                '»' => '«',
                c => c,
            }));
        }
    }
    out
}

/// Interface text ready to draw: translated into the chosen language, joined and
/// put in visual order. File names and other user text pass through `shape`.
fn tr(s: &str) -> String {
    match i18n::translate(s) {
        Some(t) => shape_rtl(&t),
        // Arabic mixed into interface text (a file's type, a name) reads right to left.
        None if rtl() && has_rtl(s) => shape_rtl(s),
        None => shape(s),
    }
}

/// A label that wraps to the space it has. Arabic is wrapped before it is put in
/// visual order, so every line still reads correctly.
fn lbl(ui: &mut Ui, s: impl Into<String>, font: FontId, color: Color32) -> Response {
    let s: String = s.into();
    if i18n::get() == i18n::Lang::Ar {
        if let Some(t) = i18n::translate(&s) {
            let max_w = ui.available_width().max(60.0);
            let mut lines: Vec<String> = Vec::new();
            let mut cur = String::new();
            for w in t.split(' ') {
                let cand = if cur.is_empty() { w.to_string() } else { format!("{} {}", cur, w) };
                if !cur.is_empty() && text_w_raw(ui, &shape_rtl(&cand), font.clone()) > max_w {
                    lines.push(std::mem::take(&mut cur));
                    cur = w.to_string();
                } else {
                    cur = cand;
                }
            }
            lines.push(cur);
            let text = lines.iter().map(|l| shape_rtl(l)).collect::<Vec<_>>().join("\n");
            return ui.label(RichText::new(text).font(font).color(color));
        }
    }
    ui.label(rt(s, font, color))
}

fn text_w_raw(ui: &Ui, shaped: &str, font: FontId) -> f32 {
    ui.fonts(|fo| fo.layout_no_wrap(shaped.to_string(), font, Color32::WHITE).size().x)
}

// ---------------------------------------------------------------------------
// Right-to-left layout. Custom-painted parts are laid out left to right and
// mirrored inside the box they are drawn in; ordinary egui layouts swap their
// direction.
// ---------------------------------------------------------------------------

fn rtl() -> bool {
    i18n::get() == i18n::Lang::Ar
}

/// Mirror an x position inside `outer`.
fn mx(outer: Rect, x: f32) -> f32 {
    if rtl() {
        outer.left() + outer.right() - x
    } else {
        x
    }
}

/// Mirror a rectangle inside `outer`.
fn mrect(outer: Rect, r: Rect) -> Rect {
    if rtl() {
        Rect::from_min_max(pos2(mx(outer, r.max.x), r.min.y), pos2(mx(outer, r.min.x), r.max.y))
    } else {
        r
    }
}

/// Swap left and right in a text anchor.
fn malign(a: Align2) -> Align2 {
    if !rtl() {
        return a;
    }
    Align2([
        match a.0[0] {
            Align::Min => Align::Max,
            Align::Max => Align::Min,
            c => c,
        },
        a.0[1],
    ])
}

/// A row of widgets that starts on the reading side (left, or right in Arabic).
fn hrow<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    if rtl() {
        ui.horizontal(|ui| ui.with_layout(Layout::right_to_left(Align::Center), add).inner).inner
    } else {
        ui.horizontal(add).inner
    }
}

/// A column whose lines start on the reading side.
fn col<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    ui.with_layout(Layout::top_down(start_align()), add).inner
}

/// The layout that fills a row from its end: right to left, or left to right in Arabic.
fn rl_layout(a: Align) -> Layout {
    if rtl() {
        Layout::left_to_right(a)
    } else {
        Layout::right_to_left(a)
    }
}

/// Where a layout's content starts: the reading side of the page.
fn start_align() -> Align {
    if rtl() {
        Align::Max
    } else {
        Align::Min
    }
}

/// Painter text that goes through the translation layer.
trait PainterTr {
    fn txt(&self, pos: egui::Pos2, anchor: Align2, text: impl AsRef<str>, font: FontId, color: Color32) -> Rect;
    /// Like `txt`, but the position was worked out left to right inside `outer`.
    fn txtm(&self, outer: Rect, pos: egui::Pos2, anchor: Align2, text: impl AsRef<str>, font: FontId, color: Color32) -> Rect;
    /// A ready-made galley placed left to right inside `outer`.
    fn galley_m(&self, outer: Rect, pos: egui::Pos2, g: Arc<egui::Galley>, color: Color32);
}
impl PainterTr for egui::Painter {
    fn txt(&self, pos: egui::Pos2, anchor: Align2, text: impl AsRef<str>, font: FontId, color: Color32) -> Rect {
        self.text(pos, anchor, tr(text.as_ref()), font, color)
    }
    fn txtm(&self, outer: Rect, pos: egui::Pos2, anchor: Align2, text: impl AsRef<str>, font: FontId, color: Color32) -> Rect {
        self.text(pos2(mx(outer, pos.x), pos.y), malign(anchor), tr(text.as_ref()), font, color)
    }
    fn galley_m(&self, outer: Rect, pos: egui::Pos2, g: Arc<egui::Galley>, color: Color32) {
        let x = if rtl() { mx(outer, pos.x) - g.size().x } else { pos.x };
        self.galley(pos2(x, pos.y), g, color);
    }
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
    reorder_visual(&forms_for_font(&cfg.to_reshaper().reshape(s)))
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
        "info" => svg!(r#"<circle cx="12" cy="12" r="10"/><path d="M12 16v-4"/><path d="M12 8h.01"/>"#),
        "globe" => svg!(r#"<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20"/><path d="M2 12h20"/>"#),
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

            hrow(ui, |ui| {
                let (r, _) = ui.allocate_exact_size(vec2(40.0, 37.0), Sense::hover());
                paint_logo(ui, r);
                ui.add_space(6.0);
                ui.with_layout(Layout::top_down(start_align()), |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    lbl(ui, "Orexa Downloader", fb(18.0), p.text);
                    lbl(ui, format!("Version {}", env!("CARGO_PKG_VERSION")), f(13.0), p.muted);
                });
            });
            ui.add_space(10.0);
            lbl(ui, "About", fs(14.0), p.text);
            lbl(ui,
                "A fast, simple download manager. Save files, web videos, YouTube and torrents, with one click from your browser.",
                f(13.0),
                p.sub,
            );

            ui.add_space(12.0);
            ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, p.border));
            ui.add_space(12.0);
            lbl(ui, "License", fs(14.0), p.text);
            egui::Frame::none()
                .fill(p.accent_soft)
                .rounding(8.0)
                .inner_margin(Margin::symmetric(10.0, 4.0))
                .show(ui, |ui| {
                    lbl(ui, "Free to use", fs(13.0), p.accent_ink);
                });
            lbl(ui, "Orexa Downloader is free to use.", f(13.0), p.sub);

            ui.add_space(12.0);
            ui.painter().hline(ui.min_rect().x_range(), ui.cursor().top(), Stroke::new(1.0, p.border));
            ui.add_space(12.0);
            hrow(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                lbl(ui, "Made with", f(13.0), p.sub);
                let (r, _) = ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
                paint_icon(ui, r, "heart", Color32::from_rgb(0xE5, 0x48, 0x4D));
            });
            lbl(ui, "by Orexa Software from Egypt", f(13.0), p.sub);
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
    ui.fonts(|fo| fo.layout_no_wrap(tr(s), font, Color32::WHITE).size().x)
}

fn trunc(ui: &Ui, s: &str, font: FontId, color: Color32, max_w: f32) -> Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::simple(tr(s), font, color, max_w.max(10.0));
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
        paint_icon(ui, mrect(rect, Rect::from_min_size(pos2(x, rect.center().y - 9.0), vec2(18.0, 18.0))), name, fg);
        x += 26.0;
    }
    ui.painter().txtm(rect, pos2(x, rect.center().y), Align2::LEFT_CENTER, label, font, fg);
    resp
}

fn icon_button_at(ui: &Ui, p: &Pal, rect: Rect, icon: &str, id: egui::Id, tip: &str) -> bool {
    let r = ui.interact(rect, id, Sense::click());
    if r.hovered() {
        ui.painter().rect_filled(rect, 8.0, p.tile);
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    paint_icon(ui, Rect::from_center_size(rect.center(), vec2(18.0, 18.0)), icon, p.sub);
    r.on_hover_text(tr(tip)).clicked()
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
    paint_icon(ui, mrect(rect, Rect::from_min_size(pos2(x0, rect.center().y - 8.0), vec2(16.0, 16.0))), icon, p.sub);
    ui.painter().txtm(rect, pos2(x0 + 22.0, rect.center().y), Align2::LEFT_CENTER, label, fs(14.0), p.text);
    r.clicked()
}

/// A sidebar row whose text is used as given (language names are never translated).
fn nav_item_raw(ui: &mut Ui, p: &Pal, icon: &str, label: &str, selected: bool) -> Response {
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
    if !icon.is_empty() {
        paint_icon(ui, mrect(rect, Rect::from_min_size(pos2(rect.left() + 12.0, rect.center().y - 10.0), vec2(20.0, 20.0))), icon, fg);
    }
    ui.painter().text(
        pos2(mx(rect, rect.left() + 44.0), rect.center().y),
        malign(Align2::LEFT_CENTER),
        shape(label),
        if selected { fs(14.0) } else { f(14.0) },
        if selected { p.accent_ink } else { p.text },
    );
    resp
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
    paint_icon(ui, mrect(rect, Rect::from_min_size(pos2(rect.left() + 12.0, rect.center().y - 10.0), vec2(20.0, 20.0))), icon, fg);
    ui.painter().txt(
        pos2(mx(rect, rect.left() + 44.0), rect.center().y),
        malign(Align2::LEFT_CENTER),
        label,
        if selected { fs(14.0) } else { f(14.0) },
        if selected { p.accent_ink } else { p.text },
    );
    if let Some(c) = count {
        ui.painter().txt(
            pos2(mx(rect, rect.right() - 12.0), rect.center().y),
            malign(Align2::RIGHT_CENTER),
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
    // Centred on its real height (measured last frame), so a taller dialog is never pushed
    // past the bottom of the window.
    let h_id = egui::Id::new((id, "height"));
    let height = ctx.data(|d| d.get_temp::<f32>(h_id)).unwrap_or(typical_height);
    let pos = pos2(
        snap(((screen.width() - full_w) / 2.0).max(8.0)),
        snap(((screen.height() - height) / 2.0).max(16.0)),
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
                    ui.with_layout(Layout::top_down(start_align()), add);
                });
        });
    let measured = dialog.response.rect.height();
    if (measured - height).abs() > 0.5 {
        ctx.data_mut(|d| d.insert_temp(h_id, measured));
        ctx.request_repaint();
    }
    // Developer aid: log the dialog's rectangle every frame to spot layout jitter.
    if let Some(path) = std::env::var_os("OREXA_DEBUG_LAYOUT") {
        let r = dialog.response.rect;
        let line = format!("{} x={:.1} y={:.1} w={:.1} h={:.1}\n", id, r.min.x, r.min.y, r.width(), r.height());
        let _ = fs::OpenOptions::new().create(true).append(true).open(path).and_then(|mut f| f.write_all(line.as_bytes()));
    }
}

/// A list of a torrent's files with a checkbox each, "All" / "None", and the total chosen.
fn torrent_file_list(ui: &mut Ui, p: &Pal, files: &[(String, u64)], sel: &mut Vec<bool>, id: &str, max_h: f32) {
    pick_list(ui, p, "Files", files, sel, id, max_h, fmt_size);
}

/// A list with a tick box per item, "All" / "None", and how much is chosen. `show` turns an
/// item's number into text (a size for files, a length for videos).
#[allow(clippy::too_many_arguments)]
fn pick_list(
    ui: &mut Ui,
    p: &Pal,
    label: &str,
    files: &[(String, u64)],
    sel: &mut Vec<bool>,
    id: &str,
    max_h: f32,
    show: fn(u64) -> String,
) {
    if sel.len() != files.len() {
        *sel = vec![true; files.len()];
    }
    let picked = sel.iter().filter(|b| **b).count();
    let picked_size: u64 = files.iter().zip(sel.iter()).filter(|(_, b)| **b).map(|(f, _)| f.1).sum();
    hrow(ui, |ui| {
        lbl(ui, label, fs(13.0), p.sub);
        lbl(ui, format!("{} of {} chosen, {}", picked, files.len(), show(picked_size)), f(13.0), p.muted);
        ui.with_layout(rl_layout(Align::Center), |ui| {
            if button(ui, p, "None", None, Kind::Secondary, 28.0, true).clicked() {
                sel.iter_mut().for_each(|b| *b = false);
            }
            if button(ui, p, "All", None, Kind::Secondary, 28.0, true).clicked() {
                sel.iter_mut().for_each(|b| *b = true);
            }
        });
    });
    ui.add_space(6.0);
    egui::Frame::none()
        .fill(p.bg)
        .stroke(Stroke::new(1.0, p.border))
        .rounding(10.0)
        .inner_margin(Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt(id).max_height(max_h).auto_shrink([false, true]).show(ui, |ui| {
                for (i, (name, size)) in files.iter().enumerate() {
                    hrow(ui, |ui| {
                        ui.checkbox(&mut sel[i], "");
                        let w = (ui.available_width() - 84.0).max(60.0);
                        let g = trunc(ui, name, f(13.0), p.text, w);
                        let (r, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
                        let gy = g.size().y / 2.0;
                        ui.painter().galley_m(r, r.left_center() - vec2(0.0, gy), g, p.text);
                        ui.with_layout(rl_layout(Align::Center), |ui| {
                            lbl(ui, show(*size), f(12.0), p.muted);
                        });
                    });
                }
            });
        });
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

enum Act {
    /// Click on a row, with the modifier keys held: 1 = Ctrl, 2 = Shift.
    Select(u64, u8),
    BulkPause,
    BulkResume,
    BulkRemove,
    ClearSelection,
    Details(u64),
    StopSeeding(u64),
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
        let m = ui.input(|i| i.modifiers);
        acts.push(Act::Select(d.id, (m.command as u8) | ((m.shift as u8) << 1)));
    }
    if resp.double_clicked() {
        acts.push(Act::Details(d.id));
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
    let tile_l = Rect::from_min_size(pos2(rect.left() + 20.0, cy - 22.0), vec2(44.0, 44.0)); // before mirroring
    let tile = mrect(rect, tile_l);
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
    let trash_l = Rect::from_min_size(pos2(rr - 36.0, cy - 18.0), vec2(36.0, 36.0));
    let folder_l = trash_l.translate(vec2(-40.0, 0.0));
    // Every row reserves the same action slot so columns line up. A torrent has one more button.
    let shift = 40.0;
    let slot_left = folder_l.left() - 8.0 - 92.0 - shift;
    let main_l = if big {
        Rect::from_min_size(pos2(slot_left, cy - 18.0), vec2(92.0, 36.0))
    } else {
        folder_l.translate(vec2(-40.0 - shift, 0.0))
    };
    let trash = mrect(rect, trash_l);
    let folder = mrect(rect, folder_l);
    let main = mrect(rect, main_l);
    let info_btn = mrect(rect, folder_l.translate(vec2(-40.0, 0.0)));
    let col_w = 170.0;
    let col_x = slot_left - 16.0 - col_w;
    let mid_x = tile_l.right() + 16.0;
    let mid_w = (col_x - 16.0 - mid_x).max(120.0);

    // Name, progress, meta
    let cat_name = if d.is_torrent() { "Torrent" } else { ext_cat(&d.ext).map(|c| c.label()).unwrap_or("Other") };
    let time = d.added.split(' ').nth(1).unwrap_or("");
    let meta = if d.status == Status::Complete {
        format!("{}, {}, {}", fmt_size(d.size), i18n::translate(cat_name).unwrap_or_else(|| cat_name.to_string()), time)
    } else if d.size == 0 {
        "Size unknown".to_string()
    } else {
        let got = if d.downloaded == 0 { "0 MB".to_string() } else { fmt_size(d.downloaded) };
        let mut m = format!("{} of {}, {:.0}%", got, fmt_size(d.size), d.progress() * 100.0);
        // How many connections are fetching it right now.
        if d.status == Status::Downloading && d.live.retry_at.is_none() && d.live.conns > 0 && !d.is_torrent() {
            m = if d.live.conns == 1 { format!("{}, 1 connection", m) } else { format!("{}, {} connections", m, d.live.conns) };
        }
        m
    };
    let show_bar = d.status != Status::Complete;
    let block_h = if show_bar { 17.0 + 8.0 + 6.0 + 8.0 + 16.0 } else { 17.0 + 4.0 + 16.0 };
    let top = cy - block_h / 2.0;
    let g = trunc(ui, &d.file_name, fs(14.0), p.text, mid_w);
    pt.galley_m(rect, pos2(mid_x, top), g, p.text);
    let mut y = top + 17.0 + if show_bar { 8.0 } else { 4.0 };
    if show_bar {
        let track_l = Rect::from_min_size(pos2(mid_x, y), vec2(mid_w, 6.0));
        let track = mrect(rect, track_l);
        pt.rect_filled(track, 3.0, p.track);
        let prog = d.progress();
        if prog > 0.0 {
            let fill = match d.status {
                Status::Paused => p.paused,
                Status::Error => p.bad,
                _ => p.progress,
            };
            pt.rect_filled(
                mrect(rect, Rect::from_min_size(track_l.min, vec2((mid_w * prog).max(6.0), 6.0))),
                3.0,
                fill,
            );
        }
        y += 6.0 + 8.0;
    }
    let g = trunc(ui, &meta, f(13.0), p.muted, mid_w);
    pt.galley_m(rect, pos2(mid_x, y), g, p.muted);

    // Status column
    let r1y = cy - 19.0;
    let (r1, r1c, r2): (String, Color32, String) = match d.status {
        Status::Downloading if d.live.retry_at.is_some() => (
            "Server not answering".into(),
            hex(0xD97706),
            retry_note(d.live.retry_at),
        ),
        Status::Downloading => (
            fmt_speed(d.speed_bps),
            p.text,
            d.eta_secs().map(|s| format!("{} left", fmt_duration(s))).unwrap_or_else(|| "Starting".into()),
        ),
        Status::Paused => ("Paused".into(), p.text, "Resume any time".into()),
        Status::Queued => ("Queued".into(), p.text, schedule::queue_note()),
        Status::Error => ("Failed".into(), p.bad, d.error.clone().unwrap_or_else(|| "Something went wrong".into())),
        Status::Complete => (String::new(), p.text, String::new()),
    };
    if let (Status::Complete, Some((uploaded, up_speed))) = (d.status, d.seeding) {
        // A finished torrent that is still being shared.
        let g = trunc(ui, &format!("Sharing, up {}", fmt_speed(up_speed)), fs(14.0), p.text, col_w);
        pt.galley_m(rect, pos2(col_x, r1y), g, p.text);
        let g = trunc(ui, &format!("{} sent so far", fmt_size(uploaded)), f(13.0), p.muted, col_w);
        pt.galley_m(rect, pos2(col_x, r1y + 19.0), g, p.muted);
    } else if d.status == Status::Complete {
        let pill_l = Rect::from_min_size(pos2(col_x, cy - 12.0), vec2(112.0, 24.0));
        let pill = mrect(rect, pill_l);
        pt.rect_filled(pill, 12.0, p.ok_soft);
        paint_icon(ui, mrect(rect, Rect::from_min_size(pos2(pill_l.left() + 10.0, cy - 7.0), vec2(14.0, 14.0))), "check", p.ok);
        pt.txtm(rect, pos2(pill_l.left() + 30.0, cy), Align2::LEFT_CENTER, "Completed", fs(12.0), p.ok);
    } else {
        let g = trunc(ui, &r1, fs(14.0), r1c, col_w);
        pt.galley_m(rect, pos2(col_x, r1y), g, r1c);
        let g = trunc(ui, &r2, f(13.0), p.muted, col_w);
        pt.galley_m(rect, pos2(col_x, r1y + 19.0), g, p.muted);
    }

    // Actions
    let id = egui::Id::new(("row", d.id));
    match d.status {
        Status::Complete if d.seeding.is_some() => {
            if text_button_at(ui, p, main, "Stop", "pause", id.with("stopseed")) {
                acts.push(Act::StopSeeding(d.id));
            }
        }
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
    if icon_button_at(ui, p, info_btn, "info", id.with("details"), if d.is_torrent() { "Torrent details" } else { "Details" }) {
        acts.push(Act::Details(d.id));
    }
    if icon_button_at(ui, p, folder, "folder", id.with("folder"), "Show in folder") {
        acts.push(Act::Folder(d.id));
    }
    if icon_button_at(ui, p, trash, "trash", id.with("trash"), "Remove from list") {
        acts.push(Act::Remove(d.id));
    }
}

/// "Trying again in 25 s" for a download waiting on a server that does not answer.
fn retry_note(at: Option<Instant>) -> String {
    match at.map(|t| t.saturating_duration_since(Instant::now()).as_secs()) {
        Some(s) if s > 0 => format!("Trying again in {} s", s),
        _ => "Trying again now".into(),
    }
}

fn draw_group(ui: &mut Ui, p: &Pal, icons: &IconCache, title: &str, rows: &[&Download], selected: &std::collections::BTreeSet<u64>, acts: &mut Vec<Act>) {
    hrow(ui, |ui| {
        lbl(ui, title, fs(14.0), p.text);
        lbl(ui, rows.len().to_string(), f(13.0), p.muted);
    });
    ui.add_space(8.0);
    egui::Frame::none()
        .fill(p.surface)
        .stroke(Stroke::new(1.0, p.border))
        .rounding(14.0)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (i, d) in rows.iter().enumerate() {
                draw_row(ui, p, icons, d, selected.contains(&d.id), i + 1 == rows.len(), acts);
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
    /// A torrent whose files have been read.
    Torrent(Box<torrent::TorrentMeta>),
    /// A YouTube playlist whose videos have been read.
    Playlist(Box<youtube::Playlist>),
    /// A YouTube link while yt-dlp is not installed yet.
    NeedsSetup,
    Failed(String),
}

enum ProbeOk {
    Http(u64, Option<String>),
    Yt(Box<YtInfo>),
    Torrent(Box<torrent::TorrentMeta>),
    Playlist(Box<youtube::Playlist>),
}

/// The quality offered for a whole playlist: (label, highest picture, sound only).
const PLAYLIST_QUALITY: [(&str, u32, bool); 4] =
    [("Best, up to 1080p", 1080, false), ("720p", 720, false), ("480p", 480, false), ("Audio only", 0, true)];

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
        .user_agent(web::DEFAULT_UA)
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
    let site = web::origin(url);
    let mut head = client.head(url).header("Accept-Language", "en-US,en;q=0.9");
    if let Some(o) = &site {
        head = head.header("Referer", o);
    }
    if let Ok(r) = head.send() {
        if r.status().is_success() {
            if let Some(len) = len_of(&r).filter(|l| *l > 0) {
                return Ok((len, name_of(&r)));
            }
        }
    }
    // Fallback: ask for one byte and read the total from Content-Range.
    let _ = RANGE;
    let (r, _) = web::get_tolerant(&client, url, None, None, Some("bytes=0-0")).map_err(|e| e.to_string())?;
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
    /// The folder is picked automatically (by file type) until the user changes it.
    save_dir_auto: bool,
    /// A torrent: which of its files are ticked.
    torrent_sel: Vec<bool>,
    /// A playlist: which videos are ticked, and whether they go in a folder of their own.
    playlist_sel: Vec<bool>,
    playlist_folder: bool,
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
            save_dir_auto: true,
            torrent_sel: Vec::new(),
            playlist_sel: Vec::new(),
            playlist_folder: true,
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
    /// Selected rows (Ctrl+click adds, Shift+click selects a range).
    selected: std::collections::BTreeSet<u64>,
    anchor: Option<u64>,
    /// The rows in the order they are shown, for ranges and arrow keys.
    order: Vec<u64>,
    copied_diag: Option<Instant>,
    search: String,
    dialog: NewDownloadDialog,
    confirm_delete: Vec<u64>,
    /// The torrent details window: which download, and the ticked files in it.
    details_id: Option<u64>,
    details_sel: Vec<bool>,
    /// Developer aid: open the details of the first torrent as soon as there is one.
    details_wait: bool,
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
    /// Put new downloads in a folder for their type (Videos, Music, ...).
    organize: bool,
    /// Keep sharing a torrent after it finishes.
    seed_after: bool,
    /// Open magnet links and .torrent files with Orexa.
    handle_types: bool,
    lang: i18n::Lang,
    lang_menu: bool,
    welcomed: bool,
    welcome_open: bool,
    /// A download shown in its own small window (started from the browser).
    floating: Option<u64>,
    /// Show the speed graph and activity log in download details.
    details_expanded: bool,
    /// Bring the pop-up back to the front (it was minimized, or a new download started).
    floating_front: bool,
    /// The newest release on GitHub, and what the update window is doing.
    update: update::UpdateState,
    update_rx: Receiver<UpdateMsg>,
    update_tx: Sender<UpdateMsg>,
    update_open: bool,
    /// When the next check for a new release is due.
    update_due: Option<Instant>,
    /// Bytes of the installer downloaded so far, while it downloads.
    update_done: Arc<std::sync::atomic::AtomicU64>,
    update_downloading: bool,
    /// What went wrong while updating, shown in the window.
    update_error: Option<String>,
    /// The installer is on its way: the app closes for it.
    update_launched: bool,
    update_closing: Option<Instant>,
    /// Downloads given back to the browser and when; if the browser never collects one (an
    /// older extension), the row says what to do instead.
    handbacks: HashMap<u64, Instant>,
    /// The Change link window, and the answers from checking a typed link.
    relink: Option<relink::Relink>,
    #[allow(clippy::type_complexity)]
    relink_tx: Sender<(u64, String, Result<(u64, Option<String>), String>)>,
    #[allow(clippy::type_complexity)]
    relink_rx: Receiver<(u64, String, Result<(u64, Option<String>), String>)>,
    /// Downloads to start once their old connection has wound down.
    delayed_starts: Vec<(u64, Instant)>,
    /// The pop-up's height (fitted to what it shows) and where it was opened.
    floating_h: f32,
    floating_pos: Option<(u64, egui::Pos2)>,
    /// The Check file window.
    check: Option<checksum::CheckDlg>,
    /// Speed limit for all downloads together (kept while switched off).
    speed_on: bool,
    speed_kbs: u64,
    /// The speed limit as typed, and whether it is in MB/s (else KB/s).
    speed_text: String,
    speed_mb: bool,
    /// Scheduled downloads: start (and optional stop) times as "HH:MM".
    sched_on: bool,
    sched_start: String,
    sched_stop: String,
    /// Between the start and stop times: the queue may start downloads.
    sched_active: bool,
    sched_last_min: Option<u32>,
    /// Leftovers found in Settings (None until looked for).
    leftovers: Option<Vec<cleanup::Leftover>>,
    /// Files to send to the Recycle Bin once their download has let go of them.
    pending_recycle: Vec<(Vec<PathBuf>, Instant)>,
    /// Removing an unfinished download also clears away its partial file.
    delete_files: bool,
    /// Keep the pop-up above other windows until then (so a finished download is seen).
    floating_top_until: Option<Instant>,
    after_finish: AfterFinish,
    /// Finished something since the queue was last idle (so "after finishing" can fire).
    batch_done: bool,
    /// The folder of the last finished download.
    last_done_dir: Option<PathBuf>,
    /// Counting down to sleep: when it will happen.
    sleep_at: Option<Instant>,
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
    lang: Option<String>,
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
        let (update_tx, update_rx) = channel::<UpdateMsg>();
        let (thumb_tx, thumb_rx) = channel::<(u64, Thumb)>();
        let (relink_tx, relink_rx) = channel();
        let mut app = Self {
            downloads: if o.demo { demo_with_details(sample_downloads()) } else { Vec::new() },
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
            selected: Default::default(),
            anchor: None,
            order: Vec::new(),
            copied_diag: None,
            search: String::new(),
            dialog: NewDownloadDialog::default(),
            confirm_delete: Vec::new(),
            help_open: false,
            help_browser: 0,
            details_id: None,
            details_sel: Vec::new(),
            details_wait: false,
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
            organize: false,
            seed_after: false,
            handle_types: true,
            lang: i18n::Lang::En,
            lang_menu: false,
            welcomed: true,
            welcome_open: false,
            floating: None,
            details_expanded: false,
            floating_front: false,
            floating_top_until: None,
            update: update::UpdateState::Idle,
            update_rx,
            update_tx,
            update_open: false,
            update_due: None,
            update_done: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            update_downloading: false,
            update_error: None,
            update_launched: false,
            update_closing: None,
            handbacks: HashMap::new(),
            relink: None,
            relink_tx,
            relink_rx,
            delayed_starts: Vec::new(),
            floating_h: 500.0,
            floating_pos: None,
            check: None,
            speed_on: false,
            speed_kbs: 1024,
            speed_text: "1".into(),
            speed_mb: true,
            sched_on: false,
            sched_start: "02:00".into(),
            sched_stop: String::new(),
            sched_active: false,
            sched_last_min: None,
            leftovers: None,
            pending_recycle: Vec::new(),
            delete_files: true,
            after_finish: AfterFinish::Nothing,
            batch_done: false,
            last_done_dir: None,
            sleep_at: None,
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
            shell::offer_extension_to_browsers();
            youtube::clean_stale_cookie_copies();
        }
        if (app.persist && app.screenshot_path.is_none()) || std::env::var_os("OREXA_UPDATE_API").is_some() {
            app.update_due = Some(Instant::now() + Duration::from_secs(6));
        }
        if app.persist {
            let first_run = !state::exists();
            if first_run {
                app.welcomed = false;
                if shell::system_language_is_arabic() {
                    app.lang = i18n::Lang::Ar;
                    i18n::set(app.lang);
                }
            }
            app.load_state();
            // Start in the tray with Windows (what the settings say, on by default).
            shell::set_autostart(app.autostart);
            // Open magnet links and .torrent files with Orexa (also on by default).
            shell::register_file_types(app.handle_types);
            torrent::SEED_AFTER.store(app.seed_after, std::sync::atomic::Ordering::Relaxed);
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
        if let Some(l) = &o.lang {
            app.lang = i18n::Lang::from_code(l);
            i18n::set(app.lang);
        }
        match o.open.as_deref() {
            Some("add") => {
                app.open_add();
                // Developer shortcut: a link given with --url always wins; otherwise an example
                // link, unless one was already picked up from the clipboard.
                if o.url.is_some() || app.dialog.url.is_empty() {
                    app.prefilled = None;
                    app.dialog.url = o
                        .url
                        .clone()
                        .unwrap_or_else(|| "https://releases.example.com/app-v3.2.1-setup.exe".into());
                    app.dialog.file_name = file_name_from_url(&app.dialog.url);
                    app.dialog.last_edit = Some(Instant::now());
                }
            }
            Some("settings") => app.page = Page::Settings,
            Some("settings-on") => {
                app.page = Page::Settings;
                app.speed_on = true;
                app.sched_on = true;
                app.sched_stop = "07:00".into();
            }
            Some("welcome") => app.welcome_open = true,
            Some("select") => app.selected = app.downloads.iter().take(3).map(|d| d.id).collect(),
            Some("details") => app.details_wait = true,
            Some("details-dl") => app.details_id = app.downloads.iter().find(|d| !d.is_torrent() && d.status == Status::Downloading).map(|d| d.id),
            // The pop-up window, for a running or a finished download.
            Some("popup") => app.floating = app.downloads.iter().find(|d| !d.is_torrent() && d.status == Status::Downloading).map(|d| d.id),
            Some("relink") => {
                if let Some(id) = app.downloads.iter().find(|d| matches!(d.source, Source::Http) && d.status != Status::Complete).map(|d| d.id) {
                    app.open_relink(id);
                    if let Some(r) = app.relink.as_mut() {
                        r.url = "https://mirror.example.org/files/ubuntu-24.04-desktop-amd64.iso".into();
                        r.state = relink::RelinkState::Same(5_368_709_120);
                    }
                }
            }
            Some("check") => {
                if let Some(id) = app.downloads.iter().find(|d| matches!(d.source, Source::Http) && d.status == Status::Complete).map(|d| d.id) {
                    app.open_check(id);
                }
            }
            Some("delete") => app.confirm_delete = app.downloads.iter().filter(|d| d.status != Status::Complete).map(|d| d.id).take(1).collect(),
            Some("popup-done") => app.floating = app.downloads.iter().find(|d| !d.is_torrent() && d.status == Status::Complete).map(|d| d.id),
            Some("help") => app.help_open = true,
            Some("help-edge") => {
                app.help_open = true;
                app.help_browser = 1;
            }
            Some("lang") => app.lang_menu = true,
            Some("update") => app.update_open = true,
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
                    files: None,
                    seeding: None,
                    live: Default::default(),
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
        // The browser picked up a download Orexa handed back: it is the browser's now.
        if v["type"].as_str() == Some("handback-taken") {
            if let Some(id) = v["id"].as_u64() {
                self.handbacks.remove(&id);
                if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                    d.error = Some("Your browser is downloading this (the site only allows the browser).".into());
                    d.live.note("Your browser took it over");
                }
            }
            return;
        }
        let text = |k: &str| {
            v[k].as_str()
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .map(String::from)
        };
        let Some(url) = text("url") else { return };
        if !(url.starts_with("http://") || url.starts_with("https://") || torrent::is_magnet(&url) || torrent::is_torrent_file(&url)) {
            return;
        }
        // Opened from Windows (a magnet link or .torrent file): show it in the Add window.
        if v["ask"].as_bool() == Some(true) {
            self.open_link_in_dialog(ctx, &url);
            return;
        }
        // The same link twice within a few seconds is one click.
        self.recent_browser.retain(|(_, t)| t.elapsed() < Duration::from_secs(10));
        if self.recent_browser.iter().any(|(u, _)| *u == url) {
            return;
        }
        self.recent_browser.push((url.clone(), Instant::now()));

        let extra = Extra { referer: text("referrer"), cookie: text("cookie"), user_agent: text("userAgent"), from_browser: true };
        let suggested = text("filename").map(|n| clean_name(&n));
        // "now" or "queue" means the browser dialog already asked the user.
        let mode = text("mode");
        // The folder the browser window chose; without one, the default (or its type folder).
        let explicit_dir = text("saveDir");

        // Already downloading or waiting: do not start it a second time.
        if v.get("yt").is_none()
            && self.downloads.iter().any(|d| same_url(&d.url, &url) && matches!(d.status, Status::Downloading | Status::Queued))
        {
            if self.notify_done {
                shell::toast("Already in your list".into(), file_name_from_url(&url));
            }
            return;
        }

        // A magnet link or .torrent file: the torrent engine takes it.
        if v.get("yt").is_none() && torrent::is_torrent_link(&url, suggested.as_deref().unwrap_or("")) {
            let queue = mode.as_deref() == Some("queue");
            let file_name = suggested
                .clone()
                .filter(|n| !n.is_empty())
                .or_else(|| torrent::magnet_name(&url).map(|n| clean_name(&n)))
                .unwrap_or_else(|| file_name_from_url(&url));
            let save_dir = explicit_dir.clone().unwrap_or_else(|| self.dir_for(&self.default_dir, &file_name, true));
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
                files: None,
                seeding: None,
                live: Default::default(),
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
            let save_dir = explicit_dir.clone().unwrap_or_else(|| self.dir_for(&self.default_dir, &file_name, false));
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
                files: None,
                seeding: None,
                live: Default::default(),
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
            let save_dir = explicit_dir.clone().unwrap_or_else(|| self.dir_for(&self.default_dir, &file_name, false));
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
                files: None,
                seeding: None,
                live: Default::default(),
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
        let save_dir = explicit_dir.clone().unwrap_or_else(|| self.dir_for(&self.default_dir, &file_name, false));
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
            files: None,
            seeding: None,
            live: Default::default(),
            extra,
        };
        self.next_id += 1;
        self.accept_browser_download(d, false);
    }

    /// Put a link or a .torrent file in the Add window, ready to check and start.
    fn open_link_in_dialog(&mut self, ctx: &egui::Context, link: &str) {
        self.open_add();
        self.dialog.file_name = file_name_from_url(link);
        self.dialog.url = link.to_string();
        self.dialog.last_edit = Some(Instant::now() - Duration::from_secs(1));
        shell::show_window();
        ctx.request_repaint();
    }

    /// Where a new file goes: the base folder, or its type folder when sorting is on.
    fn dir_for(&self, base: &str, file_name: &str, is_torrent: bool) -> String {
        if !self.organize || base.trim().is_empty() {
            return base.to_string();
        }
        PathBuf::from(base).join(type_folder(file_name, is_torrent)).to_string_lossy().into_owned()
    }

    /// Never write over a file that is already there: pick "name (1).ext" instead.
    fn make_name_unique(&self, d: &mut Download) {
        if d.is_torrent() {
            return; // a torrent's names come from the torrent itself
        }
        let (id, dir) = (d.id, d.save_dir.clone());
        let in_use = |n: &str| {
            self.downloads.iter().any(|x| {
                x.id != id
                    && x.status != Status::Complete
                    && x.save_dir.eq_ignore_ascii_case(&dir)
                    && x.file_name.eq_ignore_ascii_case(n)
            })
        };
        let name = unique_name(&d.save_dir, &d.file_name, &in_use);
        if name != d.file_name {
            d.ext = ext_of(&name);
            d.file_name = name;
        }
    }

    /// A download in the list for the same link (its name and where it stands).
    fn duplicate_of(&self, url: &str) -> Option<(String, Status)> {
        self.downloads.iter().find(|d| same_url(&d.url, url)).map(|d| (d.file_name.clone(), d.status))
    }

    /// Add a browser download to the list: start it now, or leave it queued.
    fn accept_browser_download(&mut self, mut d: Download, queue: bool) {
        // The browser found another link for a file that is half downloaded already (the old
        // server went down, or the link expired): continue that one instead of starting over.
        if matches!(d.source, Source::Http) {
            let old = self.same_file(&d.file_name, d.size, &d.url).filter(|o| o.status != Status::Complete).map(|o| (o.id, o.file_name.clone()));
            if let Some((old_id, name)) = old {
                self.relink_to(old_id, d.url.clone(), d.extra.clone());
                if self.notify_done {
                    shell::toast("Continuing your unfinished download".into(), name);
                }
                self.floating = Some(old_id);
                self.floating_front = true;
                return;
            }
        }
        self.make_name_unique(&mut d);
        if self.notify_done {
            shell::toast(
                if queue { "Added to the queue" } else { "Download started" }.into(),
                d.file_name.clone(),
            );
        }
        if !queue {
            self.engine.start(d.clone());
            // Show it right away, in a small window, without opening the main one.
            self.floating = Some(d.id);
            self.floating_front = true;
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
        self.dialog.save_dir_auto = true;
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
                Ok(ProbeOk::Playlist(pl)) => {
                    self.dialog.playlist_sel = vec![true; pl.entries.len()];
                    self.dialog.yt_sel = 0;
                    self.dialog.file_name = clean_name(&pl.title);
                    self.dialog.probe = ProbeState::Playlist(pl);
                }
                Ok(ProbeOk::Torrent(meta)) => {
                    self.dialog.torrent_sel = vec![true; meta.files.len()];
                    self.dialog.file_name = meta.name.clone();
                    self.dialog.probe = ProbeState::Torrent(meta);
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
                            // A playlist link reads the whole list; "Only this video" turns it
                            // back into a single-video link.
                            let r = if youtube::playlist_id(&url).is_some() {
                                youtube::probe_playlist(&url).map(|p| ProbeOk::Playlist(Box::new(p)))
                            } else {
                                youtube::probe_youtube(&url).map(|i| ProbeOk::Yt(Box::new(i)))
                            };
                            let _ = tx.send((url, r));
                        });
                    } else if torrent::is_torrent_link(&url, "") {
                        // Read the torrent's files, so you can choose which ones to download.
                        self.dialog.probe = ProbeState::Checking;
                        let tx = self.probe_tx.clone();
                        let extra = self.dialog.extra.clone();
                        thread::spawn(move || {
                            let r = torrent::inspect(&url, &extra).map(|m| ProbeOk::Torrent(Box::new(m)));
                            let _ = tx.send((url, r));
                        });
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
        if let ProbeState::Playlist(_) = &self.dialog.probe {
            self.dialog.playlist_sel.iter().any(|b| *b)
        } else if youtube::uses_ytdlp(self.dialog.url.trim()) {
            matches!(self.dialog.probe, ProbeState::YtReady(_))
        } else if let ProbeState::Torrent(meta) = &self.dialog.probe {
            // At least one file must be ticked.
            meta.files.is_empty() || self.dialog.torrent_sel.iter().any(|b| *b)
        } else {
            true
        }
    }

    fn url_ok(&self) -> bool {
        let u = self.dialog.url.trim();
        u.starts_with("http://") || u.starts_with("https://") || torrent::is_magnet(u) || torrent::is_torrent_file(u)
    }

    fn add_download(&mut self, start_now: bool) {
        if !self.can_submit() {
            return;
        }
        if let ProbeState::Playlist(pl) = self.dialog.probe.clone() {
            self.add_playlist(&pl);
            let _ = start_now; // the queue starts them, as many at a time as the settings allow
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
        // A torrent: only the ticked files (None = all of them).
        let mut files: Option<Vec<usize>> = None;
        if let (Source::Torrent, ProbeState::Torrent(meta)) = (&source, &self.dialog.probe) {
            let picked: Vec<usize> = (0..meta.files.len()).filter(|i| self.dialog.torrent_sel.get(*i).copied().unwrap_or(true)).collect();
            size = picked.iter().map(|i| meta.files[*i].1).sum();
            if picked.len() < meta.files.len() {
                files = Some(picked);
            }
            file_name = meta.name.clone();
        }
        let mut d = Download {
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
            files,
            seeding: None,
            live: Default::default(),
            extra: std::mem::take(&mut self.dialog.extra),
        };
        self.next_id += 1;
        self.make_name_unique(&mut d);
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

    /// Every ticked video of a playlist becomes a download of its own, numbered in playlist
    /// order, in a folder named after the playlist (unless that was switched off).
    fn add_playlist(&mut self, pl: &youtube::Playlist) {
        let (_, height, audio) = PLAYLIST_QUALITY[self.dialog.yt_sel.min(PLAYLIST_QUALITY.len() - 1)];
        let mut dir = PathBuf::from(self.dialog.save_dir.trim());
        if self.dialog.playlist_folder {
            let name = clean_name(&pl.title);
            let name = name.trim_end_matches(['.', ' ']);
            dir = dir.join(if name.is_empty() { "YouTube playlist" } else { name });
        }
        let digits = pl.entries.len().to_string().len().max(2);
        let ext = if audio { "m4a" } else { "mp4" };
        let added = chrono::Local::now().format("%Y-%m-%d %H:%M").to_string();
        for (i, e) in pl.entries.iter().enumerate() {
            if !self.dialog.playlist_sel.get(i).copied().unwrap_or(true) {
                continue;
            }
            let title: String = clean_name(&e.title).chars().take(110).collect();
            let file_name = format!("{:0w$} - {}.{}", i + 1, title.trim_end_matches(['.', ' ']), ext, w = digits);
            let mut d = Download {
                id: self.next_id,
                url: format!("https://www.youtube.com/watch?v={}", e.id),
                file_name: file_name.clone(),
                ext: ext.to_string(),
                size: 0,
                downloaded: 0,
                // Queued: started a few at a time, as the settings allow.
                status: Status::Queued,
                speed_bps: 0.0,
                save_dir: dir.to_string_lossy().into_owned(),
                added: added.clone(),
                error: None,
                source: Source::YouTube(YtOpts { id: e.id.clone(), height, audio }),
                thumb: None,
                files: None,
                seeding: None,
                live: Default::default(),
                extra: Extra::default(),
            };
            self.next_id += 1;
            self.make_name_unique(&mut d);
            self.downloads.push(d);
        }
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
        for id in std::mem::take(&mut self.confirm_delete) {
            // An unfinished file is of no use without its download: to the Recycle Bin.
            if self.delete_files {
                let files = self.downloads.iter().find(|d| d.id == id).map(cleanup::unfinished_files).unwrap_or_default();
                self.recycle_later(files);
            }
            self.engine.cancel(id);
            self.downloads.retain(|d| d.id != id);
            self.selected.remove(&id);
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
                EngineEvent::Log(id, msg) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        d.live.note(msg);
                    }
                }
                EngineEvent::HandBack(id) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        native::queue_handback(d.id, &d.url, &d.file_name, d.extra.referer.as_deref());
                        self.handbacks.insert(d.id, Instant::now());
                        d.status = Status::Error;
                        d.speed_bps = 0.0;
                        d.error = Some("The site only lets the browser download this. Waiting for your browser to take it...".into());
                        d.live.note("The site refused Orexa; handed back to the browser");
                        let _ = fs::remove_file(d.path());
                    }
                    if self.floating == Some(id) {
                        self.floating = None;
                    }
                }
                EngineEvent::Waiting(id, secs) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        d.live.retry_at = secs.map(|s| Instant::now() + Duration::from_secs(s));
                        if secs.is_some() {
                            d.speed_bps = 0.0;
                        }
                    }
                }
                EngineEvent::Conns(id, n) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        d.live.conns = n;
                    }
                }
                EngineEvent::Seeding(id, s) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        d.seeding = s;
                    }
                }
                EngineEvent::State(id, status, err) => {
                    if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                        let before = d.status;
                        d.status = status;
                        if status != before {
                            match (before, status) {
                                (_, Status::Paused) => d.live.note("Paused"),
                                (Status::Paused, Status::Downloading) => d.live.note("Resumed"),
                                (_, Status::Complete) => d.live.note("Finished"),
                                (_, Status::Error) => {
                                    let why = err.clone().unwrap_or_default();
                                    d.live.note(format!("Failed: {}", why));
                                }
                                (_, Status::Queued) => d.live.note("Stopped, waiting"),
                                _ => {}
                            }
                        }
                        if status != Status::Downloading {
                            d.speed_bps = 0.0;
                            d.live.conns = 0;
                            d.live.retry_at = None;
                        }
                        if status == Status::Complete && d.size > 0 {
                            d.downloaded = d.size;
                        }
                        if status == Status::Complete && before != Status::Complete {
                            self.batch_done = true;
                            self.last_done_dir = Some(d.path());
                            // The finished download shows up in front of whatever is on screen.
                            if self.persist && !self.demo {
                                self.floating = Some(d.id);
                                self.floating_front = true;
                                self.floating_top_until = Some(Instant::now() + Duration::from_secs(4));
                            }
                        }
                        if status == Status::Complete && self.notify_done && self.persist {
                            shell::toast_file("Download complete".into(), d.file_name.clone(), d.path());
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
        // Scheduled downloads wait for their start time.
        if self.demo || !self.queue_open() {
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
            for d in self.downloads.iter_mut().filter(|d| d.status == Status::Downloading) {
                d.live.hist.push(d.speed_bps as f32);
                if d.live.hist.len() > 160 {
                    d.live.hist.remove(0);
                }
            }
        }
    }

    fn apply(&mut self, acts: Vec<Act>) {
        for a in acts {
            match a {
                Act::Select(id, mods) => {
                    let (ctrl, shift) = (mods & 1 != 0, mods & 2 != 0);
                    if shift {
                        let a = self.anchor.unwrap_or(id);
                        let (i, j) = (
                            self.order.iter().position(|x| *x == a),
                            self.order.iter().position(|x| *x == id),
                        );
                        if let (Some(i), Some(j)) = (i, j) {
                            if !ctrl {
                                self.selected.clear();
                            }
                            for x in &self.order[i.min(j)..=i.max(j)] {
                                self.selected.insert(*x);
                            }
                        }
                    } else if ctrl {
                        if !self.selected.remove(&id) {
                            self.selected.insert(id);
                        }
                        self.anchor = Some(id);
                    } else {
                        self.selected.clear();
                        self.selected.insert(id);
                        self.anchor = Some(id);
                    }
                }
                Act::BulkPause => {
                    let ids: Vec<u64> = self
                        .downloads
                        .iter()
                        .filter(|d| self.selected.contains(&d.id) && d.status == Status::Downloading)
                        .map(|d| d.id)
                        .collect();
                    for id in ids {
                        self.engine.pause(id);
                    }
                }
                Act::BulkResume => {
                    let ids: Vec<u64> = self
                        .downloads
                        .iter()
                        .filter(|d| self.selected.contains(&d.id) && matches!(d.status, Status::Paused | Status::Queued | Status::Error))
                        .map(|d| d.id)
                        .collect();
                    for id in ids {
                        self.start_id(id);
                    }
                }
                Act::BulkRemove => {
                    self.confirm_delete = self.selected.iter().copied().collect();
                }
                Act::ClearSelection => self.selected.clear(),
                Act::Start(id) => {
                    self.selected.clear();
                    self.selected.insert(id);
                    self.anchor = Some(id);
                    self.start_id(id);
                }
                Act::Pause(id) => self.engine.pause(id),
                Act::StopSeeding(id) => self.engine.pause(id),
                Act::Details(id) => {
                    self.details_id = Some(id);
                    self.details_sel.clear();
                }
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
                Act::Remove(id) => self.confirm_delete = vec![id],
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
                let sym_l = Rect::from_min_size(pos2(bar.left() + 16.0, bar.center().y - 8.0), vec2(18.0, 16.6));
                let sym = mrect(bar, sym_l);
                paint_logo(ui, sym);
                ui.painter().txtm(
                    bar,
                    pos2(sym_l.right() + 10.0, bar.center().y),
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
                    let r = mrect(bar, Rect::from_min_size(pos2(bar.right() - 3.0 * 46.0 - 10.0 - w, bar.center().y - 14.0), vec2(w, 28.0)));
                    let resp = ui.interact(r, egui::Id::new("help_btn"), Sense::click());
                    if resp.hovered() {
                        ui.painter().rect_filled(r, 8.0, p.tile);
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let icon_x = if show_label { r.left() + 12.0 + 8.0 } else { r.center().x };
                    let icon_x = if show_label && rtl() { r.right() - 12.0 - 8.0 } else { icon_x };
                    paint_icon(ui, Rect::from_center_size(pos2(icon_x, r.center().y), vec2(16.0, 16.0)), "help", p.sub);
                    if show_label {
                        let dx = if rtl() { -14.0 } else { 14.0 };
                        ui.painter().text(pos2(icon_x + dx, r.center().y), malign(Align2::LEFT_CENTER), tr(label), f(12.5), p.sub);
                    }
                    if resp.on_hover_text(tr("How to add the Chrome extension?")).clicked() {
                        self.help_open = true;
                    }
                }

                // Window buttons, right aligned.
                let bw = 46.0;
                let mut x = bar.right();
                let mut btn = |icon: &str, id: &str, tip: &str, danger: bool| -> bool {
                    x -= bw;
                    let r = mrect(bar, Rect::from_min_size(pos2(x, bar.top()), vec2(bw, bar.height() - 1.0)));
                    let resp = ui.interact(r, egui::Id::new(id), Sense::click());
                    let hov = resp.hovered();
                    if hov {
                        ui.painter().rect_filled(r, 0.0, if danger { p.bad } else { p.tile });
                    }
                    let fg = if hov && danger { Color32::WHITE } else { p.sub };
                    paint_icon(ui, Rect::from_center_size(r.center(), vec2(16.0, 16.0)), icon, fg);
                    resp.on_hover_text(tr(tip)).clicked()
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
        let panel = if rtl() { egui::SidePanel::right("nav") } else { egui::SidePanel::left("nav") };
        panel
            .exact_width(248.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(p.surface).inner_margin(Margin::symmetric(16.0, 20.0)))
            .show(ctx, |ui| {
                let full = ui.max_rect();
                ui.painter().vline(
                    if rtl() { full.left() - 16.0 + 0.5 } else { full.right() + 16.0 - 0.5 },
                    (full.top() - 20.0)..=(full.bottom() + 20.0),
                    Stroke::new(1.0, p.border),
                );
                ui.spacing_mut().item_spacing.y = 4.0;
                let group = |ui: &mut Ui, s: &str| {
                    ui.add_space(8.0);
                    hrow(ui, |ui| {
                        ui.add_space(12.0);
                        lbl(ui, s, fs(12.0), p.muted);
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
                    // The version, and whether there is a newer one (bottom of the sidebar).
                    if self.version_row(ui, p).clicked() {
                        self.update_open = true;
                        if matches!(self.update, update::UpdateState::Idle | update::UpdateState::Failed(_)) {
                            self.start_update_check();
                        }
                    }
                    if nav_item(ui, p, "settings", "Settings", None, self.page == Page::Settings) {
                        self.page = Page::Settings;
                    }
                    // Language picker, just above Settings.
                    let label = self.lang.native_name();
                    let r = nav_item_raw(ui, p, "globe", label, self.lang_menu);
                    if r.clicked() {
                        self.lang_menu = !self.lang_menu;
                    }
                    if self.lang_menu {
                        let anchor = r.rect;
                        let mut chosen: Option<i18n::Lang> = None;
                        let area = egui::Area::new(egui::Id::new("lang_menu"))
                            .order(Order::Foreground)
                            .fixed_pos(pos2(anchor.left(), anchor.top() - 4.0 - 44.0 * i18n::Lang::ALL.len() as f32 - 8.0))
                            .show(ctx, |ui| {
                                egui::Frame::none()
                                    .fill(p.surface)
                                    .stroke(Stroke::new(1.0, p.border_strong))
                                    .rounding(10.0)
                                    .inner_margin(Margin::same(4.0))
                                    .shadow(egui::Shadow {
                                        offset: vec2(0.0, 6.0),
                                        blur: 18.0,
                                        spread: 0.0,
                                        color: Color32::from_black_alpha(40),
                                    })
                                    .show(ui, |ui| {
                                        ui.set_width(anchor.width() - 8.0);
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        for l in i18n::Lang::ALL {
                                            let r = nav_item_raw(ui, p, if l == self.lang { "check" } else { "" }, l.native_name(), l == self.lang);
                                            if r.clicked() {
                                                chosen = Some(l);
                                            }
                                        }
                                    });
                            });
                        if let Some(l) = chosen {
                            self.lang = l;
                            i18n::set(l);
                            self.lang_menu = false;
                            ctx.request_repaint();
                        } else if ctx.input(|i| i.pointer.any_click()) {
                            // A click outside the menu (and not on its own row) closes it. The
                            // press must not close it: the choice is made on release.
                            let at = ctx.input(|i| i.pointer.interact_pos());
                            let inside = at.map(|p| area.response.rect.contains(p) || r.rect.contains(p)).unwrap_or(false);
                            if !inside {
                                self.lang_menu = false;
                            }
                        }
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
                let toolbar_row = |ui: &mut Ui| {
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
                                        .id(egui::Id::new("search_box"))
                                        .layouter(&mut lay)
                                        .frame(false)
                                        .hint_text(rt("Search downloads", f(14.0), p.muted))
                                        .font(f(14.0))
                                        .text_color(p.text)
                                        .desired_width(340.0),
                                );
                            });
                        });
                    ui.with_layout(if rtl() { Layout::left_to_right(Align::Center) } else { Layout::right_to_left(Align::Center) }, |ui| {
                        if button(ui, p, "Add URL", Some("plus"), Kind::Primary, 40.0, true).clicked() {
                            self.open_add();
                        }
                        ui.add_space(4.0);
                        let any = self.downloads.iter().any(|d| d.status == Status::Downloading);
                        if button(ui, p, "Pause all", Some("pause"), Kind::Secondary, 40.0, any).clicked() {
                            self.pause_all();
                        }

                    });
                };
                if rtl() {
                    ui.with_layout(Layout::right_to_left(Align::Center), toolbar_row);
                } else {
                    ui.horizontal_centered(toolbar_row);
                }
            });
    }

    // ---- Downloads page -----------------------------------------------------
    fn ui_downloads(&mut self, ui: &mut Ui, p: &Pal) {
        let mut acts: Vec<Act> = Vec::new();
        let mut new_order: Vec<u64> = Vec::new();
        let q = self.search.trim().to_lowercase();
        let rows: Vec<&Download> = self
            .downloads
            .iter()
            .filter(|d| self.cat.matches(d) && (q.is_empty() || d.file_name.to_lowercase().contains(&q)))
            .collect();
        let selected = self.selected.clone();
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
              ui.with_layout(Layout::top_down(start_align()), |ui| {
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
                        lbl(ui, "No downloads yet", fb(36.0), p.text);
                        ui.add_space(8.0);
                        ui.scope(|ui| {
                            ui.set_max_width(560.0);
                            lbl(ui,
                                "Paste a link to save your first file. Orexa Downloader keeps it organized and picks up where it left off if your connection drops.",
                                f(16.0),
                                p.sub,
                            );
                        });
                        ui.add_space(20.0);
                        let w = text_w(ui, "Add URL", fs(14.0)) + 36.0 + 26.0;
                        ui.add_space(0.0);
                        let _ = w;
                        if button(ui, p, "Add URL", Some("plus"), Kind::Primary, 48.0, true).clicked() {
                            acts.push(Act::AddDialog);
                        }
                        ui.add_space(14.0);
                        lbl(ui, "Tip: you can also start from the Add URL button at any time.", f(13.0), p.muted);
                    });
                    return;
                }

                if cat == Category::All {
                    self.ui_summary(ui, p);
                } else {
                    lbl(ui, cat.label(), fb(32.0), p.text);
                    let n = self.downloads.iter().filter(|d| cat.matches(d)).count();
                    lbl(ui, format!("{} file{}", n, if n == 1 { "" } else { "s" }), f(14.0), p.muted);
                    ui.add_space(20.0);
                }

                if rows.is_empty() {
                    ui.add_space(40.0);
                    ui.vertical_centered(|ui| {
                        lbl(ui, "Nothing here yet", fs(20.0), p.text);
                        ui.add_space(4.0);
                        let msg = if q.is_empty() { "Downloads in this view will show up here." } else { "No downloads match your search." };
                        lbl(ui, msg, f(14.0), p.muted);
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
                new_order = groups.iter().flat_map(|(_, l)| l.iter().map(|d| d.id)).collect();
                if selected.len() >= 2 {
                    let n = selected.len();
                    egui::Frame::none()
                        .fill(p.accent_soft)
                        .rounding(12.0)
                        .inner_margin(Margin::symmetric(16.0, 8.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let start = if rtl() { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
                            ui.allocate_ui_with_layout(vec2(ui.available_width(), 32.0), start, |ui| {
                                lbl(ui, format!("{} selected", n), fs(14.0), p.accent_ink);
                                ui.with_layout(rl_layout(Align::Center), |ui| {
                                    if button(ui, p, "Clear", None, Kind::Secondary, 32.0, true).clicked() {
                                        acts.push(Act::ClearSelection);
                                    }
                                    if button(ui, p, "Remove", Some("trash"), Kind::Secondary, 32.0, true).clicked() {
                                        acts.push(Act::BulkRemove);
                                    }
                                    if button(ui, p, "Resume", Some("play"), Kind::Secondary, 32.0, true).clicked() {
                                        acts.push(Act::BulkResume);
                                    }
                                    if button(ui, p, "Pause", Some("pause"), Kind::Secondary, 32.0, true).clicked() {
                                        acts.push(Act::BulkPause);
                                    }
                                });
                            });
                        });
                    ui.add_space(16.0);
                }
                for (title, list) in groups {
                    if !list.is_empty() {
                        draw_group(ui, p, &self.icons, &title, &list, &selected, &mut acts);
                    }
                }
              });
            });
        });
        self.order = new_order;
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
                hrow(ui, |ui| {
                    ui.with_layout(Layout::top_down(start_align()), |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        lbl(ui, "Current speed", fs(13.0), p.muted);
                        let big = if speed < 1.0 { "0 MB/s".to_string() } else { fmt_speed(speed) };
                        lbl(ui, big, fb(44.0), p.text);
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
                            mrect(r, Rect::from_min_max(pos2(x, r.bottom() - h), pos2(x + 6.0, r.bottom()))),
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
                        let cell = mrect(area, Rect::from_min_size(pos2(x, area.top()), vec2(w, area.height())));
                        let anchor = if rtl() { Align2::RIGHT_TOP } else { Align2::LEFT_TOP };
                        let ax = if rtl() { cell.right() } else { cell.left() };
                        ui.painter().txt(pos2(ax, area.center().y - 22.0), anchor, &v, fs(20.0), p.text);
                        ui.painter().txt(pos2(ax, area.center().y + 6.0), anchor, l, f(13.0), p.muted);
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
        let mut area = egui::ScrollArea::vertical().auto_shrink([false, false]);
        if let Some(y) = std::env::var("OREXA_SCROLL").ok().and_then(|v| v.parse::<f32>().ok()) {
            area = area.vertical_scroll_offset(y); // developer aid for screenshots
        }
        area.show(ui, |ui| {
            egui::Frame::none().inner_margin(Margin::symmetric(48.0, 40.0)).show(ui, |ui| {
              ui.with_layout(Layout::top_down(start_align()), |ui| {
                lbl(ui, "General", fb(32.0), p.text);
                lbl(ui, "Changes apply right away.", f(14.0), p.muted);
                ui.add_space(24.0);

                // Wide enough: the About panel sits on the right, the cards get what is left.
                const SIDE_W: f32 = 290.0;
                let avail = ui.available_width();
                let side = avail >= 540.0 + 24.0 + SIDE_W;
                let cw = if side { (avail - 24.0 - SIDE_W).min(760.0) } else { avail.min(760.0) };
                if side {
                    let x = if rtl() { ui.max_rect().left() } else { ui.cursor().left() + cw + 24.0 };
                    let r = Rect::from_min_size(pos2(x, ui.cursor().top()), vec2(SIDE_W, 600.0));
                    let mut right = ui.new_child(egui::UiBuilder::new().max_rect(r).layout(Layout::top_down(start_align())));
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
                        // The controls are placed first, at the far edge; the text gets the room
                        // that is left and wraps there, so a long description never runs under them.
                        ui.horizontal(|ui| {
                            let (edge, text_align) = if rtl() {
                                (Layout::left_to_right(Align::Center), Align::Max)
                            } else {
                                (Layout::right_to_left(Align::Center), Align::Min)
                            };
                            ui.with_layout(edge, |ui| {
                                ctl(ui);
                                ui.add_space(16.0);
                                ui.with_layout(Layout::top_down(text_align), |ui| {
                                    ui.spacing_mut().item_spacing.y = 2.0;
                                    lbl(ui, title, fs(14.0), p.text);
                                    lbl(ui, desc, f(13.0), p.muted);
                                });
                            });
                        });
                    });
                    if !last {
                        let y = ui.cursor().top();
                        ui.painter().hline(ui.min_rect().x_range(), y, Stroke::new(1.0, p.border));
                    }
                };

                // A setting with a long description or wide controls: the controls go underneath.
                let stacked = |ui: &mut Ui, title: &str, desc: &str, ctl: &mut dyn FnMut(&mut Ui)| {
                    egui::Frame::none().inner_margin(Margin::symmetric(24.0, 18.0)).show(ui, |ui| {
                        ui.set_width(cw - 48.0);
                        col(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            lbl(ui, title, fs(14.0), p.text);
                            lbl(ui, desc, f(13.0), p.muted);
                            ui.add_space(10.0);
                            hrow(ui, |ui| ctl(ui));
                        });
                    });
                };

                let default_dir = &mut self.default_dir;
                let max_parallel = &mut self.max_parallel;
                let organize = &mut self.organize;
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
                    line(
                        ui,
                        "Sort into folders by type",
                        "Save new downloads in Videos, Music, Documents and so on, inside the folder above.",
                        false,
                        &mut |ui| {
                            toggle(ui, p, organize);
                        },
                    );
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
                        ui.painter().txt(r.center(), Align2::CENTER_CENTER, max_parallel.to_string(), fs(14.0), p.text);
                    });
                });

                let seed_after = &mut self.seed_after;
                let seed_before = *seed_after;
                card(ui, &mut |ui| {
                    line(
                        ui,
                        "Keep sharing finished torrents",
                        "Keep uploading a torrent after it finishes, until you stop it. Helps others; uses upload speed.",
                        true,
                        &mut |ui| {
                            toggle(ui, p, seed_after);
                        },
                    );
                });
                if self.seed_after != seed_before {
                    torrent::SEED_AFTER.store(self.seed_after, std::sync::atomic::Ordering::Relaxed);
                }

                let notify_done = &mut self.notify_done;
                let autostart = &mut self.autostart;
                let autostart_before = *autostart;
                let handle_types = &mut self.handle_types;
                let types_before = *handle_types;
                let mut open_default_apps = false;
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
                        "Open magnet links and .torrent files",
                        "Orexa opens them when you click one in any program.",
                        false,
                        &mut |ui| {
                            toggle(ui, p, handle_types);
                            ui.add_space(8.0);
                            if button(ui, p, "Default apps", None, Kind::Secondary, 36.0, true)
                                .on_hover_text(tr("If another app opens them, choose Orexa in Windows Settings"))
                                .clicked()
                            {
                                open_default_apps = true;
                            }
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
                if self.handle_types != types_before {
                    shell::register_file_types(self.handle_types);
                }
                if open_default_apps {
                    shell::open_default_apps();
                }

                // Speed limit and scheduled downloads.
                let (speed_on, speed_text, speed_mb, speed_kbs) =
                    (&mut self.speed_on, &mut self.speed_text, &mut self.speed_mb, &mut self.speed_kbs);
                let (sched_on, sched_start, sched_stop) = (&mut self.sched_on, &mut self.sched_start, &mut self.sched_stop);
                let sched_active = self.sched_active;
                card(ui, &mut |ui| {
                    line(ui, "Limit download speed", "All downloads together stay under this speed.", false, &mut |ui| {
                        toggle(ui, p, speed_on);
                        if *speed_on {
                            ui.add_space(12.0);
                            // KB/s or MB/s.
                            let (r, _) = ui.allocate_exact_size(vec2(124.0, 36.0), Sense::hover());
                            ui.painter().rect_filled(r, 9.0, p.tile);
                            let mut unit_changed = false;
                            for (i, (label, mb)) in [("KB/s", false), ("MB/s", true)].iter().enumerate() {
                                let o = mrect(r, Rect::from_min_size(pos2(r.left() + 3.0 + i as f32 * 59.0, r.top() + 3.0), vec2(59.0, 30.0)));
                                let sel = *speed_mb == *mb;
                                let resp = ui.interact(o, egui::Id::new(("speed_unit", i)), Sense::click());
                                if sel {
                                    ui.painter().rect_filled(o, 7.0, if p.dark { p.border_strong } else { p.surface });
                                }
                                if resp.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                ui.painter().txt(o.center(), Align2::CENTER_CENTER, *label, if sel { fs(13.0) } else { f(13.0) }, if sel { p.text } else { p.sub });
                                if resp.clicked() && !sel {
                                    *speed_mb = *mb;
                                    unit_changed = true;
                                }
                            }
                            ui.add_space(8.0);
                            let te = ui.add(
                                egui::TextEdit::singleline(speed_text)
                                    .font(f(14.0))
                                    .text_color(p.text)
                                    .horizontal_align(Align::Center)
                                    .margin(Margin::symmetric(8.0, 8.0))
                                    .desired_width(64.0),
                            );
                            if te.changed() || unit_changed {
                                let v: f64 = speed_text.trim().replace(',', ".").parse().unwrap_or(0.0);
                                if v > 0.0 {
                                    *speed_kbs = ((if *speed_mb { v * 1024.0 } else { v }).round() as u64).max(8);
                                }
                            }
                        }
                    });
                    stacked(
                        ui,
                        "Scheduled downloads",
                        "Queued downloads wait for the start time, for example at night. Orexa has to be running, in the tray is enough.",
                        &mut |ui| {
                            toggle(ui, p, sched_on);
                            if *sched_on {
                                ui.add_space(14.0);
                                let time_box = |ui: &mut Ui, v: &mut String, hint: &str| {
                                    let bad = !v.trim().is_empty() && schedule::parse_hm(v).is_none();
                                    let mut te = egui::TextEdit::singleline(v)
                                        .font(f(14.0))
                                        .text_color(if bad { p.bad } else { p.text })
                                        .horizontal_align(Align::Center)
                                        .margin(Margin::symmetric(8.0, 8.0))
                                        .desired_width(64.0);
                                    if !hint.is_empty() {
                                        te = te.hint_text(rt(hint, f(13.0), p.muted));
                                    }
                                    ui.add(te);
                                };
                                lbl(ui, "Start at", f(14.0), p.sub);
                                ui.add_space(6.0);
                                time_box(ui, sched_start, "02:00");
                                ui.add_space(14.0);
                                lbl(ui, "Stop at", f(14.0), p.sub);
                                ui.add_space(6.0);
                                time_box(ui, sched_stop, "--:--");
                                ui.add_space(14.0);
                                let note = if schedule::parse_hm(sched_start).is_none() {
                                    ("Use a time like 02:00".to_string(), p.bad)
                                } else if sched_active {
                                    ("Running now".to_string(), p.ok)
                                } else if sched_stop.trim().is_empty() {
                                    ("Stops when the queue is done".to_string(), p.muted)
                                } else {
                                    (format!("Starts at {}", sched_start.trim()), p.muted)
                                };
                                lbl(ui, note.0, f(13.0), note.1);
                            }
                        },
                    );
                });

                let after = &mut self.after_finish;
                card(ui, &mut |ui| {
                    stacked(ui, "When all downloads finish", "Choose what Orexa does once nothing is left to download.", &mut |ui| {
                        let opts = [
                            ("Do nothing", AfterFinish::Nothing),
                            ("Open the folder", AfterFinish::Folder),
                            ("Sleep", AfterFinish::Sleep),
                        ];
                        let widths: Vec<f32> = opts.iter().map(|(l, _)| text_w(ui, l, fs(13.0)) + 28.0).collect();
                        let total: f32 = widths.iter().sum::<f32>() + 6.0;
                        let (r, _) = ui.allocate_exact_size(vec2(total, 40.0), Sense::hover());
                        ui.painter().rect_filled(r, 10.0, p.tile);
                        let mut x = r.left() + 3.0;
                        for (i, (label, val)) in opts.iter().enumerate() {
                            let o = mrect(r, Rect::from_min_size(pos2(x, r.top() + 3.0), vec2(widths[i], 34.0)));
                            x += widths[i];
                            let sel = *after == *val;
                            let resp = ui.interact(o, egui::Id::new(("after", i)), Sense::click());
                            if sel {
                                ui.painter().rect_filled(o, 8.0, if p.dark { p.border_strong } else { p.surface });
                            }
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            ui.painter().txt(
                                o.center(),
                                Align2::CENTER_CENTER,
                                *label,
                                if sel { fs(13.0) } else { f(13.0) },
                                if sel { p.text } else { p.sub },
                            );
                            if resp.clicked() {
                                *after = *val;
                            }
                        }
                    });
                });

                card(ui, &mut |ui| {
                    line(
                        ui,
                        "Chrome extension",
                        "Add it to Chrome, Brave or Edge.",
                        true,
                        &mut |ui| {
                            if button(ui, p, "Open the Chrome Web Store", Some("open"), Kind::Secondary, 36.0, true).clicked() {
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
                            let o = mrect(r, Rect::from_min_size(pos2(r.left() + 3.0 + i as f32 * 80.0, r.top() + 3.0), vec2(80.0, 34.0)));
                            let sel = *dark == *is_dark;
                            let resp = ui.interact(o, egui::Id::new(("theme", i)), Sense::click());
                            if sel {
                                ui.painter().rect_filled(o, 8.0, if p.dark { p.border_strong } else { p.surface });
                            }
                            if resp.hovered() {
                                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            }
                            ui.painter().txt(
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

                // Leftovers of unfinished downloads that are no longer in the list.
                let mut look = false;
                let mut clear = false;
                let leftovers = self.leftovers.clone();
                card(ui, &mut |ui| {
                    let desc = match &leftovers {
                        None => "Partial files of unfinished downloads that are no longer in your list.".to_string(),
                        Some(v) if v.is_empty() => "Nothing to clean up.".to_string(),
                        Some(v) => {
                            let total: u64 = v.iter().map(|l| l.size).sum();
                            let n = v.len();
                            if n == 1 {
                                format!("Found 1 leftover, {}.", fmt_size(total))
                            } else {
                                format!("Found {} leftovers, {}.", n, fmt_size(total))
                            }
                        }
                    };
                    line(ui, "Leftovers", &desc, true, &mut |ui| match &leftovers {
                        Some(v) if !v.is_empty() => {
                            if button(ui, p, "Move to Recycle Bin", Some("trash"), Kind::Secondary, 36.0, true).clicked() {
                                clear = true;
                            }
                        }
                        _ => {
                            if button(ui, p, "Look for leftovers", Some("search"), Kind::Secondary, 36.0, true).clicked() {
                                look = true;
                            }
                        }
                    });
                });
                if look {
                    self.scan_leftovers();
                }
                if clear {
                    if let Some(v) = self.leftovers.take() {
                        let files: Vec<PathBuf> = v.iter().flat_map(|l| [Some(l.side.clone()), l.file.clone()]).flatten().collect();
                        shell::recycle(&files);
                    }
                    self.scan_leftovers();
                }

                let copied = self.copied_diag.map(|t| t.elapsed() < Duration::from_secs(3)).unwrap_or(false);
                let mut copy_diag = false;
                let mut report = false;
                card(ui, &mut |ui| {
                    stacked(
                        ui,
                        "Copy diagnostics",
                        "Copies your version, settings and recent errors, without any links or file names, to paste into a bug report.",
                        &mut |ui| {
                            if button(ui, p, if copied { "Copied" } else { "Copy diagnostics" }, Some("copy"), Kind::Secondary, 36.0, true).clicked() {
                                copy_diag = true;
                            }
                            if button(ui, p, "Report a problem", None, Kind::Secondary, 36.0, true).clicked() {
                                report = true;
                            }
                        },
                    );
                });
                if copy_diag {
                    let text = self.diagnostics();
                    if let Ok(mut cb) = arboard::Clipboard::new() {
                        let _ = cb.set_text(text);
                    }
                    self.copied_diag = Some(Instant::now());
                }
                if report {
                    let _ = Command::new("explorer")
                        .arg("https://github.com/abdelrahmanmegaly-jpg/orexa-downloader/issues/new")
                        .spawn();
                }
                if !side {
                    about_panel(ui, p, cw.max(300.0));
                }
              });
            });
        });
        if open_ext {
            open_store_page();
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

        // With "sort into folders" on, the folder follows the kind of file until you pick your own.
        if self.organize && self.dialog.save_dir_auto {
            let torrent = torrent::is_torrent_link(&self.dialog.url, &self.dialog.file_name);
            self.dialog.save_dir = self.dir_for(&self.default_dir, &self.dialog.file_name, torrent);
        }
        let dup = self.duplicate_of(self.dialog.url.trim());
        // The same file from another link (a mirror): its name and size match one in the list.
        let same: Option<(u64, Status, u64, u64, String)> = match (&dup, &self.dialog.probe) {
            (None, ProbeState::Ready(sz, _)) => self
                .same_file(&self.dialog.file_name, *sz, self.dialog.url.trim())
                .map(|d| (d.id, d.status, d.downloaded, d.size, d.added.clone())),
            _ => None,
        };
        // With the scheduler waiting, the queue starts later: say when.
        let queue_hint = if self.queue_open() {
            "Turn off to add it to the queue instead.".to_string()
        } else {
            format!("Turn off to add it to the queue. Queued downloads start at {}.", self.sched_start.trim())
        };
        let mut continue_same: Option<u64> = None;
        let mut reveal_same: Option<u64> = None;

        let dlg = &mut self.dialog;
        let mut marks: Vec<(&str, f32)> = Vec::new();
        modal(ctx, p, "add", 504.0, 560.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            hrow(ui, |ui| {
                col(ui, |ui| {
                    lbl(ui, "Add a download", fb(22.0), p.text);
                    ui.add_space(4.0);
                    lbl(ui, "Paste a link and choose where to save it.", f(14.0), p.muted);
                });
                ui.with_layout(rl_layout(Align::Min), |ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                    if icon_button_at(ui, p, r, "x", egui::Id::new("dlg_close"), "Close") {
                        close = true;
                    }
                });
            });
            marks.push(("after_header", ui.cursor().top()));
            ui.add_space(20.0);
            lbl(ui, "Download link", fs(13.0), p.sub);
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
                                    lbl(ui, format!("Setup failed: {}", e), f(13.0), p.bad);
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
                            ProbeState::Playlist(pl) => {
                                hrow(ui, |ui| {
                                    ui.spacing_mut().item_spacing.x = 12.0;
                                    let (t, _) = ui.allocate_exact_size(vec2(54.0, 54.0), Sense::hover());
                                    ui.painter().rect_filled(t, 10.0, p.accent_soft);
                                    paint_icon(ui, Rect::from_center_size(t.center(), vec2(24.0, 24.0)), "list", p.accent_ink);
                                    col(ui, |ui| {
                                        ui.spacing_mut().item_spacing.y = 2.0;
                                        lbl(ui, trunc_str(&pl.title, 60), fs(14.0), p.text);
                                        let total: u64 = pl.entries.iter().map(|e| e.duration).sum();
                                        let n = pl.entries.len();
                                        let who = if pl.channel.is_empty() {
                                            format!("{} videos, {}", n, youtube::fmt_clock(total))
                                        } else {
                                            format!("{}, {} videos, {}", pl.channel, n, youtube::fmt_clock(total))
                                        };
                                        lbl(ui, who, f(13.0), p.muted);
                                        // A link to one video inside a playlist: that video alone is one click away.
                                        if let Some(vid) = youtube::video_id(&dlg.url) {
                                            ui.add_space(4.0);
                                            if button(ui, p, "Only this video", Some("film"), Kind::Secondary, 30.0, true).clicked() {
                                                dlg.url = format!("https://www.youtube.com/watch?v={}", vid);
                                                dlg.probe = ProbeState::Idle;
                                                dlg.last_edit = Some(Instant::now() - Duration::from_secs(1));
                                            }
                                        }
                                    });
                                });
                                ui.add_space(6.0);
                                dlg.yt_sel = dlg.yt_sel.min(PLAYLIST_QUALITY.len() - 1);
                                lbl(ui, "Quality", fs(13.0), p.sub);
                                ui.horizontal_wrapped(|ui| {
                                    ui.spacing_mut().item_spacing = vec2(8.0, 8.0);
                                    for (i, (label, _, _)) in PLAYLIST_QUALITY.iter().enumerate() {
                                        let w = text_w(ui, label, fs(13.0)) + 28.0;
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
                                        ui.painter().rect_stroke(r, 8.0, Stroke::new(1.0, if sel { p.accent } else { p.border_strong }));
                                        ui.painter().txt(
                                            r.center(),
                                            Align2::CENTER_CENTER,
                                            *label,
                                            if sel { fs(13.0) } else { f(13.0) },
                                            if sel { p.accent_ink } else { p.text },
                                        );
                                        if resp.clicked() {
                                            dlg.yt_sel = i;
                                        }
                                    }
                                });
                                ui.add_space(6.0);
                                let entries: Vec<(String, u64)> = pl.entries.iter().map(|e| (e.title.clone(), e.duration)).collect();
                                pick_list(ui, p, "Videos", &entries, &mut dlg.playlist_sel, "playlist_videos", (ui.ctx().screen_rect().height() - 760.0).clamp(96.0, 260.0), youtube::fmt_clock);
                                ui.add_space(6.0);
                                ui.checkbox(&mut dlg.playlist_folder, rt("Save in a folder named after the playlist", f(13.0), p.text));
                            }
                            ProbeState::YtReady(info) => {
                                hrow(ui, |ui| {
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
                                    col(ui, |ui| {
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
                                        lbl(ui, who, f(13.0), p.muted);
                                    });
                                });
                                ui.add_space(8.0);
                                let list = youtube::choices(&info, tools_ff == ToolStatus::Ready);
                                dlg.yt_sel = dlg.yt_sel.min(list.len() - 1);
                                lbl(ui, "Quality", fs(13.0), p.sub);
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
                                        ui.painter().txt(
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
                                lbl(ui, size_line, f(13.0), p.muted);
                            }
                            ProbeState::Failed(msg) => {
                                let bot = msg.contains("Sign in to confirm")
                                    || msg.contains("not a bot")
                                    || msg.contains("--cookies");
                                if bot {
                                    lbl(ui, "YouTube wants to confirm you are not a bot", fs(14.0), p.bad);
                                    lbl(ui,
                                        "Open YouTube in Chrome (the Orexa extension shares your sign-in), then press Try again.",
                                        f(13.0),
                                        p.muted,
                                    );
                                } else {
                                    lbl(ui, "Could not read this video", fs(14.0), p.bad);
                                    lbl(ui, msg, f(13.0), p.muted);
                                }
                                ui.add_space(4.0);
                                if button(ui, p, "Try again", Some("retry"), Kind::Secondary, 36.0, true).clicked() {
                                    dlg.probe = ProbeState::Idle;
                                    dlg.last_edit = Some(Instant::now() - Duration::from_secs(1));
                                }
                            }
                            ProbeState::NeedsSetup => {
                                lbl(ui, "Video link detected", fs(14.0), p.text);
                                if tools_yt == ToolStatus::Ready {
                                    lbl(ui, "Reading video details... this can take a little while.", f(13.0), p.muted);
                                } else {
                                    let msg = if matches!(tools_yt, ToolStatus::Installing(_)) {
                                        "Getting video support ready. This happens once and continues on its own."
                                    } else {
                                        "Getting video support ready..."
                                    };
                                    lbl(ui, msg, f(13.0), p.muted);
                                    setup_row(ui, &tools_yt, Tool::YtDlp, &mut setup_req);
                                }
                            }
                            _ => {
                                lbl(ui, "Reading video details... this can take a little while.", f(13.0), p.muted);
                            }
                        }
                    });
                ui.add_space(16.0);
            } else if !dlg.url.trim().is_empty() {
                let is_torrent_now = torrent::is_torrent_link(&dlg.url, &dlg.file_name);
                egui::Frame::none()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.border))
                    .rounding(12.0)
                    .inner_margin(Margin::symmetric(16.0, 14.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        hrow(ui, |ui| {
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
                            col(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 2.0;
                                let mut lay = rtl_layouter(fs(14.0), p.text);
                                ui.add(
                                    egui::TextEdit::singleline(&mut dlg.file_name)
                                        .layouter(&mut lay)
                                        .frame(false)
                                        .horizontal_align(start_align())
                                        .font(fs(14.0))
                                        .text_color(p.text)
                                        .desired_width(240.0),
                                );
                                let is_torrent = torrent::is_torrent_link(&dlg.url, &dlg.file_name);
                                let kind = if is_torrent { "Torrent" } else { c.map(|c| c.label()).unwrap_or("Other") };
                                let line = match &dlg.probe {
                                    ProbeState::Torrent(m) => format!(
                                        "{}, {}, {}",
                                        kind,
                                        fmt_size(m.total),
                                        if m.files.len() == 1 { "1 file".to_string() } else { format!("{} files", m.files.len()) }
                                    ),
                                    ProbeState::Checking if is_torrent => format!("{}, reading its files...", kind),
                                    ProbeState::Failed(_) if is_torrent => format!("{}, files not read yet", kind),
                                    ProbeState::Ready(..) if is_torrent => format!("{}, size shows once it starts", kind),
                                    ProbeState::Ready(sz, _) if *sz > 0 => format!("{}, {}", kind, fmt_size(*sz)),
                                    ProbeState::Ready(..) | ProbeState::Failed(_) => format!("{}, size unknown", kind),
                                    _ => format!("{}, checking size...", kind),
                                };
                                lbl(ui, line, f(13.0), p.muted);
                            });
                            ui.with_layout(rl_layout(Align::Center), |ui| {
                                let (txt, col, bg) = if !url_valid {
                                    ("Use http or https", p.bad, p.bad.gamma_multiply(0.14))
                                } else {
                                    match &dlg.probe {
                                        ProbeState::Ready(..) | ProbeState::Torrent(_) | ProbeState::Playlist(_) => ("Link is ready", p.ok, p.ok_soft),
                                        ProbeState::Failed(_) if is_torrent_now => ("Starts anyway", p.sub, p.track),
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
                                ui.painter().txt(r.center(), Align2::CENTER_CENTER, txt, fs(12.0), col);
                            });
                        });
                    });
                ui.add_space(16.0);
            }

            // A torrent with several files: choose which to download.
            if let ProbeState::Torrent(meta) = &dlg.probe {
                if meta.files.len() > 1 {
                    ui.add_space(2.0);
                    torrent_file_list(ui, p, &meta.files, &mut dlg.torrent_sel, "add_files", 170.0);
                    ui.add_space(14.0);
                }
            }

            // The same link is already in the list: say so (it is saved under a new name, nothing is overwritten).
            if let Some((name, status)) = &dup {
                let state = match status {
                    Status::Complete => "downloaded before",
                    Status::Downloading => "downloading now",
                    Status::Queued => "waiting in the queue",
                    Status::Paused => "paused",
                    Status::Error => "failed earlier",
                };
                egui::Frame::none()
                    .fill(p.accent_soft)
                    .rounding(10.0)
                    .inner_margin(Margin::symmetric(12.0, 9.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        lbl(ui,
                            format!("This link is already in your list ({}: {}). A new copy is saved under a new name.", state, name),
                            f(13.0),
                            p.accent_ink,
                        );
                    });
                ui.add_space(14.0);
            }

            // The same file is already in the list, from another link.
            if let Some((sid, st, got, size, added)) = &same {
                egui::Frame::none()
                    .fill(p.accent_soft)
                    .rounding(10.0)
                    .inner_margin(Margin::symmetric(12.0, 10.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 8.0;
                        if *st == Status::Complete {
                            lbl(ui,
                                format!("You already downloaded this file ({}). Downloading it again saves a second copy.", added),
                                f(13.0),
                                p.accent_ink,
                            );
                            if button(ui, p, "Show in folder", Some("folder"), Kind::Secondary, 32.0, true).clicked() {
                                reveal_same = Some(*sid);
                            }
                        } else {
                            lbl(ui,
                                format!(
                                    "You have an unfinished copy of this file ({} of {}). Continue it from this link instead of starting over.",
                                    if *got == 0 { "0 MB".to_string() } else { fmt_size(*got) },
                                    fmt_size(*size)
                                ),
                                f(13.0),
                                p.accent_ink,
                            );
                            if button(ui, p, "Continue it", Some("play"), Kind::Primary, 32.0, true).clicked() {
                                continue_same = Some(*sid);
                            }
                        }
                    });
                ui.add_space(14.0);
            }

            marks.push(("after_card", ui.cursor().top()));
            lbl(ui, "Save to", fs(13.0), p.sub);
            ui.add_space(6.0);
            hrow(ui, |ui| {
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
                if te.changed() {
                    dlg.save_dir_auto = false; // the person chose
                }
                if button(ui, p, "Browse", None, Kind::Secondary, te.rect.height().max(44.0), true).clicked() {
                    let mut dialog = rfd::FileDialog::new().set_title("Choose a download folder");
                    let current = PathBuf::from(dlg.save_dir.trim());
                    if current.is_dir() {
                        dialog = dialog.set_directory(current);
                    }
                    if let Some(dir) = dialog.pick_folder() {
                        dlg.save_dir = dir.to_string_lossy().into_owned();
                        dlg.save_dir_auto = false;
                    }
                }
            });
            marks.push(("after_save", ui.cursor().top()));
            ui.add_space(18.0);
            let is_playlist = matches!(dlg.probe, ProbeState::Playlist(_));
            if is_playlist {
                // The videos join the queue and start a few at a time.
                lbl(ui, "The videos download a few at a time, as set in Settings.", f(13.0), p.muted);
            } else if busy {
                // Something is already downloading: let the user choose.
                lbl(ui,
                    "A download is running. Add this one to the queue to start after it, or download it now.",
                    f(13.0),
                    p.muted,
                );
            } else {
                hrow(ui, |ui| {
                    col(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        lbl(ui, "Start downloading now", fs(14.0), p.text);
                        lbl(ui, &queue_hint, f(13.0), p.muted);
                    });
                    ui.with_layout(rl_layout(Align::Center), |ui| {
                        toggle(ui, p, &mut dlg.start_now);
                    });
                });
            }
            marks.push(("after_toggle", ui.cursor().top()));
            ui.add_space(24.0);
            // Fixed-height footer so the dialog shrinks back when content goes away.
            let w = ui.available_width();
            // A playlist: say how many videos the button adds.
            let picked_videos = if let ProbeState::Playlist(_) = &dlg.probe { Some(dlg.playlist_sel.iter().filter(|b| **b).count()) } else { None };
            ui.allocate_ui_with_layout(vec2(w, 44.0), rl_layout(Align::Center), |ui| {
                if let Some(n) = picked_videos {
                    let label = if n == 1 { "Download 1 video".to_string() } else { format!("Download {} videos", n) };
                    if button(ui, p, &label, Some("download"), Kind::Primary, 44.0, ok).clicked() {
                        submit = true;
                    }
                } else if busy {
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
        if let Some(id) = reveal_same {
            if let Some(d) = self.downloads.iter().find(|x| x.id == id) {
                reveal_path(&d.path());
            }
        }
        if let Some(id) = continue_same {
            let (url, extra) = (self.dialog.url.trim().to_string(), self.dialog.extra.clone());
            self.close_dialog();
            self.relink_to(id, url, extra);
            return;
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
        let Some(&id) = self.confirm_delete.first() else { return };
        let many = self.confirm_delete.len();
        let name = self
            .downloads
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.file_name.clone())
            .unwrap_or_default();
        let mut yes = false;
        let mut no = false;
        // Unfinished files among them (partial files are useless once the download is gone).
        let unfinished: u64 = self
            .downloads
            .iter()
            .filter(|d| self.confirm_delete.contains(&d.id))
            .flat_map(cleanup::unfinished_files)
            .map(|f| fs::metadata(f).map(|m| m.len()).unwrap_or(0))
            .sum::<u64>();
        let has_unfinished = self.downloads.iter().any(|d| self.confirm_delete.contains(&d.id) && !cleanup::unfinished_files(d).is_empty());
        let delete_files = &mut self.delete_files;
        modal(ctx, p, "delete", 420.0, 240.0, |ui| {
            if many > 1 {
                lbl(ui, format!("Remove {} downloads?", many), fb(20.0), p.text);
                ui.add_space(8.0);
                lbl(ui, "They will leave the list. Finished files on your disk are kept.", f(14.0), p.sub);
            } else {
                lbl(ui, "Remove this download?", fb(20.0), p.text);
                ui.add_space(8.0);
                lbl(ui,
                    if has_unfinished {
                        format!("{} will leave the list.", name)
                    } else {
                        format!("{} will leave the list. The file on your disk is not deleted.", name)
                    },
                    f(14.0),
                    p.sub,
                );
            }
            if has_unfinished {
                ui.add_space(12.0);
                let label = if many > 1 {
                    format!("Also move the unfinished files to the Recycle Bin ({})", fmt_size(unfinished))
                } else {
                    format!("Also move the unfinished file to the Recycle Bin ({})", fmt_size(unfinished))
                };
                ui.checkbox(delete_files, rt(label, f(13.0), p.text));
            }
            ui.add_space(24.0);
            let fw = ui.available_width();
            ui.allocate_ui_with_layout(vec2(fw, 44.0), rl_layout(Align::Center), |ui| {
                // Destructive action wears the destructive color, never the primary one.
                let w = text_w(ui, "Remove", fs(14.0)) + 36.0;
                let (r, resp) = ui.allocate_exact_size(vec2(w, 44.0), Sense::click());
                ui.painter().rect_filled(r, 10.0, if resp.hovered() { p.bad.gamma_multiply(0.85) } else { p.bad });
                ui.painter().txt(r.center(), Align2::CENTER_CENTER, "Remove", fs(14.0), if p.dark { hex(0x1A0B09) } else { Color32::WHITE });
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
            self.confirm_delete.clear();
        }
    }
}

/// What the user asked for in a download's details window.
#[derive(Default)]
struct DetailsOut {
    close: bool,
    copy_link: bool,
    /// Pause, resume, retry or open, depending on the state.
    toggle: bool,
    folder: bool,
    /// "See details" / "Hide details" was pressed (the window may need a new height).
    resized: bool,
    /// Shown in the pop-up window, which has its own close button.
    popup: bool,
    /// Stop the download and close the window (it can be resumed later from the app).
    cancel: bool,
    /// Continue it from another address.
    change_link: bool,
    /// Compare the finished file with its published checksum.
    check: bool,
}

/// The content of a download's details window (used in the app and in the pop-up).
fn download_details_body(ui: &mut Ui, p: &Pal, d: &Download, screen_h: f32, expanded: &mut bool, out: &mut DetailsOut) {
        let status = match d.status {
            Status::Downloading => "Downloading",
            Status::Paused => "Paused",
            Status::Queued => "Queued",
            Status::Error => "Failed",
            Status::Complete => "Completed",
        };
        let eta = d.eta_secs().map(fmt_duration).unwrap_or_else(|| "-".into());
        let pct = (d.progress() * 100.0).round();
        let waiting = d.status == Status::Downloading && d.live.retry_at.is_some();
        let status = if waiting { "Server not answering" } else { status };
        let mut stats: Vec<(&str, String)> = vec![
            ("State", status.to_string()),
            ("File size", fmt_size(d.size)),
            (
                "Downloaded",
                if d.size == 0 {
                    fmt_size(d.downloaded)
                } else if d.status == Status::Complete {
                    format!("{} (100%)", fmt_size(d.size))
                } else {
                    format!("{} ({:.0}%)", if d.downloaded == 0 { "0 MB".to_string() } else { fmt_size(d.downloaded) }, pct)
                },
            ),
            ("Download speed", if d.status == Status::Downloading { fmt_speed(d.speed_bps) } else { "-".into() }),
            ("Time left", if d.status == Status::Downloading { eta } else { "-".into() }),
        ];
        if waiting {
            stats.insert(1, ("Next try", retry_note(d.live.retry_at)));
        } else if d.status == Status::Downloading && d.live.conns > 0 && !d.is_torrent() {
            stats.push(("Connections", d.live.conns.to_string()));
        }

        {
            ui.set_max_height(screen_h);
            ui.spacing_mut().item_spacing.y = 0.0;
            hrow(ui, |ui| {
                col(ui, |ui| {
                    lbl(ui, trunc_str(&d.file_name, 44), fb(20.0), p.text);
                    ui.add_space(3.0);
                    lbl(ui, "Download details", f(14.0), p.muted);
                });
                // The pop-up already has a close button in its title bar.
                if !out.popup {
                    ui.with_layout(rl_layout(Align::Min), |ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                        if icon_button_at(ui, p, r, "x", egui::Id::new("details_close"), "Close") {
                            out.close = true;
                        }
                    });
                }
            });
            ui.add_space(16.0);

            // How much is downloaded: the number, then a line that fills up.
            {
                let (head, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
                let big = if d.size == 0 {
                    fmt_size(d.downloaded)
                } else if d.status == Status::Complete {
                    "100%".to_string()
                } else {
                    format!("{:.0}%", pct)
                };
                let note = if d.size == 0 {
                    String::new()
                } else {
                    format!("{} of {}", fmt_size(d.downloaded), fmt_size(d.size))
                };
                ui.painter().txtm(head, head.left_center(), Align2::LEFT_CENTER, &big, fb(28.0), p.text);
                ui.painter().txtm(head, head.right_center(), Align2::RIGHT_CENTER, &note, f(13.0), p.muted);
                let (track_r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
                ui.painter().rect_filled(track_r, 5.0, p.track);
                let fill_color = match d.status {
                    Status::Paused | Status::Queued => p.paused,
                    Status::Error => p.bad,
                    Status::Complete => p.ok,
                    Status::Downloading => p.progress,
                };
                let frac = if d.status == Status::Complete { 1.0 } else { d.progress().clamp(0.0, 1.0) };
                if frac > 0.0 {
                    let w = (track_r.width() * frac).max(10.0);
                    ui.painter().rect_filled(mrect(track_r, Rect::from_min_size(track_r.min, vec2(w, 10.0))), 5.0, fill_color);
                }
            }
            ui.add_space(16.0);

            egui::Frame::none()
                .fill(p.bg)
                .stroke(Stroke::new(1.0, p.border))
                .rounding(12.0)
                .inner_margin(Margin::symmetric(16.0, 12.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = 6.0;
                    hrow(ui, |ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(130.0, 18.0), Sense::hover());
                        ui.painter().txtm(r, r.left_center(), Align2::LEFT_CENTER, "Link", f(13.0), p.muted);
                        // An unfinished file download can continue from another address.
                        let can_change = d.status != Status::Complete && matches!(d.source, Source::Http);
                        let btns = text_w(ui, "Copy", fs(14.0)) + 44.0 + if can_change { text_w(ui, "Change", fs(14.0)) + 44.0 } else { 0.0 };
                        let w = (ui.available_width() - btns).max(80.0);
                        let g = trunc(ui, &trunc_str(&d.url, 70), f(13.0), p.text, w);
                        ui.label(g);
                        if button(ui, p, "Copy", None, Kind::Secondary, 26.0, true).clicked() {
                            out.copy_link = true;
                        }
                        if can_change && button(ui, p, "Change", None, Kind::Secondary, 26.0, true).clicked() {
                            out.change_link = true;
                        }
                    });
                    for (k, v) in &stats {
                        hrow(ui, |ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(130.0, 18.0), Sense::hover());
                            ui.painter().txtm(r, r.left_center(), Align2::LEFT_CENTER, *k, f(13.0), p.muted);
                            let w = ui.available_width();
                            let g = trunc(ui, v, f(13.0), p.text, w);
                            ui.label(g);
                        });
                    }
                    // A finished file can be compared with the checksum its website publishes.
                    if d.status == Status::Complete && !d.is_torrent() {
                        hrow(ui, |ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(130.0, 18.0), Sense::hover());
                            ui.painter().txtm(r, r.left_center(), Align2::LEFT_CENTER, "Checksum", f(13.0), p.muted);
                            if button(ui, p, "Check file", Some("check"), Kind::Secondary, 26.0, true).clicked() {
                                out.check = true;
                            }
                        });
                    }
                });
            ui.add_space(14.0);

            // Details are hidden until asked for.
            let more = if *expanded { "Hide details" } else { "See details" };
            if button(ui, p, more, None, Kind::Secondary, 32.0, true).clicked() {
                *expanded = !*expanded;
                out.resized = true;
            }
            ui.add_space(14.0);
            if *expanded {
                // Speed over time.
                lbl(ui, "Speed over time", fs(13.0), p.sub);
                ui.add_space(6.0);
                let (gr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 84.0), Sense::hover());
                ui.painter().rect_filled(gr, 10.0, p.bg);
                ui.painter().rect_stroke(gr, 10.0, Stroke::new(1.0, p.border));
                if d.live.hist.len() >= 2 {
                    let peak = d.live.hist.iter().cloned().fold(1.0_f32, f32::max);
                    let mut inner = gr.shrink2(vec2(10.0, 10.0));
                    inner.min.y += 14.0;
                    let n = d.live.hist.len();
                    let step = inner.width() / (n.max(2) - 1) as f32;
                    let pts: Vec<egui::Pos2> = d
                        .live
                        .hist
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let x = inner.left() + i as f32 * step;
                            pos2(mx(inner, x), inner.bottom() - (v / peak) * inner.height())
                        })
                        .collect();
                    ui.painter().add(egui::Shape::line(pts, Stroke::new(2.0, p.accent)));
                    ui.painter().txtm(
                        gr,
                        pos2(gr.left() + 12.0, gr.top() + 6.0),
                        Align2::LEFT_TOP,
                        fmt_speed(peak as f64),
                        f(11.0),
                        p.muted,
                    );
                } else {
                    ui.painter().txt(
                        gr.center(),
                        Align2::CENTER_CENTER,
                        if d.status == Status::Downloading { "Collecting..." } else { "Shows while it downloads" },
                        f(13.0),
                        p.muted,
                    );
                }
                ui.add_space(14.0);

                // Event log.
                lbl(ui, "Activity", fs(13.0), p.sub);
                ui.add_space(6.0);
                egui::Frame::none()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.border))
                    .rounding(10.0)
                    .inner_margin(Margin::symmetric(12.0, 8.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt("dl_log")
                            .max_height(((screen_h - 640.0).max(78.0)).min(132.0))
                            .stick_to_bottom(true)
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 0.0;
                                if d.live.log.is_empty() {
                                    lbl(ui, "Nothing yet", f(12.0), p.muted);
                                }
                                let n = d.live.log.len();
                                for (i, line) in d.live.log.iter().enumerate() {
                                    let (time, msg) = line.split_at(line.find("  ").unwrap_or(0));
                                    let msg = msg.trim();
                                    // The color says what kind of event it is.
                                    let dot = if msg.starts_with("Failed") {
                                        p.bad
                                    } else if msg.contains("interrupted") {
                                        hex(0xD97706)
                                    } else if msg.starts_with("Finished") {
                                        p.ok
                                    } else if msg.starts_with("Paused") || msg.starts_with("Stopped") {
                                        p.muted
                                    } else {
                                        p.accent
                                    };
                                    let shown = i18n::translate(msg).unwrap_or_else(|| msg.to_string());
                                    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 26.0), Sense::hover());
                                    let pt = ui.painter();
                                    let x = r.left() + 8.0;
                                    // The line that ties the events together, with a dot for each.
                                    let top = if i == 0 { r.center().y } else { r.top() };
                                    let bottom = if i + 1 == n { r.center().y } else { r.bottom() };
                                    pt.vline(mx(r, x), top..=bottom, Stroke::new(1.5, p.border_strong));
                                    pt.circle_filled(pos2(mx(r, x), r.center().y), 4.5, dot);
                                    pt.circle_stroke(pos2(mx(r, x), r.center().y), 4.5, Stroke::new(1.5, p.bg));
                                    pt.txtm(r, pos2(x + 16.0, r.center().y), Align2::LEFT_CENTER, time, f(12.0), p.muted);
                                    let mw = (r.width() - 16.0 - 66.0).max(60.0);
                                    let g = trunc(ui, &shown, f(12.5), p.text, mw);
                                    let gy = g.size().y / 2.0;
                                    ui.painter().galley_m(r, pos2(x + 16.0 + 66.0, r.center().y - gy), g, p.text);
                                }
                            });
                    });
            }
            ui.add_space(16.0);
            let fw = ui.available_width();
            let start = if rtl() { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
            ui.allocate_ui_with_layout(vec2(fw, 40.0), start, |ui| {
                let (label, icon) = match d.status {
                    Status::Downloading => ("Pause", "pause"),
                    Status::Paused | Status::Queued => ("Resume", "play"),
                    Status::Error => ("Retry", "retry"),
                    Status::Complete => ("Open file", "open"),
                };
                if button(ui, p, label, Some(icon), Kind::Primary, 40.0, true).clicked() {
                    out.toggle = true;
                }
                ui.add_space(4.0);
                if button(ui, p, "Show in folder", Some("folder"), Kind::Secondary, 40.0, true).clicked() {
                    out.folder = true;
                }

                ui.with_layout(rl_layout(Align::Center), |ui| {
                    // In the pop-up this button cancels the download; a finished one just closes.
                    if out.popup && d.status != Status::Complete {
                        if button(ui, p, "Cancel", None, Kind::Secondary, 40.0, true).clicked() {
                            out.cancel = true;
                        }
                    } else if button(ui, p, "Close", None, Kind::Secondary, 40.0, true).clicked() {
                        out.close = true;
                    }
                });
            });
        }
}

impl App {
    /// "Version 0.1.7 · Up to date" / "Update available", under Settings in the sidebar.
    fn version_row(&self, ui: &mut Ui, p: &Pal) -> Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 30.0), Sense::click());
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            ui.painter().rect_filled(rect, 8.0, p.tile);
        }
        let ver = format!("{} {}", i18n::translate("Version").unwrap_or_else(|| "Version".to_string()), env!("CARGO_PKG_VERSION"));
        let cy = rect.center().y;
        ui.painter().text(
            pos2(mx(rect, rect.left() + 12.0), cy),
            malign(Align2::LEFT_CENTER),
            if rtl() { shape_rtl(&ver) } else { ver },
            f(12.0),
            p.muted,
        );
        let (label, color, dot) = match &self.update {
            update::UpdateState::Available(r) => (format!("{} {}", tr("Update"), r.version), p.accent_ink, Some(p.accent)),
            update::UpdateState::UpToDate => (tr("Up to date"), p.ok, None),
            update::UpdateState::Checking => (tr("Checking..."), p.muted, None),
            _ => (String::new(), p.muted, None),
        };
        if !label.is_empty() {
            let w = text_w_raw(ui, &label, fs(11.5));
            let pill_w = w + 20.0;
            let pill = Rect::from_min_size(pos2(rect.right() - 8.0 - pill_w, cy - 11.0), vec2(pill_w, 22.0));
            let pill = mrect(rect, pill);
            if let Some(d) = dot {
                ui.painter().rect_filled(pill, 11.0, p.accent_soft);
                ui.painter().circle_filled(pos2(if rtl() { pill.right() - 9.0 } else { pill.left() + 9.0 }, cy), 3.5, d);
            }
            let tx = if dot.is_some() { 6.0 } else { 0.0 };
            ui.painter().text(
                pos2(pill.center().x + if rtl() { -tx / 2.0 } else { tx / 2.0 }, cy),
                Align2::CENTER_CENTER,
                label,
                fs(11.5),
                color,
            );
        }
        resp
    }

    fn start_update_check(&mut self) {
        if matches!(self.update, update::UpdateState::Checking) {
            return;
        }
        self.update = update::UpdateState::Checking;
        self.update_error = None;
        let tx = self.update_tx.clone();
        thread::spawn(move || {
            let _ = tx.send(UpdateMsg::Checked(update::check(env!("CARGO_PKG_VERSION"))));
        });
        // Look again in six hours.
        self.update_due = Some(Instant::now() + Duration::from_secs(6 * 3600));
    }

    fn start_update_download(&mut self) {
        let update::UpdateState::Available(rel) = self.update.clone() else { return };
        self.update_error = None;
        self.update_downloading = true;
        self.update_done.store(0, std::sync::atomic::Ordering::Relaxed);
        let (tx, done) = (self.update_tx.clone(), self.update_done.clone());
        thread::spawn(move || {
            let _ = tx.send(UpdateMsg::Downloaded(update::download(&rel, &done)));
        });
    }

    fn poll_update(&mut self) {
        if let Some(at) = self.update_due {
            if Instant::now() >= at {
                self.update_due = None;
                self.start_update_check();
            }
        }
        while let Ok(msg) = self.update_rx.try_recv() {
            match msg {
                UpdateMsg::Checked(Ok(Some(rel))) => self.update = update::UpdateState::Available(rel),
                UpdateMsg::Checked(Ok(None)) => self.update = update::UpdateState::UpToDate,
                UpdateMsg::Checked(Err(e)) => self.update = update::UpdateState::Failed(e),
                UpdateMsg::Downloaded(Err(e)) => {
                    self.update_downloading = false;
                    self.update_error = Some(e);
                }
                UpdateMsg::Downloaded(Ok(path)) => {
                    // Run the installer on its own (it updates the app in place and opens it
                    // again), then let this copy close so its files can be replaced.
                    let started = Command::new(&path).args(["--silent", "--launch"]).spawn();
                    match started {
                        Ok(_) => {
                            self.update_launched = true;
                            self.update_downloading = false;
                        }
                        Err(e) => {
                            self.update_downloading = false;
                            self.update_error = Some(format!("The installer could not start: {}", e));
                        }
                    }
                }
            }
        }
        if self.update_launched && !self.demo {
            // Give the installer a moment to take over, then close.
            if self.update_closing.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(1500) {
                shell::request_quit();
            }
        }
    }

    /// The update window: what is new, and Update now.
    fn ui_update_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        if !self.update_open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) && !self.update_downloading && !self.update_launched {
            self.update_open = false;
            return;
        }
        if self.update_downloading || self.update_launched {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        let state = self.update.clone();
        let current = env!("CARGO_PKG_VERSION");
        let busy = self.update_downloading || self.update_launched;
        let (done_bytes, total) = match &state {
            update::UpdateState::Available(r) => (self.update_done.load(std::sync::atomic::Ordering::Relaxed), r.size),
            _ => (0, 0),
        };
        let error = self.update_error.clone();
        let mut close = false;
        let mut update_now = false;
        let mut check_again = false;
        let mut open_page: Option<String> = None;
        let screen_h = ctx.screen_rect().height();

        modal(ctx, p, "update", 520.0, 480.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            hrow(ui, |ui| {
                col(ui, |ui| {
                    let title = match &state {
                        update::UpdateState::Available(_) => "Update available",
                        update::UpdateState::UpToDate => "You are up to date",
                        update::UpdateState::Checking | update::UpdateState::Idle => "Checking for updates",
                        update::UpdateState::Failed(_) => "Could not check for updates",
                    };
                    lbl(ui, title, fb(22.0), p.text);
                    ui.add_space(4.0);
                    match &state {
                        update::UpdateState::Available(r) => {
                            lbl(ui, format!("Version {} is ready. You have {}.", r.version, current), f(14.0), p.muted);
                        }
                        update::UpdateState::UpToDate => {
                            lbl(ui, format!("Orexa Downloader {} is the newest version.", current), f(14.0), p.muted);
                        }
                        update::UpdateState::Failed(e) => {
                            lbl(ui, e.clone(), f(14.0), p.bad);
                        }
                        _ => {
                            lbl(ui, "One moment...", f(14.0), p.muted);
                        }
                    }
                });
                if !busy {
                    ui.with_layout(rl_layout(Align::Min), |ui| {
                        let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                        if icon_button_at(ui, p, r, "x", egui::Id::new("update_close"), "Close") {
                            close = true;
                        }
                    });
                }
            });

            if let update::UpdateState::Available(rel) = &state {
                ui.add_space(16.0);
                lbl(ui, "What is new", fs(14.0), p.text);
                ui.add_space(8.0);
                egui::Frame::none()
                    .fill(p.bg)
                    .stroke(Stroke::new(1.0, p.border))
                    .rounding(12.0)
                    .inner_margin(Margin::symmetric(16.0, 12.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        egui::ScrollArea::vertical()
                            .id_salt("update_notes")
                            .max_height((screen_h - 420.0).clamp(120.0, 300.0))
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                ui.spacing_mut().item_spacing.y = 4.0;
                                for line in update::plain_notes(&rel.notes) {
                                    match line {
                                        update::NoteLine::Heading(h) => {
                                            ui.add_space(4.0);
                                            lbl(ui, h, fs(13.5), p.text);
                                        }
                                        update::NoteLine::Bullet(b, level) => {
                                            let dot = if rtl() { format!("{} \u{2022}", b) } else { format!("\u{2022} {}", b) };
                                            ui.horizontal_wrapped(|ui| {
                                                ui.add_space(12.0 * level as f32);
                                                lbl(ui, dot, f(13.0), p.sub);
                                            });
                                        }
                                        update::NoteLine::Text(t) => {
                                            lbl(ui, t, f(13.0), p.sub);
                                        }
                                    }
                                }
                            });
                    });
                ui.add_space(16.0);

                if busy {
                    // Download progress, then the hand-over to the installer.
                    let frac = if total > 0 { (done_bytes as f32 / total as f32).clamp(0.0, 1.0) } else { 0.0 };
                    let note = if self.update_launched {
                        "Starting the update. Orexa will close and open again.".to_string()
                    } else if total > 0 {
                        format!("Downloading the update: {} of {}", fmt_size(done_bytes), fmt_size(total))
                    } else {
                        "Downloading the update...".to_string()
                    };
                    lbl(ui, note, f(13.0), p.sub);
                    ui.add_space(8.0);
                    let (track, _) = ui.allocate_exact_size(vec2(ui.available_width(), 10.0), Sense::hover());
                    ui.painter().rect_filled(track, 5.0, p.track);
                    let w = if self.update_launched { track.width() } else { (track.width() * frac).max(10.0) };
                    ui.painter().rect_filled(mrect(track, Rect::from_min_size(track.min, vec2(w, 10.0))), 5.0, p.progress);
                } else {
                    if let Some(e) = &error {
                        lbl(ui, e.clone(), f(13.0), p.bad);
                        ui.add_space(8.0);
                    }
                    let fw = ui.available_width();
                    let start = if rtl() { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
                    ui.allocate_ui_with_layout(vec2(fw, 44.0), start, |ui| {
                        if button(ui, p, "Update now", Some("download"), Kind::Primary, 44.0, true).clicked() {
                            update_now = true;
                        }
                        ui.add_space(4.0);
                        if button(ui, p, "Later", None, Kind::Secondary, 44.0, true).clicked() {
                            close = true;
                        }
                        if error.is_some() && !rel.page.is_empty() {
                            ui.with_layout(rl_layout(Align::Center), |ui| {
                                if button(ui, p, "Open the download page", None, Kind::Secondary, 44.0, true).clicked() {
                                    open_page = Some(rel.page.clone());
                                }
                            });
                        }
                    });
                }
            } else if !matches!(state, update::UpdateState::Checking | update::UpdateState::Idle) {
                ui.add_space(20.0);
                let fw = ui.available_width();
                let start = if rtl() { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
                ui.allocate_ui_with_layout(vec2(fw, 44.0), start, |ui| {
                    if button(ui, p, "Check again", None, Kind::Primary, 44.0, true).clicked() {
                        check_again = true;
                    }
                    ui.add_space(4.0);
                    if button(ui, p, "Close", None, Kind::Secondary, 44.0, true).clicked() {
                        close = true;
                    }
                });
            }
        });

        if update_now {
            self.start_update_download();
        }
        if check_again {
            self.start_update_check();
        }
        if let Some(url) = open_page {
            let _ = Command::new("explorer").arg(url).spawn();
        }
        if close {
            self.update_open = false;
        }
    }

    /// Keyboard shortcuts. They stay out of the way while typing or while a window is open.
    fn keyboard(&mut self, ctx: &egui::Context) {
        let modal_open = self.dialog.open
            || self.help_open
            || !self.confirm_delete.is_empty()
            || self.details_id.is_some()
            || self.welcome_open
            || self.update_open
            || self.sleep_at.is_some();
        if modal_open {
            return;
        }
        let typing = ctx.wants_keyboard_input();
        let (ctrl, key) = ctx.input(|i| {
            let k = |key| i.key_pressed(key);
            (
                i.modifiers.command,
                (
                    k(egui::Key::N),
                    k(egui::Key::V),
                    k(egui::Key::A),
                    k(egui::Key::F),
                    k(egui::Key::Comma),
                    k(egui::Key::F1),
                    k(egui::Key::Delete),
                    k(egui::Key::Space),
                    k(egui::Key::Enter),
                    k(egui::Key::Escape),
                    k(egui::Key::ArrowDown),
                    k(egui::Key::ArrowUp),
                ),
            )
        });
        let (n, v, a, find, comma, f1, del, space, enter, esc, down, up) = key;
        if ctrl && n {
            self.open_add();
            return;
        }
        if ctrl && find {
            self.page = Page::Downloads;
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("search_box")));
            return;
        }
        if ctrl && comma {
            self.page = Page::Settings;
            return;
        }
        if f1 {
            self.help_open = true;
            return;
        }
        if typing {
            if esc {
                ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("search_box")));
            }
            return;
        }
        if ctrl && v {
            // A link on the clipboard goes straight into the Add window.
            let link = arboard::Clipboard::new()
                .ok()
                .and_then(|mut c| c.get_text().ok())
                .and_then(|t| t.trim().lines().next().map(|l| l.trim().to_string()))
                .filter(|l| l.starts_with("http://") || l.starts_with("https://") || torrent::is_magnet(l));
            if let Some(link) = link {
                self.open_link_in_dialog(ctx, &link);
            }
            return;
        }
        if self.page != Page::Downloads {
            return;
        }
        if ctrl && a {
            self.selected = self.order.iter().copied().collect();
            return;
        }
        if esc {
            self.selected.clear();
        }
        if del && !self.selected.is_empty() {
            self.confirm_delete = self.selected.iter().copied().collect();
        }
        if space && !self.selected.is_empty() {
            // Pause what runs; if nothing runs, start what waits.
            let running = self.downloads.iter().any(|d| self.selected.contains(&d.id) && d.status == Status::Downloading);
            self.apply(vec![if running { Act::BulkPause } else { Act::BulkResume }]);
        }
        if enter && self.selected.len() == 1 {
            let id = *self.selected.iter().next().unwrap();
            let done = self.downloads.iter().find(|d| d.id == id).map(|d| d.status == Status::Complete).unwrap_or(false);
            self.apply(vec![if done { Act::Open(id) } else { Act::Details(id) }]);
        }
        if (down || up) && !self.order.is_empty() {
            let cur = self.anchor.and_then(|a| self.order.iter().position(|x| *x == a));
            let next = match (cur, down) {
                (None, _) => 0,
                (Some(i), true) => (i + 1).min(self.order.len() - 1),
                (Some(i), false) => i.saturating_sub(1),
            };
            let id = self.order[next];
            self.selected.clear();
            self.selected.insert(id);
            self.anchor = Some(id);
        }
    }

    /// "When all downloads finish": open the folder or put the computer to sleep.
    fn check_after_finish(&mut self) {
        if !self.batch_done || self.demo || !self.persist {
            return;
        }
        let busy = self.downloads.iter().any(|d| d.status == Status::Downloading || d.status == Status::Queued);
        if busy {
            return;
        }
        self.batch_done = false;
        match self.after_finish {
            AfterFinish::Nothing => {}
            AfterFinish::Folder => {
                if let Some(dir) = self.last_done_dir.clone() {
                    open_path(&dir);
                }
            }
            AfterFinish::Sleep => self.sleep_at = Some(Instant::now() + Duration::from_secs(30)),
        }
    }

    /// A short countdown before sleeping, so it never comes as a surprise.
    fn ui_sleep_notice(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some(at) = self.sleep_at else { return };
        let left = at.saturating_duration_since(Instant::now()).as_secs();
        if Instant::now() >= at {
            self.sleep_at = None;
            shell::sleep_computer();
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(250));
        let mut cancel = false;
        let mut now = false;
        modal(ctx, p, "sleep", 400.0, 200.0, |ui| {
            lbl(ui, "All downloads are finished", fb(20.0), p.text);
            ui.add_space(8.0);
            lbl(ui, format!("Going to sleep in {} s", left), f(14.0), p.sub);
            ui.add_space(22.0);
            let fw = ui.available_width();
            ui.allocate_ui_with_layout(vec2(fw, 44.0), rl_layout(Align::Center), |ui| {
                if button(ui, p, "Sleep now", None, Kind::Primary, 44.0, true).clicked() {
                    now = true;
                }
                ui.add_space(4.0);
                if button(ui, p, "Cancel", None, Kind::Secondary, 44.0, true).clicked() {
                    cancel = true;
                }
            });
        });
        if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.sleep_at = None;
        } else if now {
            self.sleep_at = None;
            shell::sleep_computer();
        }
    }

    /// Plain text for bug reports. No links, file names or folders: only host names and counts.
    fn diagnostics(&self) -> String {
        let host = |u: &str| -> String {
            if torrent::is_magnet(u) {
                return "magnet".into();
            }
            u.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("?").to_string()
        };
        let os = Command::new("cmd")
            .args(["/C", "ver"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Windows".into());
        let count = |st: Status| self.downloads.iter().filter(|d| d.status == st).count();
        let tool = |t: &ToolStatus| match t {
            ToolStatus::Ready => "ready".to_string(),
            ToolStatus::Missing => "missing".to_string(),
            ToolStatus::Installing(_) => "installing".to_string(),
            ToolStatus::Failed(e) => format!("failed: {}", e),
        };
        let free = shell::free_space(&PathBuf::from(self.default_dir.trim()))
            .map(fmt_size)
            .unwrap_or_else(|| "unknown".into());
        let mut out = String::new();
        out.push_str(&format!("Orexa Downloader {}\n", env!("CARGO_PKG_VERSION")));
        out.push_str(&format!("{}\n", os));
        out.push_str(&format!(
            "Language {}, theme {}, {} at a time, after finishing: {}\n",
            self.lang.code(),
            if self.dark { "dark" } else { "light" },
            self.max_parallel,
            if self.after_finish.code().is_empty() { "nothing" } else { self.after_finish.code() },
        ));
        out.push_str(&format!(
            "Sort into folders {}, keep sharing {}, start with Windows {}, open magnet/torrent {}\n",
            self.organize, self.seed_after, self.autostart, self.handle_types
        ));
        out.push_str(&format!("yt-dlp {}, ffmpeg {}\n", tool(&self.tools.ytdlp), tool(&self.tools.ffmpeg)));
        out.push_str(&format!("Extension folder found: {}\n", extension_dir().is_some()));
        out.push_str(&format!("Free space in the download folder: {}\n", free));
        out.push_str(&format!(
            "Downloads: {} (downloading {}, queued {}, paused {}, failed {}, completed {}, torrents {})\n",
            self.downloads.len(),
            count(Status::Downloading),
            count(Status::Queued),
            count(Status::Paused),
            count(Status::Error),
            count(Status::Complete),
            self.downloads.iter().filter(|d| d.is_torrent()).count(),
        ));
        let failed: Vec<&Download> = self.downloads.iter().filter(|d| d.status == Status::Error).rev().take(8).collect();
        if !failed.is_empty() {
            out.push_str("\nRecent failures:\n");
            for d in failed {
                out.push_str(&format!(
                    "- {} ({}): {}\n",
                    host(&d.url),
                    d.ext,
                    d.error.clone().unwrap_or_default()
                ));
                for l in d.live.log.iter().rev().take(4).rev() {
                    out.push_str(&format!("    {}\n", l));
                }
            }
        }
        out
    }

    /// The first-run welcome: offers the browser extension and the language.
    fn ui_welcome(&mut self, ctx: &egui::Context, p: &Pal) {
        if !self.welcome_open {
            return;
        }
        let mut guide = false;
        let mut later = false;
        let mut chosen: Option<i18n::Lang> = None;
        let cur = self.lang;
        modal(ctx, p, "welcome", 460.0, 420.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            ui.vertical_centered(|ui| {
                let (r, _) = ui.allocate_exact_size(vec2(64.0, 59.0), Sense::hover());
                paint_logo(ui, r);
            });
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                lbl(ui, "Welcome to Orexa Downloader", fb(22.0), p.text);
            });
            ui.add_space(10.0);
            ui.vertical_centered(|ui| {
                lbl(
                    ui,
                    "Add the browser extension so downloads, videos and magnet links open here with one click.",
                    f(14.0),
                    p.sub,
                );
                ui.add_space(4.0);
                lbl(ui, "It takes about a minute and you only do it once.", f(13.0), p.muted);
            });
            ui.add_space(20.0);
            // Language
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    let w = 2.0 * 120.0 + 8.0;
                    ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                    for l in i18n::Lang::ALL {
                        let (r, resp) = ui.allocate_exact_size(vec2(120.0, 36.0), Sense::click());
                        let sel = l == cur;
                        ui.painter().rect_filled(r, 10.0, if sel { p.accent_soft } else { p.tile });
                        if sel {
                            ui.painter().rect_stroke(r, 10.0, Stroke::new(1.0, p.accent));
                        }
                        if resp.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                        }
                        ui.painter().text(
                            r.center(),
                            Align2::CENTER_CENTER,
                            shape(l.native_name()),
                            if sel { fs(14.0) } else { f(14.0) },
                            if sel { p.accent_ink } else { p.text },
                        );
                        if resp.clicked() {
                            chosen = Some(l);
                        }
                    }
                });
            });
            ui.add_space(22.0);
            ui.vertical_centered(|ui| {
                if button(ui, p, "Show me how", Some("help"), Kind::Primary, 46.0, true).clicked() {
                    guide = true;
                }
                ui.add_space(8.0);
                if button(ui, p, "Maybe later", None, Kind::Secondary, 40.0, true).clicked() {
                    later = true;
                }
                ui.add_space(10.0);
                lbl(ui, "You can find it again in Settings.", f(12.0), p.muted);
            });
        });
        if let Some(l) = chosen {
            self.lang = l;
            i18n::set(l);
        }
        if guide || later {
            self.welcome_open = false;
            self.welcomed = true;
            if guide {
                self.help_open = true;
            }
        }
    }

    /// Details of an ordinary download: progress, link, speed over time, connections, event log.
    fn ui_download_details(&mut self, ctx: &egui::Context, p: &Pal, d: Download) {
        let width = 620.0_f32.min(ctx.screen_rect().width() - 96.0).max(420.0);
        let screen_h = ctx.screen_rect().height();
        let mut out = DetailsOut::default();
        let mut expanded = self.details_expanded;
        // Taller than the window (a small window, details shown): it scrolls, so the buttons
        // can always be reached.
        modal(ctx, p, "details", width, 460.0, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("details_scroll")
                .max_height((screen_h - 32.0 - 56.0).max(200.0))
                .auto_shrink([false, true])
                .show(ui, |ui| download_details_body(ui, p, &d, screen_h, &mut expanded, &mut out));
        });
        self.details_expanded = expanded;
        if out.close {
            self.details_id = None;
        }
        self.details_actions(&d, &out);
    }

    /// Carry out what a details window asked for.
    fn details_actions(&mut self, d: &Download, out: &DetailsOut) {
        if out.check {
            if self.details_id == Some(d.id) {
                self.details_id = None;
            }
            if self.floating == Some(d.id) {
                self.floating = None;
            }
            self.open_check(d.id);
            return;
        }
        if out.change_link {
            if self.details_id == Some(d.id) {
                self.details_id = None;
            }
            self.open_relink(d.id);
            return;
        }
        if out.copy_link {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(d.url.clone());
            }
        }
        // After a finished download, opening the file or its folder is the last thing the
        // window is needed for, so it closes.
        let done_and_used = d.status == Status::Complete && (out.folder || out.toggle);
        if out.folder {
            reveal_path(&d.path());
        }
        if done_and_used {
            if out.toggle {
                open_path(&d.path());
            }
            if self.floating == Some(d.id) {
                self.floating = None;
            }
            if self.details_id == Some(d.id) {
                self.details_id = None;
            }
            return;
        }
        if out.cancel {
            // Stop downloading and close the window. The download stays in the list with
            // everything saved so far, so it can be resumed later from the app.
            if d.status == Status::Downloading {
                self.engine.pause(d.id);
            }
            if self.floating == Some(d.id) {
                self.floating = None;
            }
            if self.details_id == Some(d.id) {
                self.details_id = None;
            }
            return;
        }
        if out.toggle {
            match d.status {
                Status::Downloading => self.engine.pause(d.id),
                Status::Paused | Status::Queued | Status::Error => self.start_id(d.id),
                Status::Complete => open_path(&d.path()),
            }
        }
    }

    /// A small window of its own for a download the browser just started, so the
    /// person sees it right away without the main window coming up.
    fn ui_floating_details(&mut self, ctx: &egui::Context, p: &Pal) {
        if shell::REOPEN_DOWNLOAD.swap(false, std::sync::atomic::Ordering::SeqCst) {
            // The latest download still running, or else the latest one.
            let pick = self
                .downloads
                .iter()
                .rev()
                .find(|d| d.status == Status::Downloading)
                .or_else(|| self.downloads.last())
                .map(|d| d.id);
            if pick.is_some() {
                self.floating = pick;
                self.floating_front = true;
            }
        }
        let Some(id) = self.floating else { return };
        let Some(d) = self.downloads.iter().find(|x| x.id == id).cloned() else {
            self.floating = None;
            return;
        };
        let mut out = DetailsOut { popup: true, ..Default::default() };
        let ppp = ctx.pixels_per_point().max(0.5);
        let (sw, sh) = ctx
            .input(|i| i.viewport().monitor_size)
            .map(|m| (m.x, m.y))
            .or_else(|| shell::screen_size().map(|(x, y)| (x / ppp, y / ppp)))
            .unwrap_or((1920.0, 1080.0));
        // As tall as what it shows (measured each frame), never taller than the screen.
        let max_h = (sh - 60.0).max(360.0);
        let w = 560.0_f32;
        let h = self.floating_h.clamp(300.0, max_h);
        // Opens in the middle of the screen, then stays where it is put.
        let pos = match self.floating_pos {
            Some((fid, at)) if fid == id => at,
            _ => {
                let at = pos2(((sw - w) / 2.0).max(0.0), ((sh - 560.0) / 2.0).max(0.0));
                self.floating_pos = Some((id, at));
                at
            }
        };
        let mut fit_h = h;
        let builder = egui::ViewportBuilder::default()
            .with_position(pos)
            .with_title(format!("{} - Orexa Downloader", d.file_name))
            .with_decorations(false)
            .with_inner_size([w, h])
            .with_min_inner_size([460.0, 300.0])
            .with_icon(window_icon())
            .with_active(true);
        let pal = p.clone();
        let mut expanded = self.details_expanded;
        let front = std::mem::take(&mut self.floating_front);
        // On top of other windows while it is new, then back to a normal window.
        let on_top = self.floating_top_until.map(|t| Instant::now() < t).unwrap_or(false);
        let was_on_top = self.floating_top_until.is_some();
        if !on_top {
            self.floating_top_until = None;
        }
        ctx.show_viewport_immediate(egui::ViewportId::from_hash_of("floating_details"), builder, |ctx, _| {
            // The same branded bar as the main window, not the standard Windows one.
            egui::TopBottomPanel::top("float_bar")
                .exact_height(40.0)
                .frame(egui::Frame::none().fill(pal.surface))
                .show(ctx, |ui| {
                    let bar = ui.max_rect();
                    ui.painter().hline(bar.x_range(), bar.bottom() - 0.5, Stroke::new(1.0, pal.border));
                    let drag = ui.interact(bar, egui::Id::new("float_drag"), Sense::click_and_drag());
                    if drag.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }
                    let sym_l = Rect::from_min_size(pos2(bar.left() + 16.0, bar.center().y - 8.0), vec2(18.0, 16.6));
                    paint_logo(ui, mrect(bar, sym_l));
                    ui.painter().txtm(bar, pos2(sym_l.right() + 10.0, bar.center().y), Align2::LEFT_CENTER, "Orexa Downloader", fs(13.0), pal.sub);
                    let bw = 46.0;
                    let mut x = bar.right();
                    for (icon, id, danger) in [("x", "float_close", true), ("minus", "float_min", false)] {
                        x -= bw;
                        let r = mrect(bar, Rect::from_min_size(pos2(x, bar.top()), vec2(bw, bar.height() - 1.0)));
                        let resp = ui.interact(r, egui::Id::new(id), Sense::click());
                        let hov = resp.hovered();
                        if hov {
                            ui.painter().rect_filled(r, 0.0, if danger { pal.bad } else { pal.tile });
                        }
                        let fg = if hov && danger { Color32::WHITE } else { pal.sub };
                        paint_icon(ui, Rect::from_center_size(r.center(), vec2(16.0, 16.0)), icon, fg);
                        if resp.clicked() {
                            if danger {
                                out.close = true;
                            } else {
                                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                            }
                        }
                    }
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::none().fill(pal.surface).inner_margin(Margin::same(24.0)))
                .show(ctx, |ui| {
                    // Only on a very small screen is there anything to scroll.
                    let body_max = (max_h - 40.0 - 48.0).max(200.0);
                    let o = egui::ScrollArea::vertical()
                        .id_salt("float_scroll")
                        .max_height(body_max)
                        .auto_shrink([false, true])
                        .show(ui, |ui| download_details_body(ui, &pal, &d, 2000.0, &mut expanded, &mut out));
                    fit_h = (40.0 + 48.0 + 8.0 + o.content_size.y.min(body_max)).ceil();
                });
            // Growing would push the bottom (and its buttons) off the screen: move up first.
            if fit_h > ctx.screen_rect().height() + 1.0 {
                if let Some(o) = ctx.input(|i| i.viewport().outer_rect) {
                    let limit = sh - 48.0; // above the taskbar
                    if o.top() + fit_h > limit {
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos2(o.left(), (limit - fit_h).max(0.0))));
                    }
                }
            }
            if ctx.input(|i| i.viewport().close_requested() || i.key_pressed(egui::Key::Escape)) {
                out.close = true;
            }
            window_frame_and_resize(ctx, &pal);
            if front {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::AlwaysOnTop));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            } else if !on_top && was_on_top {
                ctx.send_viewport_cmd(egui::ViewportCommand::WindowLevel(egui::WindowLevel::Normal));
            }
            ctx.request_repaint_after(Duration::from_millis(300));
        });
        // A new height takes effect next frame (the window follows its builder's size).
        if (fit_h - self.floating_h).abs() > 1.0 {
            self.floating_h = fit_h;
            ctx.request_repaint();
        }
        self.details_expanded = expanded;
        if out.close {
            self.floating = None;
        }
        self.details_actions(&d, &out);
    }

    /// Live details of a torrent: speeds, peers, and its files (which can be changed while it runs).
    fn ui_torrent_details(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some(id) = self.details_id else { return };
        let Some(d) = self.downloads.iter().find(|x| x.id == id).cloned() else {
            self.details_id = None;
            return;
        };
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.details_id = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        if !d.is_torrent() {
            self.ui_download_details(ctx, p, d);
            return;
        }
        let snap = torrent::snapshot(id);
        // The ticks start from what is chosen now.
        if let Some(sn) = &snap {
            if self.details_sel.len() != sn.files.len() {
                self.details_sel = sn.files.iter().map(|f| f.selected).collect();
            }
        }
        let mut close = false;
        let mut apply = false;
        let mut stop_sharing = false;
        let mut copy_hash = false;
        let width = 620.0_f32.min(ctx.screen_rect().width() - 96.0).max(420.0);
        let screen_h = ctx.screen_rect().height();
        let sel = &mut self.details_sel;
        let changed = snap
            .as_ref()
            .map(|sn| sn.files.iter().zip(sel.iter()).any(|(f, s)| f.selected != *s))
            .unwrap_or(false);

        modal(ctx, p, "details", width, 560.0, |ui| {
            ui.set_max_height(screen_h);
            ui.spacing_mut().item_spacing.y = 0.0;
            hrow(ui, |ui| {
                col(ui, |ui| {
                    lbl(ui, trunc_str(&d.file_name, 44), fb(20.0), p.text);
                    ui.add_space(3.0);
                    lbl(ui, "Torrent details", f(14.0), p.muted);
                });
                ui.with_layout(rl_layout(Align::Min), |ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                    if icon_button_at(ui, p, r, "x", egui::Id::new("details_close"), "Close") {
                        close = true;
                    }
                });
            });
            ui.add_space(16.0);

            match &snap {
                Some(sn) => {
                    let pct = if sn.total > 0 { sn.done as f32 / sn.total as f32 * 100.0 } else { 0.0 };
                    let ratio = if sn.done > 0 { sn.uploaded as f32 / sn.done as f32 } else { 0.0 };
                    let stats: [(&str, String); 8] = [
                        ("Progress", format!("{:.0}%, {} of {}", pct, fmt_size(sn.done), fmt_size(sn.total))),
                        ("Download speed", fmt_speed(sn.down_bps)),
                        ("Upload speed", fmt_speed(sn.up_bps)),
                        ("Sent so far", if sn.uploaded == 0 { "Nothing yet".to_string() } else { format!("{}  (ratio {:.2})", fmt_size(sn.uploaded), ratio) }),
                        ("Connected to", format!("{} peers, {} sharing the whole file", sn.peers_live, sn.seeders)),
                        ("Peers found", sn.peers_seen.to_string()),
                        ("State", if sn.finished { "Finished".to_string() } else { sn.state.clone() }),
                        ("Where", d.save_dir.clone()),
                    ];
                    egui::Frame::none()
                        .fill(p.bg)
                        .stroke(Stroke::new(1.0, p.border))
                        .rounding(12.0)
                        .inner_margin(Margin::symmetric(16.0, 12.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.spacing_mut().item_spacing.y = 6.0;
                            for (k, v) in stats {
                                hrow(ui, |ui| {
                                    let (r, _) = ui.allocate_exact_size(vec2(130.0, 18.0), Sense::hover());
                                    ui.painter().txtm(r, r.left_center(), Align2::LEFT_CENTER, k, f(13.0), p.muted);
                                    let w = ui.available_width();
                                    let g = trunc(ui, &v, f(13.0), p.text, w);
                                    ui.label(g);
                                });
                            }
                            hrow(ui, |ui| {
                                let (r, _) = ui.allocate_exact_size(vec2(130.0, 18.0), Sense::hover());
                                ui.painter().txtm(r, r.left_center(), Align2::LEFT_CENTER, "Torrent ID", f(13.0), p.muted);
                                lbl(ui, trunc_str(&sn.info_hash, 14), f(13.0), p.text);
                                if button(ui, p, "Copy", None, Kind::Secondary, 26.0, true).clicked() {
                                    copy_hash = true;
                                }
                            });
                        });
                    ui.add_space(14.0);
                    if sn.files.len() > 1 {
                        let files: Vec<(String, u64)> = sn.files.iter().map(|f| (f.path.clone(), f.size)).collect();
                        torrent_file_list(ui, p, &files, sel, "details_files", (screen_h - 470.0).clamp(120.0, 260.0));
                        ui.add_space(10.0);
                        lbl(ui,
                            "Tick or untick files while it runs. Files you untick stop downloading.",
                            f(12.0),
                            p.muted,
                        );
                        ui.add_space(12.0);
                    }
                }
                None => {
                    lbl(ui,
                        if d.status == Status::Complete {
                            "This torrent is finished. Live details show while a torrent is downloading or being shared."
                        } else {
                            "Live details show while the torrent is downloading. Start it to see peers, speeds and files."
                        },
                        f(14.0),
                        p.sub,
                    );
                    ui.add_space(10.0);
                    lbl(ui, format!("Size: {}   Saved in: {}", fmt_size(d.size), d.save_dir), f(13.0), p.muted);
                    ui.add_space(14.0);
                }
            }

            hrow(ui, |ui| {
                if d.seeding.is_some() && button(ui, p, "Stop sharing", Some("pause"), Kind::Secondary, 40.0, true).clicked() {
                    stop_sharing = true;
                }
                ui.with_layout(rl_layout(Align::Center), |ui| {
                    if snap.is_some() && changed {
                        let any = sel.iter().any(|b| *b);
                        if button(ui, p, "Apply", Some("check"), Kind::Primary, 40.0, any).clicked() {
                            apply = true;
                        }
                        ui.add_space(6.0);
                    }
                    if button(ui, p, "Close", None, Kind::Secondary, 40.0, true).clicked() {
                        close = true;
                    }
                });
            });
        });

        if copy_hash {
            if let (Some(sn), Ok(mut cb)) = (&snap, arboard::Clipboard::new()) {
                let _ = cb.set_text(sn.info_hash.clone());
            }
        }
        if stop_sharing {
            self.engine.pause(id);
        }
        if apply {
            let indices: Vec<usize> = self.details_sel.iter().enumerate().filter(|(_, b)| **b).map(|(i, _)| i).collect();
            let all = indices.len() == self.details_sel.len();
            if let Some(m) = self.downloads.iter_mut().find(|x| x.id == id) {
                m.files = if all { None } else { Some(indices.clone()) };
            }
            thread::spawn(move || {
                let _ = torrent::set_files(id, &indices);
            });
        }
        if close {
            self.details_id = None;
        }
    }
}

/// A name cut to `max` characters, with "..." when it was longer.
fn trunc_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}...", s.chars().take(max.saturating_sub(3)).collect::<String>())
    }
}

/// The extension's page in the Chrome Web Store (Brave and Edge use it too).
const STORE_URL: &str = "https://chromewebstore.google.com/detail/lhlfkhkcodflljkapdcpcecapmncjfeh";

/// Open the store page in the default browser.
fn open_store_page() {
    let _ = Command::new("explorer").arg(STORE_URL).spawn();
}

/// The pictures of the guide: the browser's own "new extension added" screens
/// (0-3 Chrome and Brave, 4-7 Edge).
const HELP_PICS: [&[u8]; 8] = [
    include_bytes!("../assets/tutorial/chrome1_action.png"),
    include_bytes!("../assets/tutorial/chrome2_added.png"),
    include_bytes!("../assets/tutorial/chrome3_enable.png"),
    include_bytes!("../assets/tutorial/chrome4_pin.png"),
    include_bytes!("../assets/tutorial/edge1_dots.png"),
    include_bytes!("../assets/tutorial/edge2_added.png"),
    include_bytes!("../assets/tutorial/edge3_turnon.png"),
    include_bytes!("../assets/tutorial/edge4_pin.png"),
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
    // Never enlarge a picture past its own size.
    let logical_width = logical_width.min(src.width() as f32);
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
    /// "How to add the extension". Chrome and Brave: turn on what the installer offered (or add it
    /// from the Chrome Web Store). Edge: load the extension folder by hand.
    fn ui_help_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        if !self.help_open {
            return;
        }
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.help_open = false;
            return;
        }
        let tabs = ["Chrome / Brave", "Edge"];
        let sel = self.help_browser.min(1);
        let edge = sel == 1;
        let mut close = false;
        let mut open_store = false;
        let mut pick = sel;
        let width = 700.0_f32.min(ctx.screen_rect().width() - 96.0).max(420.0);
        let scroll_h = (ctx.screen_rect().height() - 330.0).clamp(200.0, 560.0);

        // (title, text, picture) for each step.
        let steps: Vec<(&str, String, usize)> = if edge {
            vec![
                (
                    "Open your browser",
                    "After you install Orexa, open Edge and click the three dots in the top-right corner.".to_string(),
                    4,
                ),
                (
                    "Click Orexa Downloader extension has been added",
                    "It is at the top of the menu.".to_string(),
                    5,
                ),
                (
                    "Choose Turn on extension",
                    "Edge asks about the extension. Choose Turn on extension, not Remove extension.".to_string(),
                    6,
                ),
                (
                    "Pin it",
                    "Click the puzzle-piece icon and pin Orexa Downloader so it is always in reach.".to_string(),
                    7,
                ),
            ]
        } else {
            vec![
                (
                    "Open your browser",
                    "After you install Orexa, the browser shows a blue Action required button in the top-right corner.".to_string(),
                    0,
                ),
                (
                    "Click New extension added",
                    "Click Action required, then New extension added (Orexa Downloader).".to_string(),
                    1,
                ),
                (
                    "Choose Enable extension",
                    "The browser asks about the extension. Choose Enable extension, not Remove from Chrome.".to_string(),
                    2,
                ),
                (
                    "Pin it",
                    "Click the puzzle-piece icon and pin Orexa Downloader so it is always in reach.".to_string(),
                    3,
                ),
            ]
        };

        let screen_h = ctx.screen_rect().height();
        modal(ctx, p, "help", width, 640.0, |ui| {
            ui.set_max_height(screen_h);
            ui.spacing_mut().item_spacing.y = 0.0;
            hrow(ui, |ui| {
                col(ui, |ui| {
                    lbl(ui, "Add the Orexa extension to your browser", fb(21.0), p.text);
                    ui.add_space(4.0);
                    lbl(ui, "It takes about a minute and you only do it once.", f(14.0), p.muted);
                });
                ui.with_layout(rl_layout(Align::Min), |ui| {
                    let (r, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                    if icon_button_at(ui, p, r, "x", egui::Id::new("help_close"), "Close") {
                        close = true;
                    }
                });
            });
            ui.add_space(14.0);

            // Which browser.
            hrow(ui, |ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                for (i, name) in tabs.iter().enumerate() {
                    let kind = if i == sel { Kind::Primary } else { Kind::Secondary };
                    if button(ui, p, name, None, kind, 36.0, true).clicked() {
                        pick = i;
                    }
                }
                ui.with_layout(rl_layout(Align::Center), |ui| {
                    if button(ui, p, "Open the Chrome Web Store", Some("open"), Kind::Secondary, 36.0, true).clicked() {
                        open_store = true;
                    }
                });
            });
            ui.add_space(12.0);

            egui::ScrollArea::vertical().max_height(scroll_h).auto_shrink([false, true]).show(ui, |ui| {
                let img_w = ui.available_width() - 14.0;
                for (i, (title, body, pic)) in steps.iter().enumerate() {
                    hrow(ui, |ui| {
                        let (c, _) = ui.allocate_exact_size(vec2(28.0, 28.0), Sense::hover());
                        ui.painter().circle_filled(c.center(), 14.0, p.accent);
                        ui.painter().txt(c.center(), Align2::CENTER_CENTER, (i + 1).to_string(), fs(14.0), p.accent_ink);
                        ui.add_space(8.0);
                        col(ui, |ui| {
                            ui.spacing_mut().item_spacing.y = 2.0;
                            lbl(ui, *title, fs(15.0), p.text);
                            lbl(ui, body.as_str(), f(13.0), p.sub);
                        });
                    });
                    ui.add_space(8.0);
                    if let Some((tex, size)) = help_picture(ui.ctx(), *pic, img_w) {
                        let (r, _) = ui.allocate_exact_size(size, Sense::hover());
                        ui.painter().image(tex.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
                        ui.painter().rect_stroke(r, 8.0, Stroke::new(1.0, p.border_strong));
                    }
                    ui.add_space(20.0);
                }
                lbl(
                    ui,
                    if edge {
                        "Not seeing it? Open the Chrome Web Store with the button above. Edge may first ask to allow extensions from other stores. Choose Allow."
                    } else {
                        "Not seeing Action required? Open the Chrome Web Store with the button above and add the extension from there."
                    },
                    f(12.0),
                    p.muted,
                );
                ui.add_space(4.0);
            });
        });

        self.help_browser = pick;
        if open_store {
            open_store_page();
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

        if self.details_wait {
            if let Some(d) = self.downloads.iter().find(|d| d.is_torrent()) {
                self.details_id = Some(d.id);
                self.details_wait = false;
            }
        }

        // A .torrent file dropped on the window opens in the Add window.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            match dropped.iter().find(|f| torrent::is_torrent_file(&f.to_string_lossy())) {
                Some(f) => self.open_link_in_dialog(ctx, &f.to_string_lossy()),
                None => {
                    if self.persist {
                        shell::toast("Only .torrent files can be dropped here".into(), "Use Add URL for other links.".into());
                    }
                }
            }
        }
        let hovering = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering {
            let painter = ctx.layer_painter(egui::LayerId::new(Order::Foreground, egui::Id::new("drop_overlay")));
            let r = ctx.screen_rect();
            painter.rect_filled(r, 0.0, p.scrim);
            painter.rect_stroke(r.shrink(28.0), 20.0, Stroke::new(2.5, p.accent));
            painter.txt(
                r.center() - vec2(0.0, 12.0),
                Align2::CENTER_CENTER,
                "Drop a .torrent file to add it",
                fb(24.0),
                Color32::WHITE,
            );
            painter.txt(
                r.center() + vec2(0.0, 24.0),
                Align2::CENTER_CENTER,
                "You can also click a magnet link anywhere.",
                f(14.0),
                Color32::from_white_alpha(190),
            );
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
        // First run: greet once the window is really on screen.
        if !self.welcomed
            && self.persist
            && self.screenshot_path.is_none()
            && !self.welcome_open
            && !self.background_pending
            && self.started.elapsed() >= Duration::from_millis(900)
            && shell::window_visible()
        {
            self.welcome_open = true;
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
        self.poll_relink();
        self.run_delayed_starts();
        self.run_pending_recycle();
        let want = if self.speed_on { self.speed_kbs * 1024 } else { 0 };
        if throttle::get() != want {
            throttle::set(want);
        }
        self.tick_schedule();
        self.pump_queue();
        self.tick_demo();
        self.sample_speed();
        self.keyboard(ctx);
        self.check_after_finish();
        // A hand-back nobody collected (the browser extension is an older version).
        let stale: Vec<u64> = self
            .handbacks
            .iter()
            .filter(|(_, t)| t.elapsed() > Duration::from_secs(90))
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.handbacks.remove(&id);
            if let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) {
                d.error = Some(
                    "The site only lets the browser download this. Start it again from the browser, or update the Orexa extension so it can take over by itself."
                        .into(),
                );
            }
        }
        self.poll_update();
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
        self.ui_relink_dialog(ctx, &p);
        self.ui_check_dialog(ctx, &p);
        self.ui_help_dialog(ctx, &p);
        self.ui_welcome(ctx, &p);
        self.ui_update_dialog(ctx, &p);
        self.ui_floating_details(ctx, &p);
        self.ui_sleep_notice(ctx, &p);
        self.ui_torrent_details(ctx, &p);
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
    // Windows opened a magnet link or a .torrent file with Orexa: pass it to the app.
    for a in std::env::args().skip(1) {
        if torrent::is_magnet(&a) || torrent::is_torrent_file(&a) {
            native::hand_over(&a);
        }
    }
    let mut o = Options {
        screenshot: None,
        demo: false,
        dark: false,
        open: None,
        lang: None,
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
            "--lang" => o.lang = args.next(),
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

#[cfg(test)]
mod saving_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orexa-save-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_taken_name_gets_a_number_and_never_overwrites() {
        let d = tmp("names");
        let dir = d.to_string_lossy().to_string();
        assert_eq!(unique_name(&dir, "report.pdf", &|_| false), "report.pdf");
        fs::write(d.join("report.pdf"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "report.pdf", &|_| false), "report (1).pdf");
        fs::write(d.join("report (1).pdf"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "report.pdf", &|_| false), "report (2).pdf");
        // a name another download is about to use counts as taken too
        assert_eq!(unique_name(&dir, "fresh.zip", &|n| n == "fresh.zip"), "fresh (1).zip");
        // no extension, and dots inside the name
        fs::write(d.join("README"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "README", &|_| false), "README (1)");
        fs::write(d.join("a.b.c.tar.gz"), b"x").unwrap();
        assert_eq!(unique_name(&dir, "a.b.c.tar.gz", &|_| false), "a.b.c.tar (1).gz");
        let _ = fs::remove_dir_all(&d);
    }

    #[test]
    fn files_are_sorted_into_folders_by_type() {
        assert_eq!(type_folder("movie.mp4", false), "Videos");
        assert_eq!(type_folder("song.MP3", false), "Music");
        assert_eq!(type_folder("setup.exe", false), "Programs");
        assert_eq!(type_folder("photos.zip", false), "Compressed");
        assert_eq!(type_folder("report.pdf", false), "Documents");
        assert_eq!(type_folder("something.xyz", false), "Other");
        assert_eq!(type_folder("Sintel", true), "Torrents");
    }

    #[test]
    fn the_same_link_is_recognised_whatever_the_slash_or_case() {
        assert!(same_url("https://Example.com/a.zip", "https://example.com/a.zip/"));
        assert!(same_url("https://x.com/a#top", "https://x.com/a"));
        assert!(!same_url("https://x.com/a", "https://x.com/b"));
        assert!(!same_url("", ""));
    }

    #[test]
    fn it_refuses_a_download_that_cannot_fit() {
        let d = tmp("space");
        let mk = |size: u64| Download {
            id: 1,
            url: "https://x.com/f".into(),
            file_name: "f.bin".into(),
            ext: "bin".into(),
            size,
            downloaded: 0,
            status: Status::Downloading,
            speed_bps: 0.0,
            save_dir: d.to_string_lossy().into(),
            added: String::new(),
            error: None,
            source: Source::Http,
            thumb: None,
            files: None,
            seeding: None,
            live: Default::default(),
            extra: Extra::default(),
        };
        assert!(space_problem(&mk(1024)).is_none(), "a small file fits");
        let msg = space_problem(&mk(u64::MAX / 4)).expect("a huge file must be refused");
        assert!(msg.starts_with("Not enough free space on"), "{msg}");
        assert!(space_problem(&mk(0)).is_none(), "an unknown size is not refused");
        let _ = fs::remove_dir_all(&d);
    }
}

#[cfg(test)]
mod arabic_tests {
    use super::*;

    #[test]
    fn numbers_and_units_keep_their_order_in_arabic_text() {
        // Logical: "3.1 GB من 5.0 GB". Drawn left to right, the second number
        // comes first and each "number unit" pair stays intact.
        let v = shape_rtl("3.1 GB \u{0645}\u{0646} 5.0 GB");
        assert!(v.starts_with("5.0 GB"), "{v}");
        assert!(v.ends_with("3.1 GB"), "{v}");
    }

    #[test]
    fn standalone_letters_use_forms_the_font_has() {
        // Isolated alef has no presentation form in the font; it must come out as
        // the plain letter.
        let v = shape_rtl("\u{0627}");
        assert_eq!(v, "\u{0627}");
    }

    #[test]
    fn english_text_is_untouched() {
        assert_eq!(shape("Hello 123"), "Hello 123");
    }
}

#[cfg(test)]
mod polish_tests {
    use super::*;

    #[test]
    fn after_finish_codes_round_trip() {
        for a in [AfterFinish::Nothing, AfterFinish::Folder, AfterFinish::Sleep] {
            assert_eq!(AfterFinish::from_code(a.code()), a);
        }
        assert_eq!(AfterFinish::from_code("unknown"), AfterFinish::Nothing);
    }

    #[test]
    fn the_event_log_keeps_the_latest_lines() {
        let mut l = LiveInfo::default();
        for i in 0..200 {
            l.note(format!("event {}", i));
        }
        assert_eq!(l.log.len(), 80);
        assert!(l.log.last().unwrap().ends_with("event 199"));
    }

    #[test]
    fn new_texts_translate() {
        let _g = i18n::LANG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        i18n::set(i18n::Lang::Ar);
        assert_eq!(i18n::translate("3 selected").as_deref(), Some("3 محدد"));
        assert_eq!(i18n::translate("Remove 4 downloads?").as_deref(), Some("إزالة 4 تنزيلات؟"));
        assert_eq!(i18n::translate("Trying again in 25 s").as_deref(), Some("محاولة جديدة بعد 25 ث"));
        assert_eq!(i18n::translate("Starts at 02:00").as_deref(), Some("يبدأ في 02:00"));
        assert_eq!(i18n::translate("Matches (SHA-256)").as_deref(), Some("مطابق (SHA-256)"));
        i18n::set(i18n::Lang::En);
    }
}
