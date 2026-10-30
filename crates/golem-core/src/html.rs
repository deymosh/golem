//! HTML, read the way golem's web tools need it: a page's main content as
//! compact Markdown, and a search page as its results.
//!
//! The model never sees HTML. A page is tokenized, built into a forgiving
//! tree (browsers' implied closes - `<p>`, `<li>`, table rows - and nothing
//! fancier), stripped of what is not content (scripts, styles, navigation,
//! headers and footers, asides, forms, cookie banners, anything hidden), and
//! the main content is picked: `<main>` or `role="main"`, else the one
//! `<article>`, else the block the most paragraph text sits in. What is left
//! is rendered as Markdown - headings, paragraphs, lists, tables and code
//! blocks kept verbatim, links as their text only, because a URL per link is
//! tokens a 12 GB card pays for in prefill.

/// One node of the tree: an element or a run of text.
#[derive(Debug, Clone)]
enum Node {
    Element { name: String, attrs: Vec<(String, String)>, children: Vec<usize> },
    Text(String),
}

/// A parsed page. Node 0 is the document itself.
#[derive(Debug, Clone)]
pub struct Document {
    nodes: Vec<Node>,
    /// Each node's parent; the document's is [`NONE`].
    parents: Vec<usize>,
}

/// No node.
const NONE: usize = usize::MAX;

/// Elements that never have content.
const VOID: [&str; 15] = [
    "area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source",
    "track", "wbr", "keygen",
];

/// Elements whose content is text up to their own end tag.
const RAW_TEXT: [&str; 4] = ["script", "style", "textarea", "title"];

/// Elements that end an open `<p>`.
const CLOSES_P: [&str; 32] = [
    "address", "article", "aside", "blockquote", "details", "div", "dl", "fieldset", "figcaption",
    "figure", "footer", "form", "h1", "h2", "h3", "h4", "h5", "h6", "header", "hr", "main", "menu",
    "nav", "ol", "p", "pre", "section", "table", "ul", "dd", "dt", "li",
];

/// How deep the tree may nest before new elements stop opening. Rendering
/// recurses, and a page is someone else's input.
const MAX_DEPTH: usize = 256;

impl Document {
    /// Parse a page. Never fails: malformed markup becomes whatever tree the
    /// forgiving rules make of it.
    pub fn parse(html: &str) -> Document {
        let mut doc = Document {
            nodes: vec![Node::Element { name: "#document".into(), attrs: Vec::new(), children: Vec::new() }],
            parents: vec![NONE],
        };
        let mut stack: Vec<usize> = vec![0];
        for token in tokenize(html) {
            match token {
                Token::Text(text) => {
                    let parent = *stack.last().unwrap_or(&0);
                    doc.push(parent, Node::Text(text));
                }
                Token::Start { name, attrs, self_closing } => {
                    doc.close_implied(&mut stack, &name);
                    let parent = *stack.last().unwrap_or(&0);
                    let id = doc.push(parent, Node::Element { name: name.clone(), attrs, children: Vec::new() });
                    if !(VOID.contains(&name.as_str()) || self_closing) && stack.len() < MAX_DEPTH {
                        stack.push(id);
                    }
                }
                Token::End(name) => {
                    if name == "br" {
                        let parent = *stack.last().unwrap_or(&0);
                        doc.push(parent, Node::Element { name: "br".into(), attrs: Vec::new(), children: Vec::new() });
                        continue;
                    }
                    if let Some(at) = stack.iter().rposition(|&id| doc.name(id) == name) {
                        if at > 0 {
                            stack.truncate(at);
                        }
                    }
                }
            }
        }
        doc
    }

    fn push(&mut self, parent: usize, node: Node) -> usize {
        let id = self.nodes.len();
        self.nodes.push(node);
        self.parents.push(parent);
        if let Node::Element { children, .. } = &mut self.nodes[parent] {
            children.push(id);
        }
        id
    }

    /// The closes a browser implies before `name` opens: a block ends an open
    /// paragraph, a list item the one before it, a cell or a row the last.
    fn close_implied(&self, stack: &mut Vec<usize>, name: &str) {
        let close_to = |stack: &mut Vec<usize>, targets: &[&str], boundary: &[&str]| {
            for at in (1..stack.len()).rev() {
                let open = self.name(stack[at]);
                if boundary.contains(&open) {
                    return;
                }
                if targets.contains(&open) {
                    stack.truncate(at);
                    return;
                }
            }
        };
        if CLOSES_P.contains(&name) {
            close_to(stack, &["p"], &["div", "section", "article", "main", "td", "th", "li", "body", "blockquote"]);
        }
        match name {
            "li" => close_to(stack, &["li"], &["ul", "ol", "menu"]),
            "dt" | "dd" => close_to(stack, &["dt", "dd"], &["dl"]),
            "tr" => close_to(stack, &["tr"], &["table", "tbody", "thead", "tfoot"]),
            "td" | "th" => close_to(stack, &["td", "th"], &["tr", "table"]),
            "thead" | "tbody" | "tfoot" => close_to(stack, &["thead", "tbody", "tfoot"], &["table"]),
            "option" => close_to(stack, &["option"], &["select", "datalist"]),
            _ => {}
        }
    }

