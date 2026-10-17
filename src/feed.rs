//! RSS 2.0, RSS 1.0 (RDF) and Atom feeds, turned into a plain HTML page so
//! the normal document pipeline (links, numbering, overlays) handles them.

use roxmltree::{Document, Node};
use scraper::Html;

const SUMMARY_CHARS: usize = 300;

/// Returns an HTML page for `body` if it is a feed, `None` otherwise.
pub fn to_html(body: &str) -> Option<String> {
    let body = body.trim_start_matches('\u{feff}').trim_start();
    let head: String = body.chars().take(2000).collect();
    if !(head.contains("<rss") || head.contains("<feed") || head.contains("<rdf:RDF")) {
        return None;
    }

    let opts = roxmltree::ParsingOptions { allow_dtd: true, ..Default::default() };
    let xml = Document::parse_with_options(body, opts).ok()?;
    let root = xml.root_element();

    let (title_scope, items): (Node, Vec<Node>) = match root.tag_name().name() {
        "rss" => {
            let channel = child(root, "channel")?;
            (channel, children(channel, "item"))
        }
        "RDF" => (child(root, "channel")?, children(root, "item")),
        "feed" => (root, children(root, "entry")),
        _ => return None,
    };

    let feed_title = child_text(title_scope, "title").unwrap_or_else(|| "Feed".to_string());
    let mut out = String::new();
    out.push_str(&format!("<html><head><title>{0}</title></head><body><h1>{0}</h1>", esc(&feed_title)));
    if let Some(sub) = child_text(title_scope, "description").or_else(|| child_text(title_scope, "subtitle")) {
        out.push_str(&format!("<p>{}</p>", esc(&plain(&sub, SUMMARY_CHARS))));
    }

    for item in items {
        let title = child_text(item, "title").unwrap_or_else(|| "(untitled)".to_string());
        let heading = esc(&plain(&title, 200));
        match link_of(item) {
            Some(href) => out.push_str(&format!("<h2><a href=\"{}\">{}</a></h2>", esc(&href), heading)),
            None => out.push_str(&format!("<h2>{heading}</h2>")),
        }
        let date = ["pubDate", "published", "updated", "date"]
            .iter()
            .find_map(|n| child_text(item, n));
        if let Some(date) = date {
            out.push_str(&format!("<p><em>{}</em></p>", esc(date.trim())));
        }
        let summary = ["description", "summary", "content", "encoded"]
            .iter()
            .find_map(|n| child_text(item, n));
        if let Some(summary) = summary {
            let text = plain(&summary, SUMMARY_CHARS);
            if !text.is_empty() {
                out.push_str(&format!("<p>{}</p>", esc(&text)));
            }
        }
    }
    out.push_str("</body></html>");
    Some(out)
}

fn child<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Option<Node<'a, 'i>> {
    node.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn children<'a, 'i>(node: Node<'a, 'i>, name: &str) -> Vec<Node<'a, 'i>> {
    node.children().filter(|c| c.is_element() && c.tag_name().name() == name).collect()
}

fn child_text(node: Node, name: &str) -> Option<String> {
    let text: String = child(node, name)?.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect();
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// RSS keeps the URL as the text of `<link>`; Atom keeps it in `href`,
/// preferring the `alternate` link when there are several.
fn link_of(item: Node) -> Option<String> {
    let links = children(item, "link");
    let atom = links
        .iter()
        .find(|l| l.attribute("href").is_some() && matches!(l.attribute("rel"), None | Some("alternate")))
        .or_else(|| links.iter().find(|l| l.attribute("href").is_some()))
        .and_then(|l| l.attribute("href"))
        .map(str::to_string);
    atom.or_else(|| child_text(item, "link")).or_else(|| child_text(item, "guid").filter(|g| g.starts_with("http")))
}

/// Feed summaries are usually escaped HTML; keep only their text, shortened.
fn plain(markup: &str, max_chars: usize) -> String {
    let fragment = Html::parse_fragment(markup);
    let text: String = fragment.root_element().text().collect::<Vec<_>>().join(" ");
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() > max_chars {
        let cut: String = text.chars().take(max_chars).collect();
        format!("{}…", cut.trim_end())
    } else {
        text
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_items_become_links() {
        let xml = r#"<?xml version="1.0"?><rss version="2.0"><channel><title>News</title>
            <item><title>One &amp; two</title><link>https://e.com/1</link>
            <description>&lt;p&gt;Hello &lt;b&gt;world&lt;/b&gt;&lt;/p&gt;</description>
            <pubDate>Mon, 01 Jan 2026</pubDate></item></channel></rss>"#;
        let html = to_html(xml).unwrap();
        assert!(html.contains("<h1>News</h1>"));
        assert!(html.contains("<a href=\"https://e.com/1\">One &amp; two</a>"));
        assert!(html.contains("<p>Hello world</p>"));
        assert!(html.contains("Mon, 01 Jan 2026"));
    }

    #[test]
    fn atom_prefers_alternate_link() {
        let xml = r#"<feed xmlns="http://www.w3.org/2005/Atom"><title>T</title>
            <entry><title>E</title><link rel="replies" href="https://e.com/r"/>
            <link rel="alternate" href="https://e.com/a"/><updated>2026-01-01</updated>
            <summary>s</summary></entry></feed>"#;
        let html = to_html(xml).unwrap();
        assert!(html.contains("href=\"https://e.com/a\""));
        assert!(!html.contains("e.com/r"));
    }

    #[test]
    fn html_is_not_a_feed() {
        assert!(to_html("<html><body>hi</body></html>").is_none());
    }
}
