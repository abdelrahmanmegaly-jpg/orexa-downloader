// Torrents: magnet links and .torrent files, downloaded by the built-in engine (librqbit).
// Nothing to install. A finished torrent stops, unless "keep sharing" is on in Settings.

use super::*;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, TorrentStatsState};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

/// Keep sharing a torrent after it finishes (the "keep sharing" setting).
pub static SEED_AFTER: AtomicBool = AtomicBool::new(false);

fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("orexa-torrent")
            .build()
            .expect("torrent runtime")
    })
}

static SESSION: Mutex<Option<Arc<Session>>> = Mutex::new(None);

/// One engine for the whole app, created the first time a torrent is added.
fn session() -> Result<Arc<Session>, String> {
    let mut g = SESSION.lock().unwrap();
    if let Some(s) = g.as_ref() {
        return Ok(s.clone());
    }
    let s = runtime()
        .block_on(Session::new(downloads_dir()))
        .map_err(|e| format!("Could not start the torrent engine: {}", e))?;
    s.ratelimits.set_download_bps(bps_limit(crate::throttle::get()));
    *g = Some(s.clone());
    Ok(s)
}

fn bps_limit(bps: u64) -> Option<std::num::NonZeroU32> {
    std::num::NonZeroU32::new(bps.min(u32::MAX as u64) as u32)
}

/// The speed limit from Settings, for torrents too (0 = none). Applies once the engine runs.
pub fn apply_speed_limit(bps: u64) {
    if let Some(s) = SESSION.lock().unwrap().as_ref() {
        s.ratelimits.set_download_bps(bps_limit(bps));
    }
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

pub fn is_magnet(url: &str) -> bool {
    url.trim_start().to_lowercase().starts_with("magnet:?")
}

/// A .torrent file on this computer.
pub fn is_torrent_file(path: &str) -> bool {
    let p = path.trim();
    p.to_lowercase().ends_with(".torrent") && !p.contains("://") && std::path::Path::new(p).is_file()
}

/// A magnet link, a link to a .torrent file, or a file named *.torrent.
pub fn is_torrent_link(url: &str, file_name: &str) -> bool {
    if is_magnet(url) {
        return true;
    }
    let path = url.split(['?', '#']).next().unwrap_or("").to_lowercase();
    path.ends_with(".torrent") || file_name.trim().to_lowercase().ends_with(".torrent")
}

/// The display name inside a magnet link (`dn=`), if any.
pub fn magnet_name(url: &str) -> Option<String> {
    let q = url.split_once('?')?.1;
    q.split('&').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        if k.eq_ignore_ascii_case("dn") {
            let v = percent_decode(&v.replace('+', " "));
            let v = v.trim().to_string();
            (!v.is_empty()).then_some(v)
        } else {
            None
        }
    })
}

// ---------------------------------------------------------------------------
// The download
// ---------------------------------------------------------------------------

fn source_of(d: &Download) -> Result<Vec<u8>, String> {
    torrent_bytes(&d.url, &d.extra)
}

/// The .torrent file's bytes, from a link or from this computer. Magnets are handled by the caller.
fn torrent_bytes(url: &str, extra: &Extra) -> Result<Vec<u8>, String> {
    let u = url.trim();
    if u.starts_with("http://") || u.starts_with("https://") {
        let client = reqwest::blocking::Client::builder()
            .user_agent(
                extra
                    .user_agent
                    .clone()
                    .unwrap_or_else(|| "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
            )
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        let mut req = client.get(u);
        if let Some(r) = &extra.referer {
            req = req.header("Referer", r);
        }
        if let Some(c) = &extra.cookie {
            req = req.header("Cookie", c);
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("Server answered {}", resp.status()));
        }
        let bytes = resp.bytes().map_err(|e| e.to_string())?;
        if bytes.len() > 20 * 1024 * 1024 || bytes.first() != Some(&b'd') {
            return Err("That is not a torrent file".into());
        }
        return Ok(bytes.to_vec());
    }
    // A .torrent file on this computer.
    let b = fs::read(u).map_err(|_| "Could not read the torrent file".to_string())?;
    if b.first() != Some(&b'd') {
        return Err("That is not a torrent file".into());
    }
    Ok(b)
}

