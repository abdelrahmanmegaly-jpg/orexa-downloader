// "Check file": compare a finished file with the checksum its website publishes (SHA-256,
// SHA-512, SHA-1 or MD5). Orexa also looks for that checksum next to the file on the server
// (name.sha256, SHA256SUMS, a .sha256 file in the same folder...).

use super::*;
use md5::Md5;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// The file's checksums, in lower-case hex.
#[derive(Clone)]
pub struct Sums {
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
    pub sha512: String,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

/// Read the file once and work out all four checksums. `done` counts the bytes read.
pub fn hash_file(path: &std::path::Path, done: &AtomicU64, cancel: &AtomicBool) -> Result<Sums, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("Could not open the file: {}", e))?;
    let (mut a, mut b, mut c, mut d) = (Md5::new(), Sha1::new(), Sha256::new(), Sha512::new());
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Err("Stopped".into());
        }
        let n = f.read(&mut buf).map_err(|e| format!("Could not read the file: {}", e))?;
        if n == 0 {
            break;
        }
        a.update(&buf[..n]);
        b.update(&buf[..n]);
        c.update(&buf[..n]);
        d.update(&buf[..n]);
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    Ok(Sums { md5: hex(&a.finalize()), sha1: hex(&b.finalize()), sha256: hex(&c.finalize()), sha512: hex(&d.finalize()) })
}

/// Which checksum a pasted value is, by its length.
pub fn kind_of(h: &str) -> Option<&'static str> {
    let h = h.trim();
    if !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match h.len() {
        32 => Some("MD5"),
        40 => Some("SHA-1"),
        64 => Some("SHA-256"),
        128 => Some("SHA-512"),
        _ => None,
    }
}

/// Does the file match the checksum? None when the value is not a checksum.
pub fn matches(expected: &str, s: &Sums) -> Option<bool> {
    let e = clean(expected);
    let mine = match kind_of(&e)? {
        "MD5" => &s.md5,
        "SHA-1" => &s.sha1,
        "SHA-256" => &s.sha256,
        _ => &s.sha512,
    };
    Some(*mine == e)
}

/// A pasted checksum, without spaces, a "sha256:" prefix or a file name after it.
pub fn clean(v: &str) -> String {
    let v = v.trim();
    let v = v.split_once(':').filter(|(k, _)| k.len() <= 7).map(|(_, h)| h).unwrap_or(v);
    v.split_whitespace().find(|t| kind_of(t).is_some()).unwrap_or(v).trim().to_lowercase()
}

/// Find the checksum for `names` in a checksum file's text. A file made for this one file
/// may hold the bare value only (`single`).
pub fn pick(text: &str, names: &[String], single: bool) -> Option<String> {
    let names: Vec<String> = names.iter().filter(|n| !n.is_empty()).map(|n| n.to_lowercase()).collect();
    let mut all = Vec::new();
    for line in text.lines() {
        let low = line.to_lowercase();
        let hashes: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == '=' || c == '(' || c == ')' || c == '*')
            .filter(|t| kind_of(t).is_some())
            .collect();
        if hashes.is_empty() {
            continue;
        }
        // "hash  name", "hash *name" or "SHA256 (name) = hash".
        let named = names.iter().any(|n| {
            low.split(|c: char| c.is_whitespace() || c == '(' || c == ')' || c == '*' || c == '/')
                .any(|t| t == n)
        });
        if named {
            return Some(hashes[0].to_lowercase());
        }
        all.extend(hashes);
    }
    (single && all.len() == 1).then(|| all[0].to_lowercase())
}

