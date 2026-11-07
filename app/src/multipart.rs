// Fast downloads: one file is cut into chunks that several connections fetch at once
// (a "download accelerator"). Progress is kept in a small side file (`<name>.orexa`) so pause,
// closing the app and resuming all continue where they stopped. Servers that cannot do
// byte ranges, and small files, use the normal single-connection path instead.

use crate::{Download, EngineEvent, Status, WorkerCmd};
use reqwest::blocking::{Client, RequestBuilder};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::windows::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CONNECTIONS: usize = 8;
const MIN_SIZE: u64 = 4 * 1024 * 1024;
const RETRIES: u32 = 6;

const RUN: u8 = 0;
const PAUSE: u8 = 1;
const STOP: u8 = 2;

#[derive(Serialize, Deserialize, Clone)]
struct Side {
    url: String,
    size: u64,
    chunk: u64,
    /// Bytes already stored for each chunk.
    done: Vec<u64>,
}

pub enum Outcome {
    /// Handled here (finished, failed, paused-and-stopped); nothing more to do.
    Handled,
    /// Not suitable (no range support, small file...): use the single-connection path.
    Fallback,
}

pub fn side_path(file: &Path) -> PathBuf {
    let mut s = file.as_os_str().to_os_string();
    s.push(".orexa");
    PathBuf::from(s)
}

/// Bytes really stored so far, from the side file of an unfinished multi-part download.
pub fn saved_progress(file: &Path) -> Option<u64> {
    let s: Side = serde_json::from_slice(&fs::read(side_path(file)).ok()?).ok()?;
    Some(s.done.iter().sum())
}

fn with_headers(mut req: RequestBuilder, d: &Download) -> RequestBuilder {
    if let Some(r) = &d.extra.referer {
        req = req.header("Referer", r);
    }
    if let Some(c) = &d.extra.cookie {
        req = req.header("Cookie", c);
    }
    req
}

/// Ask for the first byte: a 206 answer with a total means ranges work.
fn probe_ranges(client: &Client, d: &Download) -> Option<u64> {
    let resp = with_headers(client.get(&d.url), d).header("Range", "bytes=0-0").send().ok()?;
    if resp.status().as_u16() != 206 {
        return None;
    }
    let cr = resp.headers().get("content-range")?.to_str().ok()?.to_string();
    cr.rsplit('/').next()?.trim().parse::<u64>().ok().filter(|t| *t > 0)
}

fn load_side(file: &Path, d: &Download) -> Option<Side> {
    let s: Side = serde_json::from_slice(&fs::read(side_path(file)).ok()?).ok()?;
    let len = fs::metadata(file).ok()?.len();
    let ok = s.url == d.url && len == s.size && s.chunk > 0 && s.done.len() as u64 == s.size.div_ceil(s.chunk);
    ok.then_some(s)
}

fn save_side(file: &Path, s: &Side) {
    if let Ok(j) = serde_json::to_vec(s) {
        let _ = fs::write(side_path(file), j);
    }
}

fn chunk_bounds(s: &Side, i: usize) -> (u64, u64) {
    let start = i as u64 * s.chunk;
    (start, (start + s.chunk).min(s.size)) // end exclusive
}