    fn name(&self, id: usize) -> &str {
        match &self.nodes[id] {
            Node::Element { name, .. } => name,
            Node::Text(_) => "#text",
        }
    }

    fn attr(&self, id: usize, key: &str) -> Option<&str> {
        match &self.nodes[id] {
            Node::Element { attrs, .. } => attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str()),
            Node::Text(_) => None,
        }
    }

    fn children(&self, id: usize) -> &[usize] {
        match &self.nodes[id] {
            Node::Element { children, .. } => children,
            Node::Text(_) => &[],
        }
    }

    fn has_class(&self, id: usize, class: &str) -> bool {
        self.attr(id, "class").is_some_and(|c| c.split_whitespace().any(|t| t == class))
    }

    /// Every element under `id`, depth first, in document order.
    fn descendants(&self, id: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut todo: Vec<usize> = self.children(id).iter().rev().copied().collect();
        while let Some(next) = todo.pop() {
            if matches!(self.nodes[next], Node::Element { .. }) {
                out.push(next);
                todo.extend(self.children(next).iter().rev());
            }
        }
        out
    }

    fn find(&self, id: usize, pred: &dyn Fn(usize) -> bool) -> Option<usize> {
        self.descendants(id).into_iter().find(|&n| pred(n))
    }

    /// All the text under `id`, whitespace collapsed.
    fn text(&self, id: usize) -> String {
        let mut raw = String::new();
        self.collect_text(id, &mut raw);
        collapse(&raw)
    }

    fn collect_text(&self, id: usize, out: &mut String) {
        match &self.nodes[id] {
            Node::Text(text) => out.push_str(text),
            Node::Element { name, children, .. } => {
                if matches!(name.as_str(), "script" | "style" | "template") {
                    return;
                }
                if name == "br" || !is_inline(name) {
                    out.push(' ');
                }
                for &child in children {
                    self.collect_text(child, out);
                }
                if !is_inline(name) {
                    out.push(' ');
                }
            }
        }
    }

    /// The page's title: `<title>`, else `og:title`, else the first `<h1>`.
    pub fn title(&self) -> String {
        if let Some(t) = self.find(0, &|n| self.name(n) == "title").map(|n| self.text(n)) {
            if !t.is_empty() {
                return t;
            }
        }
        if let Some(t) = self.meta("og:title") {
            return t;
        }
        self.find(0, &|n| self.name(n) == "h1").map(|n| self.text(n)).unwrap_or_default()
    }

    /// A `<meta name|property=key content=...>` value.
    pub fn meta(&self, key: &str) -> Option<String> {
        let id = self.find(0, &|n| {
            self.name(n) == "meta" && (self.attr(n, "name") == Some(key) || self.attr(n, "property") == Some(key))
        })?;
        let content = collapse(self.attr(id, "content")?);
        (!content.is_empty()).then_some(content)
    }

    /// Whether the page runs scripts - with next to no text of its own, the
    /// sign of a page that only renders in a browser.
    pub fn has_scripts(&self) -> bool {
        self.find(0, &|n| matches!(self.name(n), "script" | "noscript")).is_some()
    }

    /// Whether an element is not content: scripts and styles, navigation,
    /// asides, forms, hidden elements, and the classes and ids cookie banners,
    /// share bars, menus and adverts are marked with.
    fn is_noise(&self, id: usize) -> bool {
        let name = self.name(id);
        if matches!(
            name,
            "script" | "style" | "noscript" | "template" | "svg" | "canvas" | "iframe" | "object"
                | "embed" | "nav" | "aside" | "form" | "button" | "select" | "input" | "textarea"
                | "dialog" | "footer" | "head" | "title" | "meta" | "link" | "img" | "picture"
                | "video" | "audio" | "map"
        ) {
            return true;
        }
        // A header carries the article's own title more often than not; one
        // without a heading is the site's banner.
        if name == "header" {
            return self.find(id, &|n| matches!(self.name(n), "h1" | "h2")).is_none();
        }
        if self.attr(id, "hidden").is_some() || self.attr(id, "aria-hidden") == Some("true") {
            return true;
        }
        if let Some(style) = self.attr(id, "style") {
            let style: String = style.to_ascii_lowercase().chars().filter(|c| !c.is_whitespace()).collect();
            if style.contains("display:none") || style.contains("visibility:hidden") {
                return true;
            }
        }
        if let Some(role) = self.attr(id, "role") {
            if matches!(
                role,
                "navigation" | "banner" | "contentinfo" | "complementary" | "search" | "dialog"
                    | "menu" | "menubar" | "toolbar" | "alertdialog"
            ) {
                return true;
            }
        }
        let marks = [self.attr(id, "class").unwrap_or(""), self.attr(id, "id").unwrap_or("")];
        marks.iter().any(|mark| mark.split_whitespace().any(noise_token))
    }

    /// The element the page's content is in.
    fn main_content(&self) -> usize {
        let body = self.find(0, &|n| self.name(n) == "body").unwrap_or(0);
        let substantial = |n: usize| self.text(n).chars().count() >= 200;
        if let Some(main) = self.find(body, &|n| {
            (self.name(n) == "main" || self.attr(n, "role") == Some("main")) && substantial(n)
        }) {
            return main;
        }
        let articles: Vec<usize> =
            self.descendants(body).into_iter().filter(|&n| self.name(n) == "article").collect();
        if articles.len() == 1 && substantial(articles[0]) {
            return articles[0];
        }
        self.densest(body).unwrap_or(body)
    }

    /// The block the most paragraph text sits in: each run of text of 25
    /// characters or more, outside links and outside noise, counts for the
    /// container its paragraph sits in and half for the one around that.
    fn densest(&self, root: usize) -> Option<usize> {
        let mut score = vec![0usize; self.nodes.len()];
        let mut todo = vec![(root, false)];
        while let Some((id, in_link)) = todo.pop() {
            for &child in self.children(id) {
                match &self.nodes[child] {
                    Node::Text(text) => {
                        let len = collapse(text).chars().count();
                        if len < 25 || in_link {
                            continue;
                        }
                        let container = self.container_of(child);
                        score[container] += len;
                        if let Some(&up) = self.parents.get(container).filter(|&&up| up != NONE) {
                            score[up] += len / 2;
                        }
                    }
                    Node::Element { name, .. } => {
                        if !self.is_noise(child) {
                            todo.push((child, in_link || name == "a"));
                        }
                    }
                }
            }
        }
        (0..self.nodes.len()).filter(|&n| score[n] > 0).max_by_key(|&n| (score[n], NONE - n))
    }

    /// The element that holds a run of text's paragraph: the nearest
    /// ancestor that is neither inline nor a paragraph-like block itself.
    fn container_of(&self, mut id: usize) -> usize {
        loop {
            let up = self.parents[id];
            if up == NONE {
                return id;
            }
            let name = self.name(up);
            let paragraph = matches!(
                name,
                "p" | "pre" | "li" | "td" | "th" | "dd" | "dt" | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            );
            if !is_inline(name) && !paragraph {
                return up;
            }
            id = up;
        }
    }

    /// The links in the page's main content, in page order: each one's text
    /// and `href` as written, at most `max`, one per address, without
    /// in-page anchors, `javascript:` and `mailto:` - the list `golem_web_fetch` gives
    /// after the text when it is asked for links.
    pub fn links(&self, max: usize) -> Vec<(String, String)> {
        let root = self.main_content();
        let mut out: Vec<(String, String)> = Vec::new();
        let mut todo = vec![root];
        // Depth first in page order: children pushed in reverse.
        while let Some(id) = todo.pop() {
            if out.len() >= max {
                break;
            }
            if id != root && self.is_noise(id) {
                continue;
            }
            if self.name(id) == "a" {
                let href = self.attr(id, "href").unwrap_or("").trim();
                let text = self.text(id);
                if !href.is_empty()
                    && !href.starts_with('#')
                    && !href.to_ascii_lowercase().starts_with("javascript:")
                    && !href.to_ascii_lowercase().starts_with("mailto:")
                    && !out.iter().any(|(_, h)| h == href)
                {
                    out.push((if text.is_empty() { href.to_string() } else { text }, href.to_string()));
                }
                continue;
            }
            for &child in self.children(id).iter().rev() {
                if matches!(self.nodes[child], Node::Element { .. }) {
                    todo.push(child);
                }
            }
        }
        out
    }

    /// The page's main content as Markdown.
    pub fn markdown(&self) -> String {
        let root = self.main_content();
        let mut out = Markdown::default();
        self.render_children(root, &mut out, 0);
        out.finish()
    }

    // ---- rendering ----------------------------------------------------

    fn render_children(&self, id: usize, out: &mut Markdown, depth: usize) {
        let mut line = String::new();
        for &child in self.children(id) {
            match &self.nodes[child] {
                Node::Text(text) => push_text(&mut line, text),
                Node::Element { name, .. } => {
                    if self.is_noise(child) {
                        continue;
                    }
                    if is_inline(name) {
                        self.inline(child, &mut line);
                    } else {
                        out.paragraph(&line);
                        line.clear();
                        self.render_block(child, out, depth + 1);
                    }
                }
            }
        }
        out.paragraph(&line);
    }

    fn render_block(&self, id: usize, out: &mut Markdown, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let name = self.name(id);
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = (name.as_bytes()[1] - b'0') as usize;
                let text = self.inline_text(id);
                if !text.is_empty() {
                    out.block(format!("{} {text}", "#".repeat(level)));
                }
            }
            "pre" => {
                let mut code = String::new();
                self.collect_raw(id, &mut code);
                let code = code.trim_matches('\n').trim_end();
                if !code.trim().is_empty() {
                    let fence = if code.contains("```") { "````" } else { "```" };
                    out.block(format!("{fence}{}\n{code}\n{fence}", self.code_language(id)));
                }
            }
            "ul" | "ol" | "menu" => {
                let ordered = name == "ol";
                let mut items = Vec::new();
                self.list_items(id, ordered, 0, &mut items);
                if !items.is_empty() {
                    out.block(items.join("\n"));
                }
            }
            "table" => self.render_table(id, out),
            "blockquote" => {
                let mut inner = Markdown::default();
                self.render_children(id, &mut inner, depth);
                let text = inner.finish();
                if !text.is_empty() {
                    out.block(text.lines().map(|l| if l.is_empty() { ">".to_string() } else { format!("> {l}") }).collect::<Vec<_>>().join("\n"));
                }
            }
            "hr" | "br" => {}
            _ => self.render_children(id, out, depth),
        }
    }

    /// A list's items as Markdown lines, nested lists indented under their
    /// item.
    fn list_items(&self, id: usize, ordered: bool, indent: usize, items: &mut Vec<String>) {
        let mut n = 0;
        for &child in self.children(id) {
            if self.name(child) != "li" || self.is_noise(child) {
                continue;
            }
            n += 1;
            let marker = if ordered { format!("{n}.") } else { "-".to_string() };
            let pad = "  ".repeat(indent);
            let mut line = String::new();
            let mut nested = Vec::new();
            let mut blocks = Vec::new();
            for &part in self.children(child) {
                match &self.nodes[part] {
                    Node::Text(text) => push_text(&mut line, text),
                    Node::Element { .. } if self.is_noise(part) => {}
                    Node::Element { name, .. } => match name.as_str() {
                        "ul" | "ol" | "menu" => self.list_items(part, name == "ol", indent + 1, &mut nested),
                        "pre" | "table" | "blockquote" => {
                            let mut inner = Markdown::default();
                            self.render_block(part, &mut inner, 0);
                            blocks.push(inner.finish());
                        }
                        n if is_inline(n) => self.inline(part, &mut line),
                        _ => {
                            line.push(' ');
                            line.push_str(&self.inline_text(part));
                            line.push(' ');
                        }
                    },
                }
            }
            let text = collapse_lines(&line);
            items.push(format!("{pad}{marker} {text}").trim_end().to_string());
            for block in blocks {
                for l in block.lines() {
                    items.push(format!("{pad}  {l}").trim_end().to_string());
                }
            }
            items.extend(nested);
        }
    }

    fn render_table(&self, id: usize, out: &mut Markdown) {
        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut todo: Vec<usize> = self.children(id).iter().rev().copied().collect();
        while let Some(next) = todo.pop() {
            match self.name(next) {
                "tr" => {
                    let cells: Vec<String> = self
                        .children(next)
                        .iter()
                        .filter(|&&c| matches!(self.name(c), "td" | "th"))
                        .map(|&c| self.inline_text(c).replace('|', "\\|"))
                        .collect();
                    if cells.iter().any(|c| !c.is_empty()) {
                        rows.push(cells);
                    }
                }
                "thead" | "tbody" | "tfoot" => todo.extend(self.children(next).iter().rev()),
                "caption" => {
                    let text = self.inline_text(next);
                    out.paragraph(&text);
                }
                _ => {}
            }
        }
        let width = rows.iter().map(Vec::len).max().unwrap_or(0);
        if width < 2 {
            // A one-column table is layout, not data.
            for row in rows {
                for cell in row {
                    out.paragraph(&cell);
                }
            }
            return;
        }
        let mut lines = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let mut cells = row.clone();
            cells.resize(width, String::new());
            lines.push(format!("| {} |", cells.join(" | ")));
            if i == 0 {
                lines.push(format!("|{}", " --- |".repeat(width)));
            }
        }
        out.block(lines.join("\n"));
    }

    /// The language a code block names in its classes or its wrapper's:
    /// `language-x`, `lang-x`, or Sphinx's `highlight-x`.
    fn code_language(&self, pre: usize) -> String {
        let mut candidates = vec![pre];
        candidates.extend(self.children(pre).iter().filter(|&&c| self.name(c) == "code"));
        let parent = self.parents[pre];
        if parent != NONE {
            candidates.push(parent);
            if self.parents[parent] != NONE {
                candidates.push(self.parents[parent]);
            }
        }
        for id in candidates {
            for class in self.attr(id, "class").unwrap_or("").split_whitespace() {
                for prefix in ["language-", "lang-", "highlight-"] {
                    if let Some(lang) = class.strip_prefix(prefix) {
                        let lang: String = lang.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '+' || *c == '#').collect();
                        if !lang.is_empty() && lang != "default" && lang != "none" {
                            return lang;
                        }
                    }
                }
            }
        }
        String::new()
    }

    /// The text of a `<pre>`, whitespace kept.
    fn collect_raw(&self, id: usize, out: &mut String) {
        for &child in self.children(id) {
            match &self.nodes[child] {
                Node::Text(text) => out.push_str(text),
                Node::Element { name, .. } if name == "br" => out.push('\n'),
                Node::Element { name, .. } if matches!(name.as_str(), "script" | "style") => {}
                Node::Element { .. } => self.collect_raw(child, out),
            }
        }
    }

    /// Inline content of an element as one line of Markdown.
    fn inline_text(&self, id: usize) -> String {
        let mut line = String::new();
        for &child in self.children(id) {
            match &self.nodes[child] {
                Node::Text(text) => push_text(&mut line, text),
                Node::Element { .. } if self.is_noise(child) => {}
                Node::Element { name, .. } if is_inline(name) => self.inline(child, &mut line),
                Node::Element { .. } => {
                    line.push(' ');
                    line.push_str(&self.inline_text(child));
                    line.push(' ');
                }
            }
        }
        collapse_lines(&line)
    }

    /// One inline element, appended to the line being built.
    fn inline(&self, id: usize, line: &mut String) {
        if self.is_noise(id) {
            return;
        }
        match self.name(id) {
            "br" => line.push('\n'),
            "code" | "kbd" | "samp" | "tt" => {
                let text = self.text(id);
                if !text.is_empty() {
                    let tick = if text.contains('`') { "``" } else { "`" };
                    line.push_str(&format!("{tick}{text}{tick}"));
                }
            }
            _ => {
                for &child in self.children(id) {
                    match &self.nodes[child] {
                        Node::Text(text) => push_text(line, text),
                        Node::Element { .. } => self.inline(child, line),
                    }
                }
            }
        }
    }
}

