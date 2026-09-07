use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ego_tree::NodeRef;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use scraper::{Html, Node, Selector};
use std::env;
use std::io::{self, Stdout};
use std::process::ExitCode;
use url::Url;

// ---------- HTML -> readable text extraction (unchanged from stage 2) ----------

fn is_block(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "tr"
            | "table"
            | "blockquote"
            | "section"
            | "article"
    )
}

fn is_skipped(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "head" | "noscript" | "svg")
}

fn push_word(out: &mut String, word: &str) {
    if !out.is_empty() && !out.ends_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(word);
}

fn push_break(out: &mut String) {
    if !out.ends_with("\n\n") {
        if out.ends_with('\n') {
            out.push('\n');
        } else if !out.is_empty() {
            out.push_str("\n\n");
        }
    }
}

/// A followable link, plus enough info to show it in the side pane and
/// highlight it inline: the visible label text, the 0-based source line (in
/// `Page::text`) it's on, and its byte column range within that line.
///
/// `block` identifies the nearest enclosing block-level element (see
/// `is_block`) the link was found in -- the raw building block for the
/// link tree assembled by `build_link_rows`.
struct LinkInfo {
    line: usize,
    col_start: usize,
    col_end: usize,
    label: String,
    href: String,
    block: usize,
}

/// A heading (`h1`-`h6`), for the table-of-contents overlay and inline
/// styling: its nesting level, the visible text, and the 0-based source
/// line (in `Page::text`) it starts on.
struct HeadingInfo {
    level: u8,
    line: usize,
    label: String,
}

fn heading_level(tag: &str) -> Option<u8> {
    match tag {
        "h1" => Some(1),
        "h2" => Some(2),
        "h3" => Some(3),
        "h4" => Some(4),
        "h5" => Some(5),
        "h6" => Some(6),
        _ => None,
    }
}

fn walk(
    node: NodeRef<Node>,
    base: &Url,
    out: &mut String,
    links: &mut Vec<LinkInfo>,
    headings: &mut Vec<HeadingInfo>,
    anchors: &mut std::collections::HashMap<String, usize>,
    block: usize,
    next_block: &mut usize,
    block_parent: &mut Vec<usize>,
) {
    match node.value() {
        Node::Text(text) => {
            for word in text.text.split_whitespace() {
                push_word(out, word);
            }
        }
        Node::Element(el) => {
            let tag = el.name();
            if is_skipped(tag) {
                return;
            }
            if tag == "br" {
                out.push('\n');
                return;
            }
            if tag == "img" {
                let alt = el.attr("alt").unwrap_or("").trim();
                let label = if alt.is_empty() { "image".to_string() } else { format!("image: {alt}") };
                push_word(out, &format!("[{label}]"));
                return;
            }
            if tag == "td" || tag == "th" {
                if !out.is_empty() && !out.ends_with(char::is_whitespace) {
                    out.push_str("  |  ");
                }
            }
            let is_blk = is_block(tag);
            if is_blk {
                push_break(out);
            }

            if let Some(id) = el.attr("id") {
                if !id.is_empty() {
                    let line = out.matches('\n').count();
                    anchors.entry(id.to_string()).or_insert(line);
                }
            }

            let child_block = if is_blk {
                *next_block += 1;
                block_parent.push(block);
                *next_block
            } else {
                block
            };

            let text_start = out.len();
            for child in node.children() {
                walk(child, base, out, links, headings, anchors, child_block, next_block, block_parent);
            }

            if let Some(level) = heading_level(tag) {
                let label = out[text_start..].trim().to_string();
                if !label.is_empty() {
                    let line = out[..text_start].matches('\n').count();
                    headings.push(HeadingInfo { level, line, label });
                }
            }

            if tag == "a" {
                let label = out[text_start..].trim().to_string();
                let is_substantial = label.chars().filter(|c| c.is_alphanumeric()).count() >= 3;
                if !is_substantial {
                    // No visible text, or just an icon/arrow/bare page number
                    // (e.g. HN's upvote arrow or comment-thread pagination)
                    // -- not worth registering as a followable link.
                    out.truncate(text_start);
                } else if let Some(href) = el.attr("href") {
                    if let Ok(absolute) = base.join(href) {
                        let line_start = out[..text_start].rfind('\n').map_or(0, |i| i + 1);
                        let line = out[..text_start].matches('\n').count();
                        links.push(LinkInfo {
                            line,
                            col_start: text_start - line_start,
                            col_end: out.len() - line_start,
                            label,
                            href: absolute.to_string(),
                            block,
                        });
                    }
                }
            }

            if is_blk {
                push_break(out);
            }
        }
        Node::Document | Node::Fragment => {
            for child in node.children() {
                walk(child, base, out, links, headings, anchors, block, next_block, block_parent);
            }
        }
        _ => {}
    }
}

