//! Width-aware layout: turns a `Doc` into wrapped, styled terminal lines.
//!
//! The document model knows nothing about columns; everything that depends on
//! the viewport (word wrap, list/quote indentation, table alignment) happens
//! here, so resizing just re-runs `layout` on the same `Doc`.

use std::collections::HashMap;

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::doc::{Block, Cell, Doc, FieldInfo, FieldKind, Inline, TextStyle};

/// Something on the page the cursor can land on.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Target {
    /// Index into `Doc::links`.
    Link(usize),
    /// Index into `Doc::fields`.
    Field(usize),
}

/// A run of text on one line sharing a style and (optionally) a target.
#[derive(Clone)]
pub struct Seg {
    pub text: String,
    pub style: TextStyle,
    pub target: Option<Target>,
}

pub struct LaidLine {
    pub segs: Vec<Seg>,
    /// Lines wrapped from the same paragraph/row/code line share a group; used
    /// to keep the reader's place across re-layouts and to search across wraps.
    pub group: usize,
}

#[derive(Default)]
pub struct Layout {
    pub lines: Vec<LaidLine>,
    /// Line index where each `Doc::headings` entry starts.
    pub heading_lines: Vec<usize>,
    /// Element `id` -> line index.
    pub anchors: HashMap<String, usize>,
}

fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn seg(text: &str, style: TextStyle) -> Seg {
    Seg { text: text.to_string(), style, target: None }
}

fn dim() -> TextStyle {
    TextStyle { dim: true, ..TextStyle::default() }
}

fn add_seg(line: &mut Vec<Seg>, s: Seg) {
    if let Some(last) = line.last_mut()
        && last.style == s.style
        && last.target == s.target
    {
        last.text.push_str(&s.text);
        return;
    }
    line.push(s);
}

struct Frame {
    /// Prefix for the first line emitted inside the frame (bullet, number).
    lead: Option<Vec<Seg>>,
    /// Prefix for every other line (indent, quote bar).
    cont: Vec<Seg>,
    width: usize,
}

struct Emitter {
    lines: Vec<LaidLine>,
    heading_lines: Vec<usize>,
    anchors: HashMap<String, usize>,
    frames: Vec<Frame>,
    width: usize,
    group: usize,
    last_blank: bool,
    tight: usize,
    depth: usize,
    hide_chrome: bool,
    /// Display text of each `Doc::fields` entry, parallel to it.
    fields: Vec<String>,
}

enum Tok {
    Frag { text: String, style: TextStyle, link: Option<Target> },
    Space,
    Break,
    Anchor(String),
}

type Part = (String, TextStyle, Option<Target>);

/// State of the line currently being filled by `flow`.
struct Cur {
    line: Vec<Seg>,
    w: usize,
    space: bool,
    started: bool,
}

fn tokenize(inlines: &[Inline], link: Option<Target>, heading: u8, fields: &[String], out: &mut Vec<Tok>) {
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => {
                let mut style = *style;
                if heading > 0 {
                    style.heading = heading;
                }
                let mut cur = String::new();
                for ch in text.chars() {
                    if ch.is_whitespace() {
                        if !cur.is_empty() {
                            out.push(Tok::Frag { text: std::mem::take(&mut cur), style, link });
                        }
                        if !matches!(out.last(), Some(Tok::Space)) {
                            out.push(Tok::Space);
                        }
                    } else {
                        cur.push(ch);
                    }
                }
                if !cur.is_empty() {
                    out.push(Tok::Frag { text: cur, style, link });
                }
            }
            Inline::Link { id, children } => {
                tokenize(children, link.or(Some(Target::Link(*id))), heading, fields, out)
            }
            Inline::Field { id } => {
                if !matches!(out.last(), Some(Tok::Space) | None) {
                    out.push(Tok::Space);
                }
                let text = fields.get(*id).cloned().unwrap_or_default();
                out.push(Tok::Frag { text, style: TextStyle::default(), link: Some(Target::Field(*id)) });
                out.push(Tok::Space);
            }
            Inline::Anchor(a) => out.push(Tok::Anchor(a.clone())),
            Inline::Break => out.push(Tok::Break),
        }
    }
}

