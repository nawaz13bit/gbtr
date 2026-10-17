//! HTML -> intermediate document model.
//!
//! `build` walks the parsed DOM once and produces a tree of `Block`s (paragraphs,
//! headings, lists, code, quotes, tables) whose contents are `Inline`s. Nothing
//! here knows about terminal width: wrapping, indentation and column alignment
//! all happen later in `layout`, which is also where link/heading/anchor
//! positions get decided.

use ego_tree::NodeRef;
use scraper::Node;
use scraper::node::Element;
use url::Url;

/// Per-fragment text styling. `heading` is the h1-h6 level (0 = not a
/// heading); `dim` is only set by the layout step for chrome like bullets.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct TextStyle {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub dim: bool,
    pub heading: u8,
}

#[derive(Clone, Debug)]
pub enum Inline {
    /// Whitespace-collapsed text. May start and/or end with a single space,
    /// which is how word boundaries between adjacent inline elements survive.
    Text { text: String, style: TextStyle },
    /// A followable link; `id` indexes `Doc::links`.
    Link { id: usize, children: Vec<Inline> },
    /// Zero-width marker for an element `id`, so `#fragment` links can jump.
    Anchor(String),
    /// A visible form control (text box or submit button); `id` indexes
    /// `Doc::fields`.
    Field { id: usize },
    Break,
}

/// Where in the page a piece of content sits. Everything outside the main
/// content (menus, sidebars, footers) is "chrome" that the UI can move into a
/// side pane instead of spending reading space on it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Region {
    Content,
    Nav,
    Aside,
    Footer,
    /// Links to the same page in other languages (e.g. Wikipedia's list),
    /// wherever on the page they sit.
    Languages,
    /// Links to sister projects (e.g. Wikipedia's "In other projects" boxes).
    Projects,
}

impl Region {
    pub fn label(self) -> &'static str {
        match self {
            Region::Content => "In page",
            Region::Nav => "Navigation",
            Region::Aside => "Sidebar",
            Region::Footer => "Footer",
            Region::Languages => "Languages",
            Region::Projects => "Other projects",
        }
    }
}

pub struct Cell {
    pub header: bool,
    pub inlines: Vec<Inline>,
}

pub enum Block {
    Paragraph(Vec<Inline>),
    /// `idx` indexes `Doc::headings`.
    Heading { idx: usize, level: u8, inlines: Vec<Inline> },
    List { ordered: bool, start: usize, items: Vec<Vec<Block>> },
    Pre(String),
    Quote(Vec<Block>),
    Rule,
    /// A real data table (no nested layout structure) -- rows of cells.
    Table(Vec<Vec<Cell>>),
    Anchor(String),
    /// Content belonging to a non-main region; layout may skip it entirely.
    Chrome(#[allow(dead_code)] Region, Vec<Block>),
}

/// A followable link: where it goes and the visible text it carries.
pub struct LinkInfo {
    pub href: String,
    pub label: String,
    pub region: Region,
    /// Section of the menu/sidebar this link sits under (nearest heading or
    /// `aria-label`), or empty if it has none.
    pub group: String,
}

/// A heading for the table-of-contents overlay.
pub struct HeadingInfo {
    pub level: u8,
    pub label: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FieldKind {
    /// Editable single-line text (also search, email, textarea, ...).
    Text,
    /// Editable, but shown masked.
    Password,
    Submit,
    /// Never shown, always sent: hidden inputs, checked boxes, selects.
    Hidden,
}

/// One form control. `value` is live: editing a text field rewrites it.
pub struct FieldInfo {
    pub form: usize,
    pub name: String,
    pub kind: FieldKind,
    pub value: String,
    /// Caption for a submit button.
    pub label: String,
}

pub struct FormInfo {
    /// Absolute target URL (the page itself if the form has no `action`).
    pub action: String,
    pub post: bool,
}

pub struct Doc {
    pub blocks: Vec<Block>,
    pub links: Vec<LinkInfo>,
    pub headings: Vec<HeadingInfo>,
    pub forms: Vec<FormInfo>,
    pub fields: Vec<FieldInfo>,
}

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
            | "ul"
            | "ol"
            | "tr"
            | "table"
            | "caption"
            | "blockquote"
            | "pre"
            | "hr"
            | "section"
            | "article"
            | "header"
            | "footer"
            | "nav"
            | "main"
            | "aside"
            | "figure"
            | "figcaption"
            | "form"
            | "fieldset"
            | "details"
            | "summary"
            | "dl"
            | "dt"
            | "dd"
            | "address"
            | "center"
    )
}

