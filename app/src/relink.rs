// "Change link": an unfinished download continues from another address (a mirror, or a
// fresh link when the old one expired or its server went down). The new link must lead to
// the same file, which Orexa checks by its size before continuing.
//
// Also "the same file again": a new link whose file name and size match a download in the
// list is recognised, so an unfinished copy can be continued instead of starting over.

use super::*;

/// The Change link window.
pub struct Relink {
    pub id: u64,
    pub url: String,
    pub state: RelinkState,
    /// When the link was last typed in (it is checked a moment after typing stops).
    pub last_edit: Option<Instant>,
    pub focus: bool,
}

#[derive(Clone, PartialEq)]
pub enum RelinkState {
    Idle,
    Checking,
    /// The new link leads to the same file: its size.
    Same(u64),
    /// Nothing to compare (nothing saved yet, so any link can be used).
    Fresh,
    Bad(String),
}

/// A file name without the " (1)" Orexa adds to avoid overwriting, in lower case.
pub fn name_key(name: &str) -> String {
    let n = name.trim().to_lowercase();
    let (stem, ext) = match n.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{}", e)),
        _ => (n.clone(), String::new()),
    };
    let stem = match stem.rsplit_once(" (") {
        Some((base, num)) if num.ends_with(')') && num[..num.len() - 1].chars().all(|c| c.is_ascii_digit()) && !num[..num.len() - 1].is_empty() => {
            base.to_string()
        }
        _ => stem,
    };
    format!("{}{}", stem, ext)
}

/// Is a link with this size the same file as a download that has `size` bytes in total?
/// Unknown sizes cannot be compared, so they only pass when nothing was saved yet.
pub fn same_size(size: u64, downloaded: u64, new_size: u64) -> RelinkState {
    if downloaded == 0 {
        return RelinkState::Fresh;
    }
    if size == 0 || new_size == 0 {
        return RelinkState::Bad(
            "The server does not say how big the file is, so Orexa cannot tell whether it is the same file.".into(),
        );
    }
    if size == new_size {
        RelinkState::Same(new_size)
    } else {
        RelinkState::Bad(format!(
            "This link is for a different file ({}, the download is {}).",
            fmt_size(new_size),
            fmt_size(size)
        ))
    }
}

impl App {
    /// Open the Change link window for a download.
    pub fn open_relink(&mut self, id: u64) {
        self.relink = Some(Relink { id, url: String::new(), state: RelinkState::Idle, last_edit: None, focus: true });
        // It may have been asked for from the pop-up while the main window is in the tray.
        if self.persist {
            shell::show_window();
        }
    }

    /// Point an unfinished download at a new address and continue it from where it stopped.
    pub fn relink_to(&mut self, id: u64, url: String, extra: Extra) {
        let Some(d) = self.downloads.iter_mut().find(|x| x.id == id) else { return };
        let was_running = matches!(d.status, Status::Downloading | Status::Paused);
        d.url = url;
        d.extra = extra;
        d.error = None;
        d.live.retry_at = None;
        d.live.note("Link changed");
        // Paused while the old connection winds down (the queue would start a queued one at once).
        d.status = Status::Paused;
        self.engine.cancel(id);
        let delay = if was_running { Duration::from_millis(3500) } else { Duration::ZERO };
        self.delayed_starts.push((id, Instant::now() + delay));
    }

    /// Start downloads whose old connection has had time to wind down.
    pub fn run_delayed_starts(&mut self) {
        let now = Instant::now();
        let due: Vec<u64> = self.delayed_starts.iter().filter(|(_, t)| *t <= now).map(|(id, _)| *id).collect();
        self.delayed_starts.retain(|(_, t)| *t > now);
        for id in due {
            self.start_id(id);
        }
    }

    /// A download in the list with the same file name and size as a new link (another
    /// mirror of the same file). Not the same link: that is told apart already.
    pub fn same_file(&self, name: &str, size: u64, url: &str) -> Option<&Download> {
        if size == 0 || name.trim().is_empty() {
            return None;
        }
        let key = name_key(name);
        self.downloads
            .iter()
            .rev()
            .find(|d| matches!(d.source, Source::Http) && !same_url(&d.url, url) && d.size == size && name_key(&d.file_name) == key)
    }

