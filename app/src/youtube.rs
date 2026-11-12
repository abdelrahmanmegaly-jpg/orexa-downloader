// YouTube support. YouTube does not serve plain files, so this module drives
// yt-dlp (and ffmpeg to join separate video and audio streams). The app sets
// both tools up on first use, into %LOCALAPPDATA%\Orexa\tools.

use super::*;
use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;

use serde_json::Value;

const YTDLP_URL: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/yt-dlp.exe";
const YTDLP_SUMS: &str = "https://github.com/yt-dlp/yt-dlp/releases/latest/download/SHA2-256SUMS";
const FFMPEG_SUMS: &str = "https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest/checksums.sha256";
const FFMPEG_ZIP: &str = "ffmpeg-master-latest-win64-gpl-shared.zip";
const FFMPEG_URL: &str =
    "https://github.com/yt-dlp/FFmpeg-Builds/releases/download/latest/ffmpeg-master-latest-win64-gpl-shared.zip";

// ---------------------------------------------------------------------------
// Types shared with the rest of the app
// ---------------------------------------------------------------------------

/// What to fetch. `height` 0 means the best available.
#[derive(Clone, PartialEq, Debug)]
pub struct YtOpts {
    pub id: String,
    pub height: u32,
    pub audio: bool,
}

#[derive(PartialEq)]
pub struct Thumb {
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

#[derive(Clone, PartialEq)]
pub struct YtInfo {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: u64,
    pub thumb: Option<Arc<Thumb>>,
    /// Address of the thumbnail picture (the browser dialog shows it from there).
    pub thumb_url: String,
    /// Video heights offered as separate streams, highest first.
    pub heights: Vec<u32>,
    pub video_sizes: HashMap<u32, u64>,
    pub audio_size: u64,
    /// Best single-file (video and audio together) option.
    pub std_height: u32,
    pub std_size: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    YtDlp,
    Ffmpeg,
}

pub enum SetupEvent {
    Progress(Tool, u64, u64),
    Done(Tool),
    Failed(Tool, String),
}

#[derive(Clone)]
pub struct Choice {
    pub label: String,
    pub height: u32,
    pub audio: bool,
    pub size: u64,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn is_youtube(url: &str) -> bool {
    let u = url.trim();
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..host_end].to_lowercase();
    let path = &rest[host_end..];
    if host == "youtu.be" {
        return path.len() > 1;
    }
    let yt = host == "youtube.com" || host.ends_with(".youtube.com") || host == "youtube-nocookie.com";
    yt && (path.starts_with("/watch")
        || (path.starts_with("/playlist") && path.contains("list="))
        || path.starts_with("/shorts/")
        || path.starts_with("/live/")
        || path.starts_with("/embed/")
        || path.starts_with("/v/"))
}

/// A Facebook video page: a reel, a watch link, a video or a share link, or fb.watch.
pub fn is_facebook_video(url: &str) -> bool {
    let u = url.trim();
    let rest = u
        .strip_prefix("https://")
        .or_else(|| u.strip_prefix("http://"))
        .unwrap_or(u);
    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let host = rest[..host_end].to_lowercase();
    let path = rest[host_end..].to_lowercase();
    if host == "fb.watch" {
        return path.len() > 2;
    }
    if !(host == "facebook.com" || host.ends_with(".facebook.com")) {
        return false;
    }
    let digits_after = |marker: &str| {
        path.find(marker)
            .map(|i| path[i + marker.len()..].chars().take_while(|c| c.is_ascii_digit()).count() >= 6)
            .unwrap_or(false)
    };
    digits_after("/reel/")
        || digits_after("/reels/")
        || path.contains("/videos/") && path.chars().filter(|c| c.is_ascii_digit()).count() >= 6
        || (path.starts_with("/watch") || path.starts_with("/video.php")) && (path.contains("?v=") || path.contains("&v="))
        || path.starts_with("/share/r/")
        || path.starts_with("/share/v/")
}

/// A short name for a Facebook video page, used to keep its details for a little while.
pub fn facebook_key(url: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    if !is_facebook_video(url) {
        return None;
    }
    // The same video can come with different tracking parameters: ignore everything after "?"
    // except the watch id.
    let trimmed = url.trim();
    let base = trimmed.split(['#']).next().unwrap_or(trimmed);
    let canon = match (base.find("?v="), base.find("&v=")) {
        (Some(i), _) | (None, Some(i)) => {
            let v: String = base[i + 3..].chars().take_while(|c| c.is_ascii_digit()).collect();
            format!("watch:{}", v)
        }
        _ => base.split('?').next().unwrap_or(base).trim_end_matches('/').to_lowercase(),
    };
    let hash = Sha256::digest(canon.as_bytes());
    Some(format!("fb-{}", hash.iter().take(8).map(|b| format!("{:02x}", b)).collect::<String>()))
}

/// Links the video engine (yt-dlp) reads: YouTube and Facebook videos.
pub fn uses_ytdlp(url: &str) -> bool {
    is_youtube(url) || is_facebook_video(url)
}

pub fn tools_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("Orexa").join("tools")
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|p| p.join(name))
        .find(|p| p.is_file())
}