fn is_skipped(tag: &str) -> bool {
    matches!(tag, "script" | "style" | "head" | "noscript" | "svg")
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

fn collapse_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_ws = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

fn plain_text(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Link { children, .. } => plain_text(children, out),
            Inline::Break => out.push(' '),
            Inline::Anchor(_) | Inline::Field { .. } => {}
        }
    }
}

fn label_of(inlines: &[Inline]) -> String {
    let mut raw = String::new();
    plain_text(inlines, &mut raw);
    collapse_ws(&raw).trim().to_string()
}

fn visible(inline: &Inline) -> bool {
    match inline {
        Inline::Text { text, .. } => text.chars().any(|c| !c.is_whitespace()),
        Inline::Link { children, .. } => children.iter().any(visible),
        Inline::Field { .. } => true,
        _ => false,
    }
}

/// Verbatim text of a subtree (for `<pre>`): whitespace untouched, `<br>` as
/// a newline.
fn raw_text(node: NodeRef<Node>, out: &mut String) {
    match node.value() {
        Node::Text(t) => out.push_str(&t.text),
        Node::Element(el) => {
            if is_skipped(el.name()) {
                return;
            }
            if el.name() == "br" {
                out.push('\n');
            }
            for child in node.children() {
                raw_text(child, out);
            }
        }
        _ => {
            for child in node.children() {
                raw_text(child, out);
            }
        }
    }
}

/// A table is treated as tabular data -- rather than page-layout scaffolding
/// -- only if it has cells and nothing structural inside them: no nested
/// tables, no block content. Old-style layout tables (Hacker News and the
/// like) fail this and are walked transparently instead.
fn is_data_table(node: NodeRef<Node>) -> bool {
    let mut has_cell = false;
    for d in node.descendants().skip(1) {
        if let Node::Element(e) = d.value() {
            match e.name() {
                "table" | "div" | "p" | "ul" | "ol" | "pre" | "blockquote" | "form" | "hr"
                | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "article" => return false,
                "td" | "th" => has_cell = true,
                _ => {}
            }
        }
    }
    has_cell
}

/// Classifies an element as page chrome from its tag, ARIA role, or a
/// conventional id/class name. Deliberately conservative -- exact names only,
/// and never `html`/`body`/`main`/`article` -- since wrongly hiding real
/// content is worse than showing a menu.
fn region_of(el: &Element) -> Option<Region> {
    if matches!(el.name(), "html" | "body" | "main" | "article") {
        return None;
    }
    match el.name() {
        "nav" => return Some(Region::Nav),
        "footer" => return Some(Region::Footer),
        "aside" => return Some(Region::Aside),
        _ => {}
    }
    match el.attr("role") {
        // `role=search` is deliberately not chrome: it wraps search boxes,
        // which have to stay on the page to be usable.
        Some("navigation" | "banner") => return Some(Region::Nav),
        Some("contentinfo") => return Some(Region::Footer),
        Some("complementary") => return Some(Region::Aside),
        _ => {}
    }
    for attr in [el.attr("id"), el.attr("class")].into_iter().flatten() {
        for tok in attr.split_whitespace() {
            match tok.to_ascii_lowercase().as_str() {
                "nav" | "navbar" | "navigation" | "navbox" | "menu" | "breadcrumb" | "breadcrumbs"
                | "pagetop" | "mw-panel" | "mw-navigation" | "site-nav" | "main-nav" | "global-nav"
                | "top-nav" | "topbar" => return Some(Region::Nav),
                "sidebar" | "side" | "mw-sidebar" => return Some(Region::Aside),
                "p-lang" | "p-lang-btn" | "mw-portlet-lang" | "interlanguage-link"
                | "vector-page-titlebar-language" => return Some(Region::Languages),
                "sistersitebox" | "sister-box" | "sister-project" | "sisterproject"
                | "p-wikibase-otherprojects" | "mw-portlet-wikibase-otherprojects"
                | "wb-otherproject-link" => return Some(Region::Projects),
                "footer" | "site-footer" | "page-footer" | "global-footer" => {
                    return Some(Region::Footer)
                }
                _ => {}
            }
        }
    }
    None
}

