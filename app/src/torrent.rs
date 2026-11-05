// Torrents: magnet links and .torrent files, downloaded by the built-in engine (librqbit).
// Nothing to install. A finished torrent stops (Orexa does not keep seeding).

use super::*;
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, TorrentStatsState};
use std::sync::{Mutex, OnceLock};

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

/// One engine for the whole app, created the first time a torrent is added.
fn session() -> Result<Arc<Session>, String> {
    static S: Mutex<Option<Arc<Session>>> = Mutex::new(None);
    let mut g = S.lock().unwrap();
    if let Some(s) = g.as_ref() {
        return Ok(s.clone());
    }
    let s = runtime()
        .block_on(Session::new(downloads_dir()))
        .map_err(|e| format!("Could not start the torrent engine: {}", e))?;
    *g = Some(s.clone());
    Ok(s)
}

// ---------------------------------------------------------------------------
// Links
// ---------------------------------------------------------------------------

pub fn is_magnet(url: &str) -> bool {
    url.trim_start().to_lowercase().starts_with("magnet:?")
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
    // Returns the .torrent bytes; magnets are handled by the caller.
    let u = d.url.trim();
    if u.starts_with("http://") || u.starts_with("https://") {
        let client = reqwest::blocking::Client::builder()
            .user_agent(
                d.extra
                    .user_agent
                    .clone()
                    .unwrap_or_else(|| "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".to_string()),
            )
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| e.to_string())?;
        let mut req = client.get(u);
        if let Some(r) = &d.extra.referer {
            req = req.header("Referer", r);
        }
        if let Some(c) = &d.extra.cookie {
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
    let total: u64 = listed.info.iter_file_lengths().sum();
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
        ..Default::default()
    };
    let added = rt.block_on(sess.add_torrent(AddTorrent::TorrentFileBytes(listed.torrent_bytes.clone()), Some(opts)));
    let handle: Arc<ManagedTorrent> = match added {
        Ok(AddTorrentResponse::Added(_, h)) => h,
        Ok(AddTorrentResponse::AlreadyManaged(_, h)) => {
            let _ = rt.block_on(sess.unpause(&h));
            h
        }
        Ok(_) => return finish_error(&ev_tx, id, "Could not add the torrent"),
        Err(e) => return finish_error(&ev_tx, id, e),
    };

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
        let _ = ev_tx.send(EngineEvent::Progress(id, st.progress_bytes, st.total_bytes, speed));
        if st.finished {
            // Done: stop here (no seeding).
            let _ = rt.block_on(sess.pause(&handle));
            let _ = ev_tx.send(EngineEvent::Progress(id, st.total_bytes, st.total_bytes, 0.0));
            let _ = ev_tx.send(EngineEvent::State(id, Status::Complete, None));
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