pub fn ytdlp_path() -> Option<PathBuf> {
    let p = tools_dir().join("yt-dlp.exe");
    if p.is_file() {
        Some(p)
    } else {
        find_in_path("yt-dlp.exe")
    }
}

pub fn ffmpeg_path() -> Option<PathBuf> {
    let p = tools_dir().join("ffmpeg.exe");
    if p.is_file() {
        Some(p)
    } else {
        find_in_path("ffmpeg.exe")
    }
}

/// A command that never opens a console window.
fn hidden(path: &std::path::Path) -> Command {
    let mut c = Command::new(path);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

pub fn fmt_clock(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{}:{:02}:{:02}", h, m, s)
    } else {
        format!("{}:{:02}", m, s)
    }
}

/// The usual name for a video height: 1012 -> 1080, 676 -> 720, 2026 -> 2160.
pub fn nominal_height(h: u32) -> u32 {
    [144, 240, 360, 480, 720, 1080, 1440, 2160, 4320]
        .into_iter()
        .find(|n| *n >= h)
        .unwrap_or(h)
}

/// The quality choices to offer for a probed video.
pub fn choices(info: &YtInfo, has_ffmpeg: bool) -> Vec<Choice> {
    let mut v: Vec<Choice> = Vec::new();
    if has_ffmpeg {
        for h in info.heights.iter().copied().filter(|h| *h >= 200) {
            let size = info.video_sizes.get(&h).copied().unwrap_or(0) + info.audio_size;
            // Videos that are not 16:9 have odd heights (1012, 676...); show the usual name.
            let nominal = nominal_height(h);
            match v.iter_mut().find(|c| c.height == nominal) {
                Some(c) => c.size = c.size.max(size),
                None => v.push(Choice { label: format!("{}p", nominal), height: nominal, audio: false, size }),
            }
        }
    }
    if v.is_empty() {
        v.push(Choice {
            label: if info.std_height > 0 { format!("{}p", info.std_height) } else { "Standard".into() },
            height: info.std_height,
            audio: false,
            size: info.std_size,
        });
    }
    v.push(Choice { label: "Audio only".into(), height: 0, audio: true, size: info.audio_size });
    v
}

/// Which choice is selected first: the best one up to 1080p.
pub fn default_choice(list: &[Choice]) -> usize {
    list.iter()
        .position(|c| !c.audio && c.height <= 1080)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// YouTube sign-in cookies (shared by the browser extension)
// ---------------------------------------------------------------------------

pub fn cookies_dir() -> PathBuf {
    state::data_dir().join("cookies")
}

pub fn cookies_path() -> PathBuf {
    cookies_dir().join("youtube.txt")
}

/// yt-dlp rewrites the cookies file it is given, so each run gets its own copy; two runs
/// (reading one video while another downloads) never fight over one file.
pub struct CookieCopy(Option<PathBuf>);

impl CookieCopy {
    pub fn none() -> Self {
        CookieCopy(None)
    }
    pub fn new() -> Self {
        if !cookies_path().exists() {
            return Self(None);
        }
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let copy = cookies_dir().join(format!("run-{}-{}.txt", std::process::id(), stamp));
        match fs::copy(cookies_path(), &copy) {
            Ok(_) => Self(Some(copy)),
            Err(_) => Self(None),
        }
    }

    pub fn path(&self) -> Option<&std::path::Path> {
        self.0.as_deref()
    }
}

/// Remove leftover per-run copies (for example after a crash); they hold private sign-in data.
pub fn clean_stale_cookie_copies() {
    let Ok(rd) = fs::read_dir(cookies_dir()) else { return };
    for e in rd.filter_map(|e| e.ok()) {
        let name = e.file_name().to_string_lossy().into_owned();
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d > Duration::from_secs(120))
            .unwrap_or(true);
        if name.starts_with("run-") && name.ends_with(".txt") && old {
            let _ = fs::remove_file(e.path());
        }
    }
}

impl Drop for CookieCopy {
    fn drop(&mut self) {
        if let Some(p) = &self.0 {
            let _ = fs::remove_file(p);
        }
    }
}

// ---------------------------------------------------------------------------
// Tool setup
// ---------------------------------------------------------------------------

pub fn setup_tool(tool: Tool, tx: Sender<SetupEvent>) {
    thread::spawn(move || {
        let res = match tool {
            Tool::YtDlp => fetch_to(
                YTDLP_URL,
                &tools_dir().join("yt-dlp.exe"),
                tool,
                &tx,
                Some((YTDLP_SUMS, "yt-dlp.exe")),
            ),
            Tool::Ffmpeg => install_ffmpeg(&tx),
        };
        let _ = tx.send(match res {
            Ok(()) => SetupEvent::Done(tool),
            Err(e) => SetupEvent::Failed(tool, e),
        });
    });
}

