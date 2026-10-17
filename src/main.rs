use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal};
use scraper::{Html, Selector};
use std::env;
use std::io::{self, Stdout};
use std::process::ExitCode;
use url::Url;

mod cookies;
mod doc;
mod feed;
mod layout;
mod trail;
mod view;

use doc::FieldKind;
use trail::TrailNode;
use layout::Target;

struct Page {
    title: String,
    doc: doc::Doc,
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

/// Rewrites a handful of known JS-heavy hosts to a lite/legacy endpoint that
/// still serves usable server-rendered HTML -- e.g. Reddit's SPA redesign
/// vs. old.reddit.com, or DuckDuckGo's JS search UI vs. its dedicated
/// no-JS `html.duckduckgo.com` endpoint. Anything not matched here passes
/// through unchanged.
fn rewrite_url(url: &Url) -> Url {
    let host = url.host_str().unwrap_or("");
    match host {
        "reddit.com" | "www.reddit.com" => {
            let mut rewritten = url.clone();
            let _ = rewritten.set_host(Some("old.reddit.com"));
            rewritten
        }
        "duckduckgo.com" | "www.duckduckgo.com" => {
            let mut rewritten = url.clone();
            let _ = rewritten.set_host(Some("html.duckduckgo.com"));
            rewritten.set_path("/html/");
            rewritten
        }
        _ => url.clone(),
    }
}

/// Heuristic for the URL bar: does this look like a URL/domain the user
/// wants to go to, or a search term? A dot with no whitespace reads as a
/// domain (`example.com`, `sub.example.co.uk`); anything else -- plain
/// words, multiple words -- is treated as a search query instead.
fn looks_like_url(input: &str) -> bool {
    if input.chars().any(char::is_whitespace) {
        return false;
    }
    if input.starts_with("http://") || input.starts_with("https://") {
        return true;
    }
    let host = input.split(['/', ':', '?', '#']).next().unwrap_or(input);
    host == "localhost" || host.contains('.')
}

/// Builds a search-results URL for a plain query typed into the URL bar,
/// using DuckDuckGo's no-JS HTML endpoint (already gbtr's routing target
/// for duckduckgo.com via `rewrite_url`) as the default engine.
fn search_url(query: &str) -> Url {
    let mut url = Url::parse("https://html.duckduckgo.com/html/").expect("static url");
    url.query_pairs_mut().append_pair("q", query);
    url
}

/// Built-in start page, shown when gbtr is launched without a URL.
const WELCOME_HTML: &str = "<title>gbtr</title><h1>gbtr</h1>\
<p>A terminal web browser: no JavaScript, minimal cookies, keyboard first.</p>\
<ul><li>u: type a URL or a search query</li>\
<li>c: open a new tab</li>\
<li>Enter / arrows: follow links, ←/→ change page</li>\
<li>/: find, t: contents, q: quit</li></ul>";

/// Form fields as sent in a POST body.
type FormBody = Vec<(String, String)>;

fn fetch(client: &reqwest::blocking::Client, url: &Url) -> Result<Page, String> {
    fetch_with(client, url, None)
}

/// Fetches `url`, with a POST of `form` if given and a GET otherwise.
fn fetch_with(client: &reqwest::blocking::Client, url: &Url, form: Option<&FormBody>) -> Result<Page, String> {
    let body = if url.scheme() == "about" {
        WELCOME_HTML.to_string()
    } else {
        let body = fetch_body(client, url, form)?;
        feed::to_html(&body).unwrap_or(body)
    };

    let document = Html::parse_document(&body);

    let title_selector = Selector::parse("title").unwrap();
    let title = document
        .select(&title_selector)
        .next()
        .map(|el| el.text().collect::<String>())
        .unwrap_or_default();

    let doc = doc::build(document.tree.root(), url);

    Ok(Page { title, doc })
}

fn fetch_body(client: &reqwest::blocking::Client, url: &Url, form: Option<&FormBody>) -> Result<String, String> {
    cookies::jar().set_top(url);
    let send = || {
        match form {
            Some(pairs) => client.post(url.clone()).form(pairs),
            None => client.get(url.clone()),
        }
            .timeout(std::time::Duration::from_secs(20))
            .send()
            .map_err(|e| e.to_string())
    };
    let mut response = send()?;
    // Some sites (eBay, Yahoo) answer the first cookieless request with an
    // error but set a cookie on the way (possibly on a redirect hop); the
    // cookie jar now holds it, so one retry gets the real page. Never for a
    // POST, which could submit twice.
    let status = response.status();
    if form.is_none()
        && (status == reqwest::StatusCode::FORBIDDEN || status.is_server_error())
        && let Ok(retry) = send()
    {
        response = retry;
    }

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
    Ok(body.into_owned())
}

// ---------- TUI ----------

fn text_style(t: doc::TextStyle) -> ratatui::style::Style {
    use ratatui::style::{Color, Modifier, Style};
    let mut style = Style::default();
    if t.heading > 0 {
        style = style.fg(Color::Yellow).add_modifier(Modifier::BOLD);
    }
    if t.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if t.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if t.code {
        style = style.fg(Color::Green);
    }
    if t.dim {
        style = style.fg(Color::DarkGray);
    }
    style
}

/// Widest the page text gets before it's centered instead of stretched.
const MAX_TEXT_COLS: u16 = 96;
/// Terminal width from which the outline panel appears beside the page.
const OUTLINE_MIN_TOTAL: u16 = 140;
const OUTLINE_W: u16 = 36;
/// Terminal width from which the left panel (trail or site nav) appears.
const SIDE_MIN_TOTAL: u16 = 176;
const SIDE_W: u16 = 36;

enum Mode {
    Normal,
    UrlEntry,
    Toc,
    Search,
    /// Keyboard focus is on the links pane (side pane or overlay).
    Pane,
    /// Typing into the form field `App::field_edit`.
    Field,
    /// Keyboard focus is on the left panel (trail or site nav).
    Side,
}

/// All state specific to one browsing tab -- its own history, current page,
/// pagination, and per-page UI cursors. `App` holds a collection of these
/// plus the shared HTTP client and global (not per-tab) input mode.
struct Tab {
    history: Vec<Url>,
    hist_pos: usize,
    page: Page,
    laid: layout::Layout,
    page_index: usize,
    layout_dims: (u16, u16),
    link_cursor: usize,
    /// Digits typed so far for a "jump to link N" command.
    pending: String,
    status: String,
    /// Index into `page.headings` currently selected in the TOC overlay.
    toc_cursor: usize,
    /// Last query searched for with `/`, and the index of the text block it
    /// was last found in, so a repeated search resumes after that match.
    search_query: String,
    search_last_hit: Option<usize>,
    /// Whether `laid` was built without menu/sidebar/footer content.
    laid_hid_chrome: bool,
    /// Selection within `pane_rows()`.
    pane_cursor: usize,
    /// Expanded nodes of the links overlay: `(region, "")` for a region,
    /// `(region, name)` for a section within it.
    pane_expanded: std::collections::HashSet<(doc::Region, String)>,
    /// View picked with `v` for the site shown in this tab; it applies only
    /// while the tab stays on that host.
    view_pick: Option<(String, Option<view::View>)>,
    /// Pages visited in all tabs as one tree (kept on disk between runs),
    /// and the node this tab shows.
    trail: SharedTrail,
    trail_at: usize,
    /// Selection within `side_rows()` while the left panel has focus.
    side_cursor: usize,
}

/// The trail every tab adds to.
type SharedTrail = std::rc::Rc<std::cell::RefCell<Vec<TrailNode>>>;

/// What the left panel shows; `s` cycles through these.
#[derive(Clone, Copy, PartialEq)]
enum Side {
    /// The pages you've visited, as a tree.
    Trail,
    Off,
}

enum SideRow {
    /// Trail node index and depth.
    Trail(usize, usize),
}

struct PaneGroup {
    region: doc::Region,
    /// Sections by heading, in page order; the unnamed one (if any) first.
    sections: Vec<(String, Vec<usize>)>,
}

#[derive(Clone)]
enum PaneRow {
    /// Region and its total link count.
    Header(doc::Region, usize),
    /// Named section within a region, and its link count.
    Section(doc::Region, String, usize),
    /// Link id and indent depth.
    Link(usize, u8),
}

impl Tab {
    fn new(client: &reqwest::blocking::Client, url: Url, trail: SharedTrail) -> Result<Tab, String> {
        let url = rewrite_url(&url);
        let page = fetch(client, &url).map_err(|e| e.to_string())?;
        Ok(Tab::with_page(url, page, trail))
    }

