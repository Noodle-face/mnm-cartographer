//! Marker packs followed by URL.
//!
//! A pack someone publishes -- a gist, a file in a GitHub repo -- can be
//! subscribed to instead of downloaded and imported by hand. On launch, and on
//! request, each subscribed pack is fetched again and merged in the same way
//! Import merges a file: markers already on the map (same kind, within 12
//! units) are skipped, so a refresh adds only what is new. Markers the pack's
//! author later deletes stay on your map; unsubscribing can remove everything
//! that came from the pack.
//!
//! Fetching happens on a background thread; merging happens on the UI thread,
//! where the open zone's markers live, so the two can never race.

use crate::share::Pack;
use std::io::Read;
use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

/// A community pack of several thousand markers is a few MB at most.
const MAX_PACK: u64 = 16 << 20;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Subscription {
    pub url: String,
    /// What its markers are tagged with (`src`), fixed when subscribing so a
    /// pack renaming itself does not orphan what it already added.
    pub name: String,
    /// What the last refresh did, for the list.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last: String,
}

/// A GitHub page link, as people copy it out of the address bar, points at
/// HTML. Turn it into the raw file it shows.
pub fn normalise(url: &str) -> String {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix("https://github.com/") {
        if let Some((repo, path)) = rest.split_once("/blob/") {
            return format!("https://raw.githubusercontent.com/{repo}/{path}");
        }
    }
    if u.starts_with("https://gist.github.com/") && !u.contains("/raw") {
        return format!("{}/raw", u.trim_end_matches('/'));
    }
    u.to_string()
}

/// A name for a pack that does not give one: the file it was fetched from.
pub fn fallback_name(url: &str) -> String {
    let tail = url.trim_end_matches('/').rsplit('/').find(|s| !s.is_empty() && *s != "raw");
    tail.map(|t| t.trim_end_matches(".json").to_string()).unwrap_or_else(|| url.to_string())
}

pub fn fetch(url: &str) -> anyhow::Result<Pack> {
    // Plain http would let anyone on the network rewrite the pack. A debug
    // build allows it, to test against a local server.
    if !url.starts_with("https://") && !(cfg!(debug_assertions) && url.starts_with("http://")) {
        anyhow::bail!("only https:// links can be subscribed to");
    }
    let resp = crate::update::agent(Duration::from_secs(60)).get(url).call()?;
    let mut text = String::new();
    resp.into_reader().take(MAX_PACK + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_PACK {
        anyhow::bail!("that pack is larger than {} MB", MAX_PACK >> 20);
    }
    Pack::parse(&text).map_err(|e| anyhow::anyhow!("not a marker pack: {e}"))
}

/// Fetches in flight. Each finished one arrives as (url, result).
pub struct Refresh {
    rx: Receiver<(String, anyhow::Result<Pack>)>,
    pub left: usize,
}

impl Refresh {
    pub fn start(urls: Vec<String>, ctx: &egui::Context) -> Self {
        let (tx, rx) = channel();
        let left = urls.len();
        for url in urls {
            let (tx, ctx) = (tx.clone(), ctx.clone());
            std::thread::spawn(move || {
                let r = fetch(&url);
                let _ = tx.send((url, r));
                ctx.request_repaint();
            });
        }
        Self { rx, left }
    }

    /// Results that have arrived since the last call.
    pub fn take(&mut self) -> Vec<(String, anyhow::Result<Pack>)> {
        let got: Vec<_> = self.rx.try_iter().collect();
        self.left = self.left.saturating_sub(got.len());
        got
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_links_become_raw() {
        assert_eq!(normalise("https://github.com/a/b/blob/main/packs/x.json"),
                   "https://raw.githubusercontent.com/a/b/main/packs/x.json");
        assert_eq!(normalise(" https://gist.github.com/a/123 "), "https://gist.github.com/a/123/raw");
        assert_eq!(normalise("https://gist.github.com/a/123/raw/x.json"),
                   "https://gist.github.com/a/123/raw/x.json");
        assert_eq!(normalise("https://example.org/p.json"), "https://example.org/p.json");
    }

    #[test]
    fn names_from_urls() {
        assert_eq!(fallback_name("https://example.org/packs/camps.json"), "camps");
        assert_eq!(fallback_name("https://gist.github.com/a/123/raw"), "123");
    }

    /// Serve `body` once on a local port, as a pack host would.
    fn serve_once(body: &'static str) -> String {
        use std::io::Write;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut c, _) = l.accept().unwrap();
            let mut buf = [0u8; 2048];
            let _ = std::io::Read::read(&mut c, &mut buf);
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        });
        format!("http://127.0.0.1:{port}/pack.json")
    }

    #[test]
    #[cfg(debug_assertions)]
    fn fetches_a_pack() {
        let url = serve_once(r#"{"format":1,"name":"Camps","zones":{"underdocks":[
            {"id":"a","x":1.0,"z":2.0,"kind":"camp","label":"Griffons"}]}}"#);
        let p = fetch(&url).unwrap();
        assert_eq!(p.name, "Camps");
        assert_eq!(p.count(), 1);
    }

    #[test]
    #[cfg(debug_assertions)]
    fn rejects_something_else() {
        let url = serve_once("<html>not a pack</html>");
        let e = fetch(&url).err().expect("html is not a pack");
        assert!(e.to_string().contains("not a marker pack"), "{e}");
        let url = serve_once(r#"{"format":2,"zones":{}}"#);
        assert!(fetch(&url).is_err());
    }

    #[test]
    fn refuses_plain_http_in_release() {
        if !cfg!(debug_assertions) {
            assert!(fetch("http://example.org/p.json").is_err());
        }
        assert!(fetch("ftp://example.org/p.json").is_err());
    }
}