/// Whether an element flows inside a line of text.
fn is_inline(name: &str) -> bool {
    matches!(
        name,
        "a" | "abbr" | "b" | "bdi" | "bdo" | "big" | "br" | "cite" | "code" | "data" | "del" | "dfn"
            | "em" | "font" | "i" | "img" | "ins" | "kbd" | "label" | "mark" | "nobr" | "q" | "s"
            | "samp" | "small" | "span" | "strike" | "strong" | "sub" | "sup" | "time" | "tt" | "u"
            | "var" | "wbr" | "#text"
    )
}

/// A class or id that marks what is not content.
fn noise_token(token: &str) -> bool {
    let token = token.to_ascii_lowercase();
    const WHOLE: [&str; 18] = [
        "mw-editsection", "mw-jump-link", "navbox", "catlinks", "printfooter", "noprint", "toc",
        "sphinxsidebar", "skip-link", "skiplink", "visually-hidden", "sr-only", "screen-reader-text",
        "mw-indicators", "headerlink", "anchor-link", "anchorjs-link", "heading-anchor",
    ];
    const PART: [&str; 23] = [
        "cookie", "cookies", "consent", "gdpr", "newsletter", "subscribe", "share", "sharing",
        "social", "sidebar", "breadcrumb", "breadcrumbs", "navbar", "navigation", "menu", "advert",
        "advertisement", "ad", "ads", "sponsored", "promo", "popup", "modal",
    ];
    WHOLE.contains(&token.as_str()) || token.split(['-', '_']).any(|part| PART.contains(&part))
}