    /// A tab showing `page`, which starts a new root in the shared trail.
    fn with_page(url: Url, page: Page, trail: SharedTrail) -> Tab {
        let trail_at = {
            let mut nodes = trail.borrow_mut();
            nodes.push(TrailNode { url: url.clone(), title: page.title.clone(), parent: None });
            nodes.len() - 1
        };
        let mut tab = Tab {
            trail,
            trail_at,
            side_cursor: 0,
            history: vec![url],
            hist_pos: 0,
            page,
            laid: layout::Layout::default(),
            page_index: 0,
            layout_dims: (0, 0),
            link_cursor: 0,
            pending: String::new(),
            status: String::new(),
            toc_cursor: 0,
            search_query: String::new(),
            search_last_hit: None,
            laid_hid_chrome: false,
            pane_cursor: 0,
            pane_expanded: Default::default(),
            view_pick: None,
        };
        tab.save_trail();
        tab
    }

    /// The visited pages as a tree, in the order shown by the trail panel:
    /// `(node index, depth)`, children after their parent.
    fn trail_rows(&self) -> Vec<(usize, usize)> {
        let trail = self.trail.borrow();
        let mut children: Vec<Vec<usize>> = vec![Vec::new(); trail.len()];
        let mut roots = Vec::new();
        for (i, n) in trail.iter().enumerate() {
            match n.parent {
                Some(p) => children[p].push(i),
                None => roots.push(i),
            }
        }
        let mut rows = Vec::new();
        let mut stack: Vec<(usize, usize)> = roots.into_iter().rev().map(|i| (i, 0)).collect();
        while let Some((i, depth)) = stack.pop() {
            rows.push((i, depth));
            stack.extend(children[i].iter().rev().map(|&c| (c, depth + 1)));
        }
        rows
    }

    /// What the left panel lists in `side` mode.
    fn side_rows(&self, side: Side) -> Vec<SideRow> {
        match side {
            Side::Trail => self.trail_rows().into_iter().map(|(idx, depth)| SideRow::Trail(idx, depth)).collect(),
            Side::Off => Vec::new(),
        }
    }

    fn save_trail(&mut self) {
        if let Err(e) = trail::save(&self.trail.borrow()) {
            self.status = format!("could not save trail: {e}");
        }
    }

    /// After back/forward, points the trail at the node for the current URL.
    fn sync_trail(&mut self) {
        let url = &self.history[self.hist_pos];
        let found = self.trail.borrow().iter().rposition(|n| &n.url == url);
        if let Some(i) = found {
            self.trail_at = i;
        }
    }