/// Flattens inlines onto a single line (for table cells): breaks become
/// spaces, anchors are collected separately.
fn flatten(inlines: &[Inline], link: Option<Target>, fields: &[String], out: &mut Vec<Seg>, anchors: &mut Vec<String>) {
    for inline in inlines {
        match inline {
            Inline::Text { text, style } => add_seg(out, Seg { text: text.clone(), style: *style, target: link }),
            Inline::Link { id, children } => {
                flatten(children, link.or(Some(Target::Link(*id))), fields, out, anchors)
            }
            Inline::Field { id } => {
                let text = fields.get(*id).cloned().unwrap_or_default();
                add_seg(out, Seg { text: format!(" {text} "), style: TextStyle::default(), target: Some(Target::Field(*id)) })
            }
            Inline::Anchor(a) => anchors.push(a.clone()),
            Inline::Break => add_seg(out, Seg { text: " ".into(), style: TextStyle::default(), target: None }),
        }
    }
}

fn trim_segs(segs: &mut Vec<Seg>) {
    while let Some(first) = segs.first_mut() {
        first.text = first.text.trim_start().to_string();
        if first.text.is_empty() {
            segs.remove(0);
        } else {
            break;
        }
    }
    while let Some(last) = segs.last_mut() {
        last.text = last.text.trim_end().to_string();
        if last.text.is_empty() {
            segs.pop();
        } else {
            break;
        }
    }
}

fn segs_width(segs: &[Seg]) -> usize {
    segs.iter().map(|s| width(&s.text)).sum()
}

/// Inner width of a text box, in columns.
const FIELD_WIDTH: usize = 24;

/// How a form control looks on the page: `[ text____ ]` for a box (the tail
/// of the value if it is longer than the box, `*`s for a password) and
/// `< Caption >` for a submit button.
fn field_text(f: &FieldInfo) -> String {
    match f.kind {
        FieldKind::Submit => format!("< {} >", f.label),
        FieldKind::Hidden => String::new(),
        FieldKind::Text | FieldKind::Password => {
            let shown: String = if f.kind == FieldKind::Password {
                "*".repeat(f.value.chars().count())
            } else {
                f.value.replace(['\n', '\r', '\t'], " ")
            };
            let mut tail: Vec<char> = Vec::new();
            let mut w = 0;
            for ch in shown.chars().rev() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > FIELD_WIDTH {
                    break;
                }
                tail.push(ch);
                w += cw;
            }
            tail.reverse();
            let text: String = tail.into_iter().collect();
            format!("[ {text}{} ]", "_".repeat(FIELD_WIDTH - w))
        }
    }
}

/// `hide_chrome` drops menu/sidebar/footer blocks (they live in the side
/// pane instead).
pub fn layout(doc: &Doc, cols: usize, hide_chrome: bool) -> Layout {
    let mut e = Emitter {
        lines: Vec::new(),
        heading_lines: vec![0; doc.headings.len()],
        anchors: HashMap::new(),
        frames: Vec::new(),
        width: cols.max(10),
        group: 0,
        last_blank: true,
        tight: 0,
        depth: 0,
        hide_chrome,
        fields: doc.fields.iter().map(field_text).collect(),
    };
    e.blocks(&doc.blocks);
    while e
        .lines
        .last()
        .is_some_and(|l| l.segs.iter().all(|s| s.text.trim().is_empty() || s.text.trim() == "│"))
    {
        e.lines.pop();
    }
    if e.lines.is_empty() {
        e.lines.push(LaidLine { segs: Vec::new(), group: 0 });
    }
    Layout { lines: e.lines, heading_lines: e.heading_lines, anchors: e.anchors }
}

impl Emitter {
    fn avail(&self) -> usize {
        let used: usize = self.frames.iter().map(|f| f.width).sum();
        self.width.saturating_sub(used).max(1)
    }

    fn next_group(&mut self) {
        self.group += 1;
    }

    fn push_line(&mut self, segs: Vec<Seg>) {
        let mut line = Vec::new();
        for frame in &mut self.frames {
            let prefix = frame.lead.take().unwrap_or_else(|| frame.cont.clone());
            for s in prefix {
                add_seg(&mut line, s);
            }
        }
        for s in segs {
            add_seg(&mut line, s);
        }
        self.lines.push(LaidLine { segs: line, group: self.group });
        self.last_blank = false;
    }

    /// Emits a separating blank line, unless one is already there, we're
    /// inside a tight list item, or an item bullet is still waiting for its
    /// first line.
    fn blank(&mut self) {
        if self.tight > 0
            || self.last_blank
            || self.lines.is_empty()
            || self.frames.iter().any(|f| f.lead.is_some())
        {
            return;
        }
        let mut line = Vec::new();
        for frame in &self.frames {
            for s in &frame.cont {
                add_seg(&mut line, s.clone());
            }
        }
        self.next_group();
        self.lines.push(LaidLine { segs: line, group: self.group });
        self.last_blank = true;
    }