// ---------------------------------------------------------------------------
// Looking inside a torrent (before it starts) and watching it while it runs
// ---------------------------------------------------------------------------

/// What a torrent contains.
#[derive(Clone, Debug, PartialEq)]
pub struct TorrentMeta {
    pub name: String,
    pub total: u64,
    /// Path inside the torrent and size, in the torrent's own order.
    pub files: Vec<(String, u64)>,
    pub info_hash: String,
}

/// Read a torrent's name and file list. A magnet link has to fetch this from other computers,
/// which can take a while (or never answer if nobody shares it).
pub fn inspect(url: &str, extra: &Extra) -> Result<TorrentMeta, String> {
    let sess = session()?;
    let src = if is_magnet(url) {
        AddTorrent::Url(url.trim().to_string().into())
    } else {
        AddTorrent::TorrentFileBytes(torrent_bytes(url, extra)?.into())
    };
    let fut = sess.add_torrent(src, Some(AddTorrentOptions { list_only: true, ..Default::default() }));
    let got = runtime().block_on(async { tokio::time::timeout(Duration::from_secs(90), fut).await });
    match got {
        Err(_) => Err("Nobody answered yet. It can still start; the files show once it connects.".into()),
        Ok(Ok(AddTorrentResponse::ListOnly(l))) => {
            let files: Vec<(String, u64)> = l.info.iter_file_details().map(|f| (f.filename.to_string(), f.len)).collect();
            Ok(TorrentMeta {
                name: l.info.name().map(|n| clean_name(&n)).filter(|n| !n.is_empty()).unwrap_or_else(|| "Torrent".into()),
                total: files.iter().map(|f| f.1).sum(),
                files,
                info_hash: l.info_hash.as_string(),
            })
        }
        Ok(Ok(_)) => Err("Could not read the torrent".into()),
        Ok(Err(e)) => Err(e.to_string()),
    }
}

/// Torrents that are running right now, by download id (so the window can show live details).
fn handles() -> &'static Mutex<HashMap<u64, Arc<ManagedTorrent>>> {
    static H: OnceLock<Mutex<HashMap<u64, Arc<ManagedTorrent>>>> = OnceLock::new();
    H.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Removes a torrent from the live list when its worker ends, however it ends.
struct Live(u64);

impl Live {
    fn new(id: u64, h: Arc<ManagedTorrent>) -> Self {
        handles().lock().unwrap().insert(id, h);
        Live(id)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        handles().lock().unwrap().remove(&self.0);
    }
}

#[derive(Clone, Debug)]
pub struct FileSnap {
    pub path: String,
    pub size: u64,
    pub done: u64,
    pub selected: bool,
}

/// How a running torrent is doing.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub name: String,
    pub info_hash: String,
    pub state: String,
    pub total: u64,
    pub done: u64,
    pub down_bps: f64,
    pub up_bps: f64,
    pub uploaded: u64,
    pub peers_live: u32,
    pub peers_seen: u32,
    pub seeders: u32,
    pub finished: bool,
    pub files: Vec<FileSnap>,
}

/// Live details of a running torrent (None when it is not running).
pub fn snapshot(id: u64) -> Option<Snapshot> {
    let h = handles().lock().unwrap().get(&id).cloned()?;
    let st = h.stats();
    let files_meta = h
        .with_metadata(|m| m.file_infos.iter().map(|f| (f.relative_filename.to_string_lossy().into_owned(), f.len)).collect::<Vec<_>>())
        .ok()?;
    let only: Option<HashSet<usize>> = h.only_files().map(|v| v.into_iter().collect());
    let files: Vec<FileSnap> = files_meta
        .into_iter()
        .enumerate()
        .map(|(i, (path, size))| FileSnap {
            path,
            size,
            done: st.file_progress.get(i).copied().unwrap_or(0),
            selected: only.as_ref().map(|o| o.contains(&i)).unwrap_or(true),
        })
        .collect();
    let total: u64 = files.iter().filter(|f| f.selected).map(|f| f.size).sum();
    let done: u64 = files.iter().filter(|f| f.selected).map(|f| f.done.min(f.size)).sum();
    let (peers_live, peers_seen, seeders) = st
        .live
        .as_ref()
        .map(|l| (l.snapshot.peer_stats.live, l.snapshot.peer_stats.seen, l.snapshot.peer_stats.live_seeders))
        .unwrap_or((0, 0, 0));
    Some(Snapshot {
        name: h.name().unwrap_or_default(),
        info_hash: h.info_hash().as_string(),
        state: st.state.to_string(),
        total,
        done,
        down_bps: st.live.as_ref().map(|l| l.download_speed.as_bytes() as f64).unwrap_or(0.0),
        up_bps: st.live.as_ref().map(|l| l.upload_speed.as_bytes() as f64).unwrap_or(0.0),
        uploaded: st.uploaded_bytes,
        peers_live,
        peers_seen,
        seeders,
        finished: st.finished,
        files,
    })
}

