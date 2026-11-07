// Remembers your downloads, settings and window size between runs.
// Everything lives in %LOCALAPPDATA%\Orexa\state.json (thumbnails in \thumbs).

use super::*;
use serde::{Deserialize, Serialize};

pub fn data_dir() -> PathBuf {
    youtube::tools_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(std::env::temp_dir)
}

fn state_path() -> PathBuf {
    data_dir().join("state.json")
}

fn thumbs_dir() -> PathBuf {
    data_dir().join("thumbs")
}

#[derive(Serialize, Deserialize, Clone, Copy)]
pub struct SavedWindow {
    pub w: f32,
    pub h: f32,
    pub maximized: bool,
}

#[derive(Serialize, Deserialize)]
pub struct SavedYt {
    pub id: String,
    pub height: u32,
    pub audio: bool,
}

#[derive(Serialize, Deserialize)]
pub struct SavedDownload {
    pub id: u64,
    pub url: String,
    pub file_name: String,
    pub size: u64,
    pub downloaded: u64,
    pub status: String,
    pub save_dir: String,
    pub added: String,
    pub error: Option<String>,
    pub yt: Option<SavedYt>,
    #[serde(default)]
    pub referer: Option<String>,
    #[serde(default)]
    pub torrent: bool,
    /// A torrent: the files chosen for download (None = all).
    #[serde(default)]
    pub torrent_files: Option<Vec<usize>>,
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct SavedState {
    pub dark: bool,
    pub default_dir: String,
    pub max_parallel: usize,
    pub next_id: u64,
    pub window: Option<SavedWindow>,
    pub downloads: Vec<SavedDownload>,
    #[serde(default = "yes")]
    pub notify_done: bool,
    pub tray_hint_shown: bool,
    pub browser_ask: bool,
    /// Start in the tray when Windows starts.
    #[serde(default = "yes")]
    pub autostart: bool,
    /// Put new downloads in a folder for their type.
    #[serde(default)]
    pub organize: bool,
    /// Keep sharing a torrent after it finishes.
    #[serde(default)]
    pub seed_after: bool,
    /// Open magnet links and .torrent files with Orexa.
    #[serde(default = "yes")]
    pub handle_types: bool,
    /// Interface language code ("en", "ar").
    #[serde(default)]
    pub language: String,
    /// The first-run welcome has been seen. Older state files count as seen.
    #[serde(default = "yes")]
    pub welcomed: bool,
    /// What to do when every download has finished: "", "folder" or "sleep".
    #[serde(default)]
    pub after_finish: String,
}

fn yes() -> bool {
    true
}

impl Default for SavedWindow {
    fn default() -> Self {
        Self { w: 1280.0, h: 800.0, maximized: false }
    }
}

/// Has Orexa been run before (is there a saved state)?
pub fn exists() -> bool {
    state_path().exists()
}

pub fn load() -> Option<SavedState> {
    let text = fs::read_to_string(state_path()).ok()?;
    serde_json::from_str(&text).ok()
}

fn status_name(s: Status) -> &'static str {
    match s {
        Status::Queued => "queued",
        Status::Downloading => "downloading",
        Status::Paused => "paused",
        Status::Complete => "complete",
        Status::Error => "error",
    }
}

impl SavedDownload {
    fn from(d: &Download) -> Self {
        Self {
            id: d.id,
            url: d.url.clone(),
            file_name: d.file_name.clone(),
            size: d.size,
            downloaded: d.downloaded,
            status: status_name(d.status).into(),
            save_dir: d.save_dir.clone(),
            added: d.added.clone(),
            error: d.error.clone(),
            referer: d.extra.referer.clone(),
            yt: match &d.source {
                Source::YouTube(o) => Some(SavedYt { id: o.id.clone(), height: o.height, audio: o.audio }),
                Source::Http | Source::Torrent => None,
            },
            torrent: matches!(d.source, Source::Torrent),
            torrent_files: d.files.clone(),
        }
    }

    pub fn into_download(self) -> Download {
        // A download that was running when the app closed waits to be resumed.
        let status = match self.status.as_str() {
            "complete" => Status::Complete,
            "error" => Status::Error,
            "queued" => Status::Queued,
            _ => Status::Paused,
        };
        let source = match self.yt {
            Some(y) => Source::YouTube(YtOpts { id: y.id, height: y.height, audio: y.audio }),
            None if self.torrent => Source::Torrent,
            None => Source::Http,
        };
        let mut d = Download {
            id: self.id,
            url: self.url,
            ext: ext_of(&self.file_name),
            file_name: self.file_name,
            size: self.size,
            downloaded: self.downloaded,
            status,
            speed_bps: 0.0,
            save_dir: self.save_dir,
            added: self.added,
            error: self.error,
            source,
            thumb: load_thumb(self.id),
            files: self.torrent_files,
            seeding: None,
            live: Default::default(),
            extra: Extra { referer: self.referer, ..Extra::default() },
        };
        if d.status == Status::Complete {
            d.downloaded = d.size.max(d.downloaded);
        } else if matches!(d.source, Source::Http) {
            // The partial file on disk is the truth for plain downloads (a multi-part
            // download is preallocated, so its side file says how much is really there).
            if let Some(done) = crate::multipart::saved_progress(&d.path()) {
                d.downloaded = done;
            } else if let Ok(meta) = fs::metadata(d.path()) {
                d.downloaded = meta.len();
            }
        }
        d
    }
}

fn load_thumb(id: u64) -> Option<Arc<Thumb>> {
    let img = image::open(thumbs_dir().join(format!("{}.png", id))).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    Some(Arc::new(Thumb { w, h, rgba: img.into_raw() }))
}

