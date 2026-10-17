//! Cookie policy: first-party only, session-only unless allow-listed.
//!
//! - A cookie is stored or sent only when its site (registrable domain,
//!   e.g. `example.co.uk`) matches the site of the page being loaded. That
//!   also drops cookies set on a cross-site redirect hop.
//! - Cookies live in memory. On exit only those of allow-listed sites are
//!   written to disk, so everything else is forgotten.
//!
//! The allow-list is `cookie_allow.txt` next to `cookies.json`: one site per
//! line, `#` starts a comment.

use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use cookie_store::CookieStore;
use reqwest::cookie::CookieStore as ReqwestCookieStore;
use reqwest::header::HeaderValue;
use url::Url;

/// Registrable domain of `host` (`www.bbc.co.uk` -> `bbc.co.uk`). Hosts
/// without one (IP addresses, `localhost`) stand for themselves.
fn site_of(host: &str) -> String {
    let host = host.trim_start_matches('.').to_ascii_lowercase();
    psl::domain_str(&host).map(str::to_string).unwrap_or(host)
}

fn url_site(url: &Url) -> Option<String> {
    url.host_str().map(site_of)
}

pub struct Jar {
    store: Mutex<CookieStore>,
    /// Site of the page currently being loaded.
    top: Mutex<Option<String>>,
    allow: Vec<String>,
}

impl Jar {
    fn same_site(&self, url: &Url) -> bool {
        let top = self.top.lock().unwrap();
        top.is_some() && *top == url_site(url)
    }

    fn allowed(&self, site: &str) -> bool {
        self.allow.iter().any(|a| a == site)
    }

    /// Marks `url` as the page being loaded; call before each navigation.
    pub fn set_top(&self, url: &Url) {
        *self.top.lock().unwrap() = url_site(url);
    }

    /// Writes the cookies of allow-listed sites to `path`, creating the
    /// directory if needed. Best-effort, like loading.
    pub fn save(&self, path: Option<&PathBuf>) {
        let Some(path) = path else { return };
        let mut keep = self.store.lock().unwrap().clone();
        let drop: Vec<(String, String, String)> = keep
            .iter_any()
            .filter(|c| !c.domain().is_some_and(|d| self.allowed(&site_of(d))))
            .map(|c| (c.domain().unwrap_or_default().to_string(), c.path().unwrap_or("/").to_string(), c.name().to_string()))
            .collect();
        for (domain, cpath, name) in drop {
            keep.remove(&domain, &cpath, &name);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(file) = std::fs::File::create(path) {
            let _ = cookie_store::serde::json::save_incl_expired_and_nonpersistent(&keep, &mut BufWriter::new(file));
        }
    }
}

impl ReqwestCookieStore for Jar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        if !self.same_site(url) {
            return;
        }
        let mut store = self.store.lock().unwrap();
        for value in headers {
            if let Ok(s) = std::str::from_utf8(value.as_bytes()) {
                let _ = store.parse(s, url);
            }
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        if !self.same_site(url) {
            return None;
        }
        let store = self.store.lock().unwrap();
        let header = store
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if header.is_empty() { None } else { HeaderValue::from_str(&header).ok() }
    }
}

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gbtr"))
}

pub fn store_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("cookies.json"))
}

fn load_allow_list() -> Vec<String> {
    let Some(text) = config_dir().and_then(|d| std::fs::read_to_string(d.join("cookie_allow.txt")).ok()) else {
        return Vec::new();
    };
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(site_of)
        .collect()
}

/// The shared jar. Loads the saved cookies of allow-listed sites; anything
/// else in the file (from before this policy existed) is ignored and gets
/// dropped at the next save. A bad file never stops the browser starting.
pub fn jar() -> Arc<Jar> {
    static JAR: OnceLock<Arc<Jar>> = OnceLock::new();
    JAR.get_or_init(|| {
        let allow = load_allow_list();
        let mut store: CookieStore = store_path()
            .and_then(|p| std::fs::File::open(p).ok())
            .map(BufReader::new)
            .and_then(|r| cookie_store::serde::json::load(r).ok())
            .unwrap_or_default();
        let stale: Vec<(String, String, String)> = store
            .iter_any()
            .filter(|c| !c.domain().is_some_and(|d| allow.contains(&site_of(d))))
            .map(|c| (c.domain().unwrap_or_default().to_string(), c.path().unwrap_or("/").to_string(), c.name().to_string()))
            .collect();
        for (domain, path, name) in stale {
            store.remove(&domain, &path, &name);
        }
        Arc::new(Jar { store: Mutex::new(store), top: Mutex::new(None), allow })
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_is_registrable_domain() {
        assert_eq!(site_of("www.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site_of("accounts.google.com"), "google.com");
        assert_eq!(site_of("localhost"), "localhost");
    }

    #[test]
    fn only_top_site_cookies_pass() {
        let jar = Jar { store: Mutex::new(CookieStore::default()), top: Mutex::new(None), allow: vec![] };
        let page = Url::parse("https://www.example.com/").unwrap();
        let tracker = Url::parse("https://track.other.net/").unwrap();
        jar.set_top(&page);
        let header = HeaderValue::from_static("a=1");
        jar.set_cookies(&mut std::iter::once(&header), &tracker);
        jar.set_cookies(&mut std::iter::once(&header), &page);
        assert!(jar.cookies(&tracker).is_none());
        assert_eq!(jar.cookies(&page).unwrap(), "a=1");
    }
}