/// Finds the hash for `name` in a "hash  filename" checksum list.
pub fn find_hash(list: &str, name: &str) -> Option<String> {
    list.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let hash = it.next()?;
        let file = it.next()?.trim_start_matches('*');
        (file == name && hash.len() == 64).then(|| hash.to_lowercase())
    })
}

fn fetch_to(
    url: &str,
    dest: &std::path::Path,
    tool: Tool,
    tx: &Sender<SetupEvent>,
    verify: Option<(&str, &str)>,
) -> Result<(), String> {
    use sha2::{Digest, Sha256};
    fs::create_dir_all(dest.parent().unwrap_or(std::path::Path::new("."))).map_err(|e| e.to_string())?;
    let client = reqwest::blocking::Client::builder()
        .user_agent("Orexa-Downloader")
        .timeout(None)
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("Server answered {}", resp.status()));
    }
    let total = resp.content_length().unwrap_or(0);
    let part = dest.with_extension("part");
    let mut file = fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut buf = [0u8; 64 * 1024];
    let mut done = 0u64;
    let mut last = Instant::now();
    let mut hasher = Sha256::new();
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if last.elapsed() > Duration::from_millis(200) {
            last = Instant::now();
            let _ = tx.send(SetupEvent::Progress(tool, done, total));
        }
    }
    drop(file);
    // Check the file against the publisher's checksum list when it is available.
    if let Some((sums_url, name)) = verify {
        let list = client.get(sums_url).send().ok().and_then(|r| r.text().ok());
        if let Some(expected) = list.as_deref().and_then(|l| find_hash(l, name)) {
            let actual = format!("{:x}", hasher.finalize());
            if actual != expected {
                let _ = fs::remove_file(&part);
                return Err("The download did not match its checksum".into());
            }
        }
    }
    let _ = fs::remove_file(dest);
    fs::rename(&part, dest).map_err(|e| e.to_string())
}

fn install_ffmpeg(tx: &Sender<SetupEvent>) -> Result<(), String> {
    let dir = tools_dir();
    let zip_path = dir.join("ffmpeg.zip");
    fetch_to(FFMPEG_URL, &zip_path, Tool::Ffmpeg, tx, Some((FFMPEG_SUMS, FFMPEG_ZIP)))?;
    let file = fs::File::open(&zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    // The "shared" build keeps ffmpeg.exe small and puts its code in DLLs next to it,
    // so unpack everything from the bin folder (except the media player).
    let mut found_exe = false;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().replace('\\', "/");
        let Some(file_name) = name.rsplit('/').next().map(|s| s.to_string()) else { continue };
        let lower = file_name.to_lowercase();
        let in_bin = name.contains("/bin/");
        let wanted = in_bin && (lower.ends_with(".dll") || lower == "ffmpeg.exe" || lower == "ffprobe.exe");
        if !wanted {
            continue;
        }
        let part = dir.join(format!("{}.part", file_name));
        let mut out = fs::File::create(&part).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
        drop(out);
        let dest = dir.join(&file_name);
        let _ = fs::remove_file(&dest);
        fs::rename(&part, &dest).map_err(|e| e.to_string())?;
        if lower == "ffmpeg.exe" {
            found_exe = true;
        }
    }
    let _ = fs::remove_file(&zip_path);
    if found_exe {
        Ok(())
    } else {
        Err("ffmpeg.exe was not found in the download".into())
    }
}

// ---------------------------------------------------------------------------
// Probe: title, length, thumbnail and sizes, without downloading the video
// ---------------------------------------------------------------------------

fn last_error_line(text: &str) -> String {
    text.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .last()
        .map(|l| l.trim_start_matches("ERROR: ").to_string())
        .unwrap_or_else(|| "Could not read this video".into())
}

/// The 11-character video id in a YouTube link (watch, youtu.be, shorts, live, embed).
/// A YouTube playlist in the link (`list=`), if there is one.
pub fn playlist_id(url: &str) -> Option<String> {
    if !is_youtube(url) {
        return None;
    }
    let q = url.trim().split_once('?')?.1;
    q.split(['&', '#'])
        .find_map(|part| part.strip_prefix("list="))
        .map(|v| v.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect::<String>())
        .filter(|id| id.len() >= 2)
}