pub fn run(
    d: &Download,
    client: &Client,
    path: &Path,
    cmd_rx: &Receiver<WorkerCmd>,
    ev_tx: &Sender<EngineEvent>,
) -> Outcome {
    // Resume from the side file, or start fresh when the server allows ranges.
    let mut side = match load_side(path, d) {
        Some(s) => s,
        None => {
            // A leftover partial file without a side file belongs to the single path.
            if fs::metadata(path).map(|m| m.len() > 0).unwrap_or(false) {
                return Outcome::Fallback;
            }
            let Some(total) = probe_ranges(client, d) else { return Outcome::Fallback };
            if total < MIN_SIZE {
                return Outcome::Fallback;
            }
            let chunk = (total / 48).clamp(1024 * 1024, 16 * 1024 * 1024);
            let n = total.div_ceil(chunk) as usize;
            let f = match OpenOptions::new().create(true).write(true).open(path) {
                Ok(f) => f,
                Err(_) => return Outcome::Fallback,
            };
            if f.set_len(total).is_err() {
                return Outcome::Fallback;
            }
            let s = Side { url: d.url.clone(), size: total, chunk, done: vec![0; n] };
            save_side(path, &s);
            s
        }
    };

    let size = side.size;
    let n = side.done.len();
    let done: Arc<Vec<AtomicU64>> = Arc::new(side.done.iter().map(|x| AtomicU64::new(*x)).collect());
    let total_got = Arc::new(AtomicU64::new(side.done.iter().sum()));
    let claimed: Arc<Mutex<Vec<bool>>> = Arc::new(Mutex::new(vec![false; n]));
    let control = Arc::new(AtomicU8::new(RUN));
    let failed: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let finished_threads = Arc::new(AtomicU64::new(0));
    let alive = Arc::new(AtomicBool::new(true));

    let threads = CONNECTIONS.min(n);
    let already = total_got.load(Ordering::Relaxed);
    let _ = ev_tx.send(EngineEvent::Log(
        d.id,
        if already > 0 {
            format!("Continued with {} connections, {}% was already saved", threads, (already * 100 / size.max(1)))
        } else {
            format!("Started with {} connections, the file is split into {} parts", threads, n)
        },
    ));
    let _ = ev_tx.send(EngineEvent::Conns(d.id, threads));
    for _ in 0..threads {
        let (done, total_got, claimed, control, failed, finished_threads) = (
            done.clone(),
            total_got.clone(),
            claimed.clone(),
            control.clone(),
            failed.clone(),
            finished_threads.clone(),
        );
        let (client, d, path, side, ev) = (client.clone(), d.clone(), path.to_path_buf(), side.clone(), ev_tx.clone());
        thread::spawn(move || {
            worker(&client, &d, &path, &side, &done, &total_got, &claimed, &control, &failed, &ev);
            finished_threads.fetch_add(1, Ordering::SeqCst);
        });
    }

    let _ = ev_tx.send(EngineEvent::Progress(d.id, total_got.load(Ordering::Relaxed), size, 0.0));

    let mut last_report = Instant::now();
    let mut last_save = Instant::now();
    let mut window_start = Instant::now();
    let mut window_bytes = total_got.load(Ordering::Relaxed);
    let mut speed = 0.0f64;
    let mut last_conns = threads;
    let snapshot = |side: &mut Side| {
        for (i, v) in done.iter().enumerate() {
            side.done[i] = v.load(Ordering::Relaxed);
        }
    };

    loop {
        match cmd_rx.try_recv() {
            Ok(WorkerCmd::Pause) => {
                control.store(PAUSE, Ordering::SeqCst);
                snapshot(&mut side);
                save_side(path, &side);
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Paused, None));
            }
            Ok(WorkerCmd::Run) => {
                control.store(RUN, Ordering::SeqCst);
                window_start = Instant::now();
                window_bytes = total_got.load(Ordering::Relaxed);
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Downloading, None));
            }
            Ok(WorkerCmd::Stop) => {
                control.store(STOP, Ordering::SeqCst);
                alive.store(false, Ordering::SeqCst);
                // Let the connections wind down, then keep the progress.
                let t0 = Instant::now();
                while finished_threads.load(Ordering::SeqCst) < threads as u64 && t0.elapsed() < Duration::from_secs(3) {
                    thread::sleep(Duration::from_millis(30));
                }
                snapshot(&mut side);
                save_side(path, &side);
                let _ = ev_tx.send(EngineEvent::State(d.id, Status::Queued, None));
                return Outcome::Handled;
            }
            Err(_) => {}
        }

        if let Some(e) = failed.lock().unwrap().clone() {
            control.store(STOP, Ordering::SeqCst);
            snapshot(&mut side);
            save_side(path, &side);
            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Error, Some(e)));
            return Outcome::Handled;
        }

        let got = total_got.load(Ordering::Relaxed);
        if got >= size && finished_threads.load(Ordering::SeqCst) >= threads as u64 {
            let _ = fs::remove_file(side_path(path));
            let _ = ev_tx.send(EngineEvent::Progress(d.id, size, size, 0.0));
            let _ = ev_tx.send(EngineEvent::State(d.id, Status::Complete, None));
            return Outcome::Handled;
        }
        if finished_threads.load(Ordering::SeqCst) >= threads as u64 && got < size {
            snapshot(&mut side);
            save_side(path, &side);
            let _ = ev_tx.send(EngineEvent::State(
                d.id,
                Status::Error,
                Some("The download stopped before the end. Try again.".into()),
            ));
            return Outcome::Handled;
        }

        let el = window_start.elapsed().as_secs_f64();
        if el > 0.6 {
            speed = got.saturating_sub(window_bytes) as f64 / el;
            window_start = Instant::now();
            window_bytes = got;
        }
        if control.load(Ordering::SeqCst) == PAUSE {
            speed = 0.0;
        }
        if last_report.elapsed() > Duration::from_millis(200) {
            last_report = Instant::now();
            let _ = ev_tx.send(EngineEvent::Progress(d.id, got, size, speed));
            let live = (threads as u64).saturating_sub(finished_threads.load(Ordering::SeqCst)) as usize;
            if live != last_conns && control.load(Ordering::SeqCst) == RUN {
                last_conns = live;
                let _ = ev_tx.send(EngineEvent::Conns(d.id, live));
            }
        }
        if last_save.elapsed() > Duration::from_secs(2) {
            last_save = Instant::now();
            snapshot(&mut side);
            save_side(path, &side);
        }
        thread::sleep(Duration::from_millis(60));
    }
}