/// A run of text appended to a line: its own newlines are whitespace in
/// HTML, so only a `<br>` breaks the line.
fn push_text(line: &mut String, text: &str) {
    line.extend(text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }));
}

/// Whitespace runs collapsed to one space, trimmed.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Like [`collapse`], keeping the line breaks a `<br>` made.
fn collapse_lines(text: &str) -> String {
    text.split('\n').map(collapse).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n")
}

/// Markdown being built a block at a time.
#[derive(Default)]
struct Markdown {
    blocks: Vec<String>,
}

impl Markdown {
    fn paragraph(&mut self, line: &str) {
        let text = collapse_lines(line);
        if !text.is_empty() {
            self.blocks.push(text);
        }
    }

    fn block(&mut self, text: String) {
        self.blocks.push(text);
    }

    fn finish(self) -> String {
        self.blocks.join("\n\n")
    }
}

// ---------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------

enum Token {
    Text(String),
    Start { name: String, attrs: Vec<(String, String)>, self_closing: bool },
    End(String),
}

fn tokenize(html: &str) -> Vec<Token> {
    let bytes = html.as_bytes();
    let mut tokens = Vec::new();
    let mut at = 0;
    let mut text_start = 0;
    let flush = |tokens: &mut Vec<Token>, from: usize, to: usize| {
        if to > from {
            tokens.push(Token::Text(decode_entities(&html[from..to])));
        }
    };
    while at < bytes.len() {
        if bytes[at] != b'<' {
            at += 1;
            continue;
        }
        let rest = &html[at..];
        if rest.starts_with("<!--") {
            flush(&mut tokens, text_start, at);
            at = rest.find("-->").map_or(bytes.len(), |end| at + end + 3);
            text_start = at;
            continue;
        }
        let next = bytes.get(at + 1).copied().unwrap_or(b' ');
        if next == b'!' || next == b'?' {
            flush(&mut tokens, text_start, at);
            at = rest.find('>').map_or(bytes.len(), |end| at + end + 1);
            text_start = at;
            continue;
        }
        let closing = next == b'/';
        let name_from = at + if closing { 2 } else { 1 };
        if !bytes.get(name_from).is_some_and(u8::is_ascii_alphabetic) {
            at += 1;
            continue;
        }
        flush(&mut tokens, text_start, at);
        let (tag_end, name, attrs, self_closing) = read_tag(html, name_from);
        at = tag_end;
        text_start = at;
        if closing {
            tokens.push(Token::End(name));
            continue;
        }
        let raw = RAW_TEXT.contains(&name.as_str());
        tokens.push(Token::Start { name: name.clone(), attrs, self_closing });
        if raw && !self_closing {
            let end = find_end_tag(html, at, &name);
            let content = &html[at..end];
            if !content.is_empty() {
                let text = if name == "script" || name == "style" { content.to_string() } else { decode_entities(content) };
                tokens.push(Token::Text(text));
            }
            tokens.push(Token::End(name));
            at = html[end..].find('>').map_or(html.len(), |e| end + e + 1);
            text_start = at;
        }
    }
    flush(&mut tokens, text_start, bytes.len());
    tokens
}