/// One video of a playlist.
#[derive(Clone, PartialEq, Debug)]
pub struct PlEntry {
    pub id: String,
    pub title: String,
    pub duration: u64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Playlist {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub entries: Vec<PlEntry>,
}

/// Read a playlist's videos (titles and lengths only, which is quick even for long lists).
pub fn probe_playlist(url: &str) -> Result<Playlist, String> {
    let id = playlist_id(url).ok_or("Not a playlist link")?;
    let page = format!("https://www.youtube.com/playlist?list={}", id);
    match playlist_inner(&page, false) {
        // Private or unlisted lists need the browser's sign-in.
        Err(e) if cookies_path().exists() && (needs_signin(&e) || e.to_lowercase().contains("does not exist")) => {
            playlist_inner(&page, true)
        }
        other => other,
    }
}

fn playlist_inner(page: &str, use_cookies: bool) -> Result<Playlist, String> {
    let exe = ytdlp_path().ok_or("yt-dlp is not installed")?;
    let cookies = if use_cookies { CookieCopy::new() } else { CookieCopy::none() };
    let mut cmd = hidden(&exe);
    cmd.args(["--flat-playlist", "--yes-playlist", "--dump-single-json", "--no-warnings", "--encoding", "utf-8"]);
    if let Some(p) = cookies.path() {
        cmd.arg("--cookies").arg(p);
    }
    let out = cmd.arg(page).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(last_error_line(&String::from_utf8_lossy(&out.stderr)));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    parse_playlist(&v)
}

pub fn parse_playlist(v: &Value) -> Result<Playlist, String> {
    let entries: Vec<PlEntry> = v["entries"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    let id = e["id"].as_str()?.to_string();
                    let title = e["title"].as_str().unwrap_or("").to_string();
                    // Videos that are gone or private cannot be downloaded.
                    if id.len() != 11 || title == "[Private video]" || title == "[Deleted video]" {
                        return None;
                    }
                    Some(PlEntry { id, title, duration: e["duration"].as_f64().unwrap_or(0.0) as u64 })
                })
                .collect()
        })
        .unwrap_or_default();
    if entries.is_empty() {
        return Err("This playlist has no videos that can be downloaded".into());
    }
    Ok(Playlist {
        id: v["id"].as_str().unwrap_or("").to_string(),
        title: v["title"].as_str().unwrap_or("YouTube playlist").to_string(),
        channel: v["channel"].as_str().or_else(|| v["uploader"].as_str()).unwrap_or("").to_string(),
        entries,
    })
}

pub fn video_id(url: &str) -> Option<String> {
    let ok = |t: &str| -> Option<String> {
        let t: String = t.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-').collect();
        (t.len() == 11).then_some(t)
    };
    let u = url.trim();
    if let Some(q) = u.split_once('?').map(|x| x.1) {
        for part in q.split(['&', '#']) {
            if let Some(v) = part.strip_prefix("v=") {
                if let Some(id) = ok(v) {
                    return Some(id);
                }
            }
        }
    }
    for marker in ["youtu.be/", "/shorts/", "/live/", "/embed/"] {
        if let Some(i) = u.find(marker) {
            if let Some(id) = ok(&u[i + marker.len()..]) {
                return Some(id);
            }
        }
    }
    None
}

pub fn probe_youtube(url: &str) -> Result<YtInfo, String> {
    probe_youtube_opts(url, true)
}

/// Browser details sent along when a stream (not YouTube) is read or downloaded.
#[derive(Clone, Default)]
pub struct NetHeaders {
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub cookie: Option<String>,
}

fn add_net_args(cmd: &mut std::process::Command, h: &NetHeaders) {
    if let Some(r) = h.referer.as_ref().filter(|x| !x.is_empty()) {
        cmd.arg("--referer").arg(r);
    }
    if let Some(u) = h.user_agent.as_ref().filter(|x| !x.is_empty()) {
        cmd.arg("--user-agent").arg(u);
    }
    if let Some(c) = h.cookie.as_ref().filter(|x| !x.is_empty()) {
        cmd.arg("--add-header").arg(format!("Cookie:{}", c));
    }
}

/// YouTube answers "sign in" or "not a bot" when it wants the browser's cookies.
pub fn needs_signin(error: &str) -> bool {
    let e = error.to_lowercase();
    ["sign in", "not a bot", "confirm your age", "age-restricted", "private video", "members-only", "login required"]
        .iter()
        .any(|k| e.contains(k))
}

/// Like `probe_youtube`; the browser connector skips downloading the thumbnail picture.
pub fn probe_youtube_opts(url: &str, with_thumb: bool) -> Result<YtInfo, String> {
    probe_with(url, with_thumb, &NetHeaders::default())
}

/// Read a YouTube video or any other video link (HLS/DASH streams too). YouTube is asked
/// without cookies first: that is faster, and the cookies can make it answer with nothing
/// useful. They are used only when YouTube asks to sign in.
pub fn probe_with(url: &str, with_thumb: bool, h: &NetHeaders) -> Result<YtInfo, String> {
    match probe_inner(url, with_thumb, false, h) {
        Err(e) if is_youtube(url) && cookies_path().exists() && needs_signin(&e) => {
            probe_inner(url, with_thumb, true, h)
        }
        other => other,
    }
}

