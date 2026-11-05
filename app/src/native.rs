// The browser connector. Chrome starts `orexa-downloader.exe --native-host` and talks to it
// over standard input/output (length-prefixed JSON). It drops each link into an inbox
// folder that the running app watches, and starts the app (in the tray) when needed.

use super::*;
use serde_json::{json, Value};

/// The extension allowed to talk to this connector.
/// The extensions allowed to talk to this connector. Add the Chrome Web Store ID here once the
/// extension is published (the store gives it its own ID), then ship a new installer.
pub const EXTENSION_IDS: &[&str] = &[
    "eohagpikpfaoeikeifebabmmchkceeaf", // the one loaded from the extension folder
];
pub const HOST_NAME: &str = "com.orexa.downloader";

pub fn inbox_dir() -> PathBuf {
    state::data_dir().join("inbox")
}

#[link(name = "kernel32")]
extern "system" {
    fn OpenMutexW(access: u32, inherit: i32, name: *const u16) -> isize;
    fn CloseHandle(h: isize) -> i32;
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Is the Orexa app running right now?
fn app_running() -> bool {
    let name = wide("Local\\OrexaDownloaderSingleInstance");
    let h = unsafe { OpenMutexW(0x0010_0000, 0, name.as_ptr()) }; // SYNCHRONIZE
    if h != 0 {
        unsafe { CloseHandle(h) };
        true
    } else {
        false
    }
}

/// Make sure the app is running: bring it forward, or start it (hidden in the tray when
/// `visible` is false).
fn ensure_app(visible: bool) -> bool {
    if std::env::var_os("OREXA_NO_LAUNCH").is_some() {
        return false; // used by tests
    }
    if app_running() {
        if visible {
            shell::signal_running_instance();
        }
        return false;
    }
    let Ok(exe) = std::env::current_exe() else { return false };
    let mut cmd = Command::new(exe);
    if !visible {
        cmd.arg("--background");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Detached, so the app outlives the connector.
        cmd.creation_flags(0x0000_0008 | 0x0800_0000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    }
    cmd.spawn().is_ok()
}

fn is_http(url: &str) -> bool {
    let u = url.trim();
    u.starts_with("http://") || u.starts_with("https://") || torrent::is_magnet(u)
}

fn probe_cache_dir() -> PathBuf {
    state::data_dir().join("ytcache")
}

fn probe_cache_get(id: &str) -> Option<Value> {
    let path = probe_cache_dir().join(format!("{}.json", id));
    let age = fs::metadata(&path).ok()?.modified().ok()?.elapsed().ok()?;
    if age > Duration::from_secs(30 * 60) {
        return None;
    }
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// One read per video at a time: the page pre-read and the dialog share the same answer.
fn probe_cached(id: &str, read: impl FnOnce() -> Value) -> Value {
    if let Some(v) = probe_cache_get(id) {
        return v;
    }
    let dir = probe_cache_dir();
    let _ = fs::create_dir_all(&dir);
    let lock = dir.join(format!("{}.lock", id));
    let fresh = |p: &std::path::Path| {
        fs::metadata(p)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .map(|e| e < Duration::from_secs(90))
            .unwrap_or(false)
    };
    if fresh(&lock) {
        let t0 = Instant::now();
        while t0.elapsed() < Duration::from_secs(45) && lock.exists() {
            thread::sleep(Duration::from_millis(300));
            if let Some(v) = probe_cache_get(id) {
                return v;
            }
        }
        if let Some(v) = probe_cache_get(id) {
            return v;
        }
    }
    let _ = fs::write(&lock, b"1");
    let v = read();
    if v["ok"] == true {
        let _ = fs::write(dir.join(format!("{}.json", id)), v.to_string());
    }
    let _ = fs::remove_file(&lock);
    v
}

/// Handle one message from the browser and return the reply.
pub fn handle(raw: &[u8]) -> Value {
    let Ok(msg) = serde_json::from_slice::<Value>(raw) else {
        return json!({"ok": false, "error": "That was not valid JSON"});
    };
    match msg["type"].as_str() {
        Some("ping") => json!({
            "ok": true,
            "app": "Orexa Downloader",
            "version": env!("CARGO_PKG_VERSION"),
            "running": app_running(),
        }),
        Some("open") => {
            ensure_app(true);
            json!({"ok": true})
        }
        // Facts the browser dialog shows: where files go, what is running, which tools exist.
        Some("info") => {
            let st = state::load().unwrap_or_default();
            let default_dir = if st.default_dir.trim().is_empty() {
                downloads_dir().to_string_lossy().into_owned()
            } else {
                st.default_dir.clone()
            };
            let running = st.downloads.iter().filter(|d| d.status == "downloading").count();
            json!({
                "ok": true,
                "defaultDir": default_dir,
                "running": running,
                "ffmpeg": youtube::ffmpeg_path().is_some(),
                "ytdlp": youtube::ytdlp_path().is_some(),
                "appRunning": app_running(),
            })
        }
        // Read a video's details (this can take a while, the browser waits). Answers for a
        // YouTube video are kept for 30 minutes, so a page that was pre-read answers at once.
        Some("probe") => {
            let url = msg["url"].as_str().unwrap_or("").trim().to_string();
            let stream = msg["kind"].as_str() == Some("stream");
            if !is_http(&url) || (!stream && !youtube::is_youtube(&url)) {
                return json!({"ok": false, "error": "That is not a video link Orexa can read"});
            }
            if youtube::ytdlp_path().is_none() {
                return json!({"ok": false, "error": "Video support is still being set up. Open Orexa and try again in a minute."});
            }
            let text = |k: &str| msg[k].as_str().map(str::trim).filter(|x| !x.is_empty()).map(String::from);
            let headers = youtube::NetHeaders {
                referer: text("referrer"),
                user_agent: text("userAgent"),
                cookie: text("cookie"),
            };
            let read = || match youtube::probe_with(&url, false, &headers) {
                Ok(info) => {
                    let list = youtube::choices(&info, youtube::ffmpeg_path().is_some());
                    let default = youtube::default_choice(&list);
                    json!({
                        "ok": true,
                        "id": info.id,
                        "title": info.title,
                        "channel": info.channel,
                        "duration": info.duration,
                        "thumbnail": info.thumb_url,
                        "default": default,
                        "choices": list.iter().map(|c| json!({
                            "label": c.label, "height": c.height, "audio": c.audio, "size": c.size
                        })).collect::<Vec<_>>(),
                    })
                }
                Err(e) => json!({"ok": false, "error": e}),
            };
            // YouTube and Facebook answers are kept for a little while, so a video that was read
            // when it started playing opens instantly.
            let key = match (stream, youtube::video_id(&url)) {
                (false, Some(id)) => Some(id),
                _ => youtube::facebook_key(&url),
            };
            match key {
                Some(k) => probe_cached(&k, read),
                None => read(),
            }
        }
        // The "Change" button: a Windows folder picker.
        Some("pick-folder") => {
            let start = msg["start"].as_str().unwrap_or("");
            let mut dlg = rfd::FileDialog::new().set_title("Choose where to save");
            if std::path::Path::new(start).is_dir() {
                dlg = dlg.set_directory(start);
            }
            match dlg.pick_folder() {
                Some(p) => json!({"ok": true, "path": p.to_string_lossy()}),
                None => json!({"ok": true, "path": null}),
            }
        }
        Some("download") => {
            let url = msg["url"].as_str().unwrap_or("").trim().to_string();
            if !is_http(&url) {
                return json!({"ok": false, "error": "Only http and https links can be downloaded"});
            }
            let dir = inbox_dir();
            if fs::create_dir_all(&dir).is_err() {
                return json!({"ok": false, "error": "Could not create the inbox folder"});
            }
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let tmp = dir.join(format!("{}-{}.tmp", stamp, std::process::id()));
            let fin = dir.join(format!("{}-{}.json", stamp, std::process::id()));
            // Write then rename, so the app never reads a half-written file.
            if fs::write(&tmp, msg.to_string()).is_err() || fs::rename(&tmp, &fin).is_err() {
                return json!({"ok": false, "error": "Could not save the link for Orexa"});
            }
            let launched = ensure_app(false);
            json!({"ok": true, "launched": launched})
        }
        Some("cookies") => {
            // The browser's YouTube sign-in, saved privately for yt-dlp.
            let text = msg["text"].as_str().unwrap_or("");
            if !text.starts_with("# Netscape HTTP Cookie File") || text.len() > 2_000_000 {
                return json!({"ok": false, "error": "That is not a cookies file"});
            }
            let dir = youtube::cookies_dir();
            if fs::create_dir_all(&dir).is_err() {
                return json!({"ok": false, "error": "Could not create the cookies folder"});
            }
            let tmp = dir.join("youtube.txt.tmp");
            if fs::write(&tmp, text).is_err() || fs::rename(&tmp, youtube::cookies_path()).is_err() {
                return json!({"ok": false, "error": "Could not save the cookies"});
            }
            let count = text.lines().filter(|l| !l.trim().is_empty() && !l.starts_with("# ")).count();
            json!({"ok": true, "cookies": count})
        }
        _ => json!({"ok": false, "error": "Unknown request"}),
    }
}

/// Read one browser message (4-byte little-endian length, then JSON).
fn read_message(input: &mut impl Read) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len).ok()?;
    let n = u32::from_le_bytes(len) as usize;
    if n == 0 || n > 4 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; n];
    input.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn write_message(out: &mut impl Write, value: &Value) -> std::io::Result<()> {
    let text = value.to_string();
    out.write_all(&(text.len() as u32).to_le_bytes())?;
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// Entry point for `--native-host`.
pub fn run() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    while let Some(raw) = read_message(&mut input) {
        let reply = handle(&raw);
        if write_message(&mut output, &reply).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_framing_round_trips() {
        let mut wire = Vec::new();
        write_message(&mut wire, &json!({"ok": true})).unwrap();
        let mut cur = std::io::Cursor::new(wire);
        let raw = read_message(&mut cur).unwrap();
        let v: Value = serde_json::from_slice(&raw).unwrap();
        assert_eq!(v["ok"], true);
    }

    #[test]
    fn rejects_bad_requests() {
        assert_eq!(handle(b"not json")["ok"], false);
        assert_eq!(handle(br#"{"type":"nope"}"#)["ok"], false);
        assert_eq!(handle(br#"{"type":"cookies","text":"not a cookie file"}"#)["ok"], false);
        assert_eq!(handle(br#"{"type":"download","url":"file:///c:/x"}"#)["ok"], false);
        assert_eq!(handle(br#"{"type":"download","url":"javascript:alert(1)"}"#)["ok"], false);
    }
}