/// One row of the flattened link list: either a link, or a separator
/// marking the boundary between two groups of links.
enum LinkRow {
    Link(usize),
    Separator,
}

/// Turns the raw block-nesting recorded during `walk` into groups of
/// links, in document order, with a separator between groups.
///
/// The raw block nesting is mostly incidental HTML wrapper noise (div in
/// div in div), so a literal 1:1 tree would be deep and pointless. Instead,
/// any block that has no links of its own and exactly one content-bearing
/// child block is collapsed away, leaving only blocks that actually branch
/// (multiple sibling link groups) or hold both their own links and a
/// nested group. Each surviving top-level branch becomes one flat group,
/// separated from its neighbors by a `Separator` row.
fn build_link_rows(links: &[LinkInfo], next_block: usize, block_parent: &[usize]) -> Vec<LinkRow> {
    use std::collections::HashMap;

    let mut own_links: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, link) in links.iter().enumerate() {
        own_links.entry(link.block).or_default().push(i);
    }
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    for block in 1..=next_block {
        children.entry(block_parent[block - 1]).or_default().push(block);
    }

    struct Node {
        links: Vec<usize>,
        children: Vec<Node>,
    }

    fn build(block: usize, own_links: &HashMap<usize, Vec<usize>>, children: &HashMap<usize, Vec<usize>>) -> Option<Node> {
        let mut child_nodes: Vec<Node> = children
            .get(&block)
            .into_iter()
            .flatten()
            .filter_map(|&c| build(c, own_links, children))
            .collect();
        let own = own_links.get(&block).cloned().unwrap_or_default();
        if own.is_empty() {
            if child_nodes.is_empty() {
                return None;
            }
            if child_nodes.len() == 1 {
                return Some(child_nodes.pop().unwrap());
            }
        }
        Some(Node { links: own, children: child_nodes })
    }

    // Flattens a whole subtree into one uninterrupted run of links -- no
    // separators inside a group, however deep the collapsed tree happens
    // to nest.
    fn flatten_group(node: &Node, rows: &mut Vec<LinkRow>) {
        for &link_index in &node.links {
            rows.push(LinkRow::Link(link_index));
        }
        for child in &node.children {
            flatten_group(child, rows);
        }
    }

    let mut rows = Vec::new();
    if let Some(root) = build(0, &own_links, &children) {
        for &link_index in &root.links {
            rows.push(LinkRow::Link(link_index));
        }
        for child in &root.children {
            if !rows.is_empty() {
                rows.push(LinkRow::Separator);
            }
            flatten_group(child, &mut rows);
        }
    }
    rows
}

struct Page {
    title: String,
    text: String,
    links: Vec<LinkInfo>,
    link_rows: Vec<LinkRow>,
    headings: Vec<HeadingInfo>,
    /// Maps an in-page anchor id (from `id="..."` attributes) to the source
    /// line it starts on, for jumping to `#fragment` links without a fetch.
    anchors: std::collections::HashMap<String, usize>,
}

/// Pulls a `charset=...` parameter out of a `Content-Type` header value or
/// a sniffed `<meta charset=...>` string, case-insensitively.
fn extract_charset(haystack: &str) -> Option<String> {
    let lower = haystack.to_lowercase();
    let idx = lower.find("charset=")?;
    let rest = &lower[idx + "charset=".len()..];
    let value: String = rest
        .chars()
        .take_while(|c| !c.is_whitespace() && *c != ';' && *c != '"' && *c != '\'' && *c != '>')
        .collect();
    if value.is_empty() { None } else { Some(value) }
}