fn probe_inner(url: &str, with_thumb: bool, use_cookies: bool, h: &NetHeaders) -> Result<YtInfo, String> {
    let exe = ytdlp_path().ok_or("yt-dlp is not installed")?;
    let cookies = if use_cookies { CookieCopy::new() } else { CookieCopy::none() };
    let mut cmd = hidden(&exe);
    cmd.args(["--no-playlist", "--dump-single-json", "--no-warnings", "--skip-download", "--encoding", "utf-8"]);
    if let Some(p) = cookies.path() {
        cmd.arg("--cookies").arg(p);
    }
    if !is_youtube(url) {
        add_net_args(&mut cmd, h);
    }
    let out = cmd
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(last_error_line(&String::from_utf8_lossy(&out.stderr)));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| e.to_string())?;
    let duration = v["duration"].as_f64().unwrap_or(0.0);
    let size_of = |f: &Value| -> u64 {
        f["filesize"]
            .as_u64()
            .or_else(|| f["filesize_approx"].as_u64())
            .or_else(|| f["tbr"].as_f64().map(|t| (t * 1000.0 / 8.0 * duration) as u64))
            .unwrap_or(0)
    };
    let stream = !is_youtube(url);
    let mut video_sizes: HashMap<u32, u64> = HashMap::new();
    let mut audio_size = 0u64;
    let (mut std_height, mut std_size) = (0u32, 0u64);
    if let Some(formats) = v["formats"].as_array() {
        for f in formats {
            let vc = f["vcodec"].as_str().unwrap_or("none");
            let ac = f["acodec"].as_str().unwrap_or("none");
            let h = f["height"].as_u64().unwrap_or(0) as u32;
            let size = size_of(f);
            let ext = f["ext"].as_str().unwrap_or("");
            if vc != "none" && (ac == "none" || stream) && h > 0 {
                let e = video_sizes.entry(h).or_insert(0);
                // Prefer the mp4 stream's size when there are several.
                if ext == "mp4" || *e == 0 {
                    *e = size;
                }
            } else if vc == "none" && ac != "none" {
                if ext == "m4a" || audio_size == 0 {
                    audio_size = audio_size.max(size);
                }
            } else if vc != "none" && ac != "none" && h >= std_height && ext == "mp4" {
                std_height = h;
                std_size = size;
            }
        }
    }
    let mut heights: Vec<u32> = video_sizes.keys().copied().collect();
    heights.sort_unstable_by(|a, b| b.cmp(a));

    let thumb_url = v["thumbnail"].as_str().unwrap_or("").to_string();
    let thumb = if with_thumb { v["thumbnail"].as_str().and_then(fetch_thumb).map(Arc::new) } else { None };
    Ok(YtInfo {
        id: v["id"].as_str().unwrap_or("").to_string(),
        title: v["title"].as_str().unwrap_or("YouTube video").to_string(),
        channel: v["channel"]
            .as_str()
            .or_else(|| v["uploader"].as_str())
            .unwrap_or("")
            .to_string(),
        duration: duration as u64,
        thumb,
        thumb_url,
        heights,
        video_sizes,
        audio_size,
        std_height,
        std_size,
    })
}

pub fn fetch_thumb(url: &str) -> Option<Thumb> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let bytes = client.get(url).send().ok()?.bytes().ok()?;
    let img = image::load_from_memory(&bytes).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let nw = 320u32;
    let nh = (h as f32 * nw as f32 / w as f32).round().max(1.0) as u32;
    let small = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    Some(Thumb { w: nw, h: nh, rgba: small.into_raw() })
}

// ---------------------------------------------------------------------------
// Download worker
// ---------------------------------------------------------------------------

enum Outcome {
    Done,
    Failed(String),
    Paused,
    Stopped,
}

fn format_args(o: &YtOpts, has_ffmpeg: bool) -> Vec<String> {
    let s = |x: &str| x.to_string();
    if o.audio {
        return vec![s("-f"), s("bestaudio[ext=m4a]/bestaudio/ba/b")];
    }
    if !has_ffmpeg {
        // Some videos have no single file with both picture and sound; the last
        // choices still give something instead of "format not available".
        return vec![s("-f"), s("b[ext=mp4]/b/bv*/best")];
    }
    // Every list ends with catch-alls so a strict filter can never leave nothing.
    let f = if o.height == 0 {
        s("bv*[ext=mp4]+ba[ext=m4a]/bv*+ba/b/bv*/best")
    } else if o.height > 1080 {
        format!("bv*[height<={h}]+ba/b[height<={h}]/bv*+ba/b/best", h = o.height)
    } else {
        format!(
            "bv*[height<={h}][ext=mp4]+ba[ext=m4a]/bv*[height<={h}]+ba/b[height<={h}]/bv*+ba/b/best",
            h = o.height
        )
    };
    vec![s("-f"), f, s("--merge-output-format"), s("mp4")]
}