    /// Opens a node of the trail without growing the tree.
    fn jump_trail(&mut self, client: &reqwest::blocking::Client, idx: usize) {
        let Some(url) = self.trail.borrow().get(idx).map(|n| n.url.clone()) else { return };
        match fetch(client, &url) {
            Ok(page) => {
                self.history.truncate(self.hist_pos + 1);
                self.history.push(url);
                self.hist_pos = self.history.len() - 1;
                self.page = page;
                self.laid.lines.clear();
                self.page_index = 0;
                self.link_cursor = 0;
                self.pending.clear();
                self.status.clear();
                self.trail_at = idx;
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    /// The view this tab shows: a pick made with `v` on this host, else the
    /// site's saved view, else compact. The bool says whether it was chosen
    /// (true) or is the default (false).
    fn effective_view(&self, saved: &view::SiteViews) -> (view::View, bool) {
        let host = view::SiteViews::host_of(&self.history[self.hist_pos]);
        if let Some((h, v)) = &self.view_pick
            && Some(h) == host.as_ref()
        {
            // `None` is an explicit "default", which also overrides a saved view.
            return match v {
                Some(v) => (*v, true),
                None => (view::View::Compact, false),
            };
        }
        if let Some(v) = host.as_deref().and_then(|h| saved.get(h)) {
            return (v, true);
        }
        (view::View::Compact, false)
    }

    fn navigate(&mut self, client: &reqwest::blocking::Client, url: Url) {
        self.navigate_with(client, url, None);
    }

    /// Like `navigate`, but POSTs `form` when given. History records only
    /// the URL, so going back to a POST result re-fetches it with GET.
    fn navigate_with(&mut self, client: &reqwest::blocking::Client, url: Url, form: Option<FormBody>) {
        let url = if form.is_some() { url } else { rewrite_url(&url) };
        match fetch_with(client, &url, form.as_ref()) {
            Ok(page) => {
                self.history.truncate(self.hist_pos + 1);
                self.history.push(url);
                self.hist_pos = self.history.len() - 1;
                self.trail_at = {
                    let mut nodes = self.trail.borrow_mut();
                    nodes.push(TrailNode {
                        url: self.history[self.hist_pos].clone(),
                        title: page.title.clone(),
                        parent: Some(self.trail_at),
                    });
                    nodes.len() - 1
                };
                self.page = page;
                self.laid.lines.clear();
                self.page_index = 0;
                self.link_cursor = 0;
                self.pending.clear();
                self.status.clear();
                self.save_trail();
            }
            Err(e) => self.status = format!("error: {e}"),
        }
    }

    fn back(&mut self, client: &reqwest::blocking::Client) {
        if self.hist_pos == 0 {
            return;
        }
        self.hist_pos -= 1;
        let url = self.history[self.hist_pos].clone();
        match fetch(client, &url) {
            Ok(page) => {
                self.sync_trail();
                self.page = page;
                self.laid.lines.clear();
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
    fn forward(&mut self, client: &reqwest::blocking::Client) {
        if self.hist_pos + 1 >= self.history.len() {
            self.status = "no forward history".to_string();
            return;
        }
        self.hist_pos += 1;
        let url = self.history[self.hist_pos].clone();
        match fetch(client, &url) {
            Ok(page) => {
                self.sync_trail();
                self.page = page;
                self.laid.lines.clear();
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
    fn follow_selected(&mut self, client: &reqwest::blocking::Client) {
        let Some(href) = self.selected_href() else {
            self.status = "no link selected".to_string();
            return;
        };
        self.follow_href(client, &href);
    }

    fn follow_href(&mut self, client: &reqwest::blocking::Client, href: &str) {
        let url = match Url::parse(href) {
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
        if same_document && let Some(fragment) = url.fragment() {
            if let Some(&line) = self.laid.anchors.get(fragment) {
                self.jump_to_line(line);
                self.status.clear();
                return;
            }
            self.status = format!("no anchor #{fragment} on this page");
            return;
        }

        self.navigate(client, url);
    }

    /// Finds `query` (case-insensitive) in the page text, starting just
    /// after the last match if this is a repeat of the same search, and
    /// jumps to it. Wrapped lines of one paragraph are joined first so a
    /// phrase split across a line break still matches. Wraps around to the
    /// top if nothing is found after the last match.
    fn search(&mut self, query: &str) {
        let query = query.to_lowercase();
        if query.is_empty() {
            return;
        }
        // (group id, first line index, joined text)
        let mut blocks: Vec<(usize, usize, String)> = Vec::new();
        for (i, line) in self.laid.lines.iter().enumerate() {
            let text: String = line.segs.iter().map(|s| s.text.as_str()).collect();
            match blocks.last_mut() {
                Some((group, _, joined)) if *group == line.group => {
                    joined.push(' ');
                    joined.push_str(&text);
                }
                _ => blocks.push((line.group, i, text)),
            }
        }
        if blocks.is_empty() {
            self.status = format!("\"{query}\" not found");
            return;
        }
        let start = if self.search_query == query {
            self.search_last_hit.map_or(0, |l| l + 1)
        } else {
            0
        };
        let found = (0..blocks.len())
            .map(|offset| (start + offset) % blocks.len())
            .find(|&idx| blocks[idx].2.to_lowercase().contains(&query));

        self.search_query = query.clone();
        match found {
            Some(idx) => {
                self.search_last_hit = Some(idx);
                let line = blocks[idx].1;
                self.jump_to_line(line);
                self.status = format!("found \"{query}\"");
            }
            None => {
                self.search_last_hit = None;
                self.status = format!("\"{query}\" not found");
            }
        }
    }

    fn page_height(&self) -> usize {
        (self.layout_dims.1 as usize).max(1)
    }

    fn page_count(&self) -> usize {
        self.laid.lines.len().div_ceil(self.page_height()).max(1)
    }

    /// The laid-out lines that make up the current screen.
    fn current_lines(&self) -> &[layout::LaidLine] {
        let h = self.page_height();
        let len = self.laid.lines.len();
        let start = (self.page_index * h).min(len);
        &self.laid.lines[start..(start + h).min(len)]
    }

    /// Jumps to whichever screen contains the given laid-out line (a heading
    /// or anchor position), resetting the link cursor.
    fn jump_to_line(&mut self, line: usize) {
        self.page_index = (line / self.page_height()).min(self.page_count() - 1);
        self.link_cursor = 0;
    }

    /// Recomputes the layout if the render area changed size or a new page
    /// was loaded.
    fn ensure_layout(&mut self, width: u16, height: u16, hide_chrome: bool) {
        if self.layout_dims != (width, height)
            || self.laid_hid_chrome != hide_chrome
            || self.laid.lines.is_empty()
        {
            // Remember where we were reading (by paragraph group, not line
            // number) so a terminal resize re-wraps without losing the
            // reader's place, even though page boundaries shift.
            let group = self.current_lines().first().map(|l| l.group);

            self.laid = layout::layout(&self.page.doc, width as usize, hide_chrome);
            self.laid_hid_chrome = hide_chrome;
            self.layout_dims = (width, height);

            let h = self.page_height();
            self.page_index = group
                .and_then(|g| self.laid.lines.iter().position(|l| l.group == g))
                .map(|i| i / h)
                .unwrap_or_else(|| self.page_index.min(self.page_count() - 1));
        }
    }

    /// The distinct links and form fields shown on the current screen, in
    /// order of first appearance -- the sequence the cursor moves through.
    fn targets_on_current_page(&self) -> Vec<Target> {
        let mut targets = Vec::new();
        for line in self.current_lines() {
            for seg in &line.segs {
                if let Some(t) = seg.target
                    && !targets.contains(&t)
                {
                    targets.push(t);
                }
            }
        }
        targets
    }

    /// Links outside the main content (menus, sidebars, footers), one entry
    /// per distinct target, ordered by region then document order.
    /// Overlay links as a tree: region -> optional section (by heading) -> links,
    /// one entry per distinct target. Links with no section come first.
    fn pane_groups(&self) -> Vec<PaneGroup> {
        use doc::Region::*;
        let links = &self.page.doc.links;
        let mut seen = std::collections::HashSet::new();
        let mut groups = Vec::new();
        for region in [Nav, Aside, Footer, Projects, Languages] {
            let mut sections: Vec<(String, Vec<usize>)> = Vec::new();
            for (id, l) in links.iter().enumerate() {
                if l.region != region || !seen.insert(l.href.as_str()) {
                    continue;
                }
                match sections.iter_mut().find(|(g, _)| *g == l.group) {
                    Some((_, ids)) => ids.push(id),
                    None => sections.push((l.group.clone(), vec![id])),
                }
            }
            if sections.is_empty() {
                continue;
            }
            sections.sort_by_key(|(g, _)| !g.is_empty());
            groups.push(PaneGroup { region, sections });
        }
        groups
    }

    /// The rows the links overlay shows: a forest with one tree per region.
    /// Any number of regions and sections can be expanded at once.
    fn pane_rows(&self) -> Vec<PaneRow> {
        let mut rows = Vec::new();
        for group in self.pane_groups() {
            let total = group.sections.iter().map(|(_, ids)| ids.len()).sum();
            rows.push(PaneRow::Header(group.region, total));
            if !self.pane_expanded.contains(&(group.region, String::new())) {
                continue;
            }
            // A lone section adds nothing, so show its links directly.
            let flat = group.sections.len() == 1;
            for (name, ids) in group.sections {
                let nested = !flat && !name.is_empty();
                let open = !nested || self.pane_expanded.contains(&(group.region, name.clone()));
                if nested {
                    rows.push(PaneRow::Section(group.region, name, ids.len()));
                }
                if open {
                    let depth = if nested { 2 } else { 1 };
                    rows.extend(ids.into_iter().map(|id| PaneRow::Link(id, depth)));
                }
            }
        }
        rows
    }

    /// Expands or collapses a node, keeping the cursor on it.
    fn toggle_pane(&mut self, region: doc::Region, name: String) {
        let key = (region, name);
        if !self.pane_expanded.remove(&key) {
            self.pane_expanded.insert(key.clone());
        }
        let at = self.pane_rows().iter().position(|r| match r {
            PaneRow::Header(r, _) => key.1.is_empty() && *r == key.0,
            PaneRow::Section(r, n, _) => *r == key.0 && *n == key.1,
            PaneRow::Link(..) => false,
        });
        self.pane_cursor = at.unwrap_or(0);
    }

    fn pane_selected(&self) -> Option<PaneRow> {
        let rows = self.pane_rows();
        rows.get(self.pane_cursor.min(rows.len().saturating_sub(1))).cloned()
    }

    fn selected_target(&self) -> Option<Target> {
        let targets = self.targets_on_current_page();
        targets.get(self.link_cursor.min(targets.len().saturating_sub(1))).copied()
    }

    fn selected_href(&self) -> Option<String> {
        match self.selected_target() {
            Some(Target::Link(id)) => Some(self.page.doc.links[id].href.clone()),
            _ => None,
        }
    }

    /// What the status line shows for the selected target.
    fn selected_hint(&self) -> String {
        match self.selected_target() {
            Some(Target::Link(id)) => self.page.doc.links[id].href.clone(),
            Some(Target::Field(id)) => {
                let f = &self.page.doc.fields[id];
                match f.kind {
                    FieldKind::Submit => format!("submit form -> {}", self.page.doc.forms[f.form].action),
                    _ => format!("Enter: edit field \"{}\"", f.name),
                }
            }
            None => String::new(),
        }
    }

    /// Sets a field's value and forces a re-layout so the page shows it.
    fn set_field_value(&mut self, id: usize, value: String) {
        self.page.doc.fields[id].value = value;
        self.layout_dims = (0, 0);
    }

    /// What submitting `form` sends: the URL and, for a POST form, the body.
    /// `submit` is the button pressed; with `None` (Enter in a text box) the
    /// form's first button counts, as in other browsers. A password is only
    /// sent in a POST body over https, never in a URL or in the clear.
    fn form_request(&self, form: usize, submit: Option<usize>) -> Result<(Url, Option<FormBody>), String> {
        let doc = &self.page.doc;
        let post = doc.forms[form].post;
        let in_form = |f: &doc::FieldInfo| f.form == form;
        let mut url = Url::parse(&doc.forms[form].action).map_err(|e| format!("bad form action: {e}"))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(format!("cannot submit to {}: urls", url.scheme()));
        }
        if doc.fields.iter().any(|f| in_form(f) && f.kind == FieldKind::Password) {
            if !post {
                return Err("this form would put a password in the URL; not sending".to_string());
            }
            if url.scheme() != "https" {
                return Err("not sending a password over plain http".to_string());
            }
        }
        let pressed = submit.or_else(|| doc.fields.iter().position(|f| in_form(f) && f.kind == FieldKind::Submit));
        let pairs: FormBody = doc
            .fields
            .iter()
            .enumerate()
            .filter(|(i, f)| in_form(f) && !f.name.is_empty() && (f.kind != FieldKind::Submit || Some(*i) == pressed))
            .map(|(_, f)| (f.name.clone(), f.value.clone()))
            .collect();
        if post {
            return Ok((url, Some(pairs)));
        }
        if pairs.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(&pairs);
        }
        Ok((url, None))
    }
}

struct App {
    client: reqwest::blocking::Client,
    tabs: Vec<Tab>,
    active: usize,
    mode: Mode,
    input: String,
    /// Field being typed into in `Mode::Field`; `input` holds its value from
    /// before editing, for Esc to restore.
    field_edit: usize,
    /// Set by `c` (new tab) so the next `Mode::UrlEntry` submission opens a
    /// fresh tab instead of navigating the active one.
    new_tab_pending: bool,
    site_views: view::SiteViews,
    /// What the left panel shows, and whether the last frame had room for it.
    side: Side,
    side_visible: bool,
}

impl App {
    fn active_tab(&self) -> &Tab {
        &self.tabs[self.active]
    }

    fn active_tab_mut(&mut self) -> &mut Tab {
        &mut self.tabs[self.active]
    }

    fn navigate(&mut self, url: Url) {
        let client = self.client.clone();
        self.active_tab_mut().navigate(&client, url);
    }

    fn navigate_with(&mut self, url: Url, form: Option<FormBody>) {
        let client = self.client.clone();
        self.active_tab_mut().navigate_with(&client, url, form);
    }

    fn back(&mut self) {
        let client = self.client.clone();
        self.active_tab_mut().back(&client);
    }

    fn forward(&mut self) {
        let client = self.client.clone();
        self.active_tab_mut().forward(&client);
    }

    fn follow_selected(&mut self) {
        let client = self.client.clone();
        self.active_tab_mut().follow_selected(&client);
    }

    fn search(&mut self, query: &str) {
        self.active_tab_mut().search(query);
    }

    fn jump_to_line(&mut self, line: usize) {
        self.active_tab_mut().jump_to_line(line);
    }

    /// Forgets every trail node except the page each tab is showing.
    fn clear_trail(&mut self) {
        let shared = self.active_tab().trail.clone();
        let kept: Vec<TrailNode> = {
            let nodes = shared.borrow();
            self.tabs.iter().map(|t| TrailNode { parent: None, ..nodes[t.trail_at].clone() }).collect()
        };
        *shared.borrow_mut() = kept;
        for (i, t) in self.tabs.iter_mut().enumerate() {
            t.trail_at = i;
            t.side_cursor = 0;
        }
        self.active_tab_mut().save_trail();
    }

    /// Opens `url` in a brand-new tab and switches focus to it.
    fn open_tab(&mut self, url: Url) {
        let trail = self.active_tab().trail.clone();
        match Tab::new(&self.client, url, trail) {
            Ok(tab) => {
                self.tabs.push(tab);
                self.active = self.tabs.len() - 1;
            }
            Err(e) => self.active_tab_mut().status = format!("error: {e}"),
        }
    }

    /// Closes the active tab, unless it's the only one left. Focus moves to
    /// the tab before it, or stays at 0 if the first tab was closed.
    fn close_tab(&mut self) {
        if self.tabs.len() == 1 {
            self.active_tab_mut().status = "can't close the last tab".to_string();
            return;
        }
        self.tabs.remove(self.active);
        if self.active >= self.tabs.len() {
            self.active = self.tabs.len() - 1;
        }
    }

    fn next_tab(&mut self) {
        self.active = (self.active + 1) % self.tabs.len();
    }

    fn prev_tab(&mut self) {
        self.active = (self.active + self.tabs.len() - 1) % self.tabs.len();
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

    // Menus, sidebars and footers never take page space; their links live in
    // the overlay pane on demand (`l`).
    let (page_view, _) = app.active_tab().effective_view(&app.site_views);
    // On wide terminals the page's headings fill the right margin as an
    // outline, so the space beside the reading column isn't wasted.
    let outline_on = rows[0].width >= OUTLINE_MIN_TOTAL && app.active_tab().page.doc.headings.len() >= 2;
    let side_on = app.side != Side::Off && rows[0].width >= SIDE_MIN_TOTAL;
    app.side_visible = side_on;
    let mut constraints = Vec::new();
    if side_on {
        constraints.push(Constraint::Length(SIDE_W));
    }
    constraints.push(Constraint::Min(0));
    if outline_on {
        constraints.push(Constraint::Length(OUTLINE_W));
    }
    let mut split = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(rows[0])
        .to_vec()
        .into_iter();
    let side_area = if side_on { split.next() } else { None };
    let content_area = split.next().unwrap_or(rows[0]);
    let outline_area = if outline_on { split.next() } else { None };

    let inner_width = content_area.width.saturating_sub(2);
    let inner_height = content_area.height.saturating_sub(2);
    // Long lines are hard to read, so text is capped to a column and centered.
    let text_width = if page_view == view::View::Plain { inner_width } else { inner_width.min(MAX_TEXT_COLS) };
    let left_pad = ((inner_width - text_width) / 2) as usize;
    let total_pages = {
        app.active_tab_mut().ensure_layout(text_width, inner_height, true);
        app.active_tab().page_count()
    };

    let link_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);
    let selected_link_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let number_style = Style::default()
        .fg(Color::Magenta)
        .add_modifier(Modifier::BOLD);

    // The cursor's link is highlighted in place by id; every link on this
    // page also gets its jump number after its first appearance.
    let tab = app.active_tab();
    let page_targets = tab.targets_on_current_page();
    let selected_target = tab.selected_target();
    let number_of: std::collections::HashMap<Target, usize> = page_targets
        .iter()
        .enumerate()
        .map(|(i, &t)| (t, i + 1))
        .collect();
    let field_style = Style::default().fg(Color::Black).bg(Color::Gray);
    let mut numbered = std::collections::HashSet::new();

    let content_lines: Vec<Line> = tab
        .current_lines()
        .iter()
        .map(|laid_line| {
            let mut spans = Vec::new();
            if left_pad > 0 {
                spans.push(Span::raw(" ".repeat(left_pad)));
            }
            for (i, s) in laid_line.segs.iter().enumerate() {
                let mut style = text_style(s.style);
                if let Some(t) = s.target {
                    style = if selected_target == Some(t) {
                        selected_link_style
                    } else if matches!(t, Target::Field(_)) {
                        field_style.patch(style)
                    } else {
                        link_style.patch(style)
                    };
                }
                spans.push(Span::styled(s.text.clone(), style));
                if let Some(t) = s.target {
                    let ends_run = laid_line.segs.get(i + 1).is_none_or(|next| next.target != Some(t));
                    if ends_run
                        && let Some(&num) = number_of.get(&t)
                        && numbered.insert(t)
                    {
                        spans.push(Span::styled(format!("[{num}]"), number_style));
                    }
                }
            }
            Line::from(spans)
        })
        .collect();

    let site = {
        let tab = app.active_tab();
        match tab.history[tab.hist_pos].host_str() {
            Some(host) => host.to_string(),
            None if tab.page.title.is_empty() => "(untitled)".to_string(),
            None => tab.page.title.clone(),
        }
    };
    let block_title = format!(
        " tab {}/{}  page {}/{}  {} ",
        app.active + 1,
        app.tabs.len(),
        app.active_tab().page_index + 1,
        total_pages,
        page_view.label(),
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(Line::from(block_title))
        .title(Line::from(format!(" {site} ")).right_aligned());

    let paragraph = Paragraph::new(Text::from(content_lines)).block(block);
    f.render_widget(paragraph, content_area);

    if let Some(area) = side_area {
        let tab = app.active_tab();
        let rows = tab.side_rows(app.side);
        let focused = matches!(app.mode, Mode::Side);
        let cursor = tab.side_cursor.min(rows.len().saturating_sub(1));
        let room = area.width.saturating_sub(2) as usize;
        let mut lines: Vec<Line> = Vec::new();
        let mut focus_row = 0;
        for (n, row) in rows.iter().enumerate() {
            let (text, current) = match row {
                SideRow::Trail(idx, depth) => {
                    let nodes = tab.trail.borrow();
                    let node = &nodes[*idx];
                    let name = if node.title.trim().is_empty() {
                        format!("{}{}", node.url.host_str().unwrap_or(""), node.url.path())
                    } else {
                        node.title.trim().to_string()
                    };
                    // Tabs (1-based) currently showing this node.
                    let shown: Vec<String> = app
                        .tabs
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| t.trail_at == *idx)
                        .map(|(i, _)| (i + 1).to_string())
                        .collect();
                    let current = *idx == tab.trail_at;
                    let mark = if current { "● " } else if shown.is_empty() { "• " } else { "○ " };
                    let tabs = if shown.is_empty() { String::new() } else { format!(" [{}]", shown.join(",")) };
                    (format!("{}{mark}{name}{tabs}", "  ".repeat((*depth).min(6))), current)
                }
            };
            let style = if focused && n == cursor {
                selected_link_style
            } else if current {
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            if (focused && n == cursor) || (!focused && current) {
                focus_row = n;
            }
            lines.push(Line::from(Span::styled(text.chars().take(room).collect::<String>(), style)));
        }
        let title = " trail ";
        let border = if focused { Style::default().fg(Color::Cyan) } else { Style::default() };
        let visible = area.height.saturating_sub(2) as usize;
        let scroll = focus_row.saturating_sub(visible.saturating_sub(1) / 2) as u16;
        let panel = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).border_style(border).title(title))
            .scroll((scroll, 0));
        f.render_widget(panel, area);
    }

    if let Some(area) = outline_area {
        let room = area.width.saturating_sub(2) as usize;
        let tab = app.active_tab();
        // The section being read: the last heading at or above the selected
        // link, or above the top of the screen when nothing is selected.
        let page_top = tab.page_index * tab.page_height();
        let reading_line = selected_target
            .and_then(|t| {
                tab.current_lines().iter().position(|l| l.segs.iter().any(|s| s.target == Some(t)))
            })
            .map_or(page_top, |i| page_top + i);
        let active = tab.laid.heading_lines.iter().rposition(|&l| l <= reading_line);
        let lines: Vec<Line> = tab
            .page
            .doc
            .headings
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let text = format!("{}{}", "  ".repeat((h.level as usize).saturating_sub(1).min(3)), h.label);
                let style = if Some(i) == active {
                    selected_link_style
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                Line::from(Span::styled(text.chars().take(room).collect::<String>(), style))
            })
            .collect();
        let visible = area.height.saturating_sub(2) as usize;
        let scroll = active.map_or(0, |a| a.saturating_sub(visible.saturating_sub(1) / 2)) as u16;
        let outline = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" outline "))
            .scroll((scroll, 0));
        f.render_widget(outline, area);
    }

    let pane_focused = matches!(app.mode, Mode::Pane);
    let pane_target = if pane_focused {
        let r = rows[0];
        let w = (r.width * 6 / 10).clamp(20.min(r.width), r.width);
        let h = (r.height * 8 / 10).clamp(10.min(r.height), r.height);
        Some(Rect {
            x: r.x + r.width.saturating_sub(w) / 2,
            y: r.y + r.height.saturating_sub(h) / 2,
            width: w,
            height: h,
        })
    } else {
        None
    };
    if let Some(area) = pane_target {
        let tab = app.active_tab();
        let pane_rows = tab.pane_rows();
        let cursor = tab.pane_cursor.min(pane_rows.len().saturating_sub(1));
        let header_style = Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let section_style = Style::default().fg(Color::Yellow);
        let mut lines: Vec<Line> = Vec::new();
        for (n, row) in pane_rows.iter().enumerate() {
            let arrow = |open: bool| if open { "▾" } else { "▸" };
            let (text, base) = match row {
                PaneRow::Header(region, count) => {
                    let open = tab.pane_expanded.contains(&(*region, String::new()));
                    (format!("{} {} ({count})", arrow(open), region.label()), header_style)
                }
                PaneRow::Section(region, name, count) => {
                    let open = tab.pane_expanded.contains(&(*region, name.clone()));
                    (format!("  {} {name} ({count})", arrow(open)), section_style)
                }
                PaneRow::Link(id, depth) => (
                    format!("{}{}", "  ".repeat(*depth as usize + 1), tab.page.doc.links[*id].label),
                    link_style,
                ),
            };
            let style = if n == cursor { selected_link_style } else { base };
            lines.push(Line::from(Span::styled(text, style)));
        }
        if lines.is_empty() {
            lines.push(Line::raw("(no other links)"));
        }
        let highlight_row = cursor;
        let visible = area.height.saturating_sub(2) as usize;
        let scroll = highlight_row.saturating_sub(visible.saturating_sub(1)) as u16;
        let border = if pane_focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };
        let pane = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).border_style(border).title(" links "))
            .scroll((scroll, 0));
        f.render_widget(Clear, area);
        f.render_widget(pane, area);
    }

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
        if app.active_tab().page.doc.headings.is_empty() {
            toc_lines.push(Line::raw("(no headings on this page)"));
        } else {
            for (i, h) in app.active_tab().page.doc.headings.iter().enumerate() {
                let indent = "  ".repeat((h.level as usize).saturating_sub(1));
                let text = format!("{indent}{}", h.label);
                if i == app.active_tab().toc_cursor {
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
        Mode::UrlEntry => "Enter: go / search  Esc: cancel".to_string(),
        Mode::Toc => "↑/↓ select  Enter: jump  Esc: close".to_string(),
        Mode::Search => "type query  Enter: find  Esc: cancel".to_string(),
        Mode::Pane => "↑/↓ select  Enter: expand/collapse or follow  Esc: back to page".to_string(),
        Mode::Field => "type  Enter: submit  Tab: next  Esc: cancel".to_string(),
        Mode::Side => "↑/↓ select  Enter: open  D: clear trail  s: hide  Tab/Esc: back to page".to_string(),
        Mode::Normal => {
            "Tab: trail panel  s: show/hide trail  v: view  l: links  ←/→ page  g/G first/last  ↑/↓ select  <N> Enter: jump  Enter: follow  b/f: back/fwd  u: url  /: find  t: contents  c/x: new/close tab  [/]: switch tab  q: quit"
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
        Mode::Field => {
            let f = &app.active_tab().page.doc.fields[app.field_edit];
            let shown = if f.kind == FieldKind::Password { "*".repeat(f.value.chars().count()) } else { f.value.clone() };
            format!("{}: {shown}_", if f.name.is_empty() { "field" } else { &f.name })
        }
        Mode::Pane => {
            let tab = app.active_tab();
            match tab.pane_selected() {
                Some(PaneRow::Link(id, _)) => tab.page.doc.links[id].href.clone(),
                _ => String::new(),
            }
        }
        Mode::Side => {
            let tab = app.active_tab();
            let rows = tab.side_rows(app.side);
            match rows.get(tab.side_cursor.min(rows.len().saturating_sub(1))) {
                Some(SideRow::Trail(idx, _)) => tab.trail.borrow()[*idx].url.to_string(),
                None => String::new(),
            }
        }
        Mode::Normal => {
            if !app.active_tab().pending.is_empty() {
                format!("go to link: {}_", app.active_tab().pending)
            } else if !app.active_tab().status.is_empty() {
                app.active_tab().status.clone()
            } else {
                app.active_tab().selected_hint()
            }
        }
    };
    f.render_widget(Paragraph::new(status_text), bottom_bar[1]);
}

/// Acts on the selected target: follows a link, starts editing a text
/// field, or presses a submit button.
fn activate(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    match app.active_tab().selected_target() {
        Some(Target::Link(id)) => {
            let href = app.active_tab().page.doc.links[id].href.clone();
            app.active_tab_mut().status = format!("loading {href} ...");
            terminal.draw(|f| ui(f, app))?;
            app.follow_selected();
        }
        Some(Target::Field(id)) => {
            let (kind, form) = {
                let f = &app.active_tab().page.doc.fields[id];
                (f.kind, f.form)
            };
            if kind == FieldKind::Submit {
                submit_form(terminal, app, form, Some(id))?;
            } else {
                app.input = app.active_tab().page.doc.fields[id].value.clone();
                app.field_edit = id;
                app.active_tab_mut().status.clear();
                app.mode = Mode::Field;
            }
        }
        None => app.active_tab_mut().status = "no link selected".to_string(),
    }
    Ok(())
}

/// Submits `form` (see `Tab::form_request`), reporting why it can't on failure.
fn submit_form(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    form: usize,
    submit: Option<usize>,
) -> io::Result<()> {
    match app.active_tab().form_request(form, submit) {
        Ok((url, body)) => {
            app.active_tab_mut().status = format!("loading {url} ...");
            terminal.draw(|f| ui(f, app))?;
            app.navigate_with(url, body);
        }
        Err(e) => app.active_tab_mut().status = e,
    }
    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if let Event::Key(key) = event::read()? {
            {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match app.mode {
                Mode::Normal => match key.code {
                    KeyCode::Char('q') => return Ok(()),
                    KeyCode::Right | KeyCode::PageDown | KeyCode::Char('n') => {
                        if app.active_tab().page_index + 1 < app.active_tab().page_count() {
                            app.active_tab_mut().page_index += 1;
                            app.active_tab_mut().link_cursor = 0;
                        }
                    }
                    KeyCode::Left | KeyCode::PageUp | KeyCode::Char('p') => {
                        if app.active_tab().page_index > 0 {
                            app.active_tab_mut().page_index -= 1;
                            app.active_tab_mut().link_cursor = 0;
                        }
                    }
                    KeyCode::Char('g') => {
                        app.active_tab_mut().page_index = 0;
                        app.active_tab_mut().link_cursor = 0;
                    }
                    KeyCode::Char('G') => {
                        app.active_tab_mut().page_index = app.active_tab().page_count().saturating_sub(1);
                        app.active_tab_mut().link_cursor = 0;
                    }
                    KeyCode::Up => {
                        app.active_tab_mut().link_cursor = app.active_tab().link_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let len = app.active_tab().targets_on_current_page().len();
                        if app.active_tab().link_cursor + 1 < len {
                            app.active_tab_mut().link_cursor += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if app.active_tab().pending.is_empty() {
                            activate(terminal, app)?;
                        } else if let Ok(n) = app.active_tab().pending.parse::<usize>() {
                            let len = app.active_tab().targets_on_current_page().len();
                            if n >= 1 && n <= len {
                                app.active_tab_mut().link_cursor = n - 1;
                                app.active_tab_mut().pending.clear();
                                activate(terminal, app)?;
                            } else {
                                app.active_tab_mut().status = format!("no link {n} on this page");
                                app.active_tab_mut().pending.clear();
                            }
                        }
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => {
                        app.active_tab_mut().status.clear();
                        app.active_tab_mut().pending.push(c);
                    }
                    KeyCode::Backspace => {
                        app.active_tab_mut().pending.pop();
                    }
                    KeyCode::Esc => app.active_tab_mut().pending.clear(),
                    KeyCode::Char('u') => {
                        app.mode = Mode::UrlEntry;
                        app.input.clear();
                        app.new_tab_pending = false;
                        app.active_tab_mut().status.clear();
                    }
                    KeyCode::Char('b') => {
                        if app.active_tab().hist_pos > 0 {
                            let url = app.active_tab().history[app.active_tab().hist_pos - 1].to_string();
                            app.active_tab_mut().status = format!("loading {url} ...");
                            terminal.draw(|f| ui(f, app))?;
                        }
                        app.back();
                    }
                    KeyCode::Char('f') => {
                        if app.active_tab().hist_pos + 1 < app.active_tab().history.len() {
                            let url = app.active_tab().history[app.active_tab().hist_pos + 1].to_string();
                            app.active_tab_mut().status = format!("loading {url} ...");
                            terminal.draw(|f| ui(f, app))?;
                        }
                        app.forward();
                    }
                    KeyCode::Char('/') => {
                        app.mode = Mode::Search;
                        app.input.clear();
                    }
                    KeyCode::Char('l') => {
                        app.active_tab_mut().pane_cursor = 0;
                        app.mode = Mode::Pane;
                    }
                    KeyCode::Char('s') => {
                        app.side = match app.side {
                            Side::Trail => Side::Off,
                            Side::Off => Side::Trail,
                        };
                        app.active_tab_mut().status = match app.side {
                            Side::Trail => "left panel: trail of visited pages",
                            Side::Off => "left panel: off",
                        }
                        .to_string();
                    }
                    KeyCode::Tab => {
                        if app.side_visible {
                            let tab = app.active_tab();
                            let at = tab
                                .side_rows(app.side)
                                .iter()
                                .position(|r| matches!(r, SideRow::Trail(i, _) if *i == tab.trail_at))
                                .unwrap_or(0);
                            app.active_tab_mut().side_cursor = at;
                            app.mode = Mode::Side;
                        } else {
                            app.active_tab_mut().status =
                                format!("left panel needs a terminal at least {SIDE_MIN_TOTAL} columns wide (s turns it on)");
                        }
                    }
                    KeyCode::Char('t') => {
                        app.active_tab_mut().toc_cursor = 0;
                        app.mode = Mode::Toc;
                    }
                    KeyCode::Char('c') => {
                        app.mode = Mode::UrlEntry;
                        app.input.clear();
                        app.new_tab_pending = true;
                    }
                    KeyCode::Char('x') => {
                        app.close_tab();
                    }
                    KeyCode::Char('v') => {
                        let tab = app.active_tab_mut();
                        if let Some(host) = view::SiteViews::host_of(&tab.history[tab.hist_pos]) {
                            let current = tab.view_pick.as_ref().filter(|(h, _)| *h == host).and_then(|&(_, v)| v);
                            let next = view::View::cycle(current);
                            tab.view_pick = Some((host, next));
                            tab.status = match next {
                                Some(v) => format!("view: {} (V saves it for this site)", v.label()),
                                None => "view: default (V forgets the saved view)".to_string(),
                            };
                        }
                    }
                    KeyCode::Char('V') => {
                        // Saves what's on screen for this site; with an
                        // automatic view it forgets the saved choice instead.
                        let (shown, chosen) = app.active_tab().effective_view(&app.site_views);
                        let tab = &app.tabs[app.active];
                        if let Some(host) = view::SiteViews::host_of(&tab.history[tab.hist_pos]) {
                            let pick = if chosen { Some(shown) } else { None };
                            let msg = match app.site_views.set(&host, pick) {
                                Ok(()) => match pick {
                                    Some(v) => format!("{host} will always open as {}", v.label()),
                                    None => format!("forgot the saved view for {host}"),
                                },
                                Err(e) => format!("could not save view: {e}"),
                            };
                            app.active_tab_mut().status = msg;
                        }
                    }
                    KeyCode::Char(']') => {
                        app.next_tab();
                    }
                    KeyCode::Char('[') => {
                        app.prev_tab();
                    }
                    _ => {}
                },
                Mode::Pane => match key.code {
                    KeyCode::Up => {
                        app.active_tab_mut().pane_cursor = app.active_tab().pane_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let len = app.active_tab().pane_rows().len();
                        if app.active_tab().pane_cursor + 1 < len {
                            app.active_tab_mut().pane_cursor += 1;
                        }
                    }
                    KeyCode::Enter => match app.active_tab().pane_selected() {
                        Some(PaneRow::Header(region, _)) => app.active_tab_mut().toggle_pane(region, String::new()),
                        Some(PaneRow::Section(region, name, _)) => app.active_tab_mut().toggle_pane(region, name),
                        Some(PaneRow::Link(id, _)) => {
                            app.mode = Mode::Normal;
                            let href = app.active_tab().page.doc.links[id].href.clone();
                            app.active_tab_mut().status = format!("loading {href} ...");
                            terminal.draw(|f| ui(f, app))?;
                            let client = app.client.clone();
                            app.active_tab_mut().follow_href(&client, &href);
                        }
                        None => app.mode = Mode::Normal,
                    },
                    KeyCode::Esc | KeyCode::Char('l') => app.mode = Mode::Normal,
                    _ => {}
                },
                Mode::Side => match key.code {
                    KeyCode::Up => {
                        app.active_tab_mut().side_cursor = app.active_tab().side_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        let len = app.active_tab().side_rows(app.side).len();
                        if app.active_tab().side_cursor + 1 < len {
                            app.active_tab_mut().side_cursor += 1;
                        }
                    }
                    KeyCode::Enter => {
                        let tab = app.active_tab();
                        let rows = tab.side_rows(app.side);
                        if let Some(SideRow::Trail(idx, _)) = rows.get(tab.side_cursor.min(rows.len().saturating_sub(1))) {
                            let idx = *idx;
                            app.mode = Mode::Normal;
                            app.active_tab_mut().status = "loading ...".to_string();
                            terminal.draw(|f| ui(f, app))?;
                            let client = app.client.clone();
                            app.active_tab_mut().jump_trail(&client, idx);
                        }
                    }
                    KeyCode::Char('s') => {
                        app.side = Side::Off;
                        app.mode = Mode::Normal;
                    }
                    KeyCode::Char('D') => {
                        app.clear_trail();
                        app.active_tab_mut().status = "trail cleared".to_string();
                    }
                    KeyCode::Esc | KeyCode::Tab => app.mode = Mode::Normal,
                    _ => {}
                },
                Mode::Toc => match key.code {
                    KeyCode::Up => {
                        app.active_tab_mut().toc_cursor = app.active_tab().toc_cursor.saturating_sub(1);
                    }
                    KeyCode::Down => {
                        if app.active_tab().toc_cursor + 1 < app.active_tab().page.doc.headings.len() {
                            app.active_tab_mut().toc_cursor += 1;
                        }
                    }
                    KeyCode::Enter => {
                        let idx = app.active_tab().toc_cursor;
                        if let Some(&line) = app.active_tab().laid.heading_lines.get(idx) {
                            app.jump_to_line(line);
                        }
                        app.mode = Mode::Normal;
                    }
                    KeyCode::Esc | KeyCode::Char('t') => app.mode = Mode::Normal,
                    _ => {}
                },
                Mode::Field => match key.code {
                    KeyCode::Enter => {
                        app.mode = Mode::Normal;
                        let form = app.active_tab().page.doc.fields[app.field_edit].form;
                        submit_form(terminal, app, form, None)?;
                    }
                    KeyCode::Esc => {
                        app.mode = Mode::Normal;
                        let (id, old) = (app.field_edit, app.input.clone());
                        app.active_tab_mut().set_field_value(id, old);
                    }
                    KeyCode::Tab => {
                        app.mode = Mode::Normal;
                        let len = app.active_tab().targets_on_current_page().len();
                        if app.active_tab().link_cursor + 1 < len {
                            app.active_tab_mut().link_cursor += 1;
                        }
                    }
                    KeyCode::Backspace => {
                        let id = app.field_edit;
                        let mut value = app.active_tab().page.doc.fields[id].value.clone();
                        value.pop();
                        app.active_tab_mut().set_field_value(id, value);
                    }
                    KeyCode::Char(c) => {
                        let id = app.field_edit;
                        let mut value = app.active_tab().page.doc.fields[id].value.clone();
                        value.push(c);
                        app.active_tab_mut().set_field_value(id, value);
                    }
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
                        let new_tab = app.new_tab_pending;
                        app.new_tab_pending = false;
                        if !input.is_empty() {
                            let parsed = if looks_like_url(&input) {
                                Url::parse(&input)
                                    .or_else(|_| Url::parse(&format!("https://{input}")))
                            } else {
                                Ok(search_url(&input))
                            };
                            match parsed {
                                Ok(url) => {
                                    if new_tab {
                                        app.open_tab(url);
                                    } else {
                                        app.active_tab_mut().status = format!("loading {url} ...");
                                        terminal.draw(|f| ui(f, app))?;
                                        app.navigate(url);
                                    }
                                }
                                Err(e) => app.active_tab_mut().status = format!("invalid url: {e}"),
                            }
                        }
                    }
                    KeyCode::Esc => {
                        app.mode = Mode::Normal;
                        app.new_tab_pending = false;
                    }
                    KeyCode::Backspace => {
                        app.input.pop();
                    }
                    KeyCode::Char(c) => app.input.push(c),
                    _ => {}
                },
            }
            }
        }
    }
}

fn main() -> ExitCode {
    // No argument: open the built-in start page with the URL bar ready.
    let url_arg = env::args().nth(1);
    let start_url = match &url_arg {
        None => Url::parse("about:blank").expect("static url"),
        Some(arg) => match Url::parse(arg).or_else(|_| Url::parse(&format!("https://{arg}"))) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("invalid url {arg}: {e}");
                return ExitCode::FAILURE;
            }
        },
    };

    let cookie_jar = cookies::jar();

    let client = match reqwest::blocking::Client::builder()
        .user_agent("Mozilla/5.0 (compatible; termbrowse/0.1)")
        .cookie_provider(cookie_jar.clone())
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("failed to build http client: {e}");
            return ExitCode::FAILURE;
        }
    };

    let start_url = rewrite_url(&start_url);

    let page = match fetch(&client, &start_url) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("failed to fetch {start_url}: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Earlier runs' trail comes first; this run's start page is a new root.
    let shared: SharedTrail = std::rc::Rc::new(std::cell::RefCell::new(trail::load()));
    let tab = Tab::with_page(start_url, page, shared);

    let mut app = App {
        client,
        tabs: vec![tab],
        active: 0,
        mode: if url_arg.is_none() { Mode::UrlEntry } else { Mode::Normal },
        input: String::new(),
        field_edit: 0,
        new_tab_pending: false,
        site_views: view::SiteViews::load(),
        side: Side::Trail,
        side_visible: false,
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

    cookie_jar.save(cookies::store_path().as_ref());

    if let Err(e) = result {
        eprintln!("error: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