#[allow(clippy::too_many_arguments)]
fn worker(
    client: &Client,
    d: &Download,
    path: &Path,
    side: &Side,
    done: &[AtomicU64],
    total_got: &AtomicU64,
    claimed: &Mutex<Vec<bool>>,
    control: &AtomicU8,
    failed: &Mutex<Option<String>>,
    ev: &Sender<EngineEvent>,
) {
    let Ok(file) = OpenOptions::new().write(true).open(path) else {
        *failed.lock().unwrap() = Some("Could not open the file".into());
        return;
    };
    let mut buf = vec![0u8; 128 * 1024];

    loop {
        if control.load(Ordering::SeqCst) == STOP {
            return;
        }
        // Take the next chunk nobody has and that is not finished.
        let idx = {
            let mut c = claimed.lock().unwrap();
            let mut pick = None;
            for i in 0..c.len() {
                let (s, e) = chunk_bounds(side, i);
                if !c[i] && done[i].load(Ordering::Relaxed) < e - s {
                    c[i] = true;
                    pick = Some(i);
                    break;
                }
            }
            match pick {
                Some(i) => i,
                None => return,
            }
        };
        let (start, end) = chunk_bounds(side, idx);

        let mut attempts = 0u32;
        loop {
            let have = done[idx].load(Ordering::Relaxed);
            if have >= end - start {
                break;
            }
            match fetch(client, d, &file, start + have, end - 1, &mut buf, &done[idx], total_got, control) {
                Ok(()) => {}
                Err(Stop) => return,
            }
            if done[idx].load(Ordering::Relaxed) >= end - start {
                break;
            }
            attempts += 1;
            let _ = ev.send(EngineEvent::Log(
                d.id,
                format!("Part {} was interrupted, trying again ({} of {})", idx + 1, attempts, RETRIES),
            ));
            if attempts > RETRIES {
                *failed.lock().unwrap() = Some("The connection kept failing. Try again.".into());
                return;
            }
            thread::sleep(Duration::from_millis(400 * attempts as u64));
        }
    }
}

struct Stop;