/// Change which files a running torrent downloads.
pub fn set_files(id: u64, indices: &[usize]) -> Result<(), String> {
    let h = handles().lock().unwrap().get(&id).cloned().ok_or("This torrent is not running")?;
    let sess = session()?;
    let set: HashSet<usize> = indices.iter().copied().collect();
    runtime().block_on(sess.update_only_files(&h, &set)).map_err(|e| e.to_string())
}

/// A finished torrent: let the engine forget it (the files stay), and remove the empty placeholders
/// it made for files that were not chosen, so the folder holds only what was really downloaded.
fn tidy_after_finish(
    rt: &tokio::runtime::Runtime,
    sess: &Arc<Session>,
    handle: &Arc<ManagedTorrent>,
    out: &std::path::Path,
    only: &Option<Vec<usize>>,
    file_progress: &[u64],
) {
    let paths: Vec<(PathBuf, u64)> = handle
        .with_metadata(|m| m.file_infos.iter().map(|f| (f.relative_filename.clone(), f.len)).collect())
        .unwrap_or_default();
    let _ = rt.block_on(sess.delete(librqbit::api::TorrentIdOrHash::Hash(handle.info_hash()), false));
    if let Some(sel) = only {
        let sel: HashSet<usize> = sel.iter().copied().collect();
        for (i, (rel, size)) in paths.iter().enumerate() {
            // A file nobody asked for, of which (almost) nothing is there: only the engine's placeholder.
            // (Pieces are shared with neighbouring files, so a few bytes at its edges can be present.)
            let present = file_progress.get(i).copied().unwrap_or(0);
            if sel.contains(&i) || present.saturating_mul(50) >= *size {
                continue;
            }
            let f = out.join(rel);
            if fs::metadata(&f).map(|m| m.is_file()).unwrap_or(false) {
                let _ = fs::remove_file(&f);
            }
            let mut dir = f.parent().map(|p| p.to_path_buf());
            while let Some(d) = dir {
                if d == out || fs::remove_dir(&d).is_err() {
                    break; // not empty, or the torrent's own folder
                }
                dir = d.parent().map(|p| p.to_path_buf());
            }
        }
    }
}

fn finish_error(ev_tx: &Sender<EngineEvent>, id: u64, e: impl ToString) {
    let _ = ev_tx.send(EngineEvent::State(id, Status::Error, Some(e.to_string())));
}