/// Whether a menu/sidebar element titles the link list that follows it: an
/// `h1`-`h6`, or a conventionally named heading class (e.g. MediaWiki's
/// `vector-menu-heading`).
fn is_group_heading(el: &Element) -> bool {
    matches!(el.name(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
        || el.attr("class").is_some_and(|c| {
            c.split_whitespace().any(|t| {
                let t = t.to_ascii_lowercase();
                t.ends_with("-heading") || t.ends_with("-title") && t.contains("menu")
            })
        })
}

/// Accumulates the content of one container: finished blocks plus the run of
/// inline content still open. In `inline_only` mode (link text, headings,
/// table cells) block boundaries become `Break`s instead of closing a
/// paragraph, so nothing structural can escape the container.
struct Collector {
    blocks: Vec<Block>,
    run: Vec<Inline>,
    inline_only: bool,
}

impl Collector {
    fn new(inline_only: bool) -> Self {
        Collector { blocks: Vec::new(), run: Vec::new(), inline_only }
    }

    fn boundary(&mut self) {
        if self.inline_only {
            if !matches!(self.run.last(), None | Some(Inline::Break)) {
                self.run.push(Inline::Break);
            }
        } else {
            self.flush();
        }
    }

    fn flush(&mut self) {
        let run = std::mem::take(&mut self.run);
        if run.iter().any(visible) {
            self.blocks.push(Block::Paragraph(run));
        } else {
            for inline in run {
                if let Inline::Anchor(id) = inline {
                    self.blocks.push(Block::Anchor(id));
                }
            }
        }
    }

    /// Closes the open run, then adds a finished block after it.
    fn push_block(&mut self, block: Block) {
        self.flush();
        self.blocks.push(block);
    }

    fn finish(mut self) -> Vec<Block> {
        self.flush();
        self.blocks
    }
}

struct Builder<'a> {
    base: &'a Url,
    links: Vec<LinkInfo>,
    headings: Vec<HeadingInfo>,
    style: TextStyle,
    region: Region,
    group: String,
    forms: Vec<FormInfo>,
    fields: Vec<FieldInfo>,
    /// The `<form>` being walked, if any; controls outside one are ignored.
    form: Option<usize>,
}

pub fn build(root: NodeRef<Node>, base: &Url) -> Doc {
    let mut builder = Builder {
        base,
        links: Vec::new(),
        headings: Vec::new(),
        style: TextStyle::default(),
        region: Region::Content,
        group: String::new(),
        forms: Vec::new(),
        fields: Vec::new(),
        form: None,
    };
    let mut col = Collector::new(false);
    builder.walk(root, &mut col);
    Doc {
        blocks: col.finish(),
        links: builder.links,
        headings: builder.headings,
        forms: builder.forms,
        fields: builder.fields,
    }
}

impl Builder<'_> {
    fn walk(&mut self, node: NodeRef<Node>, col: &mut Collector) {
        match node.value() {
            Node::Text(t) => {
                let text = collapse_ws(&t.text);
                if !text.is_empty() {
                    col.run.push(Inline::Text { text, style: self.style });
                }
            }
            Node::Element(el) => self.element(node, el, col),
            Node::Document | Node::Fragment => {
                for child in node.children() {
                    self.walk(child, col);
                }
            }
            _ => {}
        }
    }

    fn walk_children(&mut self, node: NodeRef<Node>, col: &mut Collector) {
        for child in node.children() {
            self.walk(child, col);
        }
    }

    fn element(&mut self, node: NodeRef<Node>, el: &Element, col: &mut Collector) {
        let tag = el.name();
        if is_skipped(tag) {
            return;
        }
        if !col.inline_only
            && self.region == Region::Content
            && let Some(region) = region_of(el)
        {
            self.region = region;
            self.group = el.attr("aria-label").map(|s| s.trim().to_string()).unwrap_or_default();
            let mut sub = Collector::new(false);
            self.element(node, el, &mut sub);
            self.region = Region::Content;
            self.group.clear();
            let blocks = sub.finish();
            if !blocks.is_empty() {
                col.push_block(Block::Chrome(region, blocks));
            }
            return;
        }
        if self.region != Region::Content && !col.inline_only && is_group_heading(el) {
            let mut text = String::new();
            raw_text(node, &mut text);
            let text = collapse_ws(&text);
            if !text.is_empty() {
                self.group = text.trim().to_string();
            }
        }
        let blockish = is_block(tag);
        if blockish {
            col.boundary();
        }
        if let Some(id) = el.attr("id").filter(|s| !s.is_empty()) {
            col.run.push(Inline::Anchor(id.to_string()));
        }

        match tag {
            "br" => col.run.push(Inline::Break),
            "img" => {
                let alt = el.attr("alt").unwrap_or("").trim();
                let label = if alt.is_empty() { "image".to_string() } else { format!("image: {alt}") };
                col.run.push(Inline::Text {
                    text: format!(" [{label}] "),
                    style: self.style,
                });
            }
            "a" => self.link(node, el, col),
            "form" => self.form(node, el, col),
            "input" => self.input(el, col),
            "textarea" => self.textarea(node, el, col),
            "select" => self.select(node, el),
            "button" => self.button(node, el, col),
            "hr" if !col.inline_only => col.push_block(Block::Rule),
            "pre" if !col.inline_only => {
                let mut text = String::new();
                raw_text(node, &mut text);
                let text = text.trim_start_matches('\n').trim_end().to_string();
                if !text.is_empty() {
                    col.push_block(Block::Pre(text));
                }
            }
            "ul" | "ol" if !col.inline_only => self.list(node, el, col),
            "blockquote" if !col.inline_only => {
                let mut sub = Collector::new(false);
                self.walk_children(node, &mut sub);
                let blocks = sub.finish();
                if !blocks.is_empty() {
                    col.push_block(Block::Quote(blocks));
                }
            }
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" if !col.inline_only => {
                let level = heading_level(tag).unwrap_or(1);
                let mut sub = Collector::new(true);
                self.walk_children(node, &mut sub);
                let label = label_of(&sub.run);
                if !label.is_empty() && self.region != Region::Content {
                    // Chrome headings (e.g. "Navigation menu") aren't part of the
                    // article's outline, so keep them out of the contents list.
                    col.push_block(Block::Paragraph(sub.run));
                } else if !label.is_empty() {
                    let idx = self.headings.len();
                    self.headings.push(HeadingInfo { level, label });
                    col.push_block(Block::Heading { idx, level, inlines: sub.run });
                }
            }
            "table" if !col.inline_only && is_data_table(node) => self.data_table(node, col),
            "td" | "th" => {
                if col.run.iter().any(visible) {
                    col.run.push(Inline::Text { text: " | ".to_string(), style: self.style });
                }
                self.walk_children(node, col);
            }
            "strong" | "b" => self.with_style(node, col, |s| s.bold = true),
            "em" | "i" | "cite" | "dfn" | "var" => self.with_style(node, col, |s| s.italic = true),
            "code" | "kbd" | "samp" | "tt" => self.with_style(node, col, |s| s.code = true),
            _ => self.walk_children(node, col),
        }

        if blockish {
            col.boundary();
        }
    }

    fn with_style(&mut self, node: NodeRef<Node>, col: &mut Collector, apply: impl FnOnce(&mut TextStyle)) {
        let saved = self.style;
        apply(&mut self.style);
        self.walk_children(node, col);
        self.style = saved;
    }

    fn link(&mut self, node: NodeRef<Node>, el: &Element, col: &mut Collector) {
        let mut sub = Collector::new(true);
        self.walk_children(node, &mut sub);
        let label = label_of(&sub.run);
        let substantial = label.chars().filter(|c| c.is_alphanumeric()).count() >= 3;
        if !substantial {
            // No visible text, or just an icon/arrow/bare page number (e.g.
            // HN's upvote arrow or comment-thread pagination) -- not worth
            // showing or registering as a followable link. Anchors inside
            // it are still worth keeping.
            for inline in sub.run {
                if let Inline::Anchor(id) = inline {
                    col.run.push(Inline::Anchor(id));
                }
            }
            return;
        }
        match el.attr("href").and_then(|h| self.base.join(h).ok()) {
            Some(absolute) => {
                let id = self.links.len();
                // `hreflang` marks a link to a translation of this page.
                let region = if self.region != Region::Content && el.attr("hreflang").is_some() {
                    Region::Languages
                } else {
                    self.region
                };
                self.links.push(LinkInfo { href: absolute.to_string(), label, region, group: self.group.clone() });
                col.run.push(Inline::Link { id, children: sub.run });
            }
            None => col.run.extend(sub.run),
        }
    }

    fn form(&mut self, node: NodeRef<Node>, el: &Element, col: &mut Collector) {
        let action = el.attr("action").map(str::trim).unwrap_or("");
        let action = self.base.join(action).unwrap_or_else(|_| self.base.clone()).to_string();
        let post = el.attr("method").is_some_and(|m| m.trim().eq_ignore_ascii_case("post"));
        let idx = self.forms.len();
        self.forms.push(FormInfo { action, post });
        let saved = self.form.replace(idx);
        self.walk_children(node, col);
        self.form = saved;
    }

    /// Registers a control of the current form; returns its id, or `None`
    /// if there is no current form or the control is disabled.
    fn add_field(&mut self, el: &Element, kind: FieldKind, value: String, label: String) -> Option<usize> {
        let form = self.form?;
        if el.attr("disabled").is_some() {
            return None;
        }
        let name = el.attr("name").unwrap_or("").to_string();
        self.fields.push(FieldInfo { form, name, kind, value, label });
        Some(self.fields.len() - 1)
    }

    fn input(&mut self, el: &Element, col: &mut Collector) {
        let ty = el.attr("type").unwrap_or("text").trim().to_ascii_lowercase();
        let value = el.attr("value").unwrap_or("").to_string();
        let (kind, value) = match ty.as_str() {
            "hidden" => (FieldKind::Hidden, value),
            "checkbox" | "radio" if el.attr("checked").is_some() => {
                (FieldKind::Hidden, if value.is_empty() { "on".to_string() } else { value })
            }
            "checkbox" | "radio" | "button" | "reset" | "file" => return,
            "submit" | "image" => (FieldKind::Submit, value),
            "password" => (FieldKind::Password, value),
            _ => (FieldKind::Text, value),
        };
        let label = if kind != FieldKind::Submit {
            String::new()
        } else if !value.is_empty() {
            value.clone()
        } else {
            el.attr("alt").map(str::trim).filter(|s| !s.is_empty()).unwrap_or("Submit").to_string()
        };
        if let Some(id) = self.add_field(el, kind, value, label)
            && kind != FieldKind::Hidden
        {
            col.run.push(Inline::Field { id });
        }
    }

    fn textarea(&mut self, node: NodeRef<Node>, el: &Element, col: &mut Collector) {
        let mut text = String::new();
        raw_text(node, &mut text);
        let text = text.trim_start_matches('\n').to_string();
        if let Some(id) = self.add_field(el, FieldKind::Text, text, String::new()) {
            col.run.push(Inline::Field { id });
        }
    }

    /// A dropdown is sent with its selected (else first) option's value; it
    /// isn't shown or editable yet.
    fn select(&mut self, node: NodeRef<Node>, el: &Element) {
        let mut first = None;
        let mut chosen = None;
        for d in node.descendants() {
            if let Node::Element(opt) = d.value()
                && opt.name() == "option"
            {
                let value = match opt.attr("value") {
                    Some(v) => v.to_string(),
                    None => {
                        let mut text = String::new();
                        raw_text(d, &mut text);
                        collapse_ws(&text).trim().to_string()
                    }
                };
                if chosen.is_none() && opt.attr("selected").is_some() {
                    chosen = Some(value.clone());
                }
                first.get_or_insert(value);
            }
        }
        if let Some(value) = chosen.or(first) {
            self.add_field(el, FieldKind::Hidden, value, String::new());
        }
    }

    fn button(&mut self, node: NodeRef<Node>, el: &Element, col: &mut Collector) {
        let submits = matches!(el.attr("type").map(|t| t.trim().to_ascii_lowercase()).as_deref(), None | Some("submit") | Some(""));
        if !submits || self.form.is_none() {
            self.walk_children(node, col);
            return;
        }
        let mut sub = Collector::new(true);
        self.walk_children(node, &mut sub);
        let label = label_of(&sub.run);
        let label = if label.is_empty() { "Submit".to_string() } else { label };
        let value = el.attr("value").unwrap_or("").to_string();
        if let Some(id) = self.add_field(el, FieldKind::Submit, value, label) {
            col.run.push(Inline::Field { id });
        }
    }

    fn list(&mut self, node: NodeRef<Node>, el: &Element, col: &mut Collector) {
        let ordered = el.name() == "ol";
        let start = el.attr("start").and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1);
        let mut items = Vec::new();
        for child in node.children() {
            let mut sub = Collector::new(false);
            match child.value() {
                Node::Element(ce) if ce.name() == "li" => {
                    if let Some(id) = ce.attr("id").filter(|s| !s.is_empty()) {
                        sub.run.push(Inline::Anchor(id.to_string()));
                    }
                    self.walk_children(child, &mut sub);
                }
                Node::Text(t) if t.text.trim().is_empty() => continue,
                // Stray content directly inside the list: keep it as its own item.
                _ => self.walk(child, &mut sub),
            }
            let blocks = sub.finish();
            if !blocks.is_empty() {
                items.push(blocks);
            }
        }
        if !items.is_empty() {
            col.push_block(Block::List { ordered, start, items });
        }
    }

    fn data_table(&mut self, node: NodeRef<Node>, col: &mut Collector) {
        let mut rows = Vec::new();
        for tr in node.descendants() {
            if !matches!(tr.value(), Node::Element(e) if e.name() == "tr") {
                continue;
            }
            let mut cells = Vec::new();
            for cell in tr.children() {
                let Node::Element(ce) = cell.value() else { continue };
                if ce.name() != "td" && ce.name() != "th" {
                    continue;
                }
                let header = ce.name() == "th";
                let mut sub = Collector::new(true);
                if let Some(id) = ce.attr("id").filter(|s| !s.is_empty()) {
                    sub.run.push(Inline::Anchor(id.to_string()));
                }
                self.with_style(cell, &mut sub, |s| {
                    if header {
                        s.bold = true;
                    }
                });
                cells.push(Cell { header, inlines: sub.run });
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if !rows.is_empty() {
            col.push_block(Block::Table(rows));
        }
    }
}