/// Fetch bytes `from..=to` and store them. Returns Err(Stop) when asked to stop; a
/// network error just returns Ok so the caller retries from where it got to.
#[allow(clippy::too_many_arguments)]
fn fetch(
    client: &Client,
    d: &Download,
    file: &fs::File,
    from: u64,
    to: u64,
    buf: &mut [u8],
    chunk_done: &AtomicU64,
    total_got: &AtomicU64,
    control: &AtomicU8,
) -> Result<(), Stop> {
    let req = with_headers(client.get(&d.url), d).header("Range", format!("bytes={}-{}", from, to));
    let Ok(mut resp) = req.send() else { return Ok(()) };
    if resp.status().as_u16() != 206 {
        return Ok(()); // counts as a failed attempt
    }
    let mut pos = from;
    loop {
        match control.load(Ordering::SeqCst) {
            STOP => return Err(Stop),
            PAUSE => {
                thread::sleep(Duration::from_millis(120));
                continue;
            }
            _ => {}
        }
        let want = ((to + 1 - pos) as usize).min(buf.len());
        if want == 0 {
            return Ok(());
        }
        match resp.read(&mut buf[..want]) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                let mut off = 0usize;
                while off < n {
                    match file.seek_write(&buf[off..n], pos + off as u64) {
                        Ok(w) if w > 0 => off += w,
                        _ => return Ok(()),
                    }
                }
                pos += n as u64;
                chunk_done.fetch_add(n as u64, Ordering::Relaxed);
                total_got.fetch_add(n as u64, Ordering::Relaxed);
            }
            Err(_) => return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc::channel;

    fn data(len: usize) -> Vec<u8> {
        (0..len).map(|i| ((i * 31 + i / 7) % 251) as u8).collect()
    }

    /// A tiny server that understands `Range: bytes=a-b`.
    fn serve(body: Arc<Vec<u8>>, ranges: bool) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut s) = conn else { continue };
                let body = body.clone();
                thread::spawn(move || {
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut range = None;
                    loop {
                        let mut line = String::new();
                        if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                            break;
                        }
                        if let Some(v) = line.to_lowercase().strip_prefix("range: bytes=") {
                            let v = v.trim();
                            let (a, b) = v.split_once('-').unwrap();
                            range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap_or(body.len() - 1)));
                        }
                    }
                    let (code, from, to) = match (range, ranges) {
                        (Some((a, b)), true) => ("206 Partial Content", a, b.min(body.len() - 1)),
                        _ => ("200 OK", 0, body.len() - 1),
                    };
                    let part = &body[from..=to];
                    let mut head = format!("HTTP/1.1 {}\r\nContent-Length: {}\r\nConnection: close\r\n", code, part.len());
                    if code.starts_with("206") {
                        head += &format!("Content-Range: bytes {}-{}/{}\r\n", from, to, body.len());
                    }
                    head += "\r\n";
                    let _ = s.write_all(head.as_bytes());
                    let _ = s.write_all(part);
                });
            }
        });
        format!("http://{}/file.bin", addr)
    }

    fn dl(url: &str, dir: &Path) -> Download {
        Download {
            id: 1,
            url: url.into(),
            file_name: "file.bin".into(),
            ext: "bin".into(),
            size: 0,
            downloaded: 0,
            status: Status::Downloading,
            speed_bps: 0.0,
            save_dir: dir.to_string_lossy().into(),
            added: String::new(),
            error: None,
            source: crate::Source::Http,
            thumb: None,
            files: None,
            seeding: None,
            live: Default::default(),
            extra: Default::default(),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("orexa-mp-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn last_state(rx: &Receiver<EngineEvent>) -> Option<Status> {
        let mut st = None;
        while let Ok(e) = rx.try_recv() {
            if let EngineEvent::State(_, s, _) = e {
                st = Some(s);
            }
        }
        st
    }

    #[test]
    fn downloads_in_parts_and_matches() {
        let body = Arc::new(data(20 * 1024 * 1024 + 12345));
        let url = serve(body.clone(), true);
        let dir = tmp("full");
        let path = dir.join("file.bin");
        let (_ctx, crx) = channel::<WorkerCmd>();
        let (etx, erx) = channel::<EngineEvent>();
        let out = run(&dl(&url, &dir), &Client::new(), &path, &crx, &etx);
        assert!(matches!(out, Outcome::Handled));
        assert!(last_state(&erx) == Some(Status::Complete));
        assert_eq!(fs::read(&path).unwrap(), *body);
        assert!(!side_path(&path).exists(), "side file is removed at the end");
    }

    #[test]
    fn stop_then_resume_finishes_correctly() {
        let body = Arc::new(data(24 * 1024 * 1024 + 7));
        let url = serve(body.clone(), true);
        let dir = tmp("resume");
        let path = dir.join("file.bin");
        let d = dl(&url, &dir);

        // First run: stop right after it starts.
        let (ctx, crx) = channel::<WorkerCmd>();
        let (etx, erx) = channel::<EngineEvent>();
        ctx.send(WorkerCmd::Stop).unwrap();
        let _ = run(&d, &Client::new(), &path, &crx, &etx);
        assert!(last_state(&erx) == Some(Status::Queued));
        assert!(side_path(&path).exists());

        // Second run: continues and completes.
        let (_c2, crx2) = channel::<WorkerCmd>();
        let (etx2, erx2) = channel::<EngineEvent>();
        let out = run(&d, &Client::new(), &path, &crx2, &etx2);
        assert!(matches!(out, Outcome::Handled));
        assert!(last_state(&erx2) == Some(Status::Complete));
        assert_eq!(fs::read(&path).unwrap(), *body);
    }

    #[test]
    fn servers_without_ranges_use_the_normal_path() {
        let body = Arc::new(data(6 * 1024 * 1024));
        let url = serve(body, false);
        let dir = tmp("noranges");
        let path = dir.join("file.bin");
        let (_c, crx) = channel::<WorkerCmd>();
        let (etx, _e) = channel::<EngineEvent>();
        let out = run(&dl(&url, &dir), &Client::new(), &path, &crx, &etx);
        assert!(matches!(out, Outcome::Fallback));
        assert!(!path.exists());
    }
}