    /// Check a link typed into the Change link window, a moment after typing stops.
    pub fn poll_relink(&mut self) {
        while let Ok((id, url, res)) = self.relink_rx.try_recv() {
            let Some(r) = self.relink.as_mut() else { continue };
            if r.id != id || r.url.trim() != url {
                continue; // an answer for a link that was changed since
            }
            let Some(d) = self.downloads.iter().find(|x| x.id == id) else { continue };
            r.state = match res {
                Ok((new_size, _)) => same_size(d.size, d.downloaded, new_size),
                Err(e) => RelinkState::Bad(format!("Could not reach this link: {}", e)),
            };
        }
        let Some(r) = self.relink.as_mut() else { return };
        let url = r.url.trim().to_string();
        let due = r.last_edit.map(|t| t.elapsed() > Duration::from_millis(600)).unwrap_or(false);
        if due && r.state == RelinkState::Idle {
            r.last_edit = None;
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                r.state = RelinkState::Bad("Use a link that starts with http or https.".into());
                return;
            }
            r.state = RelinkState::Checking;
            let (tx, id) = (self.relink_tx.clone(), r.id);
            thread::spawn(move || {
                let res = probe_link(&url);
                let _ = tx.send((id, url, res));
            });
        }
    }

    /// The Change link window.
    pub fn ui_relink_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some(r) = self.relink.as_mut() else { return };
        let Some(d) = self.downloads.iter().find(|x| x.id == r.id).cloned() else {
            self.relink = None;
            return;
        };
        let mut close = false;
        let mut go = false;
        modal(ctx, p, "relink", 520.0, 330.0, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            hrow(ui, |ui| {
                col(ui, |ui| {
                    lbl(ui, "Change link", fb(22.0), p.text);
                    ui.add_space(4.0);
                    lbl(ui, trunc_str(&d.file_name, 60), f(14.0), p.muted);
                });
                ui.with_layout(rl_layout(Align::Min), |ui| {
                    let (rc, _) = ui.allocate_exact_size(vec2(36.0, 36.0), Sense::hover());
                    if icon_button_at(ui, p, rc, "x", egui::Id::new("relink_close"), "Close") {
                        close = true;
                    }
                });
            });
            ui.add_space(16.0);
            let note = if d.downloaded > 0 {
                format!(
                    "Paste another address for the same file, like a mirror or a fresh link from the website. Orexa keeps the {} already saved and continues from there.",
                    fmt_size(d.downloaded)
                )
            } else {
                "Paste another address for this file, like a mirror or a fresh link from the website.".to_string()
            };
            lbl(ui, note, f(13.0), p.sub);
            ui.add_space(14.0);
            lbl(ui, "New link", fs(13.0), p.sub);
            ui.add_space(6.0);
            let x0 = ui.cursor().left();
            let te = ui.add(
                egui::TextEdit::singleline(&mut r.url)
                    .hint_text(rt("https://", f(14.0), p.muted))
                    .font(f(14.0))
                    .text_color(p.text)
                    .margin(Margin { left: 42.0, right: 12.0, top: 11.0, bottom: 11.0 })
                    .desired_width(f32::INFINITY),
            );
            paint_icon(ui, Rect::from_center_size(pos2(x0 + 22.0, te.rect.center().y), vec2(18.0, 18.0)), "link", p.muted);
            if r.focus {
                r.focus = false;
                te.request_focus();
            }
            if te.changed() {
                r.state = RelinkState::Idle;
                r.last_edit = Some(Instant::now());
            }
            ui.add_space(10.0);
            let (msg, color) = match &r.state {
                RelinkState::Idle if r.url.trim().is_empty() => (String::new(), p.muted),
                RelinkState::Idle | RelinkState::Checking => ("Checking the link...".to_string(), p.sub),
                RelinkState::Same(sz) => (format!("Same file ({}). Ready to continue.", fmt_size(*sz)), p.ok),
                RelinkState::Fresh => ("Ready. Nothing was saved yet, so it starts from the beginning.".to_string(), p.ok),
                RelinkState::Bad(e) => (e.clone(), p.bad),
            };
            if !msg.is_empty() {
                lbl(ui, msg, f(13.0), color);
            }
            ui.add_space(22.0);
            let ok = matches!(r.state, RelinkState::Same(_) | RelinkState::Fresh);
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, 44.0), rl_layout(Align::Center), |ui| {
                let label = if d.downloaded > 0 { "Continue download" } else { "Use this link" };
                if button(ui, p, label, Some("download"), Kind::Primary, 44.0, ok).clicked() {
                    go = true;
                }
                ui.add_space(4.0);
                if button(ui, p, "Cancel", None, Kind::Secondary, 44.0, true).clicked() {
                    close = true;
                }
            });
            if ok && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                go = true;
            }
        });
        if go {
            let url = r.url.trim().to_string();
            let id = r.id;
            self.relink = None;
            // A new site: what the old one needed (its referrer and cookies) does not apply.
            self.relink_to(id, url, Extra::default());
        } else if close || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.relink = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_numbered_to_avoid_overwriting_count_as_the_same_name() {
        assert_eq!(name_key("blender-5.3.0-windows-x64 (1).msi"), "blender-5.3.0-windows-x64.msi");
        assert_eq!(name_key("Blender-5.3.0-Windows-x64.MSI"), "blender-5.3.0-windows-x64.msi");
        assert_eq!(name_key("report (final).pdf"), "report (final).pdf");
        assert_eq!(name_key("song (2)"), "song");
        assert_eq!(name_key("(3).zip"), "(3).zip");
    }

    #[test]
    fn a_new_link_must_be_for_a_file_of_the_same_size() {
        assert!(same_size(383_557_632, 154_000_000, 383_557_632) == RelinkState::Same(383_557_632));
        assert!(matches!(same_size(383_557_632, 154_000_000, 383_557_633), RelinkState::Bad(_)));
        assert!(matches!(same_size(383_557_632, 154_000_000, 0), RelinkState::Bad(_)), "unknown size cannot be compared");
        assert!(same_size(0, 0, 0) == RelinkState::Fresh, "nothing saved yet: any link will do");
    }
}
