// Asking a server for a file the way a browser would.
//
// Many sites refuse ("403 Forbidden") requests that do not look like they come from a browser:
// a short or missing browser name, no Accept-Language, or no Referer (hotlink protection). Some
// refuse the opposite: a Referer from another site. So every request carries the headers a
// browser sends for a download, and a refused request is tried again with a different Referer
// before giving up.

use reqwest::blocking::{Client, RequestBuilder, Response};

/// What a current Chrome on Windows calls itself.
pub const DEFAULT_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// "https://host:port/" of a link.
pub fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("{}://{}/", scheme, host))
}

fn host(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("://")?;
    Some(rest.split(['/', '?', '#']).next()?.to_ascii_lowercase())
}

/// The Referers worth trying, best first: the page the link came from (or the site itself),
/// then none at all, then the site itself.
pub fn referer_choices(url: &str, given: Option<&str>) -> Vec<Option<String>> {
    let given = given.filter(|g| !g.trim().is_empty()).map(|g| g.to_string());
    let site = origin(url);
    let mut out: Vec<Option<String>> = Vec::new();
    for c in [given.clone().or_else(|| site.clone()), None, site] {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// A GET with a browser's headers.
pub fn browser_get(client: &Client, url: &str, referer: Option<&str>, cookie: Option<&str>) -> RequestBuilder {
    let mut req = client
        .get(url)
        .header("Accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
        .header("Accept-Language", "en-US,en;q=0.9")
        .header("Upgrade-Insecure-Requests", "1")
        .header("Sec-Fetch-Dest", "document")
        .header("Sec-Fetch-Mode", "navigate")
        .header("Sec-Fetch-User", "?1");
    let site = match referer {
        None => "none",
        Some(r) if host(r) == host(url) => "same-origin",
        Some(_) => "cross-site",
    };
    req = req.header("Sec-Fetch-Site", site);
    if let Some(r) = referer {
        req = req.header("Referer", r);
    }
    if let Some(c) = cookie.filter(|c| !c.is_empty()) {
        req = req.header("Cookie", c);
    }
    req
}

/// Send, and when the server refuses (401/403), try the other Referers. Returns the answer and
/// the Referer that got it, so later requests for the same file can use the same one.
pub fn get_tolerant(
    client: &Client,
    url: &str,
    referer: Option<&str>,
    cookie: Option<&str>,
    range: Option<&str>,
) -> reqwest::Result<(Response, Option<String>)> {
    let choices = referer_choices(url, referer);
    let mut last: Option<(Response, Option<String>)> = None;
    for (i, choice) in choices.iter().enumerate() {
        let mut req = browser_get(client, url, choice.as_deref(), cookie);
        if let Some(r) = range {
            req = req.header("Range", r);
        }
        let resp = match req.send() {
            Ok(r) => r,
            Err(e) if i + 1 == choices.len() && last.is_none() => return Err(e),
            Err(_) => continue,
        };
        let refused = matches!(resp.status().as_u16(), 401 | 403);
        if !refused || i + 1 == choices.len() {
            return Ok((resp, choice.clone()));
        }
        last = Some((resp, choice.clone()));
    }
    // Every Referer was tried; hand back the last refusal.
    Ok(last.expect("at least one attempt"))
}

/// A friendly message for a refused or failed answer.
/// The server could not be reached or went silent (as opposed to answering "no").
pub fn unreachable(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect()
}

/// Seconds to wait between tries while a server is not answering: about ten minutes in all.
pub const PATIENT_WAITS: [u64; 10] = [5, 10, 20, 30, 60, 60, 60, 120, 120, 120];

pub const NOT_ANSWERING: &str =
    "The server stopped answering. Try again later, or use Change link to continue from another address.";

pub fn status_message(status: reqwest::StatusCode) -> String {
    match status.as_u16() {
        401 | 403 => format!(
            "The site refused the download ({}). The link may have expired or need you to be signed in: start it again from the browser.",
            status
        ),
        404 | 410 => format!("The file is no longer there ({})", status),
        _ => format!("Server answered {}", status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_keeps_scheme_host_and_port() {
        assert_eq!(origin("https://cdn.example.com/a/b.zip?x=1").as_deref(), Some("https://cdn.example.com/"));
        assert_eq!(origin("http://127.0.0.1:8080/f").as_deref(), Some("http://127.0.0.1:8080/"));
        assert_eq!(origin("nonsense"), None);
    }

    #[test]
    fn referers_are_tried_best_first_without_repeats() {
        let url = "https://files.example.com/x.zip";
        assert_eq!(
            referer_choices(url, Some("https://blog.example.org/post")),
            vec![Some("https://blog.example.org/post".into()), None, Some("https://files.example.com/".into())]
        );
        assert_eq!(referer_choices(url, None), vec![Some("https://files.example.com/".into()), None]);
        assert_eq!(referer_choices(url, Some("")), vec![Some("https://files.example.com/".into()), None]);
    }
}
