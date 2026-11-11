// Checking GitHub for a newer release, and downloading its installer.
//
// The latest release of the project is read from the GitHub API. Its installer is only
// downloaded from the project's own release address, and checked against the SHA-256 that
// GitHub publishes for the file when there is one.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use sha2::{Digest, Sha256};

pub const REPO: &str = "abdelrahmanmegaly-jpg/orexa-downloader";
const INSTALLER_NAME: &str = "Orexa-Downloader-Setup.exe";

/// A release that is newer than the running app.
#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    /// "0.1.8" (no leading v).
    pub version: String,
    /// The release notes, as written on GitHub.
    pub notes: String,
    /// Where the installer can be downloaded.
    pub url: String,
    pub size: u64,
    /// Lower-case hex SHA-256 of the installer, when GitHub gives one.
    pub sha256: Option<String>,
    /// The release page, for a manual download.
    pub page: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum UpdateState {
    /// Nothing checked yet.
    Idle,
    Checking,
    UpToDate,
    Available(Release),
    Failed(String),
}

/// Where the latest release is read from (a test can point this somewhere else).
fn api_url() -> String {
    std::env::var("OREXA_UPDATE_API").unwrap_or_else(|_| format!("https://api.github.com/repos/{}/releases/latest", REPO))
}

/// "v0.1.8" or "0.1.8" as numbers; anything that is not a version gives None.
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let core = s.split(['-', '+']).next()?;
    let parts: Vec<u64> = core.split('.').map(|p| p.parse::<u64>()).collect::<Result<_, _>>().ok()?;
    (!parts.is_empty()).then_some(parts)
}

/// Is `candidate` newer than `current`?
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(mut a), Some(mut b)) => {
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            a > b
        }
        _ => false,
    }
}

/// Only the project's own release files may be downloaded and run.
fn trusted_download(url: &str) -> bool {
    if std::env::var_os("OREXA_UPDATE_API").is_some() {
        return true; // a test setup points everything at a local server
    }
    url.starts_with(&format!("https://github.com/{}/releases/download/", REPO))
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .user_agent(format!("OrexaDownloader/{}", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())
}

/// Read the latest release. `Ok(None)` means the app is up to date.
pub fn check(current: &str) -> Result<Option<Release>, String> {
    let resp = client()?
        .get(api_url())
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|_| "Could not reach GitHub".to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub answered {}", resp.status()));
    }
    let text = resp.text().map_err(|e| e.to_string())?;
    parse_release(&text, current)
}

pub fn parse_release(json: &str, current: &str) -> Result<Option<Release>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|_| "GitHub sent something unexpected".to_string())?;
    let tag = v["tag_name"].as_str().ok_or("No version in the release")?;
    if !is_newer(tag, current) {
        return Ok(None);
    }
    let asset = v["assets"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["name"].as_str() == Some(INSTALLER_NAME)))
        .ok_or("The release has no installer")?;
    let url = asset["browser_download_url"].as_str().ok_or("The installer has no address")?.to_string();
    if !trusted_download(&url) {
        return Err("The installer is not from the project's releases".into());
    }
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|h| h.to_ascii_lowercase());
    Ok(Some(Release {
        version: tag.trim_start_matches(['v', 'V']).to_string(),
        notes: v["body"].as_str().unwrap_or("").to_string(),
        url,
        size: asset["size"].as_u64().unwrap_or(0),
        sha256,
        page: v["html_url"].as_str().unwrap_or("").to_string(),
    }))
}

