// Leftovers: an unfinished multi-part download keeps a partial file and a small progress
// file ("name.orexa") next to it. When the download leaves the list, both are of no use, so
// they can go to the Recycle Bin, either when it is removed or later from Settings.

use super::*;
use std::collections::HashSet;

/// One leftover: the progress file, the partial file next to it (if there), and its size.
#[derive(Clone)]
pub struct Leftover {
    pub side: PathBuf,
    pub file: Option<PathBuf>,
    pub size: u64,
}

fn key(p: &std::path::Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// Progress files in `dirs` (not their subfolders) whose download is not in `in_use`.
pub fn find_leftovers(dirs: &[PathBuf], in_use: &HashSet<String>) -> Vec<Leftover> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for dir in dirs {
        if !seen.insert(key(dir)) {
            continue;
        }
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for e in rd.filter_map(|e| e.ok()) {
            let side = e.path();
            let name = side.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let Some(main_name) = name.strip_suffix(".orexa") else { continue };
            if main_name.is_empty() || !side.is_file() {
                continue;
            }
            let file = dir.join(main_name);
            if in_use.contains(&key(&file)) {
                continue;
            }
            let size = fs::metadata(&file).map(|m| m.len()).unwrap_or(0) + fs::metadata(&side).map(|m| m.len()).unwrap_or(0);
            out.push(Leftover { file: file.exists().then_some(file), side, size });
        }
    }
    out
}

/// The files to clear away for an unfinished download (its partial file and progress file).
pub fn unfinished_files(d: &Download) -> Vec<PathBuf> {
    if d.status == Status::Complete || !matches!(d.source, Source::Http) {
        return Vec::new();
    }
    let path = d.path();
    [path.clone(), multipart::side_path(&path)].into_iter().filter(|p| p.exists()).collect()
}

impl App {
    /// The folders downloads go to: the default one and its type folders, and every folder
    /// a download in the list uses.
    pub fn download_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![PathBuf::from(&self.default_dir)];
        if let Ok(rd) = fs::read_dir(&self.default_dir) {
            dirs.extend(rd.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.is_dir()));
        }
        dirs.extend(self.downloads.iter().map(|d| d.path().parent().map(|p| p.to_path_buf()).unwrap_or_default()));
        dirs.retain(|d| !d.as_os_str().is_empty());
        dirs
    }

    pub fn scan_leftovers(&mut self) {
        let in_use: HashSet<String> = self.downloads.iter().map(|d| key(&d.path())).collect();
        self.leftovers = Some(find_leftovers(&self.download_dirs(), &in_use));
    }

    /// Send files to the Recycle Bin once the download using them has let go of them.
    pub fn recycle_later(&mut self, files: Vec<PathBuf>) {
        if !files.is_empty() {
            self.pending_recycle.push((files, Instant::now() + Duration::from_millis(3500)));
        }
    }

    pub fn run_pending_recycle(&mut self) {
        let now = Instant::now();
        let due: Vec<Vec<PathBuf>> = self.pending_recycle.iter().filter(|(_, t)| *t <= now).map(|(f, _)| f.clone()).collect();
        self.pending_recycle.retain(|(_, t)| *t > now);
        for files in due {
            shell::recycle(&files);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore] // puts a small file in the real Recycle Bin
    fn files_go_to_the_recycle_bin() {
        let dir = std::env::temp_dir().join(format!("orexa-bin-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("orexa-recycle-test.txt");
        fs::write(&f, b"test").unwrap();
        assert!(shell::recycle(&[f.clone(), dir.join("missing.txt")]));
        assert!(!f.exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_progress_files_of_downloads_no_longer_listed_are_leftovers() {
        let dir = std::env::temp_dir().join(format!("orexa-left-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        for f in ["old.msi", "old.msi.orexa", "kept.zip", "kept.zip.orexa", "alone.iso.orexa", "done.pdf"] {
            fs::write(dir.join(f), b"12345").unwrap();
        }
        let in_use: HashSet<String> = [key(&dir.join("kept.zip"))].into_iter().collect();
        let mut found = find_leftovers(&[dir.clone(), dir.clone()], &in_use);
        found.sort_by_key(|l| l.side.clone());
        let names: Vec<String> = found.iter().map(|l| l.side.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, vec!["alone.iso.orexa", "old.msi.orexa"], "listed once each, the kept one left alone");
        assert!(found[0].file.is_none(), "a progress file whose partial file is gone");
        assert_eq!(found[1].size, 10);
        let _ = fs::remove_dir_all(&dir);
    }
}