    fn register_anchor(&mut self, id: &str) {
        let at = self.lines.len();
        self.anchors.entry(id.to_string()).or_insert(at);
    }

    fn blocks(&mut self, blocks: &[Block]) {
        for b in blocks {
            self.block(b);
        }
    }

    fn block(&mut self, block: &Block) {
        match block {
            Block::Anchor(id) => self.register_anchor(id),
            Block::Chrome(_, blocks) => {
                if !self.hide_chrome {
                    self.blocks(blocks);
                }
            }
            Block::Paragraph(inlines) => {
                self.blank();
                self.next_group();
                self.flow(inlines, 0);
                self.blank();
            }
            Block::Heading { idx, level, inlines } => {
                self.blank();
                self.next_group();
                if let Some(slot) = self.heading_lines.get_mut(*idx) {
                    *slot = self.lines.len();
                }
                self.flow(inlines, *level);
                self.blank();
            }
            Block::Pre(text) => self.pre(text),
            Block::Rule => {
                self.blank();
                self.next_group();
                let n = self.avail();
                self.push_line(vec![seg(&"─".repeat(n), dim())]);
                self.blank();
            }
            Block::Quote(blocks) => {
                self.blank();
                let bar = vec![seg("│ ", dim())];
                self.frames.push(Frame { lead: Some(bar.clone()), cont: bar, width: 2 });
                self.blocks(blocks);
                self.frames.pop();
                self.blank();
            }
            Block::List { ordered, start, items } => self.list(*ordered, *start, items),
            Block::Table(rows) => self.table(rows),
        }
    }

    fn pre(&mut self, text: &str) {
        self.blank();
        let code = TextStyle { code: true, ..TextStyle::default() };
        for src in text.lines() {
            self.next_group();
            let src = src.replace('\t', "    ");
            let avail = self.avail();
            let mut chunk = String::new();
            let mut w = 0;
            for ch in src.chars() {
                let cw = ch.width().unwrap_or(0);
                if w + cw > avail && w > 0 {
                    self.push_line(vec![seg(&chunk, code)]);
                    chunk.clear();
                    w = 0;
                }
                chunk.push(ch);
                w += cw;
            }
            self.push_line(vec![seg(&chunk, code)]);
        }
        self.blank();
    }

    fn list(&mut self, ordered: bool, start: usize, items: &[Vec<Block>]) {
        self.blank();
        let label_w = if ordered {
            format!("{}. ", start + items.len().saturating_sub(1)).chars().count()
        } else {
            2
        };
        for (i, item) in items.iter().enumerate() {
            let label = if ordered {
                format!("{:<w$}", format!("{}. ", start + i), w = label_w)
            } else {
                format!("{} ", ["•", "◦", "▪"][self.depth % 3])
            };
            let w = width(&label);
            self.frames.push(Frame {
                lead: Some(vec![seg(&label, dim())]),
                cont: vec![seg(&" ".repeat(w), TextStyle::default())],
                width: w,
            });
            let nested = item.iter().filter(|b| !matches!(b, Block::List { .. } | Block::Anchor(_))).count();
            let tight = nested <= 1;
            if tight {
                self.tight += 1;
            }
            self.depth += 1;
            self.blocks(item);
            self.depth -= 1;
            if tight {
                self.tight -= 1;
            }
            self.frames.pop();
            if !tight {
                self.blank();
            }
        }
        self.blank();
    }