fn run_once(
    d: &Download,
    o: &YtOpts,
    exe: &std::path::Path,
    dir: &std::path::Path,
    stem: &str,
    cmd_rx: &Receiver<WorkerCmd>,
    ev_tx: &Sender<EngineEvent>,
    last_reported: &mut u64,
    use_cookies: bool,
) -> Outcome {
    let ffmpeg = ffmpeg_path();
    let yt = is_youtube(&d.url);
    // YouTube's sign-in is never sent to other sites.
    let cookies = if use_cookies && yt { CookieCopy::new() } else { CookieCopy::none() };
    let mut cmd = hidden(exe);
    cmd.args(["--no-playlist", "--newline", "--no-colors", "--no-warnings", "--encoding", "utf-8"]);
    if let Some(p) = cookies.path() {
        cmd.arg("--cookies").arg(p);
    }
    if !yt {
        add_net_args(
            &mut cmd,
            &NetHeaders {
                referer: d.extra.referer.clone(),
                user_agent: d.extra.user_agent.clone(),
                cookie: d.extra.cookie.clone(),
            },
        );
    }
    cmd.arg("-P").arg(dir);
    cmd.arg("-o").arg(format!("{}.%(ext)s", stem));
    cmd.args(format_args(o, ffmpeg.is_some()));
    if let Some(ff) = &ffmpeg {
        if let Some(parent) = ff.parent() {
            cmd.arg("--ffmpeg-location").arg(parent);
        }
    }
    cmd.arg("--progress-template").arg(
        "download:ORX|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s",
    );
    cmd.arg(&d.url);
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(e.to_string()),
    };
    let (line_tx, line_rx) = channel::<String>();
    if let Some(out) = child.stdout.take() {
        let tx = line_tx.clone();
        thread::spawn(move || {
            for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(l).is_err() {
                    break;
                }
            }
        });
    }
    if let Some(err) = child.stderr.take() {
        let tx = line_tx.clone();
        thread::spawn(move || {
            for l in std::io::BufReader::new(err).lines().map_while(Result::ok) {
                if tx.send(format!("E:{}", l)).is_err() {
                    break;
                }
            }
        });
    }
    drop(line_tx);

    let mut last_err = String::new();
    let mut base = 0u64; // bytes of streams that already finished
    let mut prev_d = 0u64;
    let mut prev_total = 0u64;

    loop {
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Pause) => {
                let _ = child.kill();
                let _ = child.wait();
                return Outcome::Paused;
            }
            Ok(WorkerCmd::Stop) => {
                let _ = child.kill();
                let _ = child.wait();
                return Outcome::Stopped;
            }
            _ => {}
        }
        match line_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                if let Some(rest) = line.strip_prefix("ORX|") {
                    let p: Vec<&str> = rest.split('|').collect();
                    let num = |i: usize| -> Option<f64> { p.get(i).and_then(|x| x.trim().parse::<f64>().ok()) };
                    let done = num(0).unwrap_or(0.0) as u64;
                    let total = num(1).or_else(|| num(2)).unwrap_or(0.0) as u64;
                    let speed = num(3).unwrap_or(0.0);
                    // A new stream (audio after video) starts again from zero.
                    if done < prev_d {
                        base += prev_total.max(prev_d);
                    }
                    prev_d = done;
                    prev_total = total;
                    let overall = (base + done).max(*last_reported);
                    *last_reported = overall;
                    let size = d.size.max(overall);
                    let _ = ev_tx.send(EngineEvent::Progress(d.id, overall, size, speed));
                } else if let Some(e) = line.strip_prefix("E:") {
                    if !e.trim().is_empty() {
                        last_err = e.trim().to_string();
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                let ok = child.wait().map(|s| s.success()).unwrap_or(false);
                return if ok {
                    Outcome::Done
                } else {
                    Outcome::Failed(last_error_line(&last_err))
                };
            }
        }
    }
}

/// A failure that usually goes away on a second try (a broken or empty stream, a dropped
/// connection), as opposed to one that will fail again (private video, no such format).
fn is_transient(e: &str) -> bool {
    let e = e.to_ascii_lowercase();
    [
        "downloaded file is empty",
        "http error 5",
        "http error 403",
        "timed out",
        "connection reset",
        "connection aborted",
        "incomplete read",
        "fragment",
        "unable to download video data",
        "giving up after",
    ]
    .iter()
    .any(|k| e.contains(k))
}

