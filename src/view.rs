//! How a page is presented, overridable per site.
//!
//! Choices live in `site_views.txt` next to the cookie allow-list, one
//! `host view` pair per line.

use std::path::PathBuf;
use url::Url;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    /// One centered reading column, capped in width.
    Compact,
    /// Full terminal width, no column cap.
    Plain,
}

impl View {
    pub fn label(self) -> &'static str {
        match self {
            View::Compact => "compact",
            View::Plain => "plain",
        }
    }

    /// Also accepts the names of the retired `article` and `list` views.
    fn parse(s: &str) -> Option<View> {
        match s {
            "compact" | "article" | "list" => Some(View::Compact),
            "plain" => Some(View::Plain),
            _ => None,
        }
    }

    /// The next view in the `v` cycle: default (compact) -> plain -> compact -> default.
    pub fn cycle(current: Option<View>) -> Option<View> {
        match current {
            None => Some(View::Plain),
            Some(View::Plain) => Some(View::Compact),
            Some(View::Compact) => None,
        }
    }
}

/// Views the user saved per site (by host).
#[derive(Default)]
pub struct SiteViews {
    entries: Vec<(String, View)>,
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gbtr").join("site_views.txt"))
}

impl SiteViews {
    /// A missing or unreadable file just means nothing is saved yet.
    pub fn load() -> SiteViews {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return SiteViews::default();
        };
        let entries = text
            .lines()
            .map(|l| l.split('#').next().unwrap_or("").trim())
            .filter_map(|l| {
                let (host, view) = l.split_once(char::is_whitespace)?;
                Some((host.to_ascii_lowercase(), View::parse(view.trim())?))
            })
            .collect();
        SiteViews { entries }
    }

    pub fn host_of(url: &Url) -> Option<String> {
        url.host_str().map(str::to_ascii_lowercase)
    }

    pub fn get(&self, host: &str) -> Option<View> {
        self.entries.iter().find(|(h, _)| h == host).map(|&(_, v)| v)
    }

    /// Saves (or, with `None`, forgets) the view for a host and rewrites the
    /// file. Errors are returned for the status line.
    pub fn set(&mut self, host: &str, view: Option<View>) -> Result<(), String> {
        self.entries.retain(|(h, _)| h != host);
        if let Some(v) = view {
            self.entries.push((host.to_string(), v));
        }
        let path = path().ok_or("no config directory")?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let text: String = self.entries.iter().map(|(h, v)| format!("{h} {}\n", v.label())).collect();
        std::fs::write(path, text).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_view_names_round_trip() {
        for v in [View::Compact, View::Plain] {
            assert_eq!(View::parse(v.label()), Some(v));
        }
    }

    #[test]
    fn retired_view_names_mean_compact() {
        assert_eq!(View::parse("article"), Some(View::Compact));
        assert_eq!(View::parse("list"), Some(View::Compact));
        assert_eq!(View::parse("table"), None);
    }
}