/// Look on the server for the file's published checksum: (checksum, where it was found).
pub fn find_published(url: &str, file_name: &str) -> Option<(String, String)> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let base = url.split(['?', '#']).next().unwrap_or(url);
    let (dir, remote_name) = base.rsplit_once('/')?;
    let names = vec![percent_decode(remote_name), file_name.to_string()];
    let client = reqwest::blocking::Client::builder()
        .user_agent(web::DEFAULT_UA)
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let get = |u: &str, max: usize| -> Option<String> {
        let r = client.get(u).send().ok()?;
        if !r.status().is_success() || r.content_length().unwrap_or(0) > max as u64 {
            return None;
        }
        let mut body = Vec::new();
        r.take(max as u64 + 1).read_to_end(&mut body).ok()?;
        (body.len() <= max).then(|| String::from_utf8_lossy(&body).into_owned())
    };
    // Made for this file: "name.sha256" and the like.
    for ext in ["sha256", "sha512", "sha1", "md5", "sha256sum", "sha256.txt"] {
        let u = format!("{}.{}", base, ext);
        if let Some(t) = get(&u, 64 * 1024) {
            if let Some(h) = pick(&t, &names, true) {
                return Some((h, u));
            }
        }
    }
    // A list for the whole folder: from the folder's index page, or the usual names.
    let mut lists: Vec<String> = Vec::new();
    if let Some(index) = get(&format!("{}/", dir), 2 * 1024 * 1024) {
        for part in index.split("href=\"").skip(1) {
            let href = part.split('"').next().unwrap_or("");
            let low = href.to_lowercase();
            let looks = [".sha256", ".sha512", ".sha1", ".md5", "sums", "checksums", ".sha256.txt"]
                .iter()
                .any(|e| low.ends_with(e) || low.ends_with(&format!("{}.txt", e)));
            if looks && !href.contains('/') && !href.starts_with('?') && !lists.contains(&href.to_string()) {
                lists.push(href.to_string());
            }
        }
    }
    for n in ["SHA256SUMS", "sha256sums.txt", "SHA512SUMS", "checksums.txt", "CHECKSUMS", "MD5SUMS"] {
        if !lists.iter().any(|l| l.eq_ignore_ascii_case(n)) {
            lists.push(n.to_string());
        }
    }
    // The strongest kind first.
    let rank = |l: &String| {
        let l = l.to_lowercase();
        if l.contains("sha512") {
            0
        } else if l.contains("sha256") {
            1
        } else if l.contains("sha1") {
            3
        } else if l.contains("md5") {
            4
        } else {
            2
        }
    };
    lists.sort_by_key(rank);
    for l in lists.iter().take(10) {
        let u = format!("{}/{}", dir, l);
        if let Some(t) = get(&u, 512 * 1024) {
            if let Some(h) = pick(&t, &names, false) {
                return Some((h, u));
            }
        }
    }
    None
}

/// The Check file window.
pub struct CheckDlg {
    pub id: u64,
    pub expected: String,
    /// Looking on the server; then what was found there (checksum, where).
    pub finding: bool,
    pub found: Option<(String, String)>,
    pub total: u64,
    pub done: Arc<AtomicU64>,
    pub cancel: Arc<AtomicBool>,
    pub result: Option<Result<Sums, String>>,
    pub rx: Receiver<CheckMsg>,
    pub copied: Option<Instant>,
}

pub enum CheckMsg {
    Found(Option<(String, String)>),
    Hashed(Result<Sums, String>),
}

impl App {
    pub fn open_check(&mut self, id: u64) {
        let Some(d) = self.downloads.iter().find(|x| x.id == id) else { return };
        if let Some(old) = &self.check {
            old.cancel.store(true, Ordering::Relaxed);
        }
        let (tx, rx) = channel();
        let done = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let path = d.path();
        {
            let (tx, done, cancel) = (tx.clone(), done.clone(), cancel.clone());
            thread::spawn(move || {
                let _ = tx.send(CheckMsg::Hashed(hash_file(&path, &done, &cancel)));
            });
        }
        let (url, name) = (d.url.clone(), d.file_name.clone());
        let looks = matches!(d.source, Source::Http);
        if looks {
            thread::spawn(move || {
                let _ = tx.send(CheckMsg::Found(find_published(&url, &name)));
            });
        }
        let total = fs::metadata(d.path()).map(|m| m.len()).unwrap_or(d.size);
        self.check = Some(CheckDlg {
            id,
            expected: String::new(),
            finding: looks,
            found: None,
            total,
            done,
            cancel,
            result: None,
            rx,
            copied: None,
        });
        if self.persist {
            shell::show_window();
        }
    }