    fn table(&mut self, rows: &[Vec<Cell>]) {
        self.blank();
        let ncols = rows.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut flat: Vec<Vec<(Vec<Seg>, Vec<String>)>> = Vec::new();
        for row in rows {
            let mut cells = Vec::new();
            for cell in row {
                let mut segs = Vec::new();
                let mut anchors = Vec::new();
                flatten(&cell.inlines, None, &self.fields, &mut segs, &mut anchors);
                trim_segs(&mut segs);
                cells.push((segs, anchors));
            }
            flat.push(cells);
        }
        let mut widths = vec![0usize; ncols];
        for row in &flat {
            for (c, (segs, _)) in row.iter().enumerate() {
                widths[c] = widths[c].max(segs_width(segs));
            }
        }
        let total = widths.iter().sum::<usize>() + 3 * ncols.saturating_sub(1);
        if ncols >= 2 && total <= self.avail() {
            let sep_style = dim();
            for (r, row) in flat.iter().enumerate() {
                self.next_group();
                let mut line = Vec::new();
                #[allow(clippy::needless_range_loop)]
                for c in 0..ncols {
                    if c > 0 {
                        add_seg(&mut line, seg(" │ ", sep_style));
                    }
                    if let Some((segs, anchors)) = row.get(c) {
                        for a in anchors {
                            self.register_anchor(a);
                        }
                        for s in segs {
                            add_seg(&mut line, s.clone());
                        }
                        if c + 1 < ncols {
                            let pad = widths[c].saturating_sub(segs_width(segs));
                            if pad > 0 {
                                add_seg(&mut line, seg(&" ".repeat(pad), TextStyle::default()));
                            }
                        }
                    } else if c + 1 < ncols {
                        add_seg(&mut line, seg(&" ".repeat(widths[c]), TextStyle::default()));
                    }
                }
                self.push_line(line);
                if r == 0 && flat.len() > 1 && rows[0].iter().all(|c| c.header) {
                    let rule = widths.iter().map(|w| "─".repeat(*w)).collect::<Vec<_>>().join("─┼─");
                    self.push_line(vec![seg(&rule, sep_style)]);
                }
            }
        } else {
            for row in rows {
                self.next_group();
                let mut inlines: Vec<Inline> = Vec::new();
                for (c, cell) in row.iter().enumerate() {
                    if c > 0 {
                        inlines.push(Inline::Text { text: " | ".into(), style: TextStyle::default() });
                    }
                    inlines.extend(cell.inlines.iter().cloned());
                }
                self.flow(&inlines, 0);
            }
        }
        self.blank();
    }

    /// Word-wraps inlines into the current frame width. Fragments with no
    /// whitespace between them (`foo<b>bar</b>`) stay together as one word.
    fn flow(&mut self, inlines: &[Inline], heading: u8) {
        let mut toks = Vec::new();
        tokenize(inlines, None, heading, &self.fields, &mut toks);

        let mut cur = Cur { line: Vec::new(), w: 0, space: false, started: false };
        let mut word: Vec<Part> = Vec::new();
        let mut pending: Vec<String> = Vec::new();

        for tok in toks {
            match tok {
                Tok::Frag { text, style, link } => word.push((text, style, link)),
                Tok::Anchor(a) => pending.push(a),
                Tok::Space => {
                    self.place_word(&mut cur, &mut word, &mut pending);
                    cur.space = true;
                }
                Tok::Break => {
                    self.place_word(&mut cur, &mut word, &mut pending);
                    if cur.w > 0 || cur.started {
                        self.flush_cur(&mut cur);
                    }
                }
            }
        }
        self.place_word(&mut cur, &mut word, &mut pending);
        for a in pending.drain(..) {
            self.register_anchor(&a);
        }
        if cur.w > 0 {
            self.flush_cur(&mut cur);
        }
    }

    fn flush_cur(&mut self, cur: &mut Cur) {
        let segs = std::mem::take(&mut cur.line);
        self.push_line(segs);
        cur.w = 0;
        cur.space = false;
        cur.started = true;
    }

    fn place_word(&mut self, cur: &mut Cur, word: &mut Vec<Part>, pending: &mut Vec<String>) {
        if word.is_empty() {
            return;
        }
        let parts = std::mem::take(word);
        let ww: usize = parts.iter().map(|(t, _, _)| width(t)).sum();
        let avail = self.avail();
        let mut sp = if cur.space && cur.w > 0 { 1 } else { 0 };
        if cur.w > 0 && cur.w + sp + ww > avail {
            self.flush_cur(cur);
            sp = 0;
        }
        for a in pending.drain(..) {
            self.register_anchor(&a);
        }
        if sp == 1 {
            let prev = cur.line.last().and_then(|s| s.target);
            let link = if prev == parts[0].2 { prev } else { None };
            add_seg(&mut cur.line, Seg { text: " ".into(), style: TextStyle::default(), target: link });
            cur.w += 1;
        }
        if ww <= avail {
            for (text, style, link) in parts {
                cur.w += width(&text);
                add_seg(&mut cur.line, Seg { text, style, target: link });
            }
        } else {
            // Longer than a whole line (long URL, etc.): break anywhere.
            for (text, style, link) in parts {
                for ch in text.chars() {
                    let cw = ch.width().unwrap_or(0);
                    if cur.w > 0 && cur.w + cw > avail {
                        self.flush_cur(cur);
                    }
                    add_seg(&mut cur.line, Seg { text: ch.to_string(), style, target: link });
                    cur.w += cw;
                }
            }
        }
        cur.space = false;
        cur.started = true;
    }
}