/// Download the installer into a folder of its own and check it. `done` counts the bytes so
/// far (for a progress bar). Returns the path of the installer.
pub fn download(rel: &Release, done: &Arc<AtomicU64>) -> Result<PathBuf, String> {
    if !trusted_download(&rel.url) {
        return Err("The installer is not from the project's releases".into());
    }
    let dir = std::env::temp_dir().join("Orexa-Update");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| format!("Could not prepare the download: {}", e))?;
    let path = dir.join(INSTALLER_NAME);

    let mut resp = reqwest::blocking::Client::builder()
        .user_agent(format!("OrexaDownloader/{}", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|e| e.to_string())?
        .get(&rel.url)
        .send()
        .map_err(|_| "The download could not start".to_string())?;
    if !resp.status().is_success() {
        return Err(format!("The download failed ({})", resp.status()));
    }
    let mut file = std::fs::File::create(&path).map_err(|e| format!("Could not save the installer: {}", e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = resp.read(&mut buf).map_err(|_| "The download was interrupted".to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        file.write_all(&buf[..n]).map_err(|e| format!("Could not save the installer: {}", e))?;
        done.fetch_add(n as u64, Ordering::Relaxed);
    }
    drop(file);
    if let Some(expected) = &rel.sha256 {
        let got = hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>();
        if &got != expected {
            let _ = std::fs::remove_file(&path);
            return Err("The downloaded file did not match its checksum, so it was not run".into());
        }
    }
    Ok(path)
}

/// The release notes without Markdown marks, as (kind, text) lines for display.
#[derive(Clone, Debug, PartialEq)]
pub enum NoteLine {
    Heading(String),
    Bullet(String, usize),
    Text(String),
}

pub fn plain_notes(md: &str) -> Vec<NoteLine> {
    fn clean(s: &str) -> String {
        // [text](url) -> text, then drop ** and `
        let mut out = String::new();
        let mut rest = s;
        while let Some(i) = rest.find('[') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            match (after.find("]("), after.find(')')) {
                (Some(a), Some(b)) if a < b => {
                    out.push_str(&after[..a]);
                    rest = &after[b + 1..];
                }
                _ => {
                    out.push('[');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out.replace("**", "").replace('`', "")
    }
    let mut lines = Vec::new();
    for raw in md.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let trimmed = line.trim_start();
        let indent = (line.len() - trimmed.len()) / 2;
        if let Some(h) = trimmed.strip_prefix("### ").or_else(|| trimmed.strip_prefix("## ")).or_else(|| trimmed.strip_prefix("# ")) {
            lines.push(NoteLine::Heading(clean(h)));
        } else if let Some(b) = trimmed.strip_prefix("- ").or_else(|| trimmed.strip_prefix("* ")) {
            lines.push(NoteLine::Bullet(clean(b), indent.min(2)));
        } else {
            lines.push(NoteLine::Text(clean(trimmed)));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_not_text() {
        assert!(is_newer("v0.1.8", "0.1.7"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.1.10", "0.1.9")); // 10 is more than 9
        assert!(!is_newer("0.1.7", "0.1.7"));
        assert!(!is_newer("0.1.6", "0.1.7"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("nightly", "0.1.7"));
    }

    fn sample(tag: &str, url: &str) -> String {
        format!(
            r#"{{"tag_name":"{tag}","html_url":"https://github.com/{REPO}/releases/tag/{tag}","body":"Notes\n- A **thing** [link](https://x.y)",
            "assets":[{{"name":"Orexa-Downloader-Setup.exe","browser_download_url":"{url}","size":123,"digest":"sha256:ABCDEF"}}]}}"#
        )
    }

    #[test]
    fn a_newer_release_is_read_with_its_installer() {
        let url = format!("https://github.com/{REPO}/releases/download/v0.1.8/Orexa-Downloader-Setup.exe");
        let r = parse_release(&sample("v0.1.8", &url), "0.1.7").unwrap().unwrap();
        assert_eq!(r.version, "0.1.8");
        assert_eq!(r.size, 123);
        assert_eq!(r.sha256.as_deref(), Some("abcdef"));
        assert!(r.notes.contains("Notes"));
    }

    #[test]
    fn the_same_version_means_up_to_date() {
        let url = format!("https://github.com/{REPO}/releases/download/v0.1.7/Orexa-Downloader-Setup.exe");
        assert_eq!(parse_release(&sample("v0.1.7", &url), "0.1.7").unwrap(), None);
    }

    #[test]
    fn an_installer_from_somewhere_else_is_refused() {
        let r = parse_release(&sample("v0.1.8", "https://evil.example.com/Orexa-Downloader-Setup.exe"), "0.1.7");
        assert!(r.is_err());
    }

    #[test]
    fn notes_lose_their_markdown() {
        let n = plain_notes("### Added\n- A **thing** [link](https://x.y)\n  - nested\nplain");
        assert_eq!(n[0], NoteLine::Heading("Added".into()));
        assert_eq!(n[1], NoteLine::Bullet("A thing link".into(), 0));
        assert_eq!(n[2], NoteLine::Bullet("nested".into(), 1));
        assert_eq!(n[3], NoteLine::Text("plain".into()));
    }
}