/// Where `</name` next starts at or after `from`, any case; the end of the
/// page when it never does.
fn find_end_tag(html: &str, from: usize, name: &str) -> usize {
    let bytes = html.as_bytes();
    let mut at = from;
    while let Some(offset) = html[at..].find("</") {
        let tag = at + offset;
        let name_at = tag + 2;
        if bytes.len() >= name_at + name.len() && bytes[name_at..name_at + name.len()].eq_ignore_ascii_case(name.as_bytes()) {
            return tag;
        }
        at = tag + 2;
    }
    html.len()
}

/// A tag from its name to its `>`: where it ends, its lower-cased name, its
/// attributes (values entity-decoded), and whether it closed itself.
fn read_tag(html: &str, from: usize) -> (usize, String, Vec<(String, String)>, bool) {
    let bytes = html.as_bytes();
    let mut at = from;
    while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' && bytes[at] != b'/' {
        at += 1;
    }
    let name = html[from..at].to_ascii_lowercase();
    let mut attrs = Vec::new();
    let mut self_closing = false;
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at >= bytes.len() {
            return (at, name, attrs, self_closing);
        }
        match bytes[at] {
            b'>' => return (at + 1, name, attrs, self_closing),
            b'/' => {
                self_closing = true;
                at += 1;
                continue;
            }
            _ => {}
        }
        self_closing = false;
        let key_from = at;
        while at < bytes.len() && !bytes[at].is_ascii_whitespace() && !matches!(bytes[at], b'=' | b'>' | b'/') {
            at += 1;
        }
        let key = html[key_from..at].to_ascii_lowercase();
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        let mut value = String::new();
        if bytes.get(at) == Some(&b'=') {
            at += 1;
            while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                at += 1;
            }
            match bytes.get(at) {
                Some(&q) if q == b'"' || q == b'\'' => {
                    let end = html[at + 1..].find(q as char).map_or(bytes.len(), |e| at + 1 + e);
                    value = decode_entities(&html[at + 1..end]);
                    at = (end + 1).min(bytes.len());
                }
                _ => {
                    let value_from = at;
                    while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                        at += 1;
                    }
                    value = decode_entities(&html[value_from..at]);
                }
            }
        }
        if !key.is_empty() {
            attrs.push((key, value));
        } else {
            at += 1;
        }
    }
}