pub fn worker(d: Download, cmd_rx: Receiver<WorkerCmd>, ev_tx: Sender<EngineEvent>) {
    let id = d.id;
    let _ = ev_tx.send(EngineEvent::State(id, Status::Downloading, None));

    let dir = if d.save_dir.is_empty() { downloads_dir() } else { PathBuf::from(&d.save_dir) };
    let _ = fs::create_dir_all(&dir);

    let sess = match session() {
        Ok(s) => s,
        Err(e) => return finish_error(&ev_tx, id, e),
    };
    let rt = runtime();

    // 1. Look inside first (a magnet link has to fetch its details from peers): the name,
    //    and whether it holds several files, which then go into their own folder.
    let magnet = is_magnet(&d.url);
    let first_bytes = if magnet {
        None
    } else {
        match source_of(&d) {
            Ok(b) => Some(b),
            Err(e) => return finish_error(&ev_tx, id, e),
        }
    };
    let url = d.url.trim().to_string();
    let sess2 = sess.clone();
    let looking = rt.spawn(async move {
        let src = match first_bytes {
            Some(b) => AddTorrent::TorrentFileBytes(b.into()),
            None => AddTorrent::Url(url.into()),
        };
        sess2
            .add_torrent(src, Some(AddTorrentOptions { list_only: true, ..Default::default() }))
            .await
    });
    loop {
        if let Ok(WorkerCmd::Stop) = cmd_rx.try_recv() {
            looking.abort();
            let _ = ev_tx.send(EngineEvent::State(id, Status::Queued, None));
            return;
        }
        if looking.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }
    let listed = match rt.block_on(looking) {
        Ok(Ok(AddTorrentResponse::ListOnly(l))) => l,
        Ok(Ok(_)) => return finish_error(&ev_tx, id, "Could not read the torrent"),
        Ok(Err(e)) => return finish_error(&ev_tx, id, e),
        Err(e) => return finish_error(&ev_tx, id, e),
    };
    let tname = listed.info.name().map(|n| clean_name(&n)).filter(|n| !n.is_empty());
    let multi = listed.info.iter_file_details().count() > 1;
    let lens: Vec<u64> = listed.info.iter_file_lengths().collect();
    let total: u64 = lens.iter().sum();
    // The total of what is chosen (everything when nothing was picked).
    let chosen_total = |only: &Option<Vec<usize>>| -> u64 {
        match only {
            Some(f) => f.iter().filter_map(|i| lens.get(*i)).sum(),
            None => total,
        }
    };
    let total = chosen_total(&d.files);
    if let Some(n) = &tname {
        let _ = ev_tx.send(EngineEvent::FileName(id, n.clone()));
    }
    let _ = ev_tx.send(EngineEvent::Progress(id, 0, total, 0.0));
    let out = match (&tname, multi) {
        (Some(n), true) => dir.join(n),
        _ => dir.clone(),
    };
    let _ = fs::create_dir_all(&out);

    // 2. Download it.
    let opts = AddTorrentOptions {
        // Needed to continue a torrent whose files are already (partly) on disk.
        overwrite: true,
        output_folder: Some(out.to_string_lossy().into_owned()),
        only_files: d.files.clone(),
        ..Default::default()
    };
    let added = rt.block_on(sess.add_torrent(AddTorrent::TorrentFileBytes(listed.torrent_bytes.clone()), Some(opts)));
    let handle: Arc<ManagedTorrent> = match added {
        Ok(AddTorrentResponse::Added(_, h)) => h,
        Ok(AddTorrentResponse::AlreadyManaged(_, h)) => {
            if let Some(f) = &d.files {
                let _ = rt.block_on(sess.update_only_files(&h, &f.iter().copied().collect()));
            }
            let _ = rt.block_on(sess.unpause(&h));
            h
        }
        Ok(_) => return finish_error(&ev_tx, id, "Could not add the torrent"),
        Err(e) => return finish_error(&ev_tx, id, e),
    };
    let _live = Live::new(id, handle.clone()); // visible to the details window while it runs

    let mut named = tname.is_some();
    let mut paused = false;
    loop {
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Pause) => {
                let _ = rt.block_on(sess.pause(&handle));
                paused = true;
                let _ = ev_tx.send(EngineEvent::State(id, Status::Paused, None));
            }
            Ok(WorkerCmd::Run) => {
                let _ = rt.block_on(sess.unpause(&handle));
                paused = false;
                let _ = ev_tx.send(EngineEvent::State(id, Status::Downloading, None));
            }
            Ok(WorkerCmd::Stop) => {
                let _ = rt.block_on(sess.pause(&handle));
                let _ = ev_tx.send(EngineEvent::State(id, Status::Queued, None));
                return;
            }
            Err(_) => {}
        }
        if paused {
            thread::sleep(Duration::from_millis(150));
            continue;
        }

        let st = handle.stats();
        if !named {
            if let Some(n) = handle.name() {
                let n = clean_name(&n);
                if !n.is_empty() {
                    let _ = ev_tx.send(EngineEvent::FileName(id, n));
                    named = true;
                }
            }
        }
        if let TorrentStatsState::Error = st.state {
            return finish_error(&ev_tx, id, st.error.unwrap_or_else(|| "The torrent failed".into()));
        }
        let speed = st.live.as_ref().map(|l| l.download_speed.as_bytes() as f64).unwrap_or(0.0);
        // Only the chosen files count (the choice can change while it runs).
        let only = handle.only_files();
        let want = chosen_total(&only);
        let have: u64 = match &only {
            Some(f) => f.iter().map(|i| st.file_progress.get(*i).copied().unwrap_or(0).min(lens.get(*i).copied().unwrap_or(0))).sum(),
            None => st.progress_bytes,
        };
        let _ = ev_tx.send(EngineEvent::Progress(id, have, want, speed));
        if st.finished {
            let _ = ev_tx.send(EngineEvent::Progress(id, want, want, 0.0));
            let _ = ev_tx.send(EngineEvent::State(id, Status::Complete, None));
            if SEED_AFTER.load(Ordering::Relaxed) {
                // Keep sharing until it is stopped.
                loop {
                    match cmd_rx.try_recv() {
                        Ok(WorkerCmd::Pause) | Ok(WorkerCmd::Stop) => break,
                        _ => {}
                    }
                    let s = handle.stats();
                    let up = s.live.as_ref().map(|l| l.upload_speed.as_bytes() as f64).unwrap_or(0.0);
                    let _ = ev_tx.send(EngineEvent::Seeding(id, Some((s.uploaded_bytes, up))));
                    thread::sleep(Duration::from_millis(1000));
                }
                tidy_after_finish(rt, &sess, &handle, &out, &only, &st.file_progress);
                let _ = ev_tx.send(EngineEvent::Seeding(id, None));
                return;
            }
            tidy_after_finish(rt, &sess, &handle, &out, &only, &st.file_progress); // done: stop here
            return;
        }
        thread::sleep(Duration::from_millis(400));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_torrent_links() {
        assert!(is_torrent_link("magnet:?xt=urn:btih:abc&dn=Linux", ""));
        assert!(is_torrent_link("MAGNET:?xt=urn:btih:abc", ""));
        assert!(is_torrent_link("https://x.org/dl/ubuntu.torrent?token=1", ""));
        assert!(is_torrent_link("https://x.org/get?id=1", "Ubuntu.TORRENT"));
        assert!(!is_torrent_link("https://x.org/file.zip", "file.zip"));
    }

    #[test]
    fn reads_the_name_in_a_magnet() {
        assert_eq!(
            magnet_name("magnet:?xt=urn:btih:abc&dn=Big+Buck%20Bunny&tr=udp%3A%2F%2Ft").as_deref(),
            Some("Big Buck Bunny")
        );
        assert_eq!(magnet_name("magnet:?xt=urn:btih:abc"), None);
    }

    /// Needs the internet: choose only the small files of a torrent, finish, then keep sharing.
    #[test]
    #[ignore]
    fn torrent_live_select_and_seed() {
        let url = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=wss%3A%2F%2Ftracker.webtorrent.dev";
        let meta = inspect(url, &Extra::default()).expect("could not read the torrent");
        println!("torrent: {} ({} files, {} bytes)", meta.name, meta.files.len(), meta.total);
        for (i, (n, s)) in meta.files.iter().enumerate() {
            println!("  [{i}] {n}  {s}");
        }
        assert!(meta.files.len() > 3 && meta.total > 100_000_000);
        let small: Vec<usize> = meta.files.iter().enumerate().filter(|(_, f)| f.1 < 2_000_000).map(|(i, _)| i).collect();
        assert!(!small.is_empty() && small.len() < meta.files.len());
        let small_total: u64 = small.iter().map(|i| meta.files[*i].1).sum();

        SEED_AFTER.store(true, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orexa-select-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let d = Download {
            id: 7,
            url: url.into(),
            file_name: "Sintel".into(),
            ext: String::new(),
            size: 0,
            downloaded: 0,
            status: Status::Downloading,
            speed_bps: 0.0,
            save_dir: dir.to_string_lossy().into(),
            added: String::new(),
            error: None,
            source: Source::Torrent,
            thumb: None,
            files: Some(small.clone()),
            seeding: None,
            live: Default::default(),
            extra: Extra::default(),
        };
        let (ctx, crx) = channel::<WorkerCmd>();
        let (etx, erx) = channel::<EngineEvent>();
        let t = thread::spawn(move || worker(d, crx, etx));
        let t0 = Instant::now();
        let (mut complete, mut last_size, mut seeding, mut snap_ok) = (false, 0u64, false, false);
        while t0.elapsed() < Duration::from_secs(120) {
            while let Ok(e) = erx.try_recv() {
                match e {
                    EngineEvent::Progress(_, _, size, _) if size > 0 => last_size = size,
                    EngineEvent::State(_, Status::Complete, _) => complete = true,
                    EngineEvent::State(_, Status::Error, m) => panic!("torrent error: {:?}", m),
                    EngineEvent::Seeding(_, Some(_)) => seeding = true,
                    _ => {}
                }
            }
            if let Some(sn) = snapshot(7) {
                snap_ok = sn.files.len() == meta.files.len() && sn.files.iter().filter(|f| f.selected).count() == small.len();
            }
            if complete && seeding {
                break;
            }
            thread::sleep(Duration::from_millis(400));
        }
        println!("complete={complete} seeding={seeding} size_reported={last_size} wanted={small_total} snapshot_ok={snap_ok} after {:?}", t0.elapsed());
        assert!(complete, "the chosen files never finished");
        assert_eq!(last_size, small_total, "the size must be the chosen files only");
        assert!(seeding, "it should keep sharing when asked to");
        assert!(snap_ok, "the live snapshot should list every file with the chosen ones selected");
        // Stop sharing: the engine forgets the torrent and removes the empty placeholder of the movie.
        let _ = ctx.send(WorkerCmd::Stop);
        let _ = t.join();
        let mut files = Vec::new();
        fn walk(d: &std::path::Path, out: &mut Vec<(String, u64)>) {
            if let Ok(rd) = fs::read_dir(d) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.is_dir() { walk(&p, out) } else if let Ok(m) = p.metadata() { out.push((p.file_name().unwrap().to_string_lossy().into_owned(), m.len())) }
                }
            }
        }
        walk(&dir, &mut files);
        println!("on disk after stopping: {:?}", files);
        assert!(!files.iter().any(|(n, _)| n == "Sintel.mp4"), "the movie was not chosen, so no placeholder may stay");
        assert_eq!(files.len(), small.len(), "exactly the chosen files are left");
        let _ = fs::remove_dir_all(&dir);
        SEED_AFTER.store(false, Ordering::Relaxed);
    }

    /// Needs the internet: start a whole torrent, then untick the big file while it runs.
    #[test]
    #[ignore]
    fn torrent_live_change_files_while_running() {
        let url = "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=wss%3A%2F%2Ftracker.webtorrent.dev";
        let meta = inspect(url, &Extra::default()).expect("could not read the torrent");
        let small: Vec<usize> = meta.files.iter().enumerate().filter(|(_, f)| f.1 < 2_000_000).map(|(i, _)| i).collect();
        let small_total: u64 = small.iter().map(|i| meta.files[*i].1).sum();
        SEED_AFTER.store(false, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("orexa-change-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let d = Download {
            id: 8,
            url: url.into(),
            file_name: "Sintel".into(),
            ext: String::new(),
            size: 0,
            downloaded: 0,
            status: Status::Downloading,
            speed_bps: 0.0,
            save_dir: dir.to_string_lossy().into(),
            added: String::new(),
            error: None,
            source: Source::Torrent,
            thumb: None,
            files: None, // everything, to begin with
            seeding: None,
            live: Default::default(),
            extra: Extra::default(),
        };
        let (ctx, crx) = channel::<WorkerCmd>();
        let (etx, erx) = channel::<EngineEvent>();
        let t = thread::spawn(move || worker(d, crx, etx));
        let t0 = Instant::now();
        // wait for it to be running with every file selected
        let mut first_size = 0u64;
        while t0.elapsed() < Duration::from_secs(60) {
            while let Ok(e) = erx.try_recv() {
                if let EngineEvent::Progress(_, _, size, _) = e {
                    if size > 0 && first_size == 0 {
                        first_size = size;
                    }
                }
            }
            if snapshot(8).map(|s| s.files.iter().all(|f| f.selected)).unwrap_or(false) {
                break;
            }
            thread::sleep(Duration::from_millis(300));
        }
        assert!(first_size > 100_000_000, "it starts with everything chosen ({first_size})");
        set_files(8, &small).expect("could not change the files");
        let (mut complete, mut last_size) = (false, 0u64);
        while t0.elapsed() < Duration::from_secs(90) && !complete {
            while let Ok(e) = erx.try_recv() {
                match e {
                    EngineEvent::Progress(_, _, size, _) if size > 0 => last_size = size,
                    EngineEvent::State(_, Status::Complete, _) => complete = true,
                    EngineEvent::State(_, Status::Error, m) => panic!("torrent error: {:?}", m),
                    _ => {}
                }
            }
            thread::sleep(Duration::from_millis(300));
        }
        println!("first_size={first_size} complete={complete} size_after_change={last_size} wanted={small_total} after {:?}", t0.elapsed());
        assert!(complete, "unticking the big file should let the torrent finish");
        assert_eq!(last_size, small_total);
        let _ = ctx.send(WorkerCmd::Stop);
        let _ = t.join();
        let _ = fs::remove_dir_all(&dir);
    }

    /// Needs the internet: `cargo test --release -- --ignored torrent_live`
    #[test]
    #[ignore]
    fn torrent_live() {
        // OREXA_TORRENT_DIR keeps the files between runs (to test continuing a torrent).
        let keep = std::env::var("OREXA_TORRENT_DIR").ok();
        let dir = match &keep {
            Some(k) => PathBuf::from(k),
            None => std::env::temp_dir().join(format!("orexa-torrent-{}", std::process::id())),
        };
        if keep.is_none() {
            let _ = fs::remove_dir_all(&dir);
        }
        let d = Download {
            id: 1,
            url: "magnet:?xt=urn:btih:08ada5a7a6183aae1e09d831df6748d566095a10&dn=Sintel&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&tr=wss%3A%2F%2Ftracker.webtorrent.dev".into(),
            file_name: "Sintel".into(),
            ext: String::new(),
            size: 0,
            downloaded: 0,
            status: Status::Downloading,
            speed_bps: 0.0,
            save_dir: dir.to_string_lossy().into(),
            added: String::new(),
            error: None,
            source: Source::Torrent,
            thumb: None,
            files: None,
            seeding: None,
            live: Default::default(),
            extra: Extra::default(),
        };
        let (ctx, crx) = channel::<WorkerCmd>();
        let (etx, erx) = channel::<EngineEvent>();
        let t = thread::spawn(move || worker(d, crx, etx));
        let t0 = Instant::now();
        let (mut name, mut best, mut total, mut fastest, mut first) = (String::new(), 0u64, 0u64, 0f64, None::<u64>);
        while t0.elapsed() < Duration::from_secs(75) {
            while let Ok(e) = erx.try_recv() {
                match e {
                    EngineEvent::FileName(_, n) => name = n,
                    EngineEvent::Progress(_, got, size, sp) => {
                        if first.is_none() && got > 0 {
                            first = Some(got);
                        }
                        best = best.max(got);
                        total = total.max(size);
                        fastest = fastest.max(sp);
                    }
                    EngineEvent::State(_, Status::Error, m) => panic!("torrent error: {:?}", m),
                    _ => {}
                }
            }
            if best > 6 * 1024 * 1024 {
                break;
            }
            thread::sleep(Duration::from_millis(500));
        }
        let _ = ctx.send(WorkerCmd::Stop);
        let _ = t.join();
        println!("name={name} first_nonzero_report={first:?} downloaded={best} total={total} fastest={fastest:.0} B/s after {:?}", t0.elapsed());
        assert!(!name.is_empty(), "metadata never arrived");
        assert!(total > 0);
        assert!(best > 0, "no data arrived");
        if keep.is_none() {
            let _ = fs::remove_dir_all(&dir);
        }
    }
}
