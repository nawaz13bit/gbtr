//! The trail of visited pages, kept between runs in `trail.txt` next to the
//! cookie allow-list. One node per line: `parent<TAB>url<TAB>title`, where
//! the parent is an earlier line's index or `-` for a root. `about:` pages
//! are never written, and only the newest `MAX_NODES` nodes are kept.

use std::path::PathBuf;
use url::Url;

#[derive(Clone)]
pub struct TrailNode {
    pub url: Url,
    pub title: String,
    pub parent: Option<usize>,
}

const MAX_NODES: usize = 1000;

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("gbtr").join("trail.txt"))
}

/// A missing or unreadable file just means there is no trail yet.
pub fn load() -> Vec<TrailNode> {
    let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
        return Vec::new();
    };
    let mut nodes: Vec<TrailNode> = Vec::new();
    for line in text.lines() {
        let mut parts = line.splitn(3, '\t');
        let (Some(parent), Some(url)) = (parts.next(), parts.next()) else { continue };
        let Ok(url) = Url::parse(url) else { continue };
        let parent = parent.parse::<usize>().ok().filter(|&p| p < nodes.len());
        nodes.push(TrailNode { url, title: parts.next().unwrap_or("").to_string(), parent });
    }
    nodes
}

/// Rewrites the file. Errors are returned for the status line.
pub fn save(nodes: &[TrailNode]) -> Result<(), String> {
    let path = path().ok_or("no config directory")?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, render(nodes)).map_err(|e| e.to_string())
}

fn render(nodes: &[TrailNode]) -> String {
    // Where each node ended up in the file; an `about:` node stands in for
    // its own parent so its children don't lose their place.
    let mut index: Vec<Option<usize>> = vec![None; nodes.len()];
    let mut kept = 0;
    let mut text = String::new();
    for (i, n) in nodes.iter().enumerate().skip(nodes.len().saturating_sub(MAX_NODES)) {
        let parent = n.parent.and_then(|p| index[p]);
        if n.url.scheme() == "about" {
            index[i] = parent;
            continue;
        }
        let parent = parent.map_or("-".to_string(), |p| p.to_string());
        let title = n.title.replace(['\t', '\n', '\r'], " ");
        text.push_str(&format!("{parent}\t{}\t{}\n", n.url, title.trim()));
        index[i] = Some(kept);
        kept += 1;
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(url: &str, parent: Option<usize>) -> TrailNode {
        TrailNode { url: Url::parse(url).unwrap(), title: "t".into(), parent }
    }

    #[test]
    fn about_pages_are_skipped_and_children_reattach() {
        let nodes = [node("about:blank", None), node("https://a.test/", Some(0)), node("https://b.test/", Some(1))];
        assert_eq!(render(&nodes), "-\thttps://a.test/\tt\n0\thttps://b.test/\tt\n");
    }
}