/// Character references decoded: numeric ones, and the named ones pages use.
pub fn decode_entities(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let end = rest[1..].find(|c: char| !(c.is_ascii_alphanumeric() || c == '#')).map(|e| e + 1);
        let (body, used) = match end {
            Some(e) if rest[e..].starts_with(';') => (&rest[1..e], e + 1),
            Some(e) => (&rest[1..e], e),
            None => (&rest[1..], rest.len()),
        };
        // Invisible formatting characters are dropped: they are noise to a
        // model and a trap in a copied identifier.
        if used > 1 && matches!(body, "zwj" | "zwnj" | "shy" | "lrm" | "rlm" | "#8203" | "#x200b" | "#x200B") {
            rest = &rest[used..];
            continue;
        }
        match entity(body) {
            Some(c) if used > 1 => {
                out.push(c);
                rest = &rest[used..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(body: &str) -> Option<char> {
    if let Some(num) = body.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code).filter(|&c| c != '\0');
    }
    Some(match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "middot" => '·',
        "bull" => '•',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "deg" => '°',
        "times" => '×',
        "divide" => '÷',
        "euro" => '€',
        "pound" => '£',
        "yen" => '¥',
        "cent" => '¢',
        "sect" => '§',
        "para" => '¶',
        "larr" => '←',
        "rarr" => '→',
        "uarr" => '↑',
        "darr" => '↓',
        "le" => '≤',
        "ge" => '≥',
        "ne" => '≠',
        "minus" => '−',
        "plusmn" => '±',
        _ => return None,
    })
}

// ---------------------------------------------------------------
// DuckDuckGo
// ---------------------------------------------------------------

/// One search result, as the model and the client see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// The results of DuckDuckGo's no-JavaScript page (`html.duckduckgo.com/html/`),
/// at most `max`: ads dropped, DuckDuckGo's `/l/?uddg=` redirect unwrapped to
/// the real address, duplicates removed, each snippet cut near 300
/// characters.
pub fn duckduckgo_results(page: &str, max: usize) -> Vec<SearchResult> {
    let doc = Document::parse(page);
    let mut results: Vec<SearchResult> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for id in doc.descendants(0) {
        if results.len() >= max {
            break;
        }
        if !(doc.has_class(id, "result") || doc.has_class(id, "web-result")) || doc.has_class(id, "result--ad") {
            continue;
        }
        let Some(link) = doc.find(id, &|n| doc.has_class(n, "result__a")) else { continue };
        let href = doc.attr(link, "href").unwrap_or("");
        if href.contains("duckduckgo.com/y.js") || href.contains("ad_domain=") {
            continue;
        }
        let url = unwrap_redirect(href);
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            continue;
        }
        let key = dedupe_key(&url);
        if seen.contains(&key) {
            continue;
        }
        let title = doc.text(link);
        if title.is_empty() {
            continue;
        }
        let snippet = doc
            .find(id, &|n| doc.has_class(n, "result__snippet"))
            .map(|n| cut(&doc.text(n), 300))
            .unwrap_or_default();
        seen.push(key);
        results.push(SearchResult { title, url, snippet });
    }
    results
}