fn save_thumbs(downloads: &[Download]) {
    for d in downloads {
        let Some(t) = &d.thumb else { continue };
        let path = thumbs_dir().join(format!("{}.png", d.id));
        if path.exists() {
            continue;
        }
        let _ = fs::create_dir_all(thumbs_dir());
        if let Some(img) = image::RgbaImage::from_raw(t.w, t.h, t.rgba.clone()) {
            let _ = img.save(&path);
        }
    }
}

impl App {
    /// Restore settings and the download list from the last run.
    pub fn load_state(&mut self) {
        let Some(st) = load() else { return };
        self.dark = st.dark;
        if !st.default_dir.is_empty() {
            self.default_dir = st.default_dir;
        }
        if st.max_parallel >= 1 {
            self.max_parallel = st.max_parallel.min(8);
        }
        self.win = st.window;
        self.notify_done = st.notify_done;
        self.tray_hint_shown = st.tray_hint_shown;
        self.browser_ask = st.browser_ask;
        self.autostart = st.autostart;
        self.organize = st.organize;
        self.seed_after = st.seed_after;
        self.handle_types = st.handle_types;
        self.lang = i18n::Lang::from_code(&st.language);
        i18n::set(self.lang);
        self.welcomed = st.welcomed;
        self.after_finish = AfterFinish::from_code(&st.after_finish);
        self.downloads = st.downloads.into_iter().map(|d| d.into_download()).collect();
        let top = self.downloads.iter().map(|d| d.id).max().unwrap_or(0);
        self.next_id = st.next_id.max(top + 1).max(100);
        // Keep the last JSON so we do not rewrite an unchanged file.
        self.last_json = fs::read_to_string(state_path()).unwrap_or_default();
    }

    pub fn track_window(&mut self, ctx: &egui::Context) {
        let (maximized, size) = ctx.input(|i| {
            let v = i.viewport();
            (v.maximized.unwrap_or(false), v.inner_rect.map(|r| (r.width(), r.height())))
        });
        let mut w = self.win.unwrap_or_default();
        w.maximized = maximized;
        if !maximized {
            if let Some((sw, sh)) = size {
                if sw >= 300.0 && sh >= 300.0 {
                    w.w = sw;
                    w.h = sh;
                }
            }
        }
        self.win = Some(w);
    }

    pub fn save_state(&mut self, force: bool) {
        if !self.persist {
            return;
        }
        if !force && self.last_save.elapsed() < Duration::from_secs(2) {
            return;
        }
        self.last_save = Instant::now();
        let st = SavedState {
            dark: self.dark,
            default_dir: self.default_dir.clone(),
            max_parallel: self.max_parallel,
            next_id: self.next_id,
            window: self.win,
            downloads: self.downloads.iter().map(SavedDownload::from).collect(),
            notify_done: self.notify_done,
            tray_hint_shown: self.tray_hint_shown,
            browser_ask: self.browser_ask,
            autostart: self.autostart,
            organize: self.organize,
            seed_after: self.seed_after,
            handle_types: self.handle_types,
            language: self.lang.code().to_string(),
            welcomed: self.welcomed,
            after_finish: self.after_finish.code().to_string(),
        };
        let Ok(json) = serde_json::to_string_pretty(&st) else { return };
        if json == self.last_json {
            return;
        }
        let dir = data_dir();
        let _ = fs::create_dir_all(&dir);
        // Write to a temp file first so a crash never leaves a half-written state.
        let tmp = dir.join("state.json.tmp");
        if fs::write(&tmp, &json).is_ok() && fs::rename(&tmp, state_path()).is_ok() {
            self.last_json = json;
        }
        save_thumbs(&self.downloads);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(status: &str) -> SavedDownload {
        SavedDownload {
            id: 7,
            url: "https://example.com/a.zip".into(),
            file_name: "a.zip".into(),
            size: 1000,
            downloaded: 400,
            status: status.into(),
            save_dir: String::new(),
            added: "2026-11-05 10:00".into(),
            error: None,
            yt: None,
            referer: None,
            torrent: false,
            torrent_files: None,
        }
    }

    #[test]
    fn running_download_comes_back_paused() {
        assert_eq!(saved("downloading").into_download().status, Status::Paused);
        assert_eq!(saved("paused").into_download().status, Status::Paused);
        assert_eq!(saved("queued").into_download().status, Status::Queued);
        assert_eq!(saved("error").into_download().status, Status::Error);
    }

    #[test]
    fn finished_download_stays_finished() {
        let d = saved("complete").into_download();
        assert_eq!(d.status, Status::Complete);
        assert_eq!(d.downloaded, 1000);
    }

    #[test]
    fn state_round_trips_through_json_and_tolerates_missing_fields() {
        let st = SavedState {
            dark: true,
            default_dir: "D:\\Files".into(),
            max_parallel: 2,
            next_id: 120,
            window: Some(SavedWindow { w: 1400.0, h: 850.0, maximized: false }),
            downloads: vec![saved("paused")],
            notify_done: true,
            tray_hint_shown: false,
            browser_ask: false,
            autostart: true,
            organize: false,
            seed_after: false,
            handle_types: true,
            language: "ar".into(),
            welcomed: true,
            after_finish: "sleep".into(),
        };
        let json = serde_json::to_string(&st).unwrap();
        let back: SavedState = serde_json::from_str(&json).unwrap();
        assert!(back.dark);
        assert_eq!(back.max_parallel, 2);
        assert_eq!(back.downloads.len(), 1);
        // An older or partial file must still load.
        let partial: SavedState = serde_json::from_str(r#"{"dark":true}"#).unwrap();
        assert!(partial.dark && partial.downloads.is_empty());
        // A file from before the welcome existed must not greet a returning user again.
        assert!(partial.welcomed);
        assert_eq!(partial.after_finish, "");
    }
}