fn fetch(client: &reqwest::blocking::Client, url: &Url) -> Result<Page, String> {
    let response = client
        .get(url.clone())
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .map_err(|e| e.to_string())?;

    let content_type_charset = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(extract_charset);

    let raw = response.bytes().map_err(|e| e.to_string())?;

    let charset = content_type_charset.or_else(|| {
        let prefix_len = raw.len().min(4096);
        let prefix = String::from_utf8_lossy(&raw[..prefix_len]);
        extract_charset(&prefix)
    });

    let encoding = charset
        .as_deref()
        .and_then(|c| encoding_rs::Encoding::for_label(c.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (body, _, _) = encoding.decode(&raw);
    let body = body.into_owned();

    let document = Html::parse_document(&body);

    let title_selector = Selector::parse("title").unwrap();
    let title = document
        .select(&title_selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default();

    let mut text = String::new();
    let mut links = Vec::new();
    let mut headings = Vec::new();
    let mut anchors = std::collections::HashMap::new();
    let mut next_block = 0usize;
    let mut block_parent = Vec::new();
    walk(
        document.tree.root(),
        url,
        &mut text,
        &mut links,
        &mut headings,
        &mut anchors,
        0,
        &mut next_block,
        &mut block_parent,
    );
    let link_rows = build_link_rows(&links, next_block, &block_parent);

    Ok(Page {
        title,
        text: text.trim().to_string(),
        links,
        link_rows,
        headings,
        anchors,
    })
}

// ---------- TUI ----------

enum Mode {
    Normal,
    UrlEntry,
    Toc,
    Search,
}

/// One line of already-wrapped output text, tagged with where it came from
/// in the source (`Page::text`) so links can be mapped back onto it: the
/// 0-based source line, and the byte column range within that source line
/// this wrapped segment covers.
struct WrappedLine {
    src_line: usize,
    col_start: usize,
    col_end: usize,
    text: String,
}

/// Greedy word-wrap of a single line to `width` columns, returning byte
/// column ranges into `line` (not copies) so callers can map other
/// byte-range data (like link positions) onto the wrapped segments exactly.
fn wrap_line(line: &str, width: usize) -> Vec<(usize, usize)> {
    let mut words = Vec::new();
    let mut idx = 0;
    for word in line.split(' ') {
        if !word.is_empty() {
            words.push((idx, idx + word.len()));
        }
        idx += word.len() + 1;
    }
    if words.is_empty() {
        return vec![(0, 0)];
    }

    let mut result = Vec::new();
    let (mut seg_start, mut seg_end) = words[0];
    for &(ws, we) in &words[1..] {
        if we - seg_start > width {
            result.push((seg_start, seg_end));
            seg_start = ws;
            seg_end = we;
        } else {
            seg_end = we;
        }
    }
    result.push((seg_start, seg_end));
    result
}

/// Wraps `text` to `width` columns and splits the result into whole-screen
/// pages of `height` lines each, so a page boundary never cuts a line in
/// half -- navigation moves a full screen at a time, like a book.
fn layout_pages(text: &str, width: usize, height: usize) -> Vec<Vec<WrappedLine>> {
    let width = width.max(10);
    let height = height.max(1);

    let mut lines: Vec<WrappedLine> = Vec::new();
    for (src_line, raw_line) in text.split('\n').enumerate() {
        if raw_line.is_empty() {
            lines.push(WrappedLine {
                src_line,
                col_start: 0,
                col_end: 0,
                text: String::new(),
            });
        } else {
            for (col_start, col_end) in wrap_line(raw_line, width) {
                lines.push(WrappedLine {
                    src_line,
                    col_start,
                    col_end,
                    text: raw_line[col_start..col_end].to_string(),
                });
            }
        }
    }
    if lines.is_empty() {
        lines.push(WrappedLine {
            src_line: 0,
            col_start: 0,
            col_end: 0,
            text: String::new(),
        });
    }

    lines
        .chunks(height)
        .map(|c| {
            c.iter()
                .map(|l| WrappedLine {
                    src_line: l.src_line,
                    col_start: l.col_start,
                    col_end: l.col_end,
                    text: l.text.clone(),
                })
                .collect()
        })
        .collect()
}

struct App {
    client: reqwest::blocking::Client,
    history: Vec<Url>,
    hist_pos: usize,
    page: Page,
    pages: Vec<Vec<WrappedLine>>,
    page_index: usize,
    layout_dims: (u16, u16),
    mode: Mode,
    input: String,
    link_cursor: usize,
    /// Digits typed so far for a "jump to link N" command.
    pending: String,
    status: String,
    /// Index into `page.headings` currently selected in the TOC overlay.
    toc_cursor: usize,
    /// Last query searched for with `/`, and the source line it was last
    /// found on, so a repeated search resumes after that match.
    search_query: String,
    search_last_line: Option<usize>,
}

impl App {
    fn navigate(&mut self, url: Url) {
        match fetch(&self.client, &url) {
            Ok(page) => {
                self.history.truncate(self.hist_pos + 1);
                self.history.push(url);
                self.hist_pos = self.history.len() - 1;
                self.page = page;
                self.pages.clear();
                self.page_index = 0;
                self.link_cursor = 0;
                self.pending.clear();
                self.status.clear();
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    fn back(&mut self) {
        if self.hist_pos == 0 {
            return;
        }
        self.hist_pos -= 1;
        let url = self.history[self.hist_pos].clone();
        match fetch(&self.client, &url) {
            Ok(page) => {
                self.page = page;
                self.pages.clear();
                self.page_index = 0;
                self.link_cursor = 0;
                self.pending.clear();
                self.status.clear();
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    /// Re-fetches the page ahead in history, if `back` was used previously
    /// and no new navigation has overwritten it.
    fn forward(&mut self) {
        if self.hist_pos + 1 >= self.history.len() {
            self.status = "no forward history".to_string();
            return;
        }
        self.hist_pos += 1;
        let url = self.history[self.hist_pos].clone();
        match fetch(&self.client, &url) {
            Ok(page) => {
                self.page = page;
                self.pages.clear();
                self.page_index = 0;
                self.link_cursor = 0;
                self.pending.clear();
                self.status.clear();
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    /// Follows the link currently selected by the cursor in the links pane.
    /// Non-http(s) schemes (mailto/tel/javascript/...) aren't navigable, so
    /// they're reported instead of attempted. A same-document `#fragment`
    /// link jumps to the matching `id` on the current page rather than
    /// re-fetching it.
    fn follow_selected(&mut self) {
        let links = self.links_on_current_page();
        let Some(link) = links.get(self.link_cursor) else {
            self.status = "no link selected".to_string();
            return;
        };
        let href = link.href.clone();
        let url = match Url::parse(&href) {
            Ok(url) => url,
            Err(e) => {
                self.status = format!("bad link url: {e}");
                return;
            }
        };

        if url.scheme() != "http" && url.scheme() != "https" {
            self.status = format!("cannot open {}: links", url.scheme());
            return;
        }

        let current = &self.history[self.hist_pos];
        let same_document = url.as_str().split('#').next() == current.as_str().split('#').next();
        if same_document {
            if let Some(fragment) = url.fragment() {
                if let Some(&line) = self.page.anchors.get(fragment) {
                    self.jump_to_line(line);
                    self.status.clear();
                    return;
                }
                self.status = format!("no anchor #{fragment} on this page");
                return;
            }
        }

        self.navigate(url);
    }

    /// Finds `query` (case-insensitive) in the page text, starting just
    /// after the last match if this is a repeat of the same search, and
    /// jumps to it. Wraps around to the top if nothing is found after the
    /// last match.
    fn search(&mut self, query: &str) {
        let query = query.to_lowercase();
        if query.is_empty() {
            return;
        }
        let lines: Vec<&str> = self.page.text.split('\n').collect();
        if lines.is_empty() {
            self.status = format!("\"{query}\" not found");
            return;
        }
        let start = if self.search_query == query {
            self.search_last_line.map_or(0, |l| l + 1)
        } else {
            0
        };
        let found = (0..lines.len())
            .map(|offset| (start + offset) % lines.len())
            .find(|&idx| lines[idx].to_lowercase().contains(&query));

        self.search_query = query.clone();
        match found {
            Some(line) => {
                self.search_last_line = Some(line);
                self.jump_to_line(line);
                self.status = format!("found \"{query}\"");
            }
            None => {
                self.search_last_line = None;
                self.status = format!("\"{query}\" not found");
            }
        }
    }

    /// Jumps to whichever paginated screen contains the given source line
    /// (as recorded on a `HeadingInfo`), for the TOC overlay.
    fn jump_to_line(&mut self, line: usize) {
        if let Some(index) = self.pages.iter().position(|p| {
            p.first().map_or(false, |first| first.src_line <= line)
                && p.last().map_or(false, |last| last.src_line >= line)
        }) {
            self.page_index = index;
            self.link_cursor = 0;
        }
    }

    /// Recomputes `pages` if the render area changed size or a new page was
    /// loaded.
    fn ensure_layout(&mut self, width: u16, height: u16) {
        if self.layout_dims != (width, height) || self.pages.is_empty() {
            // Remember where we were reading (by source line, not page
            // index) so a terminal resize re-paginates without losing the
            // reader's place, even though page boundaries shift.
            let current_line = self
                .pages
                .get(self.page_index)
                .and_then(|p| p.first())
                .map(|l| l.src_line);

            self.pages = layout_pages(&self.page.text, width as usize, height as usize);
            self.layout_dims = (width, height);

            self.page_index = current_line
                .and_then(|line| {
                    self.pages.iter().position(|p| {
                        p.first().map_or(false, |first| first.src_line <= line)
                            && p.last().map_or(false, |last| last.src_line >= line)
                    })
                })
                .unwrap_or_else(|| self.page_index.min(self.pages.len().saturating_sub(1)));
        }
    }

    /// The link rows (links plus group separators) that fall on the
    /// current page, in document order, ready for the side pane. A
    /// separator is only kept when it sits between two links that both
    /// survived the page filter -- otherwise a group that's entirely off
    /// this page would leave a dangling separator behind.
    fn link_rows_on_current_page(&self) -> Vec<LinkRow> {
        let Some(lines) = self.pages.get(self.page_index) else {
            return Vec::new();
        };
        let Some(lo) = lines.first().map(|l| l.src_line) else {
            return Vec::new();
        };
        let hi = lines.last().map(|l| l.src_line).unwrap_or(lo);

        let mut rows = Vec::new();
        let mut pending_separator = false;
        for row in &self.page.link_rows {
            match row {
                LinkRow::Separator => pending_separator = true,
                LinkRow::Link(idx) => {
                    let link = &self.page.links[*idx];
                    if link.line < lo || link.line > hi {
                        continue;
                    }
                    if pending_separator && !rows.is_empty() {
                        rows.push(LinkRow::Separator);
                    }
                    pending_separator = false;
                    rows.push(LinkRow::Link(*idx));
                }
            }
        }
        rows
    }

    /// Just the links (no separators) that fall on the current page, in
    /// document order -- the sequence the cursor moves through.
    fn links_on_current_page(&self) -> Vec<&LinkInfo> {
        self.link_rows_on_current_page()
            .into_iter()
            .filter_map(|row| match row {
                LinkRow::Link(idx) => Some(&self.page.links[idx]),
                LinkRow::Separator => None,
            })
            .collect()
    }
}

fn ui(f: &mut Frame, app: &mut App) {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span, Text};

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(f.area());

    let bottom_bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    let content_area = rows[0];

    let inner_width = content_area.width.saturating_sub(2);
    let inner_height = content_area.height.saturating_sub(2);
    let total_pages = {
        app.ensure_layout(inner_width, inner_height);
        app.pages.len().max(1)
    };

    let link_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);
    let selected_link_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    // Which link the cursor is currently on, identified by its exact
    // (line, col_start, col_end) so it can be picked out of the per-line
    // overlap scan below and highlighted in place -- no side pane needed to
    // see which link is selected.
    let current_page_links = app.links_on_current_page();
    let selected_link = current_page_links
        .get(app.link_cursor)
        .map(|l| (l.line, l.col_start, l.col_end));
    let link_index_by_range: std::collections::HashMap<(usize, usize, usize), usize> =
        current_page_links
            .iter()
            .enumerate()
            .map(|(i, l)| ((l.line, l.col_start, l.col_end), i))
            .collect();
    let number_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);
    let mut numbered = vec![false; current_page_links.len()];

    // Headings render in bold so the article's structure is visible without
    // opening the table-of-contents overlay (`t`).
    let heading_lines: std::collections::HashMap<usize, u8> = app
        .page
        .headings
        .iter()
        .map(|h| (h.line, h.level))
        .collect();

    // Build the content pane, coloring the byte ranges that are links so
    // they're visible in place without cluttering the text with markers.
    let content_lines: Vec<Line> = app.pages[app.page_index]
        .iter()
        .map(|wl| {
            let base_style = if heading_lines.contains_key(&wl.src_line) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let mut spans = Vec::new();
            let mut pos = wl.col_start;
            let mut overlaps: Vec<(usize, usize, bool, (usize, usize, usize))> = app
                .page
                .links
                .iter()
                .filter(|link| {
                    link.line == wl.src_line && link.col_start < wl.col_end && link.col_end > wl.col_start
                })
                .map(|link| {
                    let identity = (link.line, link.col_start, link.col_end);
                    let is_selected = selected_link == Some(identity);
                    (link.col_start.max(wl.col_start), link.col_end.min(wl.col_end), is_selected, identity)
                })
                .collect();
            overlaps.sort_by_key(|&(s, _, _, _)| s);
            for (s, e, is_selected, identity) in overlaps {
                if s > pos {
                    spans.push(Span::styled(
                        wl.text[pos - wl.col_start..s - wl.col_start].to_string(),
                        base_style,
                    ));
                }
                let style = if is_selected {
                    selected_link_style
                } else {
                    link_style.patch(base_style)
                };
                spans.push(Span::styled(
                    wl.text[s - wl.col_start..e - wl.col_start].to_string(),
                    style,
                ));
                if let Some(&idx) = link_index_by_range.get(&identity) {
                    if !numbered[idx] {
                        numbered[idx] = true;
                        spans.push(Span::styled(format!("[{}]", idx + 1), number_style));
                    }
                }
                pos = e;
            }
            if pos < wl.col_end {
                spans.push(Span::styled(
                    wl.text[pos - wl.col_start..].to_string(),
                    base_style,
                ));
            }
            Line::from(spans)
        })
        .collect();

    let title_text = if app.page.title.is_empty() {
        "(untitled)".to_string()
    } else {
        app.page.title.clone()
    };
    let block_title = format!(
        " {} — {}  [page {}/{}] ",
        title_text,
        app.history[app.hist_pos],
        app.page_index + 1,
        total_pages
    );
    let block = Block::default().borders(Borders::ALL).title(block_title);

    let paragraph = Paragraph::new(Text::from(content_lines)).block(block);
    f.render_widget(paragraph, content_area);

    let total_links = app.links_on_current_page().len();
    let link_cursor = app.link_cursor.min(total_links.saturating_sub(1));

    if matches!(app.mode, Mode::Toc) {
        let popup_width = (rows[0].width * 5 / 10).clamp(20.min(rows[0].width), rows[0].width);
        let popup_height = (rows[0].height * 7 / 10).clamp(10.min(rows[0].height), rows[0].height);
        let toc_area = Rect {
            x: rows[0].x + (rows[0].width.saturating_sub(popup_width)) / 2,
            y: rows[0].y + (rows[0].height.saturating_sub(popup_height)) / 2,
            width: popup_width,
            height: popup_height,
        };
        let toc_block = Block::default().borders(Borders::ALL).title(" contents ");
        let mut toc_lines: Vec<Line> = Vec::new();
        let mut highlight_row = 0usize;
        if app.page.headings.is_empty() {
            toc_lines.push(Line::raw("(no headings on this page)"));
        } else {
            for (i, h) in app.page.headings.iter().enumerate() {
                let indent = "  ".repeat((h.level as usize).saturating_sub(1));
                let text = format!("{indent}{}", h.label);
                if i == app.toc_cursor {
                    highlight_row = toc_lines.len();
                    toc_lines.push(Line::from(Span::styled(
                        text,
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )));
                } else {
                    toc_lines.push(Line::raw(text));
                }
            }
        }
        let visible_rows = toc_area.height.saturating_sub(2) as usize;
        let scroll = if visible_rows == 0 {
            0
        } else {
            highlight_row
                .saturating_sub(visible_rows - 1)
                .min(toc_lines.len().saturating_sub(visible_rows)) as u16
        };
        let toc_paragraph = Paragraph::new(toc_lines)
            .block(toc_block)
            .scroll((scroll, 0));
        f.render_widget(Clear, toc_area);
        f.render_widget(toc_paragraph, toc_area);
    }

    let shortcuts_text = match app.mode {
        Mode::UrlEntry => "Enter: go  Esc: cancel".to_string(),
        Mode::Toc => "↑/↓ select  Enter: jump  Esc: close".to_string(),
        Mode::Search => "type query  Enter: find  Esc: cancel".to_string(),
        Mode::Normal => {
            "←/→ page  g/G first/last  ↑/↓ select  <N> Enter: jump  Enter: follow  b/f: back/fwd  u: url  /: find  t: contents  q: quit"
                .to_string()
        }
    };
    let shortcuts_style = Style::default().fg(Color::DarkGray);
    f.render_widget(
        Paragraph::new(shortcuts_text).style(shortcuts_style),
        bottom_bar[0],
    );

    let status_text = match app.mode {
        Mode::UrlEntry => format!("URL: {}_", app.input),
        Mode::Toc => String::new(),
        Mode::Search => format!("find: {}_", app.input),
        Mode::Normal => {
            if !app.pending.is_empty() {
                format!("go to link: {}_", app.pending)
            } else if !app.status.is_empty() {
                app.status.clone()
            } else if let Some(link) = app.links_on_current_page().get(link_cursor) {
                link.href.clone()
            } else {
                String::new()
            }
        }
    };
    f.render_widget(Paragraph::new(status_text), bottom_bar[1]);
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        match event::read()? {
            Event::Key(key) => {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.mode {
                Mode::Normal => match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Right | KeyCode::PageDown | KeyCode::Char('n') => {
                        if app.page_index + 1 < app.pages.len() {
                            app.page_index += 1;
                            app.link_cursor = 0;
                        }
                    }
                    KeyCode::Left | KeyCode::PageUp | KeyCode::Char('p') => {
                        if app.page_index > 0 {
                            app.page_index -= 1;
                            app.link_cursor = 0;
                        }
                    }
                    KeyCode::Char('g') => {
                        app.page_index = 0;
                        app.link_cursor = 0;
                    }
                    KeyCode::Char('G') => {
                        app.page_index = app.pages.len().saturating_sub(1);
                        app.link_cursor = 0;
                    }
                    KeyCode::Up => {
                        app.link_cursor = app.link_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let len = app.links_on_current_page().len();
                        if app.link_cursor + 1 < len {
                            app.link_cursor += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if app.pending.is_empty() {
                            if let Some(href) =
                                app.links_on_current_page().get(app.link_cursor).map(|l| l.href.clone())
                            {
                                app.status = format!("loading {href} ...");
                                terminal.draw(|f| ui(f, app))?;
                            }
                            app.follow_selected();
                        } else if let Ok(n) = app.pending.parse::<usize>() {
                            let len = app.links_on_current_page().len();
                            if n >= 1 && n <= len {
                                app.link_cursor = n - 1;
                                app.pending.clear();
                                if let Some(href) =
                                    app.links_on_current_page().get(app.link_cursor).map(|l| l.href.clone())
                                {
                                    app.status = format!("loading {href} ...");
                                    terminal.draw(|f| ui(f, app))?;
                                }
                                app.follow_selected();
                            } else {
                                app.status = format!("no link {n} on this page");
                                app.pending.clear();
                            }
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        app.status.clear();
                        app.pending.push(c);
                    }
                    KeyCode::Backspace => {
                        app.pending.pop();
                    }
                    KeyCode::Esc => app.pending.clear(),
                    KeyCode::Char('u') => {
                        app.mode = Mode::UrlEntry;
                        app.input.clear();
                        app.status.clear();
                    }
                    KeyCode::Char('b') => {
                        if app.hist_pos > 0 {
                            let url = app.history[app.hist_pos - 1].to_string();
                            app.status = format!("loading {url} ...");
                            terminal.draw(|f| ui(f, app))?;
                        }
                        app.back();
                    }
                    KeyCode::Char('f') => {
                        if app.hist_pos + 1 < app.history.len() {
                            let url = app.history[app.hist_pos + 1].to_string();
                            app.status = format!("loading {url} ...");
                            terminal.draw(|f| ui(f, app))?;
                        }
                        app.forward();
                    }
                    KeyCode::Char('/') => {
                        app.mode = Mode::Search;
                        app.input.clear();
                    }
                    KeyCode::Char('t') => {
                        app.toc_cursor = 0;
                        app.mode = Mode::Toc;
                    }
                    _ => {}
                },
                Mode::Toc => match key.code {
                    KeyCode::Up => {
                        app.toc_cursor = app.toc_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if app.toc_cursor + 1 < app.page.headings.len() {
                            app.toc_cursor += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(h) = app.page.headings.get(app.toc_cursor) {
                            let line = h.line;
                            app.jump_to_line(line);
                        }
                        app.mode = Mode::Normal;
                    }
                    KeyCode::Esc | KeyCode::Char('t') => app.mode = Mode::Normal,
                    _ => {}
                },
                Mode::Search => match key.code {
                    KeyCode::Enter => {
                        let query = app.input.trim().to_string();
                        app.mode = Mode::Normal;
                        app.search(&query);
                    }
                    KeyCode::Esc => app.mode = Mode::Normal,
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => app.input.push(c),
                    _ => {}
                },
                Mode::UrlEntry => match key.code {
                    KeyCode::Enter => {
                        let input = app.input.trim().to_string();
                        app.mode = Mode::Normal;
                        if !input.is_empty() {
                            let parsed = Url::parse(&input)
                                .or_else(|_| Url::parse(&format!("https://{input}")));
                            match parsed {
                                Ok(url) => {
                                    app.status = format!("loading {url} ...");
                                    terminal.draw(|f| ui(f, app))?;
                                    app.navigate(url);
                                }
                                Err(e) => app.status = format!("invalid url: {e}"),
                            }
                        }
                    }
                    KeyCode::Esc => app.mode = Mode::Normal,
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => app.input.push(c),
                    _ => {}
                },
            }
            }
            _ => {}
        }
    }
}

fn main() -> ExitCode {
    let url_arg = match env::args().nth(1) {
        Some(u) => u,
        None => {
            eprintln!("usage: termbrowse <url>");
            return ExitCode::FAILURE;
        }
    };

    let start_url = match Url::parse(&url_arg) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("invalid url {url_arg}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let client = match reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; termbrowse/0.1)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to build http client: {e}");
            return ExitCode::FAILURE;
        }
    };

    let page = match fetch(&client, &start_url) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to fetch {start_url}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut app = App {
        client,
        history: vec![start_url],
        hist_pos: 0,
        page,
        pages: Vec::new(),
        page_index: 0,
        layout_dims: (0, 0),
        mode: Mode::Normal,
        input: String::new(),
        link_cursor: 0,
        pending: String::new(),
        status: String::new(),
        toc_cursor: 0,
        search_query: String::new(),
        search_last_line: None,
    };

    if let Err(e) = enable_raw_mode() {
        eprintln!("failed to enable raw mode: {e}");
        return ExitCode::FAILURE;
    }
    let mut stdout = io::stdout();
    if let Err(e) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        eprintln!("failed to enter alternate screen: {e}");
        return ExitCode::FAILURE;
    }
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = match Terminal::new(backend) {
        Ok(t) => t,
        Err(e) => {
            let _ = disable_raw_mode();
            eprintln!("failed to create terminal: {e}");
            return ExitCode::FAILURE;
        }
    };

    let result = run(&mut terminal, &mut app);

    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();

    if let Err(e) = result {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