/// Whether a DuckDuckGo page is its bot check rather than an answer.
pub fn duckduckgo_blocked(page: &str) -> bool {
    page.contains("anomaly-modal") || page.contains("challenge-form") || page.contains("/anomaly.js")
}

/// A DuckDuckGo result link with its redirect taken off.
pub fn unwrap_redirect(href: &str) -> String {
    let href = href.trim();
    let is_redirect = ["//duckduckgo.com/l/?", "https://duckduckgo.com/l/?", "/l/?"]
        .iter()
        .any(|p| href.starts_with(p));
    if is_redirect {
        if let Some(query) = href.split_once('?').map(|(_, q)| q) {
            for pair in query.split('&') {
                if let Some(value) = pair.strip_prefix("uddg=") {
                    return percent_decode(value);
                }
            }
        }
    }
    if let Some(rest) = href.strip_prefix("//") {
        return format!("https://{rest}");
    }
    href.to_string()
}

/// `%XX` escapes (and `+` as a space) decoded; a broken escape is kept as
/// it is.
pub fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    while at < bytes.len() {
        match bytes[at] {
            b'%' if at + 2 < bytes.len() => match (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                (Some(high), Some(low)) => {
                    out.push(high << 4 | low);
                    at += 3;
                }
                _ => {
                    out.push(b'%');
                    at += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            b => {
                out.push(b);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The form two links share when they are the same page.
fn dedupe_key(url: &str) -> String {
    let lowered = url.to_ascii_lowercase();
    let rest = lowered.split_once("://").map_or(lowered.as_str(), |(_, r)| r);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let rest = rest.split('#').next().unwrap_or(rest);
    rest.trim_end_matches('/').to_string()
}

/// `text` cut to about `max` characters at a word, with an ellipsis.
pub fn cut(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max).collect();
    let head = match head.rfind(' ') {
        Some(at) if at > max / 2 => &head[..at],
        _ => head.as_str(),
    };
    format!("{}…", head.trim_end_matches([',', '.', ';', ':', ' ']))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn md(html: &str) -> String {
        Document::parse(html).markdown()
    }

    #[test]
    fn entities_decode_and_a_bare_ampersand_stays() {
        assert_eq!(decode_entities("a &amp; b &lt;c&gt; &#39;d&#x27; &nbsp;e"), "a & b <c> 'd'  e");
        assert_eq!(decode_entities("read_&shy;to_&#8203;string"), "read_to_string");
        assert_eq!(decode_entities("AT&T & co &unknown; &#0;"), "AT&T & co &unknown; &#0;");
        assert_eq!(decode_entities("&eacute"), "&eacute");
        assert_eq!(decode_entities("x&mdash;y"), "x—y");
    }

    #[test]
    fn headings_paragraphs_lists_and_code_survive() {
        let page = "<html><head><title>T</title><style>p{}</style></head><body><main>\
            <h1>Reading files</h1><p>Use <code>read_to_string</code> for   small files.<p>Second\nparagraph, \
            long enough to count as content for the extractor's threshold of two hundred characters, \
            which is what a real page easily has and this one has to pad out a little bit more.</p>\
            <ul><li>one<li>two <b>bold</b><ul><li>nested</li></ul></ul>\
            <ol><li>first</li><li>second</li></ol>\
            <div class=\"highlight-python3\"><pre>def f():\n    return 1 &lt; 2\n</pre></div>\
            </main></body></html>";
        let text = md(page);
        assert!(text.starts_with("# Reading files\n\nUse `read_to_string` for small files.\n\nSecond paragraph"), "{text}");
        assert!(text.contains("- one\n- two bold\n  - nested"), "{text}");
        assert!(text.contains("1. first\n2. second"), "{text}");
        assert!(text.contains("```python3\ndef f():\n    return 1 < 2\n```"), "{text}");
        assert!(!text.contains("p{}"), "{text}");
    }

    #[test]
    fn noise_is_dropped_and_the_main_element_wins() {
        let filler = "Real content sentence that is long enough to matter. ".repeat(6);
        let page = format!(
            "<body><header><a>Logo</a><nav>Home About</nav></header>\
             <div class=\"cookie-banner\">We use cookies</div>\
             <main><article><p>{filler}</p><aside>Related stuff</aside>\
             <div class=\"share-bar\">Share this</div><p hidden>secret</p>\
             <form><input value=\"x\">Search</form></article></main>\
             <footer>Copyright</footer><script>var x = '<p>no</p>';</script></body>"
        );
        let text = md(&page);
        assert!(text.starts_with("Real content sentence"), "{text}");
        for gone in ["Logo", "Home", "cookies", "Related", "Share", "secret", "Search", "Copyright", "var x"] {
            assert!(!text.contains(gone), "{gone} should be gone: {text}");
        }
    }

    #[test]
    fn without_landmarks_the_densest_block_is_the_content() {
        let body = "This is the article body, written in sentences of some length. ".repeat(5);
        let page = format!(
            "<body><div id=\"top\"><a href=\"/\">a link that is long enough to count otherwise</a></div>\
             <div class=\"wrap\"><div class=\"story\"><p>{body}</p><p>{body}</p></div>\
             <div class=\"more\"><p>A short teaser paragraph that is still over 25 chars.</p></div></div></body>"
        );
        let text = md(&page);
        assert!(text.starts_with("This is the article body"), "{text}");
        assert!(!text.contains("a link that is long"), "{text}");
    }

    #[test]
    fn a_data_table_becomes_markdown_and_a_layout_table_does_not() {
        let page = "<main><table><tr><th>JSON</th><th>Python</th></tr><tr><td>object</td><td>dict</td></tr>\
                    <tr><td>a|b</td><td>list</td></tr></table>\
                    <table><tr><td>Just one column of layout text here.</td></tr></table>\
                    <p>Padding so that the main element has two hundred characters of text in it, which is \
                    the threshold below which golem goes looking for a better block on the page.</p></main>";
        let text = md(page);
        assert!(text.contains("| JSON | Python |\n| --- | --- |\n| object | dict |\n| a\\|b | list |"), "{text}");
        assert!(text.contains("\n\nJust one column of layout text here.\n\n"), "{text}");
    }

    #[test]
    fn the_title_comes_from_the_title_element_or_the_page() {
        assert_eq!(Document::parse("<title> A &amp; B </title>").title(), "A & B");
        assert_eq!(
            Document::parse("<meta property=\"og:title\" content=\"OG\"><h1>H</h1>").title(),
            "OG"
        );
        assert_eq!(Document::parse("<h1>Only <i>h1</i></h1>").title(), "Only h1");
    }

    #[test]
    fn unclosed_and_stray_tags_do_not_lose_text() {
        let text = md("<div><p>one<p>two</span></div></div><p>three");
        assert_eq!(text, "one\n\ntwo\n\nthree");
    }

    #[test]
    fn a_duckduckgo_redirect_unwraps_to_the_real_address() {
        assert_eq!(
            unwrap_redirect("//duckduckgo.com/l/?uddg=https%3A%2F%2Fwww.rtings.com%2Frunning%2Dshoes&amp;rut=8d08"),
            "https://www.rtings.com/running-shoes"
        );
        assert_eq!(unwrap_redirect("https://doc.rust-lang.org/std/"), "https://doc.rust-lang.org/std/");
        assert_eq!(percent_decode("a%20b+c%zz%"), "a b c%zz%");
    }

    #[test]
    fn ads_and_duplicates_are_dropped_and_snippets_cut() {
        let long = "word ".repeat(100);
        let page = format!(
            "<div class=\"result results_links result--ad\"><a class=\"result__a\" href=\"https://duckduckgo.com/y.js?ad_domain=x\">Ad</a></div>\
             <div class=\"result results_links web-result\"><h2><a class=\"result__a\" href=\"//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%2F&rut=1\">Example &amp; A</a></h2>\
             <a class=\"result__snippet\">An <b>example</b> snippet.</a></div>\
             <div class=\"result web-result\"><a class=\"result__a\" href=\"https://www.example.com/a\">Same page</a></div>\
             <div class=\"result web-result\"><a class=\"result__a\" href=\"https://b.example/\">B</a><a class=\"result__snippet\">{long}</a></div>"
        );
        let results = duckduckgo_results(&page, 8);
        assert_eq!(results.len(), 2, "{results:?}");
        assert_eq!(
            results[0],
            SearchResult { title: "Example & A".into(), url: "https://example.com/a/".into(), snippet: "An example snippet.".into() }
        );
        assert!(results[1].snippet.ends_with('…') && results[1].snippet.chars().count() <= 301, "{}", results[1].snippet);
        assert_eq!(duckduckgo_results(&page, 1).len(), 1);
    }
}