    pub fn ui_check_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some(c) = self.check.as_mut() else { return };
        while let Ok(m) = c.rx.try_recv() {
            match m {
                CheckMsg::Found(f) => {
                    c.finding = false;
                    if let Some((h, _)) = &f {
                        if c.expected.trim().is_empty() {
                            c.expected = h.clone();
                        }
                    }
                    c.found = f;
                }
                CheckMsg::Hashed(r) => c.result = Some(r),
            }
        }
        let Some(d) = self.downloads.iter().find(|x| x.id == c.id).cloned() else {
            self.check = None;
            return;
        };
        let mut close = false;
        let mut copy: Option<String> = None;
        modal(ctx, p, "check", 560.0, 420.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            hrow(ui, |ui| {
                col(ui, |ui| {
                    lbl(ui, "Check file", fb(22.0), p.text);
                    ui.add_space(4.0);
                    lbl(ui, trunc_str(&d.file_name, 60), f(14.0), p.muted);
                });
                ui.with_layout(rl_layout(Align::Min), |ui| {
                    let (rc, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                    if icon_button_at(ui, p, rc, "x", egui::Id::new("check_close"), "Close") {
                        close = true;
                    }
                });
            });
            ui.add_space(14.0);
            lbl(ui,
                "Compare the file with the checksum on its download page, to be sure it is complete and unchanged.",
                f(13.0),
                p.sub,
            );
            ui.add_space(14.0);
            lbl(ui, "Checksum from the website", fs(13.0), p.sub);
            ui.add_space(6.0);
            ui.add(
                egui::TextEdit::singleline(&mut c.expected)
                    .hint_text(rt("SHA-256, SHA-512, SHA-1 or MD5", f(14.0), p.muted))
                    .font(egui::FontId::monospace(13.0))
                    .text_color(p.text)
                    .margin(Margin::symmetric(12.0, 11.0))
                    .desired_width(f32::INFINITY),
            );
            ui.add_space(6.0);
            let found_note = if c.finding {
                "Looking for it on the server...".to_string()
            } else if let Some((_, src)) = &c.found {
                let file = src.rsplit('/').next().unwrap_or(src);
                format!("Found on the server, in {}", file)
            } else if matches!(d.source, Source::Http) {
                "Not found on the server: paste it from the download page.".to_string()
            } else {
                String::new()
            };
            if !found_note.is_empty() {
                lbl(ui, found_note, f(12.0), p.muted);
            }
            ui.add_space(16.0);

            match &c.result {
                None => {
                    let got = c.done.load(Ordering::Relaxed);
                    let frac = if c.total > 0 { (got as f32 / c.total as f32).clamp(0.0, 1.0) } else { 0.0 };
                    lbl(ui, format!("Reading the file... {:.0}%", frac * 100.0), f(13.0), p.sub);
                    ui.add_space(6.0);
                    let (tr_, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
                    ui.painter().rect_filled(tr_, 4.0, p.track);
                    if frac > 0.0 {
                        ui.painter().rect_filled(mrect(tr_, Rect::from_min_size(tr_.min, vec2((tr_.width() * frac).max(8.0), 8.0))), 4.0, p.progress);
                    }
                    ui.ctx().request_repaint_after(Duration::from_millis(100));
                }
                Some(Err(e)) => {
                    lbl(ui, e, f(13.0), p.bad);
                }
                Some(Ok(sums)) => {
                    let verdict = matches(&c.expected, sums);
                    let kind = kind_of(&clean(&c.expected)).unwrap_or("SHA-256");
                    match verdict {
                        Some(true) => {
                            hrow(ui, |ui| {
                                let (ir, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                                paint_icon(ui, ir, "check", p.ok);
                                ui.add_space(6.0);
                                lbl(ui, format!("Matches ({})", kind), fs(15.0), p.ok);
                            });
                            ui.add_space(4.0);
                            lbl(ui, "The file is exactly what the website published.", f(13.0), p.sub);
                        }
                        Some(false) => {
                            hrow(ui, |ui| {
                                let (ir, _) = ui.allocate_exact_size(vec2(20.0, 20.0), Sense::hover());
                                paint_icon(ui, ir, "x", p.bad);
                                ui.add_space(6.0);
                                lbl(ui, format!("Does not match ({})", kind), fs(15.0), p.bad);
                            });
                            ui.add_space(4.0);
                            lbl(ui, "The file is damaged or not the one published. Download it again.", f(13.0), p.sub);
                        }
                        None if c.expected.trim().is_empty() => {}
                        None => {
                            lbl(ui, "That does not look like a checksum.", f(13.0), p.bad);
                        }
                    }
                    ui.add_space(14.0);
                    let mine = match kind {
                        "MD5" => sums.md5.clone(),
                        "SHA-1" => sums.sha1.clone(),
                        "SHA-512" => sums.sha512.clone(),
                        _ => sums.sha256.clone(),
                    };
                    lbl(ui, format!("This file's {}", kind), fs(13.0), p.sub);
                    ui.add_space(6.0);
                    hrow(ui, |ui| {
                        let copied = c.copied.map(|t| t.elapsed() < Duration::from_secs(2)).unwrap_or(false);
                        let bw = text_w(ui, if copied { "Copied" } else { "Copy" }, fs(14.0)) + 44.0;
                        let w = (ui.available_width() - bw - 8.0).max(120.0);
                        let shown = if mine.len() > 64 { format!("{}...", &mine[..61]) } else { mine.clone() };
                        ui.add_sized(vec2(w, 30.0), egui::Label::new(rt(shown, egui::FontId::monospace(12.0), p.text)).truncate());
                        if button(ui, p, if copied { "Copied" } else { "Copy" }, None, Kind::Secondary, 30.0, true).clicked() {
                            copy = Some(mine.clone());
                        }
                    });
                }
            }
            ui.add_space(22.0);
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, 44.0), rl_layout(Align::Center), |ui| {
                if button(ui, p, "Close", None, Kind::Secondary, 44.0, true).clicked() {
                    close = true;
                }
            });
        });
        if let Some(t) = copy {
            if let Ok(mut cb) = arboard::Clipboard::new() {
                let _ = cb.set_text(t);
            }
            c.copied = Some(Instant::now());
        }
        if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            c.cancel.store(true, Ordering::Relaxed);
            self.check = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLENDER: &str = "3ff4ec8a488be1148aa2bdb2cb229be21b8050e75931da617934cac254dd726c  blender-5.3.0-windows-x64.msi\n\
2f31651a53e5aced73108bd945d2b3285f4b77fbeac01fdcdae844f5517dd4af  blender-5.3.0-windows-x64.msix\n";