pub fn yt_worker(d: Download, o: YtOpts, cmd_rx: Receiver<WorkerCmd>, ev_tx: Sender<EngineEvent>) {
    let id = d.id;
    let _ = ev_tx.send(EngineEvent::State(id, Status::Downloading, None));
    let Some(exe) = ytdlp_path() else {
        let _ = ev_tx.send(EngineEvent::State(id, Status::Error, Some("yt-dlp is not installed".into())));
        return;
    };
    let dir = if d.save_dir.is_empty() { downloads_dir() } else { PathBuf::from(&d.save_dir) };
    let _ = fs::create_dir_all(&dir);
    let stem = d
        .file_name
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| d.file_name.clone());
    let mut last_reported = 0u64;
    // Without cookies first (faster and more reliable); with them if YouTube asks to sign in.
    let mut use_cookies = false;
    // YouTube sometimes sends an empty or broken stream, more so with several downloads at
    // once; a fresh try a moment later usually works.
    let mut retries = 0u32;

    loop {
        match run_once(&d, &o, &exe, &dir, &stem, &cmd_rx, &ev_tx, &mut last_reported, use_cookies) {
            Outcome::Done => {
                // The container can differ from the guess; report the real name.
                let prefix = format!("{}.", stem);
                let found = fs::read_dir(&dir).ok().and_then(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .filter(|n| n.starts_with(&prefix) && !n.ends_with(".part") && !n.ends_with(".ytdl"))
                        .find(|n| *n == d.file_name)
                        .or_else(|| {
                            fs::read_dir(&dir).ok().and_then(|rd| {
                                rd.filter_map(|e| e.ok())
                                    .map(|e| e.file_name().to_string_lossy().into_owned())
                                    .find(|n| n.starts_with(&prefix) && !n.ends_with(".part") && !n.ends_with(".ytdl"))
                            })
                        })
                });
                if let Some(name) = found {
                    if name != d.file_name {
                        let _ = ev_tx.send(EngineEvent::FileName(id, name));
                    }
                }
                let size = d.size.max(last_reported);
                let _ = ev_tx.send(EngineEvent::Progress(id, size, size, 0.0));
                let _ = ev_tx.send(EngineEvent::State(id, Status::Complete, None));
                return;
            }
            Outcome::Failed(e) => {
                // YouTube asked to sign in: try once more with the browser's cookies.
                if !use_cookies && is_youtube(&d.url) && cookies_path().exists() && needs_signin(&e) {
                    use_cookies = true;
                    continue;
                }
                if retries < 3 && is_transient(&e) {
                    retries += 1;
                    let _ = ev_tx.send(EngineEvent::Log(id, format!("{} Trying again ({} of 3)", e, retries)));
                    // Wait, but stay responsive to Pause / Stop.
                    match cmd_rx.recv_timeout(Duration::from_secs(3 * retries as u64)) {
                        Ok(WorkerCmd::Stop) => {
                            let _ = ev_tx.send(EngineEvent::State(id, Status::Queued, None));
                            return;
                        }
                        Ok(WorkerCmd::Pause) => {
                            let _ = ev_tx.send(EngineEvent::State(id, Status::Paused, None));
                            match cmd_rx.recv() {
                                Ok(WorkerCmd::Run) => {
                                    let _ = ev_tx.send(EngineEvent::State(id, Status::Downloading, None));
                                }
                                _ => {
                                    let _ = ev_tx.send(EngineEvent::State(id, Status::Queued, None));
                                    return;
                                }
                            }
                        }
                        _ => {}
                    }
                    continue;
                }
                let _ = ev_tx.send(EngineEvent::State(id, Status::Error, Some(e)));
                return;
            }
            Outcome::Stopped => {
                let _ = ev_tx.send(EngineEvent::State(id, Status::Queued, None));
                return;
            }
            Outcome::Paused => {
                let _ = ev_tx.send(EngineEvent::State(id, Status::Paused, None));
                loop {
                    match cmd_rx.recv() {
                        Ok(WorkerCmd::Run) => {
                            let _ = ev_tx.send(EngineEvent::State(id, Status::Downloading, None));
                            break;
                        }
                        Ok(WorkerCmd::Pause) => {}
                        Ok(WorkerCmd::Stop) | Err(_) => {
                            let _ = ev_tx.send(EngineEvent::State(id, Status::Queued, None));
                            return;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn passing_failures_are_tried_again_but_lasting_ones_are_not() {
        use super::is_transient as t;
        assert!(t("The downloaded file is empty"));
        assert!(t("unable to download video data: HTTP Error 503: Service Unavailable"));
        assert!(!t("Private video. Sign in if you've been granted access to this video"));
        assert!(!t("Requested format is not available"));
    }

    #[test]
    fn odd_heights_get_the_usual_names() {
        use super::nominal_height as n;
        assert_eq!((n(1012), n(676), n(2026), n(1080), n(338), n(450)), (1080, 720, 2160, 1080, 360, 480));
    }

    #[test]
    fn recognises_facebook_video_pages() {
        use super::is_facebook_video as fb;
        for u in [
            "https://www.facebook.com/reel/1234567890123?s=single_unit",
            "https://m.facebook.com/reels/1234567890123/",
            "https://www.facebook.com/watch/?v=9876543210",
            "https://www.facebook.com/video.php?v=9876543210",
            "https://www.facebook.com/SomePage/videos/1112223334445/",
            "https://www.facebook.com/share/r/1AbCdEf2Gh/",
            "https://fb.watch/xYz12AbC/",
        ] {
            assert!(fb(u), "should accept {u}");
        }
        for u in ["https://www.facebook.com/", "https://www.facebook.com/SomeUser", "https://example.com/reel/1234567890123", "https://www.facebook.com/watch/"] {
            assert!(!fb(u), "should reject {u}");
        }
    }

    #[test]
    fn a_facebook_video_has_one_short_name_whatever_the_tracking() {
        use super::facebook_key as k;
        let a = k("https://www.facebook.com/reel/1234567890123?s=single_unit&fs=e").unwrap();
        let b = k("https://www.facebook.com/reel/1234567890123/").unwrap();
        let c = k("https://www.facebook.com/reel/9999999999999").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(
            k("https://www.facebook.com/watch/?v=9876543210&t=5"),
            k("https://www.facebook.com/watch/?v=9876543210")
        );
        assert!(a.starts_with("fb-") && a.len() == 19);
        assert_eq!(k("https://www.youtube.com/watch?v=5M0xujy6aZM"), None);
    }

    #[test]
    fn finds_the_video_id() {
        use super::video_id;
        for u in [
            "https://www.youtube.com/watch?v=5M0xujy6aZM",
            "https://www.youtube.com/watch?list=PL1&v=5M0xujy6aZM&t=3",
            "https://youtu.be/5M0xujy6aZM?si=abc",
            "https://www.youtube.com/shorts/5M0xujy6aZM",
        ] {
            assert_eq!(video_id(u).as_deref(), Some("5M0xujy6aZM"), "{u}");
        }
        assert_eq!(video_id("https://www.youtube.com/feed/subscriptions"), None);
    }

    #[test]
    fn knows_when_youtube_wants_a_sign_in() {
        assert!(super::needs_signin("Sign in to confirm you\u{2019}re not a bot"));
        assert!(!super::needs_signin("Requested format is not available"));
    }

    use super::*;

    #[test]
    fn detects_youtube_video_links() {
        for u in [
            "https://www.youtube.com/watch?v=jNQXAC9IJRE",
            "http://youtube.com/watch?v=abc",
            "https://m.youtube.com/watch?v=abc&t=10s",
            "https://youtu.be/jNQXAC9IJRE",
            "https://www.youtube.com/shorts/abc123",
            "https://music.youtube.com/watch?v=abc",
            "https://www.youtube.com/live/abc123",
        ] {
            assert!(is_youtube(u), "should accept {}", u);
        }
    }

    #[test]
    fn ignores_other_links() {
        for u in [
            "https://example.com/file.zip",
            "https://www.youtube.com/",
            "https://www.youtube.com/@somechannel",
            "https://notyoutube.com/watch?v=abc",
            "https://youtube.com.evil.example/watch?v=abc",
            "https://youtu.be/",
        ] {
            assert!(!is_youtube(u), "should reject {}", u);
        }
    }

    #[test]
    fn finds_hash_in_list() {
        let h = "a".repeat(64);
        let list = format!("{h}  yt-dlp.exe\n{}  other.bin\n{h} *star.zip\n", "b".repeat(64));
        assert_eq!(find_hash(&list, "yt-dlp.exe").as_deref(), Some(h.as_str()));
        assert_eq!(find_hash(&list, "star.zip").as_deref(), Some(h.as_str()));
        assert_eq!(find_hash(&list, "missing.exe"), None);
    }

    #[test]
    fn clock_format() {
        assert_eq!(fmt_clock(75), "1:15");
        assert_eq!(fmt_clock(3725), "1:02:05");
    }
}

#[cfg(test)]
mod playlist_tests {
    use super::*;

    #[test]
    fn playlist_links_are_recognised() {
        let id = "PLrAXtmErZgOeiKm4sgNOknGvNjby9efdf";
        assert_eq!(playlist_id(&format!("https://www.youtube.com/playlist?list={}", id)).as_deref(), Some(id));
        assert_eq!(playlist_id(&format!("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list={}&index=3", id)).as_deref(), Some(id));
        assert_eq!(playlist_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), None);
        assert_eq!(playlist_id(&format!("https://example.com/watch?list={}", id)), None);
        assert!(is_youtube(&format!("https://www.youtube.com/playlist?list={}", id)));
        assert!(!is_youtube("https://www.youtube.com/playlist"));
    }

    #[test]
    fn a_playlist_is_read_without_private_or_deleted_videos() {
        let v: Value = serde_json::from_str(
            r#"{"id":"PL1","title":"Lessons","channel":"Teacher","entries":[
                {"id":"aaaaaaaaaaa","title":"One","duration":61.0},
                {"id":"bbbbbbbbbbb","title":"[Private video]","duration":null},
                {"id":"ccccccccccc","title":"Two","duration":125.5},
                {"id":"ddddddddddd","title":"[Deleted video]"}]}"#,
        )
        .unwrap();
        let p = parse_playlist(&v).unwrap();
        assert_eq!(p.title, "Lessons");
        assert_eq!(p.channel, "Teacher");
        assert_eq!(p.entries.iter().map(|e| e.title.as_str()).collect::<Vec<_>>(), ["One", "Two"]);
        assert_eq!(p.entries[1].duration, 125);
        let empty: Value = serde_json::from_str(r#"{"id":"PL2","entries":[]}"#).unwrap();
        assert!(parse_playlist(&empty).is_err());
    }
}