    #[test]
    fn finds_the_line_for_the_file_in_a_list() {
        let names = vec!["blender-5.3.0-windows-x64.msi".to_string()];
        assert_eq!(pick(BLENDER, &names, false).as_deref(), Some("3ff4ec8a488be1148aa2bdb2cb229be21b8050e75931da617934cac254dd726c"));
        let msix = vec!["blender-5.3.0-windows-x64.msix".to_string()];
        assert_eq!(pick(BLENDER, &msix, false).as_deref(), Some("2f31651a53e5aced73108bd945d2b3285f4b77fbeac01fdcdae844f5517dd4af"));
        assert_eq!(pick(BLENDER, &["other.zip".to_string()], false), None, "a list without the file says nothing");
        // BSD style and "*name".
        let bsd = "SHA256 (app.zip) = 3ff4ec8a488be1148aa2bdb2cb229be21b8050e75931da617934cac254dd726c";
        assert!(pick(bsd, &["app.zip".to_string()], false).is_some());
        let star = "d41d8cd98f00b204e9800998ecf8427e *app.zip";
        assert_eq!(pick(star, &["App.zip".to_string()], false).as_deref(), Some("d41d8cd98f00b204e9800998ecf8427e"));
        // A file made for one file may hold only the value.
        assert!(pick("d41d8cd98f00b204e9800998ecf8427e\n", &["x".to_string()], true).is_some());
        assert!(pick("d41d8cd98f00b204e9800998ecf8427e\n", &["x".to_string()], false).is_none());
    }

    #[test]
    fn pasted_values_are_recognised_and_compared() {
        let s = Sums {
            md5: "d41d8cd98f00b204e9800998ecf8427e".into(),
            sha1: "da39a3ee5e6b4b0d3255bfef95601890afd80709".into(),
            sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
            sha512: "x".repeat(128),
        };
        assert_eq!(matches("  E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855 ", &s), Some(true));
        assert_eq!(matches("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", &s), Some(true));
        assert_eq!(matches("d41d8cd98f00b204e9800998ecf8427e  empty.txt", &s), Some(true));
        assert_eq!(matches("da39a3ee5e6b4b0d3255bfef95601890afd80700", &s), Some(false));
        assert_eq!(matches("not a checksum", &s), None);
    }

    #[test]
    #[ignore] // needs the internet
    fn finds_blenders_published_checksum() {
        let r = find_published("https://download.blender.org/release/Blender5.3/blender-5.3.0-windows-x64.msi", "blender-5.3.0-windows-x64 (1).msi");
        let (h, src) = r.expect("found");
        println!("found in {}", src);
        assert_eq!(h, "3ff4ec8a488be1148aa2bdb2cb229be21b8050e75931da617934cac254dd726c");
    }

    #[test]
    fn hashes_a_file() {
        let dir = std::env::temp_dir().join(format!("orexa-sum-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        let f = dir.join("empty.txt");
        fs::write(&f, b"").unwrap();
        let s = hash_file(&f, &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        assert_eq!(s.sha256, "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(s.md5, "d41d8cd98f00b204e9800998ecf8427e");
        let _ = fs::remove_dir_all(&dir);
    }
}
