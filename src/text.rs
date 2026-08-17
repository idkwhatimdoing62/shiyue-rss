//! HTML→有序 文字/图片 块 + 时间格式化（ADR-16）。
//! ponytail: 够读就行，不追求完整 HTML 渲染；坏在这里也只是排版丑，不会崩。

/// 正文按 HTML 解析出的一个有序单元。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineLinkRange {
    pub url: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineTextRange {
    pub start: usize,
    pub end: usize,
}

/// One semantic table cell. Spans are retained instead of flattening the
/// source table, so the renderer can reserve the same logical columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCell {
    pub text: String,
    pub row_span: usize,
    pub col_span: usize,
    pub header: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionItem {
    pub term: String,
    pub definitions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Text(String),
    Strong(String),
    /// Inline HTML `<code>` that remains part of the surrounding sentence.
    /// The renderer gives it a compact monospace treatment without turning it
    /// into a separate paragraph.
    InlineCode(String),
    /// HTML `h1`–`h6` 标题；与正文中的行内加粗分开保留。
    Heading(String),
    HeadingWithInlineCode {
        text: String,
        inline_code_ranges: Vec<InlineTextRange>,
    },
    /// A heading whose visible text is also an anchor. Keeping this separate
    /// lets index/guide pages retain both their card hierarchy and navigation.
    HeadingLink {
        text: String,
        links: Vec<InlineLinkRange>,
    },
    /// HTML `blockquote` 引用块。块内的行内标签会被折叠为可读文本，
    /// 但引用边界会保留给渲染层。
    Quote(String),
    /// 没有语言提示的 HTML `pre` / `code` 代码块。
    Code(String),
    /// 带语言提示的代码块。语言来自 `class="language-rust"`、
    /// `class="lang-js"` 或 `data-language` 等常见标记。
    CodeBlock {
        text: String,
        language: String,
    },
    /// 带有原始目标地址的 HTML 链接。
    Link {
        /// Text that appears before the anchor in the same HTML paragraph.
        /// It is kept in the same block so `---- <a>来源</a>` stays on one
        /// line, while `link_start` lets the UI style only the anchor text.
        text: String,
        url: String,
        link_start: usize,
        /// Literal whitespace followed the closing anchor before the next
        /// inline text. This preserves `<a>docs</a> and` without breaking
        /// `<a>modif</a>y`.
        space_after: bool,
    },
    /// Structural boundary for one HTML list item. The markers keep inline
    /// fragments inside the item without letting the renderer absorb the
    /// paragraph that follows `</li>`.
    ListItemStart {
        depth: usize,
    },
    ListItemEnd {
        depth: usize,
    },
    /// 绝对图片 URL（已按文章 base 补全）。
    Image(String),
    /// 图片位于链接中。保留图片目标与点击地址，尤其用于专题页的图片标题卡片。
    LinkedImage {
        uri: String,
        url: String,
        alt: Option<String>,
    },
    /// Text attached to the image immediately before it by `<figcaption>`.
    Caption(String),
    /// HTML definition list, retaining the term/definition relationship.
    DefinitionList(Vec<DefinitionItem>),
    /// HTML 表格的可读投影。`header_rows` 表示从头开始有多少行含 `<th>`。
    Table {
        rows: Vec<Vec<TableCell>>,
        header_rows: usize,
        column_count: usize,
    },
    /// MathML、KaTeX/MathJax 容器或 `math/tex` script 中保留下来的公式源码。
    Math {
        source: String,
        display: bool,
    },
}

const BLOCK_TAGS: &[&str] = &["p", "br", "div", "li", "tr"];
const HEADING_INLINE_CODE_START: char = '\u{e000}';
const HEADING_INLINE_CODE_END: char = '\u{e001}';

/// Elements that belong to the surrounding web application rather than to
/// the article a reader wants to keep.  Filtering these here also protects
/// ordinary RSS entries whose payload happens to contain a complete HTML
/// document instead of an article fragment.
const IGNORED_ELEMENTS: &[&str] = &[
    "head", "title", "base", "meta", "link", "script", "style", "noscript", "svg", "nav", "header",
    "footer", "aside", "form", "iframe",
];

/// The useful parts extracted from a complete HTML document before it is
/// stored as a local reading snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HtmlSnapshot {
    /// Page title, using `og:title`, `<title>` and `<h1>` in that order.
    pub title: Option<String>,
    /// Article-oriented HTML, with page chrome and executable content removed.
    pub content: String,
    /// The document's declared `<base href>`, if present.
    pub base_href: Option<String>,
}

struct LinkState {
    url: String,
    prefix: String,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompletePageKind {
    SingleArticle,
    IndexOrDocument,
}

struct Html5ReadingScope {
    content: String,
    kind: CompletePageKind,
}

#[derive(Default)]
struct SemanticProfile {
    text_chars: usize,
    images: usize,
    figures: usize,
    captions: usize,
    definitions: usize,
    tables: usize,
    table_cells: usize,
    spanning_cells: usize,
    lists: usize,
    links: usize,
    code_blocks: usize,
    math_nodes: usize,
}

/// Prepare a complete web page for local storage and later rendering.
///
/// The page's `<main>`, then `<body>`, and finally the full input determines
/// the broad reading scope.  A single `<article>` inside that scope narrows it
/// further; multiple articles mean the page is probably an index and are kept
/// together. Script, style and surrounding navigation elements are removed
/// even when the caller supplied only an HTML fragment.
pub fn prepare_html_snapshot(html: &str) -> HtmlSnapshot {
    let title = extract_html_title(html);
    let base_href = extract_html_base_href(html);
    let scope = html5_reading_scope(html);
    let scoped_content = strip_ignored_elements(&scope.content);
    let readable = match scope.kind {
        CompletePageKind::SingleArticle => {
            guarded_readability_content(html, base_href.as_deref(), &scoped_content)
                .unwrap_or(scoped_content)
        }
        // Readability is designed to pick one dominant article. An index or
        // guide page intentionally contains several useful cards, so it must
        // retain the HTML5 DOM scope instead of competing for a single winner.
        CompletePageKind::IndexOrDocument => scoped_content,
    };
    let readable = remove_duplicate_leading_h1(&readable, title.as_deref());

    HtmlSnapshot {
        title,
        content: readable.trim().to_owned(),
        base_href,
    }
}

/// Select the broad reading scope with a browser-grade HTML5 parser.
///
/// This is deliberately only called by `prepare_html_snapshot`: RSS payloads
/// continue to enter `content_blocks` as fragments and never pass through
/// Readability. A single `<article>` is a safe candidate for Readability;
/// zero or multiple articles are treated as a document/index and kept whole.
fn html5_reading_scope(html: &str) -> Html5ReadingScope {
    use scraper::{ElementRef, Html, Selector};

    fn largest<'a>(elements: impl Iterator<Item = ElementRef<'a>>) -> Option<ElementRef<'a>> {
        elements.max_by_key(|element| {
            element.text().map(str::len).sum::<usize>() + element.inner_html().len() / 8
        })
    }

    let document = Html::parse_document(html);
    let main = Selector::parse("main").expect("static selector is valid");
    let body = Selector::parse("body").expect("static selector is valid");
    let article = Selector::parse("article").expect("static selector is valid");
    let broad = largest(document.select(&main)).or_else(|| largest(document.select(&body)));

    let Some(broad) = broad else {
        return Html5ReadingScope {
            content: html.to_owned(),
            kind: CompletePageKind::IndexOrDocument,
        };
    };
    let mut articles = broad.select(&article);
    let first = articles.next();
    let second = articles.next();
    match (first, second) {
        (Some(article), None) => Html5ReadingScope {
            content: article.inner_html(),
            kind: CompletePageKind::SingleArticle,
        },
        _ => Html5ReadingScope {
            content: broad.inner_html(),
            kind: CompletePageKind::IndexOrDocument,
        },
    }
}

/// Readability is an optional selector behind a strict semantic gate. It is
/// accepted only when it keeps the text and every structure that Shiyue knows
/// how to render. Otherwise the repaired HTML5 DOM scope remains authoritative.
fn guarded_readability_content(
    complete_html: &str,
    base_url: Option<&str>,
    html5_scope: &str,
) -> Option<String> {
    let mut readability = dom_smoothie::Readability::new(
        complete_html,
        base_url,
        Some(dom_smoothie::Config {
            char_threshold: 0,
            ..Default::default()
        }),
    )
    .ok()?;
    let article = readability.parse().ok()?;
    let candidate = strip_ignored_elements(article.content.as_ref());
    readability_candidate_is_safe(html5_scope, &candidate).then_some(candidate)
}

fn readability_candidate_is_safe(source: &str, candidate: &str) -> bool {
    let source_text = normalized_dom_text(source);
    let candidate_text = normalized_dom_text(candidate);
    let source = semantic_profile(source);
    let candidate = semantic_profile(candidate);
    let allowed_extra_text = (source.text_chars / 10).max(8);
    if source.text_chars == 0
        || candidate.text_chars * 4 < source.text_chars * 3
        || candidate.text_chars > source.text_chars + allowed_extra_text
        || !candidate_text.contains(&source_text)
    {
        return false;
    }

    candidate.images >= source.images
        && candidate.figures >= source.figures
        && candidate.captions >= source.captions
        && candidate.definitions >= source.definitions
        && candidate.tables >= source.tables
        && candidate.table_cells >= source.table_cells
        && candidate.spanning_cells >= source.spanning_cells
        && candidate.lists >= source.lists
        && candidate.links >= source.links
        && candidate.code_blocks >= source.code_blocks
        && candidate.math_nodes >= source.math_nodes
}

fn normalized_dom_text(html: &str) -> String {
    scraper::Html::parse_fragment(html)
        .root_element()
        .text()
        .flat_map(str::chars)
        .filter(|ch| !ch.is_whitespace())
        .collect()
}

fn semantic_profile(html: &str) -> SemanticProfile {
    use scraper::{Html, Selector};

    fn count(document: &Html, selector: &str) -> usize {
        document
            .select(&Selector::parse(selector).expect("static selector is valid"))
            .count()
    }

    let document = Html::parse_fragment(html);
    let text_chars = normalized_dom_text(html).chars().count();
    SemanticProfile {
        text_chars,
        images: count(&document, "img"),
        figures: count(&document, "figure"),
        captions: count(&document, "figcaption"),
        definitions: count(&document, "dl, dt, dd"),
        tables: count(&document, "table"),
        table_cells: count(&document, "th, td"),
        spanning_cells: count(
            &document,
            "th[rowspan], th[colspan], td[rowspan], td[colspan]",
        ),
        lists: count(&document, "ol, ul, li"),
        links: count(&document, "a[href]"),
        code_blocks: count(&document, "pre, code"),
        math_nodes: count(&document, "math, script[type^='math/tex']"),
    }
}

/// Extract a human-facing page title without retaining any HTML markup.
pub fn extract_html_title(html: &str) -> Option<String> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let metadata =
        Selector::parse("meta[property], meta[name]").expect("static metadata selector is valid");
    for element in document.select(&metadata) {
        let kind = element
            .value()
            .attr("property")
            .or_else(|| element.value().attr("name"));
        if kind.is_some_and(|value| value.trim().eq_ignore_ascii_case("og:title"))
            && let Some(title) = element
                .value()
                .attr("content")
                .and_then(|value| clean_title(value.to_owned()))
        {
            return Some(title);
        }
    }

    for selector in ["title", "h1"] {
        let selector = Selector::parse(selector).expect("static title selector is valid");
        if let Some(title) = document
            .select(&selector)
            .filter_map(|element| clean_title(element.text().collect::<String>()))
            .max_by_key(String::len)
        {
            return Some(title);
        }
    }
    None
}

/// Read the first declared `<base href>` from a complete HTML document.
pub fn extract_html_base_href(html: &str) -> Option<String> {
    use scraper::{Html, Selector};

    let document = Html::parse_document(html);
    let selector = Selector::parse("base[href]").expect("static base selector is valid");
    let href = document
        .select(&selector)
        .next()?
        .value()
        .attr("href")?
        .trim();
    (!href.is_empty()).then(|| href.to_owned())
}

#[derive(Clone, Copy)]
struct HtmlTagToken {
    start: usize,
    end: usize,
    closing: bool,
    self_closing: bool,
}

#[derive(Clone, Copy)]
struct HtmlElementBounds {
    outer_start: usize,
    outer_end: usize,
    inner_start: usize,
    inner_end: usize,
}

fn remove_duplicate_leading_h1<'a>(html: &'a str, title: Option<&str>) -> &'a str {
    let Some(title) = title else {
        return html;
    };
    let Some(first_content) = first_meaningful_content_offset(html) else {
        return html;
    };
    let Some(h1) = element_bounds(html, "h1").into_iter().next() else {
        return html;
    };
    if h1.outer_start != first_content {
        return html;
    }
    let h1_title = clean_title(html[h1.inner_start..h1.inner_end].to_owned());
    if h1_title.as_deref() == Some(title) {
        &html[h1.outer_end..]
    } else {
        html
    }
}

fn first_meaningful_content_offset(html: &str) -> Option<usize> {
    let mut cursor = 0;
    loop {
        let remaining = &html[cursor..];
        let trimmed = remaining.trim_start_matches(char::is_whitespace);
        cursor += remaining.len() - trimmed.len();
        if trimmed.starts_with("<!--") {
            let end = trimmed.find("-->")? + 3;
            cursor += end;
            continue;
        }
        return (!trimmed.is_empty()).then_some(cursor);
    }
}

fn clean_title(raw: String) -> Option<String> {
    let plain = strip_all_tags(&raw);
    let title = decode_entities(&plain)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then_some(title)
}

fn strip_all_tags(html: &str) -> String {
    let mut output = String::with_capacity(html.len());
    let mut cursor = 0;
    while let Some(relative) = html[cursor..].find('<') {
        let start = cursor + relative;
        output.push_str(&html[cursor..start]);
        match tag_end(html, start) {
            Some(end) => cursor = end,
            None => {
                output.push_str(&html[start..]);
                return output;
            }
        }
    }
    output.push_str(&html[cursor..]);
    output
}

fn strip_ignored_elements(html: &str) -> String {
    use scraper::{Html, Selector};

    let mut document = Html::parse_fragment(html);
    let all = Selector::parse("*").expect("static universal selector is valid");
    let removable = document
        .select(&all)
        .filter(|element| {
            let name = element.value().name();
            IGNORED_ELEMENTS.contains(&name)
                && !(name == "script" && is_math_script_element(element.value()))
        })
        .map(|element| element.id())
        .chain(
            document
                .tree
                .nodes()
                .filter(|node| node.value().is_comment())
                .map(|node| node.id()),
        )
        .collect::<Vec<_>>();

    for id in removable {
        if let Some(mut node) = document.tree.get_mut(id) {
            node.detach();
        }
    }
    document.root_element().inner_html()
}

fn normalize_responsive_pictures(html: &str) -> String {
    use scraper::{ElementRef, Html, Node, Selector};

    let mut document = Html::parse_fragment(html);
    let pictures = Selector::parse("picture").expect("static picture selector is valid");
    let images = Selector::parse("img").expect("static image selector is valid");
    let sources = Selector::parse("source").expect("static source selector is valid");
    let picture_ids = document
        .select(&pictures)
        .map(|picture| picture.id())
        .collect::<Vec<_>>();

    for picture_id in picture_ids {
        let Some(picture_node) = document.tree.get(picture_id) else {
            continue;
        };
        let Some(picture) = ElementRef::wrap(picture_node) else {
            continue;
        };
        let image_nodes = picture
            .select(&images)
            .filter(|image| dom_image_src(image.value()).is_some())
            .map(|image| {
                (
                    image.id(),
                    image.value().attr("srcset").is_some()
                        || image.value().attr("data-srcset").is_some(),
                )
            })
            .collect::<Vec<_>>();
        let source_nodes = picture
            .select(&sources)
            .filter_map(|source| dom_image_src(source.value()).map(|url| (source.id(), url)))
            .collect::<Vec<_>>();
        let responsive_image = image_nodes
            .iter()
            .find_map(|(id, responsive)| responsive.then_some(*id));
        let fallback_image = image_nodes.first().map(|(id, _)| *id);
        let selected_source = source_nodes.last().cloned();

        let (kept, source_replacement) = if let Some(id) = responsive_image {
            (Some(id), None)
        } else if let Some((source_id, ref url)) = selected_source {
            if let Some(image_id) = fallback_image {
                (Some(image_id), Some((image_id, url.clone())))
            } else {
                (Some(source_id), None)
            }
        } else {
            (fallback_image, None)
        };

        if let Some((image_id, url)) = source_replacement
            && let Some(mut node) = document.tree.get_mut(image_id)
            && let Node::Element(element) = node.value()
        {
            set_existing_dom_image_url(element, &url);
        }
        if let Some((source_id, _)) = selected_source
            && kept == Some(source_id)
            && let Some(mut node) = document.tree.get_mut(source_id)
            && let Node::Element(element) = node.value()
        {
            element.name.local = "img".into();
        }

        for id in image_nodes
            .into_iter()
            .map(|(id, _)| id)
            .chain(source_nodes.into_iter().map(|(id, _)| id))
        {
            if Some(id) != kept
                && let Some(mut node) = document.tree.get_mut(id)
            {
                node.detach();
            }
        }
    }

    document.root_element().inner_html()
}

fn is_math_script_tag(tag: &str) -> bool {
    attr(tag, "type").is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("math/tex"))
}

fn is_math_script_element(element: &scraper::node::Element) -> bool {
    element
        .attr("type")
        .is_some_and(|value| value.trim().to_ascii_lowercase().starts_with("math/tex"))
}

fn dom_image_src(element: &scraper::node::Element) -> Option<String> {
    for name in [
        "data-original",
        "data-lazy-src",
        "data-src",
        "data-url",
        "data-image",
    ] {
        if let Some(value) = element.attr(name).filter(|value| usable_image_url(value)) {
            return Some(value.to_owned());
        }
    }
    for name in ["data-srcset", "srcset"] {
        if let Some(value) = element.attr(name)
            && let Some(candidate) = srcset_largest(value)
        {
            return Some(candidate);
        }
    }
    element
        .attr("src")
        .filter(|value| usable_image_url(value))
        .map(str::to_owned)
}

fn set_existing_dom_image_url(element: &mut scraper::node::Element, url: &str) -> bool {
    for attribute in [
        "data-original",
        "data-lazy-src",
        "data-src",
        "data-url",
        "data-image",
        "src",
    ] {
        if let Some((_, value)) = element
            .attrs
            .iter_mut()
            .find(|(name, _)| name.local.as_ref() == attribute)
        {
            *value = url.into();
            return true;
        }
    }
    false
}

fn element_bounds(html: &str, name: &str) -> Vec<HtmlElementBounds> {
    let mut stack = Vec::new();
    let mut candidates = Vec::new();
    let mut cursor = 0;
    while let Some(token) = next_named_tag(html, name, cursor) {
        cursor = token.end;
        if token.closing {
            if let Some((outer_start, inner_start)) = stack.pop() {
                candidates.push(HtmlElementBounds {
                    outer_start,
                    outer_end: token.end,
                    inner_start,
                    inner_end: token.start,
                });
            }
        } else if !token.self_closing {
            stack.push((token.start, token.end));
        }
    }
    for (outer_start, inner_start) in stack {
        candidates.push(HtmlElementBounds {
            outer_start,
            outer_end: html.len(),
            inner_start,
            inner_end: html.len(),
        });
    }
    candidates.sort_unstable_by_key(|bounds| bounds.outer_start);
    candidates
}

fn next_named_tag(html: &str, name: &str, from: usize) -> Option<HtmlTagToken> {
    let bytes = html.as_bytes();
    let mut cursor = from.min(bytes.len());
    while cursor < bytes.len() {
        let relative = html[cursor..].find('<')?;
        let start = cursor + relative;
        let mut at = start + 1;
        let closing = bytes.get(at) == Some(&b'/');
        if closing {
            at += 1;
        }
        while bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            at += 1;
        }
        let name_start = at;
        while bytes
            .get(at)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b':' | b'_'))
        {
            at += 1;
        }
        let end = tag_end(html, start)?;
        if name_start < at && html[name_start..at].eq_ignore_ascii_case(name) {
            let tail = html[at..end.saturating_sub(1)].trim_end();
            return Some(HtmlTagToken {
                start,
                end,
                closing,
                self_closing: tail.ends_with('/'),
            });
        }
        cursor = end;
    }
    None
}

fn tag_end(html: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (relative, ch) in html[start + 1..].char_indices() {
        match (quote, ch) {
            (Some(expected), current) if current == expected => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, '>') => return Some(start + 1 + relative + ch.len_utf8()),
            _ => {}
        }
    }
    None
}

/// 把正文 HTML 拆成有序的 文字块 / 图片块，图片按它在原文里的位置穿插。
/// `base` 是文章 URL，用来把相对 `<img src>` 补成绝对地址。
pub fn content_blocks(html: &str, base: Option<&str>) -> Vec<Block> {
    // Read `<base>` before removing `<head>`, and resolve a relative base
    // against the caller-provided page URL.  This keeps full-page snapshots
    // working while preserving the old RSS-fragment behaviour.
    let declared_base = extract_html_base_href(html);
    let effective_base = declared_base
        .as_deref()
        .and_then(|href| resolve(href, base).or_else(|| Some(href.to_owned())))
        .or_else(|| base.map(str::to_owned));
    let filtered_html = strip_ignored_elements(html);
    // A `<picture>` can contain several `<source>` candidates followed by an
    // `<img>` fallback. Normalizing it to one chosen `<img>` prevents the old
    // streaming parser from rendering every candidate as a duplicate image.
    let responsive_html = normalize_responsive_pictures(&filtered_html);
    let html = responsive_html.as_str();
    let base = effective_base.as_deref();
    let mut blocks = Vec::new();
    let mut buf = String::new();
    let mut strong = false;
    let mut heading = false;
    let mut heading_link_block = None;
    let mut list_depth = 0usize;
    let mut list_item_depths = Vec::new();
    let mut quote_depth = 0usize;
    let mut quote_buf = String::new();
    let mut code_depth = 0usize;
    let mut code_buf = String::new();
    let mut code_language = None;
    let mut inline_code_depth = 0usize;
    let mut inline_code_buf = String::new();
    let mut inline_code_in_heading = false;
    let mut table_depth = 0usize;
    let mut table_buf = String::new();
    let mut definition_depth = 0usize;
    let mut definition_buf = String::new();
    let mut caption_depth = 0usize;
    let mut caption_buf = String::new();
    let mut math_container: Option<(String, usize, bool)> = None;
    let mut math_buf = String::new();
    let mut sup_depth = 0usize;
    let mut link: Option<LinkState> = None;
    let mut pending_link_boundary: Option<(usize, bool)> = None;
    let mut seen_images = std::collections::HashSet::new();
    let mut chars = html.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '<' {
            let mut tag = String::new();
            for t in chars.by_ref() {
                if t == '>' {
                    break;
                }
                tag.push(t);
            }
            let name = tag
                .trim_start_matches('/')
                .split(|ch: char| ch.is_whitespace())
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            let closing = tag.trim_start().starts_with('/');

            if definition_depth > 0 {
                if name == "dl" {
                    if closing {
                        definition_depth = definition_depth.saturating_sub(1);
                        if definition_depth == 0 {
                            flush_definition_list(&mut definition_buf, &mut blocks);
                        } else {
                            definition_buf.push_str(&format!("<{tag}>"));
                        }
                    } else {
                        definition_depth += 1;
                        definition_buf.push_str(&format!("<{tag}>"));
                    }
                } else {
                    definition_buf.push_str(&format!("<{tag}>"));
                }
                continue;
            }

            if caption_depth > 0 {
                if name == "figcaption" {
                    if closing {
                        caption_depth = caption_depth.saturating_sub(1);
                        if caption_depth == 0 {
                            flush_caption(&mut caption_buf, &mut blocks);
                        } else {
                            caption_buf.push_str(&format!("<{tag}>"));
                        }
                    } else {
                        caption_depth += 1;
                        caption_buf.push_str(&format!("<{tag}>"));
                    }
                } else {
                    caption_buf.push_str(&format!("<{tag}>"));
                }
                continue;
            }

            if table_depth > 0 {
                if name == "table" {
                    if closing {
                        table_depth = table_depth.saturating_sub(1);
                        if table_depth == 0 {
                            flush_table(&mut table_buf, &mut blocks);
                        } else {
                            table_buf.push_str(&format!("<{tag}>"));
                        }
                    } else {
                        table_depth += 1;
                        table_buf.push_str(&format!("<{tag}>"));
                    }
                } else {
                    table_buf.push_str(&format!("<{tag}>"));
                }
                continue;
            }

            if let Some((container_name, depth, display)) = math_container.as_mut() {
                if name == *container_name {
                    if closing {
                        *depth = depth.saturating_sub(1);
                        if *depth == 0 {
                            let display = *display;
                            math_container = None;
                            flush_math(&mut math_buf, &mut blocks, display);
                        } else {
                            math_buf.push_str(&format!("<{tag}>"));
                        }
                    } else {
                        *depth += 1;
                        math_buf.push_str(&format!("<{tag}>"));
                    }
                } else {
                    math_buf.push_str(&format!("<{tag}>"));
                }
                continue;
            }

            if name == "table" && !closing {
                if let Some(state) = link.take() {
                    finish_link(state, &mut buf, &mut blocks);
                }
                flush_text_kind(&mut buf, &mut blocks, strong, heading);
                table_depth = 1;
                table_buf.clear();
                continue;
            }

            if name == "dl" && !closing {
                if let Some(state) = link.take() {
                    finish_link(state, &mut buf, &mut blocks);
                }
                flush_text_kind(&mut buf, &mut blocks, strong, heading);
                definition_depth = 1;
                definition_buf.clear();
                continue;
            }

            if name == "figcaption" && !closing {
                if let Some(state) = link.take() {
                    finish_link(state, &mut buf, &mut blocks);
                }
                flush_text_kind(&mut buf, &mut blocks, strong, heading);
                caption_depth = 1;
                caption_buf.clear();
                continue;
            }

            if !closing && let Some(display) = math_tag_display(&name, &tag) {
                if let Some(state) = link.take() {
                    finish_link(state, &mut buf, &mut blocks);
                }
                flush_text_kind(&mut buf, &mut blocks, strong, heading);
                math_container = Some((name.clone(), 1, display));
                math_buf.clear();
                continue;
            }

            if pending_link_boundary.is_some() && inline_flow_boundary(&name) {
                pending_link_boundary = None;
            }

            // Once inside a code block, HTML formatting must no longer alter
            // the surrounding parser state. `<pre><code>…</code></pre>` is
            // treated as one semantic block; `<br>` is the only formatting
            // tag that contributes visible content.
            if code_depth > 0 {
                if name == "pre" || name == "code" {
                    if closing {
                        code_depth = code_depth.saturating_sub(1);
                        if code_depth == 0 {
                            flush_code(&mut code_buf, &mut code_language, &mut blocks);
                        }
                    } else {
                        if code_language.is_none() {
                            code_language = code_language_from_tag(&tag);
                        }
                        code_depth += 1;
                    }
                } else if name == "br" && !closing {
                    code_buf.push('\n');
                }
                continue;
            }

            if inline_code_depth > 0 {
                if name == "code" {
                    if closing {
                        inline_code_depth = inline_code_depth.saturating_sub(1);
                        if inline_code_depth == 0 {
                            if inline_code_in_heading {
                                append_inline_code_to_heading(&mut inline_code_buf, &mut buf);
                                inline_code_in_heading = false;
                            } else {
                                flush_inline_code(&mut inline_code_buf, &mut blocks);
                            }
                        }
                    } else {
                        inline_code_depth += 1;
                    }
                } else if name == "br" && !closing {
                    inline_code_buf.push(' ');
                }
                continue;
            }

            // A quote owns all of its inline content. Links, emphasis and
            // paragraph tags remain readable text, while the outer
            // `blockquote` boundary becomes a single `Block::Quote`.
            if quote_depth > 0 {
                if name == "img" || name == "source" {
                    // Do not regress the existing image extraction merely
                    // because the image is nested in a quote. Split the quote
                    // around the image so document order and both semantics
                    // remain available to the renderer.
                    flush_quote(&mut quote_buf, &mut blocks);
                    if let Some(src) = image_src(&tag).and_then(|s| resolve(&s, base)) {
                        if seen_images.insert(src.clone()) {
                            blocks.push(Block::Image(src));
                        }
                    }
                } else if name == "blockquote" {
                    if closing {
                        quote_depth = quote_depth.saturating_sub(1);
                        if quote_depth == 0 {
                            flush_quote(&mut quote_buf, &mut blocks);
                        } else {
                            push_line_break(&mut quote_buf);
                        }
                    } else {
                        push_line_break(&mut quote_buf);
                        quote_depth += 1;
                    }
                } else if name == "br" && !closing {
                    quote_buf.push('\n');
                } else if name == "li" && !closing {
                    push_line_break(&mut quote_buf);
                    quote_buf.push_str("▪ ");
                } else if matches!(name.as_str(), "p" | "div" | "li" | "tr") && closing {
                    push_line_break(&mut quote_buf);
                }
                continue;
            }

            if name == "blockquote" && !closing {
                if let Some(state) = link.take() {
                    finish_link(state, &mut buf, &mut blocks);
                }
                flush_text_kind(&mut buf, &mut blocks, strong, heading);
                quote_depth = 1;
                continue;
            }

            if name == "pre" && !closing {
                if let Some(state) = link.take() {
                    finish_link(state, &mut buf, &mut blocks);
                }
                flush_text_kind(&mut buf, &mut blocks, strong, heading);
                code_language = code_language_from_tag(&tag);
                code_depth = 1;
                continue;
            }

            if name == "sup" {
                if closing {
                    sup_depth = sup_depth.saturating_sub(1);
                } else {
                    sup_depth += 1;
                }
                continue;
            }

            if name == "a" {
                if closing {
                    if let Some(mut state) = link.take() {
                        if sup_depth > 0 && is_footnote_target(&state.url) {
                            let text = clean_text(&state.text);
                            if !text.is_empty() && !text.starts_with('[') {
                                state.text = format!("[{text}]");
                            }
                        }
                        if heading {
                            heading_link_block = finish_heading_link(
                                state,
                                heading_link_block,
                                &mut buf,
                                &mut blocks,
                            );
                        } else {
                            let before = blocks.len();
                            finish_link(state, &mut buf, &mut blocks);
                            if blocks.len() > before {
                                pending_link_boundary = Some((blocks.len() - 1, false));
                            }
                        }
                    }
                } else if link.is_none() {
                    // Ignore anchors without a usable target in place. This
                    // keeps their text in the surrounding paragraph instead
                    // of splitting `前<a href="#">锚点</a>后` into two blocks.
                    if let Some(url) = link_href(&tag, base) {
                        let prefix = std::mem::take(&mut buf);
                        link = Some(LinkState {
                            url,
                            prefix,
                            text: String::new(),
                        });
                    }
                }
                continue;
            }
            if name == "img" || name == "source" {
                if let Some(src) = image_src(&tag).and_then(|s| resolve(&s, base)) {
                    let linked = link.as_ref().map(|state| state.url.clone());
                    if let Some(state) = link.take() {
                        let url = state.url.clone();
                        if heading {
                            heading_link_block = finish_heading_link(
                                state,
                                heading_link_block,
                                &mut buf,
                                &mut blocks,
                            );
                        } else {
                            finish_link(state, &mut buf, &mut blocks);
                        }
                        // Keep collecting any visible text that follows the image
                        // before the same closing anchor.
                        link = Some(LinkState {
                            url,
                            prefix: String::new(),
                            text: String::new(),
                        });
                    }
                    flush_text_kind(&mut buf, &mut blocks, strong, heading);
                    if seen_images.insert(src.clone()) {
                        let alt = attr(&tag, "alt")
                            .map(|value| clean_text(&value))
                            .filter(|value| !value.is_empty());
                        if let Some(url) = linked {
                            blocks.push(Block::LinkedImage { uri: src, url, alt });
                            if heading {
                                heading_link_block = None;
                            }
                        } else {
                            blocks.push(Block::Image(src));
                        }
                    }
                }
                continue;
            }
            if let Some(state) = link.as_mut() {
                // Inline markup inside an anchor belongs to the same link text.
                // Preserve explicit line breaks, but do not let `<strong>` split
                // the link into unrelated semantic blocks.
                if name == "br" && !closing {
                    state.text.push('\n');
                }
                continue;
            }
            if name == "code" && !closing {
                if !heading {
                    flush_text_kind(&mut buf, &mut blocks, strong, false);
                }
                inline_code_depth = 1;
                inline_code_buf.clear();
                inline_code_in_heading = heading;
                continue;
            }
            if is_html_heading_tag(&name) {
                if closing {
                    if let Some(index) = heading_link_block.take() {
                        append_heading_suffix(index, &mut buf, &mut blocks);
                    } else {
                        flush_text_kind(&mut buf, &mut blocks, strong, heading);
                    }
                    heading = false;
                } else {
                    flush_text_kind(&mut buf, &mut blocks, strong, heading);
                    heading = true;
                    heading_link_block = None;
                }
                continue;
            }
            if name == "strong" || name == "b" {
                // A nested `<strong>` changes the visual weight inside a heading,
                // but must not split one HTML heading into several semantic blocks.
                if !heading {
                    flush_text_kind(&mut buf, &mut blocks, strong, false);
                    strong = !closing;
                }
                continue;
            }
            if matches!(name.as_str(), "ul" | "ol") {
                if closing {
                    list_depth = list_depth.saturating_sub(1);
                } else {
                    list_depth += 1;
                }
                continue;
            }
            if BLOCK_TAGS.contains(&name.as_str()) {
                if name == "li" && !closing {
                    if !list_item_depths.is_empty() {
                        flush_text_kind(&mut buf, &mut blocks, strong, heading);
                    }
                    let depth = list_depth.max(1);
                    list_item_depths.push(depth);
                    blocks.push(Block::ListItemStart { depth });
                } else if name == "li" && closing {
                    flush_text_kind(&mut buf, &mut blocks, strong, heading);
                    if let Some(depth) = list_item_depths.pop() {
                        blocks.push(Block::ListItemEnd { depth });
                    }
                } else if closing {
                    flush_text_kind(&mut buf, &mut blocks, strong, heading);
                } else {
                    buf.push('\n');
                }
            }
        } else {
            if table_depth > 0 {
                table_buf.push(c);
                continue;
            }
            if definition_depth > 0 {
                definition_buf.push(c);
                continue;
            }
            if caption_depth > 0 {
                caption_buf.push(c);
                continue;
            }
            if math_container.is_some() {
                math_buf.push(c);
                continue;
            }
            if inline_code_depth > 0 {
                inline_code_buf.push(c);
                continue;
            }
            if let Some((block_index, saw_space)) = pending_link_boundary.as_mut() {
                if c.is_whitespace() || (c == '&' && consume_whitespace_entity(&mut chars)) {
                    *saw_space = true;
                    continue;
                }
                if *saw_space
                    && let Some(Block::Link { space_after, .. }) = blocks.get_mut(*block_index)
                {
                    *space_after = true;
                }
                pending_link_boundary = None;
            }
            if code_depth > 0 {
                code_buf.push(c);
            } else if quote_depth > 0 {
                quote_buf.push(c);
            } else if let Some(state) = link.as_mut() {
                state.text.push(c);
            } else {
                if !list_item_depths.is_empty() && c.is_whitespace() {
                    if !buf.is_empty() && !buf.ends_with(char::is_whitespace) {
                        buf.push(' ');
                    }
                } else {
                    buf.push(c);
                }
            }
        }
    }
    if let Some(state) = link.take() {
        finish_link(state, &mut buf, &mut blocks);
    }
    if quote_depth > 0 {
        flush_quote(&mut quote_buf, &mut blocks);
    }
    if code_depth > 0 {
        flush_code(&mut code_buf, &mut code_language, &mut blocks);
    }
    if inline_code_depth > 0 {
        if inline_code_in_heading {
            append_inline_code_to_heading(&mut inline_code_buf, &mut buf);
        } else {
            flush_inline_code(&mut inline_code_buf, &mut blocks);
        }
    }
    if table_depth > 0 {
        flush_table(&mut table_buf, &mut blocks);
    }
    if definition_depth > 0 {
        flush_definition_list(&mut definition_buf, &mut blocks);
    }
    if caption_depth > 0 {
        flush_caption(&mut caption_buf, &mut blocks);
    }
    if let Some((_, _, display)) = math_container.take() {
        flush_math(&mut math_buf, &mut blocks, display);
    }
    flush_text_kind(&mut buf, &mut blocks, strong, heading);
    while let Some(depth) = list_item_depths.pop() {
        blocks.push(Block::ListItemEnd { depth });
    }
    merge_bare_number_markers(&mut blocks);
    blocks
}

fn push_line_break(buf: &mut String) {
    if !buf.is_empty() && !buf.ends_with('\n') {
        buf.push('\n');
    }
}

fn flush_quote(buf: &mut String, blocks: &mut Vec<Block>) {
    let cleaned = clean_text(buf);
    if !cleaned.is_empty() {
        blocks.push(Block::Quote(cleaned));
    }
    buf.clear();
}

fn flush_caption(buf: &mut String, blocks: &mut Vec<Block>) {
    if let Some(caption) = clean_title(std::mem::take(buf)) {
        blocks.push(Block::Caption(caption));
    }
}

fn flush_definition_list(buf: &mut String, blocks: &mut Vec<Block>) {
    let mut entries: Vec<(usize, bool, String)> = Vec::new();
    for (name, is_term) in [("dt", true), ("dd", false)] {
        for bounds in element_bounds(buf, name) {
            if let Some(text) = clean_title(buf[bounds.inner_start..bounds.inner_end].to_owned()) {
                entries.push((bounds.outer_start, is_term, text));
            }
        }
    }
    entries.sort_unstable_by_key(|entry| entry.0);
    let mut items: Vec<DefinitionItem> = Vec::new();
    for (_, is_term, text) in entries {
        if is_term {
            items.push(DefinitionItem {
                term: text,
                definitions: Vec::new(),
            });
        } else if let Some(item) = items.last_mut() {
            item.definitions.push(text);
        }
    }
    items.retain(|item| !item.term.is_empty() && !item.definitions.is_empty());
    if !items.is_empty() {
        blocks.push(Block::DefinitionList(items));
    }
    buf.clear();
}

fn flush_code(buf: &mut String, language: &mut Option<String>, blocks: &mut Vec<Block>) {
    let decoded = decode_entities(buf)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    // Authors commonly place a newline immediately inside `<pre>` solely to
    // keep their HTML readable. Drop only those boundary newlines; indentation
    // and whitespace inside every code line remain untouched.
    let cleaned = decoded.trim_matches('\n');
    if !cleaned.is_empty() {
        if let Some(language) = language.take().filter(|value| !value.is_empty()) {
            blocks.push(Block::CodeBlock {
                text: cleaned.to_owned(),
                language,
            });
        } else {
            blocks.push(Block::Code(cleaned.to_owned()));
        }
    }
    buf.clear();
    *language = None;
}

fn flush_inline_code(buf: &mut String, blocks: &mut Vec<Block>) {
    let cleaned = clean_text(buf);
    if !cleaned.is_empty() {
        blocks.push(Block::InlineCode(cleaned));
    }
    buf.clear();
}

fn append_inline_code_to_heading(code_buf: &mut String, heading_buf: &mut String) {
    let cleaned = clean_text(code_buf);
    heading_buf.push(HEADING_INLINE_CODE_START);
    heading_buf.push_str(&cleaned);
    heading_buf.push(HEADING_INLINE_CODE_END);
    code_buf.clear();
}

fn code_language_from_tag(tag: &str) -> Option<String> {
    for name in ["data-language", "data-lang", "lang"] {
        if let Some(value) = attr(tag, name) {
            let value = value.trim().to_ascii_lowercase();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    let class = attr(tag, "class")?;
    for token in class.split_whitespace() {
        let lower = token.trim().to_ascii_lowercase();
        for prefix in ["language-", "lang-", "highlight-source-"] {
            if let Some(language) = lower.strip_prefix(prefix)
                && !language.is_empty()
            {
                return Some(language.to_owned());
            }
        }
        if let Some(language) = lower.strip_prefix("brush:") {
            let language = language.trim_end_matches(';');
            if !language.is_empty() {
                return Some(language.to_owned());
            }
        }
    }
    None
}

fn flush_table(buf: &mut String, blocks: &mut Vec<Block>) {
    let mut rows = Vec::new();
    let mut header_rows = 0usize;
    let mut column_count = 0usize;
    for row in element_bounds(buf, "tr") {
        let row_html = &buf[row.inner_start..row.inner_end];
        let mut cells: Vec<(usize, TableCell)> = Vec::new();
        for (tag_name, header) in [("th", true), ("td", false)] {
            for cell in element_bounds(row_html, tag_name) {
                if let Some(text) =
                    clean_title(row_html[cell.inner_start..cell.inner_end].to_owned())
                {
                    let opening = &row_html[cell.outer_start..cell.inner_start];
                    let row_span = positive_span_attr(opening, "rowspan");
                    let col_span = positive_span_attr(opening, "colspan");
                    cells.push((
                        cell.outer_start,
                        TableCell {
                            text,
                            row_span,
                            col_span,
                            header,
                        },
                    ));
                }
            }
        }
        cells.sort_unstable_by_key(|cell| cell.0);
        if !cells.is_empty() {
            if header_rows == rows.len() && cells.iter().any(|cell| cell.1.header) {
                header_rows += 1;
            }
            column_count = column_count.max(cells.iter().map(|cell| cell.1.col_span).sum());
            rows.push(cells.into_iter().map(|cell| cell.1).collect());
        }
    }
    if !rows.is_empty() {
        blocks.push(Block::Table {
            rows,
            header_rows,
            column_count: column_count.max(1),
        });
    }
    buf.clear();
}

fn positive_span_attr(tag: &str, name: &str) -> usize {
    attr(tag, name)
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(1)
        .clamp(1, 64)
}

/// Resolve each source cell to its first logical column while respecting
/// row-spans from previous rows. Both the egui renderer and visual snapshots
/// use this function, so regression tests exercise the same placement rules.
pub fn table_cell_columns(
    rows: &[Vec<TableCell>],
    column_count: usize,
) -> Vec<Vec<(usize, usize)>> {
    let column_count = column_count.max(1);
    let mut active_row_spans = vec![0usize; column_count];
    let mut layout = Vec::with_capacity(rows.len());

    for row in rows {
        let mut placements = Vec::with_capacity(row.len());
        let mut cursor = 0usize;
        for (cell_index, cell) in row.iter().enumerate() {
            let width = cell.col_span.min(column_count);
            while cursor < column_count {
                while cursor < column_count && active_row_spans[cursor] > 0 {
                    cursor += 1;
                }
                if cursor + width <= column_count
                    && active_row_spans[cursor..cursor + width]
                        .iter()
                        .all(|span| *span == 0)
                {
                    break;
                }
                cursor += 1;
            }
            let start = cursor.min(column_count.saturating_sub(1));
            placements.push((start, cell_index));
            if cell.row_span > 1 {
                for span in active_row_spans
                    .iter_mut()
                    .take((start + width).min(column_count))
                    .skip(start)
                {
                    *span = (*span).max(cell.row_span);
                }
            }
            cursor = (start + width).min(column_count);
        }
        layout.push(placements);
        for span in &mut active_row_spans {
            *span = span.saturating_sub(1);
        }
    }
    layout
}

fn math_tag_display(name: &str, tag: &str) -> Option<bool> {
    if name == "script" && is_math_script_tag(tag) {
        let kind = attr(tag, "type").unwrap_or_default().to_ascii_lowercase();
        return Some(kind.contains("mode=display"));
    }
    if name == "math" {
        let display = attr(tag, "display").is_some_and(|value| value.eq_ignore_ascii_case("block"));
        return Some(display);
    }
    let class = attr(tag, "class")?.to_ascii_lowercase();
    let is_math = class.split_whitespace().any(|token| {
        token.contains("mathjax")
            || token.contains("katex")
            || token == "math"
            || token.contains("equation")
    });
    is_math.then(|| {
        class.contains("display") || class.contains("equation") || matches!(name, "div" | "figure")
    })
}

fn flush_math(buf: &mut String, blocks: &mut Vec<Block>, display: bool) {
    let annotation = element_bounds(buf, "annotation")
        .into_iter()
        .find_map(|bounds| {
            let opening = &buf[bounds.outer_start..bounds.inner_start];
            let encoding = attr(opening, "encoding")?.to_ascii_lowercase();
            (encoding.contains("tex") || encoding.contains("latex"))
                .then(|| buf[bounds.inner_start..bounds.inner_end].to_owned())
        });
    let raw = annotation.unwrap_or_else(|| strip_all_tags(buf));
    let source = decode_entities(&raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if !source.is_empty() {
        blocks.push(Block::Math { source, display });
    }
    buf.clear();
}

fn is_footnote_target(url: &str) -> bool {
    let fragment = url
        .rsplit_once('#')
        .map(|(_, fragment)| fragment)
        .unwrap_or(url)
        .to_ascii_lowercase();
    fragment.starts_with("fn")
        || fragment.starts_with("footnote")
        || fragment.starts_with("cite_note")
}

fn is_html_heading_tag(name: &str) -> bool {
    matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
}

fn finish_link(state: LinkState, buf: &mut String, blocks: &mut Vec<Block>) {
    let link_text = clean_text(&state.text);
    if link_text.is_empty() {
        // A link with no visible text should not swallow the text that
        // preceded it in the same paragraph.
        buf.push_str(&state.prefix);
        return;
    }
    let prefix = clean_text(&state.prefix);
    let has_separator =
        !prefix.is_empty() && state.prefix.chars().last().is_some_and(char::is_whitespace);
    let combined = if prefix.is_empty() {
        link_text.clone()
    } else if has_separator {
        format!("{prefix} {link_text}")
    } else {
        format!("{prefix}{link_text}")
    };
    let link_start = combined.len() - link_text.len();
    blocks.push(Block::Link {
        text: combined,
        url: state.url,
        link_start,
        space_after: false,
    });
}

fn finish_heading_link(
    state: LinkState,
    existing_index: Option<usize>,
    buf: &mut String,
    blocks: &mut Vec<Block>,
) -> Option<usize> {
    let link_text = clean_text(&state.text);
    if link_text.is_empty() {
        // Permalink anchors are often visually empty. They must not swallow
        // the title text that appeared before the anchor.
        if let Some(index) = existing_index {
            if let Some(Block::HeadingLink { text, .. }) = blocks.get_mut(index) {
                append_clean_fragment(text, &state.prefix);
            }
            return Some(index);
        }
        buf.push_str(&state.prefix);
        return None;
    }

    if let Some(index) = existing_index {
        if let Some(Block::HeadingLink { text, links }) = blocks.get_mut(index) {
            append_clean_fragment(text, &state.prefix);
            let prefix_ended_with_space =
                state.prefix.chars().last().is_some_and(char::is_whitespace);
            let link_started_with_space =
                state.text.chars().next().is_some_and(char::is_whitespace);
            if !text.is_empty()
                && (prefix_ended_with_space || link_started_with_space)
                && !text.ends_with(char::is_whitespace)
            {
                text.push(' ');
            }
            let start = text.len();
            text.push_str(&link_text);
            links.push(InlineLinkRange {
                url: state.url,
                start,
                end: text.len(),
            });
        }
        return Some(index);
    }

    let mut text = String::new();
    append_clean_fragment(&mut text, &state.prefix);
    let prefix_ended_with_space = state.prefix.chars().last().is_some_and(char::is_whitespace);
    let link_started_with_space = state.text.chars().next().is_some_and(char::is_whitespace);
    if !text.is_empty()
        && (prefix_ended_with_space || link_started_with_space)
        && !text.ends_with(char::is_whitespace)
    {
        text.push(' ');
    }
    let start = text.len();
    text.push_str(&link_text);
    blocks.push(Block::HeadingLink {
        text,
        links: vec![InlineLinkRange {
            url: state.url,
            start,
            end: start + link_text.len(),
        }],
    });
    Some(blocks.len() - 1)
}

fn append_heading_suffix(index: usize, buf: &mut String, blocks: &mut [Block]) {
    if let Some(Block::HeadingLink { text, .. }) = blocks.get_mut(index) {
        append_clean_fragment(text, buf);
    }
    buf.clear();
}

fn append_clean_fragment(target: &mut String, raw: &str) {
    let cleaned = clean_text(raw);
    if cleaned.is_empty() {
        return;
    }
    if !target.is_empty()
        && raw.chars().next().is_some_and(char::is_whitespace)
        && !target.ends_with(char::is_whitespace)
    {
        target.push(' ');
    }
    target.push_str(&cleaned);
}

fn inline_flow_boundary(name: &str) -> bool {
    BLOCK_TAGS.contains(&name)
        || is_html_heading_tag(name)
        || matches!(
            name,
            "img" | "source" | "blockquote" | "pre" | "code" | "hr" | "ul" | "ol"
        )
}

fn consume_whitespace_entity(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) -> bool {
    let mut lookahead = chars.clone();
    let mut body = String::new();
    let mut consumed = 0usize;
    let mut terminated = false;
    for next in lookahead.by_ref().take(16) {
        consumed += 1;
        if next == ';' {
            terminated = true;
            break;
        }
        if !(next.is_ascii_alphanumeric() || matches!(next, '#' | 'x' | 'X')) {
            return false;
        }
        body.push(next);
    }
    if !terminated || body.is_empty() {
        return false;
    }
    let decoded = decode_entities(&format!("&{body};"));
    if decoded.is_empty() || !decoded.chars().all(char::is_whitespace) {
        return false;
    }
    for _ in 0..consumed {
        chars.next();
    }
    true
}

fn link_href(tag: &str, base: Option<&str>) -> Option<String> {
    let href = decode_entities(&attr(tag, "href")?);
    let href = href.trim();
    if href.is_empty() || href == "#" {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("javascript:") || lower.starts_with("data:") || lower.starts_with("about:")
    {
        return None;
    }
    if lower.starts_with("mailto:") || lower.starts_with("tel:") {
        return Some(href.to_owned());
    }
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(href.to_owned());
    }
    if href.starts_with('#') {
        return Some(match base {
            Some(base) => format!("{base}{href}"),
            None => href.to_owned(),
        });
    }
    // Keep the original relative target when the feed did not provide a base
    // URL; the UI can still expose/copy the citation instead of dropping it.
    resolve(href, base).or_else(|| Some(href.to_owned()))
}

/// Sites commonly put a tiny placeholder in `src` and the real image in a lazy-loading
/// attribute. Prefer those attributes, then choose the largest candidate from `srcset`.
fn image_src(tag: &str) -> Option<String> {
    for name in [
        "data-original",
        "data-lazy-src",
        "data-src",
        "data-url",
        "data-image",
    ] {
        if let Some(value) = attr(tag, name).filter(|v| usable_image_url(v)) {
            return Some(value);
        }
    }
    for name in ["data-srcset", "srcset"] {
        if let Some(value) = attr(tag, name) {
            if let Some(candidate) = srcset_largest(&value) {
                return Some(candidate);
            }
        }
    }
    attr(tag, "src").filter(|v| usable_image_url(v))
}

fn usable_image_url(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && !value.starts_with("data:") && !value.starts_with("about:") && value != "#"
}

fn srcset_largest(value: &str) -> Option<String> {
    value
        .split(',')
        .filter_map(|candidate| {
            let mut fields = candidate.split_whitespace();
            let url = fields.next()?.trim();
            if usable_image_url(url) {
                Some(url.to_string())
            } else {
                None
            }
        })
        .last()
}

/// 清洗累积的文字（解码实体、折叠空行），按当前 HTML 语义推入块并清空缓冲。
fn extract_heading_inline_code_ranges(marked: &str) -> (String, Vec<InlineTextRange>) {
    let mut text = String::with_capacity(marked.len());
    let mut ranges = Vec::new();
    let mut start = None;
    for ch in marked.chars() {
        if ch == HEADING_INLINE_CODE_START {
            start = Some(text.len());
        } else if ch == HEADING_INLINE_CODE_END {
            if let Some(start) = start.take()
                && start < text.len()
            {
                ranges.push(InlineTextRange {
                    start,
                    end: text.len(),
                });
            }
        } else {
            text.push(ch);
        }
    }
    (text, ranges)
}

fn flush_text_kind(buf: &mut String, blocks: &mut Vec<Block>, strong: bool, heading: bool) {
    let marked = join_numbered_marker_linebreak(clean_text(buf));
    let (mut cleaned, inline_code_ranges) = extract_heading_inline_code_ranges(&marked);
    // Some feeds put the sentence-final punctuation in a separate HTML node,
    // e.g. `<p>（5）解决方法</p><p>。研究人员提出...</p>`. Keep that
    // punctuation attached to the preceding numbered heading instead of
    // rendering it as a new paragraph.
    if let Some(first) = cleaned.chars().next() {
        if matches!(first, '。' | '．' | '.' | '！' | '？' | '，' | ',')
            && blocks.last().is_some_and(
                |block| matches!(block, Block::Strong(text) if is_numbered_heading(text)),
            )
        {
            if let Some(Block::Strong(previous)) = blocks.last_mut() {
                previous.push(first);
            }
            cleaned = cleaned[first.len_utf8()..].trim_start().to_owned();
        }
    }
    if !cleaned.is_empty() {
        if heading {
            if inline_code_ranges.is_empty() {
                blocks.push(Block::Heading(cleaned));
            } else {
                blocks.push(Block::HeadingWithInlineCode {
                    text: cleaned,
                    inline_code_ranges,
                });
            }
            buf.clear();
            return;
        }
        let parts = split_numbered_headings(&cleaned);
        if parts.len() > 1 {
            let mut parts = parts;
            for i in 1..parts.len() {
                let trimmed = parts[i].trim_start();
                if trimmed.starts_with(['。', '．', '.', '！', '？', '，', ',']) {
                    let leading_len = parts[i].len() - trimmed.len();
                    let punctuation = trimmed.chars().next().unwrap();
                    parts[i - 1].push(punctuation);
                    parts[i].replace_range(..leading_len + punctuation.len_utf8(), "");
                }
            }
            for part in parts {
                if !part.is_empty() {
                    if strong || (is_numbered_heading(&part) && !is_numbered_marker_only(&part)) {
                        blocks.push(Block::Strong(part));
                    } else {
                        blocks.push(Block::Text(part));
                    }
                }
            }
        } else if cleaned.lines().any(is_bullet_line) {
            let mut paragraph = String::new();
            for line in cleaned.lines() {
                if is_bullet_line(line) {
                    if !paragraph.trim().is_empty() {
                        blocks.push(if strong {
                            Block::Strong(paragraph.trim().to_owned())
                        } else {
                            Block::Text(paragraph.trim().to_owned())
                        });
                        paragraph.clear();
                    }
                    blocks.push(Block::Text(line.trim().to_owned()));
                } else {
                    if !paragraph.is_empty() {
                        paragraph.push('\n');
                    }
                    paragraph.push_str(line);
                }
            }
            if !paragraph.trim().is_empty() {
                blocks.push(if strong {
                    Block::Strong(paragraph.trim().to_owned())
                } else {
                    Block::Text(paragraph.trim().to_owned())
                });
            }
        } else if strong || (is_numbered_heading(&cleaned) && !is_numbered_marker_only(&cleaned)) {
            blocks.push(Block::Strong(cleaned));
        } else {
            blocks.push(Block::Text(cleaned));
        }
    }
    buf.clear();
}

fn split_numbered_headings(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut starts = vec![0usize];
    let mut i = 0;
    while i < chars.len() {
        let marker_len = numbered_marker_len(&chars, i);
        let at_boundary = i == 0 || chars[i - 1].is_whitespace();
        if marker_len > 0 && i > 0 && at_boundary {
            starts.push(i);
        }
        i += marker_len.max(1);
    }
    if starts.len() == 1 {
        return vec![text.to_string()];
    }
    starts.push(chars.len());
    starts
        .windows(2)
        .filter_map(|w| {
            let part: String = chars[w[0]..w[1]].iter().collect();
            let part = part.trim().to_string();
            (!part.is_empty()).then_some(part)
        })
        .collect()
}

pub(crate) fn is_numbered_heading(text: &str) -> bool {
    let s = text.trim_start();
    let c: Vec<char> = s.chars().collect();
    numbered_marker_len(&c, 0) > 0
}

/// 是否只有编号标记、没有跟随的标题文字，例如 `(1)`、`（1）`、`1.`。
pub(crate) fn is_numbered_marker_only(text: &str) -> bool {
    let chars: Vec<char> = text.trim().chars().collect();
    !chars.is_empty() && numbered_marker_len(&chars, 0) == chars.len()
}

fn numbered_marker_len(chars: &[char], start: usize) -> usize {
    if start >= chars.len() {
        return 0;
    }
    if (chars[start] == '（' || chars[start] == '(')
        && start + 2 < chars.len()
        && chars[start + 1].is_ascii_digit()
        && (chars[start + 2] == '）' || chars[start + 2] == ')')
    {
        return 3;
    }
    let mut i = start;
    while i < chars.len() && chars[i].is_ascii_digit() {
        i += 1;
    }
    if i == start || i >= chars.len() {
        return 0;
    }
    let punctuation = chars[i];
    if !matches!(punctuation, '、' | '.' | '．' | ',' | '，' | ':' | '：') {
        return 0;
    }
    // ASCII `.` is also the decimal/version separator. Treat it as a numbered
    // marker only when it ends the input or is followed by whitespace:
    // `1. 标题` / `1.` are headings, while `1.97.1` and `32.6GB` are not.
    let after = i + 1;
    if punctuation == '.' && after < chars.len() && !chars[after].is_whitespace() {
        return 0;
    }
    after - start
}

fn merge_bare_number_markers(blocks: &mut Vec<Block>) {
    let mut i = 0;
    while i + 1 < blocks.len() {
        let marker = match &blocks[i] {
            Block::Strong(text) | Block::Text(text) if is_bare_plain_number_marker(text) => {
                text.trim().to_owned()
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let following = match &blocks[i + 1] {
            Block::Text(text) | Block::Strong(text) => text.trim().to_owned(),
            Block::Heading(_)
            | Block::HeadingWithInlineCode { .. }
            | Block::HeadingLink { .. }
            | Block::Quote(_)
            | Block::InlineCode(_)
            | Block::Code(_)
            | Block::CodeBlock { .. }
            | Block::Caption(_)
            | Block::DefinitionList(_)
            | Block::Link { .. }
            | Block::ListItemStart { .. }
            | Block::ListItemEnd { .. }
            | Block::Image(_)
            | Block::LinkedImage { .. }
            | Block::Table { .. }
            | Block::Math { .. } => {
                i += 1;
                continue;
            }
        };
        let separator = if marker.ends_with(['.', '．']) {
            " "
        } else {
            ""
        };
        blocks[i] = Block::Text(format!("{marker}{separator}{following}"));
        blocks.remove(i + 1);
    }
}

pub(crate) fn is_bare_plain_number_marker(text: &str) -> bool {
    let s = text.trim();
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() || chars[0] == '(' || chars[0] == '（' {
        return false;
    }
    is_numbered_marker_only(s)
}

/// 有些站点把编号和标题拆成两行（例如 `1.` + 换行 + `如果你是太阳`）。
/// 这不是一个真正的段落分隔，应在解析阶段合并，否则阅读器会显示成
/// “1.” 单独占一行。只处理开头的编号，正文中的普通换行保持不变。
fn join_numbered_marker_linebreak(text: String) -> String {
    let chars: Vec<char> = text.chars().collect();
    let leading = chars.iter().take_while(|ch| ch.is_whitespace()).count();
    if leading >= chars.len() {
        return text;
    }
    let marker_len = numbered_marker_len(&chars, leading);
    if marker_len == 0 {
        return text;
    }
    let after_marker = leading + marker_len;
    let mut cursor = after_marker;
    let mut has_linebreak = false;
    while cursor < chars.len() && chars[cursor].is_whitespace() {
        has_linebreak |= matches!(chars[cursor], '\n' | '\r');
        cursor += 1;
    }
    if !has_linebreak || cursor == after_marker {
        return text;
    }
    let prefix: String = chars[..after_marker].iter().collect();
    let suffix: String = chars[cursor..].iter().collect();
    if suffix.is_empty() {
        prefix
    } else {
        format!("{prefix} {suffix}")
    }
}

fn is_bullet_line(line: &str) -> bool {
    matches!(
        line.trim_start().chars().next(),
        Some('▪' | '•' | '·' | '‣' | '◦')
    )
}

/// 解码实体 + 折叠连续空行 + 去行首尾空白。
fn clean_text(raw: &str) -> String {
    let decoded = decode_entities(raw);
    let mut result = String::new();
    let mut blank = 0;
    for line in decoded.lines() {
        let line = line.trim();
        if line.is_empty() {
            blank += 1;
            if blank == 1 {
                result.push('\n');
            }
        } else {
            blank = 0;
            result.push_str(line);
            result.push('\n');
        }
    }
    result.trim().to_string()
}

/// 从一个标签片段里取属性值（支持双/单引号）。
fn attr(tag: &str, wanted: &str) -> Option<String> {
    let bytes = tag.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len()
            && (bytes[at].is_ascii_whitespace() || matches!(bytes[at], b'<' | b'/' | b'>'))
        {
            at += 1;
        }
        let name_start = at;
        while at < bytes.len()
            && !bytes[at].is_ascii_whitespace()
            && !matches!(bytes[at], b'=' | b'>' | b'/')
        {
            at += 1;
        }
        if name_start == at {
            at += 1;
            continue;
        }
        let attribute_name = &tag[name_start..at];
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if bytes.get(at) != Some(&b'=') {
            continue;
        }
        at += 1;
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }

        let (value_start, value_end) = match bytes.get(at).copied() {
            Some(quote @ (b'"' | b'\'')) => {
                at += 1;
                let value_start = at;
                while at < bytes.len() && bytes[at] != quote {
                    at += 1;
                }
                let value_end = at;
                at = (at + 1).min(bytes.len());
                (value_start, value_end)
            }
            Some(_) => {
                let value_start = at;
                while at < bytes.len() && !bytes[at].is_ascii_whitespace() && bytes[at] != b'>' {
                    at += 1;
                }
                (value_start, at)
            }
            None => return None,
        };
        if attribute_name.eq_ignore_ascii_case(wanted) {
            return Some(tag[value_start..value_end].to_owned());
        }
    }
    None
}

/// 把 `<img src>` 补成绝对 http(s) 地址；纯相对路径靠文章 `base` 拼。
/// ponytail: 手写小拼接，够覆盖 RSS 里常见的 绝对/协议相对/根相对/同目录 四种；
/// 复杂相对（`../`）不化简，真需要再上 `url` crate。
fn resolve(src: &str, base: Option<&str>) -> Option<String> {
    let s = src.trim();
    if s.starts_with("http://") || s.starts_with("https://") {
        return Some(s.to_string());
    }
    if let Some(rest) = s.strip_prefix("//") {
        return Some(format!("https://{rest}"));
    }
    let base = base?.trim();
    let scheme_end = base.find("://")? + 3;
    let authority_end = base[scheme_end..]
        .find('/')
        .map(|i| scheme_end + i)
        .unwrap_or(base.len());
    let authority_end = base[scheme_end..authority_end]
        .find(['?', '#'])
        .map(|i| scheme_end + i)
        .unwrap_or(authority_end);
    if s.starts_with('#') {
        return Some(format!("{}{}", base.split('#').next().unwrap_or(base), s));
    }
    if s.starts_with('?') {
        let page = base.split(['?', '#']).next().unwrap_or(base);
        return Some(format!("{page}{s}"));
    }
    let (path, suffix) = s
        .find(['?', '#'])
        .map(|at| (&s[..at], &s[at..]))
        .unwrap_or((s, ""));
    if s.starts_with('/') {
        Some(format!(
            "{}{}{}",
            &base[..authority_end],
            normalize_url_path(path),
            suffix
        )) // 根相对：scheme+host + /...
    } else {
        // 同目录相对：base 去掉最后一段文件名
        let base_path_end = base.find(['?', '#']).unwrap_or(base.len());
        let dir_end = base[..base_path_end]
            .rfind('/')
            .filter(|&i| i >= authority_end)
            .map(|i| i + 1)
            .unwrap_or(authority_end);
        let base_dir = &base[authority_end..dir_end];
        let joined = if base_dir.is_empty() {
            format!("/{path}")
        } else {
            format!("{base_dir}{path}")
        };
        Some(format!(
            "{}{}{}",
            &base[..authority_end],
            normalize_url_path(&joined),
            suffix
        ))
    }
}

fn normalize_url_path(path: &str) -> String {
    let leading_slash = path.starts_with('/');
    let trailing_slash = path.ends_with('/');
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            _ => parts.push(part),
        }
    }
    let mut normalized = parts.join("/");
    if leading_slash {
        normalized.insert(0, '/');
    }
    if trailing_slash && !normalized.ends_with('/') {
        normalized.push('/');
    }
    if normalized.is_empty() && leading_slash {
        normalized.push('/');
    }
    normalized
}

/// 解码常见 HTML 实体（命名 + 十进制/十六进制数字），认不出的原样保留。
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        // rest 以 '&'(1字节) 开头，从 rest[1..] 找 ';'，全在 ASCII 边界上，不会切进多字节字符。
        let semi = rest[1..].find(';').map(|r| r + 1);
        let ent = semi.map(|s| &rest[1..s]);
        let valid = ent.is_some_and(|e| {
            e.len() <= 10 && !e.contains(|c: char| c == '<' || c == '&' || c.is_whitespace())
        });
        if let (true, Some(semi), Some(ent)) = (valid, semi, ent) {
            let ch = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" | "#39" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                "mdash" => Some('—'),
                "hellip" => Some('…'),
                _ => ent.strip_prefix('#').and_then(|num| {
                    let code = match num.strip_prefix(['x', 'X']) {
                        Some(h) => u32::from_str_radix(h, 16).ok(),
                        None => num.parse().ok(),
                    };
                    code.and_then(char::from_u32)
                }),
            };
            match ch {
                Some(c) => {
                    out.push(c);
                    rest = &rest[semi + 1..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

pub fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{
        Block, CompletePageKind, content_blocks, extract_html_base_href, extract_html_title,
        guarded_readability_content, html5_reading_scope, is_numbered_heading,
        is_numbered_marker_only, prepare_html_snapshot, strip_ignored_elements, table_cell_columns,
    };

    fn escape_html_attribute(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    fn texts(blocks: &[Block]) -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::Text(t) => Some(t.as_str()),
                Block::Link { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    fn images(blocks: &[Block]) -> Vec<&str> {
        blocks
            .iter()
            .filter_map(|b| match b {
                Block::Image(u) => Some(u.as_str()),
                Block::LinkedImage { uri, .. } => Some(uri.as_str()),
                _ => None,
            })
            .collect()
    }

    fn all_visible_text(blocks: &[Block]) -> String {
        blocks
            .iter()
            .filter_map(|block| match block {
                Block::Text(text)
                | Block::Strong(text)
                | Block::InlineCode(text)
                | Block::Heading(text)
                | Block::HeadingWithInlineCode { text, .. }
                | Block::HeadingLink { text, .. }
                | Block::Quote(text)
                | Block::Code(text)
                | Block::Caption(text)
                | Block::Link { text, .. } => Some(text.as_str()),
                Block::CodeBlock { text, .. } | Block::Math { source: text, .. } => {
                    Some(text.as_str())
                }
                Block::ListItemStart { .. }
                | Block::ListItemEnd { .. }
                | Block::Image(_)
                | Block::LinkedImage { .. }
                | Block::DefinitionList(_)
                | Block::Table { .. } => None,
            })
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn prepares_readable_snapshot_from_complete_page() {
        let html = r#"
            <!doctype html><html><head>
              <base href="https://cdn.example.com/articles/">
              <meta property = "og:title" content = "  Saved &amp; readable  ">
              <title>Fallback title</title><style>.ad { display: none }</style>
              <script>window.secret = "must not render"</script>
            </head><body>
              <header>site masthead</header><nav>many links</nav>
              <article class="story"><h1>Article heading</h1>
                <p>First paragraph.</p><img src="../hero.webp">
                <aside>related stories</aside><p>Last paragraph.</p>
              </article>
              <footer>copyright</footer>
            </body></html>
        "#;

        let snapshot = prepare_html_snapshot(html);
        assert_eq!(snapshot.title.as_deref(), Some("Saved & readable"));
        assert_eq!(
            snapshot.base_href.as_deref(),
            Some("https://cdn.example.com/articles/")
        );
        assert!(snapshot.content.contains("Article heading"));
        assert!(snapshot.content.contains("First paragraph."));
        assert!(!snapshot.content.contains("site masthead"));
        assert!(!snapshot.content.contains("related stories"));
        assert!(!snapshot.content.contains("window.secret"));

        let blocks = content_blocks(&snapshot.content, snapshot.base_href.as_deref());
        assert_eq!(images(&blocks), vec!["https://cdn.example.com/hero.webp"]);
    }

    #[test]
    fn title_falls_back_from_og_to_title_then_h1() {
        assert_eq!(
            extract_html_title("<title>  Page <em>title</em> &amp; more </title>"),
            Some("Page title & more".to_owned())
        );
        assert_eq!(
            extract_html_title("<main><h1>First <strong>heading</strong></h1></main>"),
            Some("First heading".to_owned())
        );
        assert_eq!(extract_html_title("<p>No title</p>"), None);
    }

    #[test]
    fn finds_base_href_with_spacing_and_unquoted_value() {
        assert_eq!(
            extract_html_base_href("<head><BASE HREF = https://example.com/a/b/></head>"),
            Some("https://example.com/a/b/".to_owned())
        );
    }

    #[test]
    fn complete_html_never_renders_page_chrome_or_executable_content() {
        let html = concat!(
            "<html><head><title>Do not render title</title></head><body>",
            "<header>Header text</header><nav>Navigation</nav>",
            "<main><p>Readable body</p><script>evil()</script>",
            "<style>body { color: red }</style><noscript>Turn JS on</noscript>",
            "<svg><text>SVG label</text></svg><form>Form label</form>",
            "<iframe>Frame fallback</iframe><aside>Related</aside></main>",
            "<footer>Footer text</footer></body></html>"
        );
        let visible = all_visible_text(&content_blocks(html, None));
        assert_eq!(visible, "Readable body");
    }

    #[test]
    fn script_source_that_mentions_script_tag_does_not_hide_following_article() {
        let html = r#"
            <body><script>const sample = "<script>nested-looking text";</script>
            <article><p>Still readable after script.</p></article></body>
        "#;
        let visible = all_visible_text(&content_blocks(html, None));
        assert_eq!(visible, "Still readable after script.");
    }

    #[test]
    fn main_scope_takes_priority_over_articles_elsewhere_on_page() {
        let html = concat!(
            "<article><p>teaser</p></article>",
            "<article><div><p>long article paragraph</p></div><p>ending</p></article>",
            "<main><p>main fallback should not win</p></main>"
        );
        let snapshot = prepare_html_snapshot(html);
        let visible = all_visible_text(&content_blocks(&snapshot.content, None));
        assert!(!visible.contains("long article paragraph"));
        assert!(!visible.contains("ending"));
        assert!(!visible.contains("teaser"));
        assert!(visible.contains("main fallback"));
    }

    #[test]
    fn one_article_inside_main_is_selected() {
        let snapshot = prepare_html_snapshot(concat!(
            "<main><p>section chrome</p><article>",
            "<div><p>article paragraph</p></div><p>ending</p>",
            "</article><p>related footer</p></main>"
        ));
        let visible = all_visible_text(&content_blocks(&snapshot.content, None));
        assert!(visible.contains("article paragraph"));
        assert!(visible.contains("ending"));
        assert!(!visible.contains("section chrome"));
        assert!(!visible.contains("related footer"));
    }

    #[test]
    fn html5_dom_repairs_malformed_article_before_block_parsing() {
        let snapshot = prepare_html_snapshot(concat!(
            "<!doctype html><html><body><main><article>",
            "<h1>Broken but readable</h1><p>First paragraph<div>Second block",
            "</article><p>outside article</p></main></body></html>"
        ));

        assert!(
            snapshot.content.contains("<p>First paragraph</p>"),
            "{}",
            snapshot.content
        );
        assert!(
            snapshot.content.contains("<div>Second block</div>"),
            "{}",
            snapshot.content
        );
        assert!(!snapshot.content.contains("outside article"));
        let visible = all_visible_text(&content_blocks(&snapshot.content, None));
        assert!(visible.contains("First paragraph"));
        assert!(visible.contains("Second block"));
    }

    #[test]
    fn rust_blog_full_page_without_article_keeps_the_main_post() {
        let html = include_str!("../tests/fixtures/rust-blog-full-page-no-article.html");
        let scope = html5_reading_scope(html);
        assert_eq!(scope.kind, CompletePageKind::IndexOrDocument);

        let snapshot = prepare_html_snapshot(html);
        assert_eq!(snapshot.title.as_deref(), Some("Announcing Rust 1.97.0"));
        assert_eq!(
            snapshot.base_href.as_deref(),
            Some("https://blog.rust-lang.org/2026/07/09/")
        );
        assert!(!snapshot.content.contains("siteNavigation"));
        assert!(!snapshot.content.contains("Related releases"));

        let blocks = content_blocks(&snapshot.content, snapshot.base_href.as_deref());
        let visible = all_visible_text(&blocks);
        for expected in [
            "The Rust team is happy",
            "What is in 1.97.0 stable",
            "One repaired list item",
            "A second item with release details",
        ] {
            assert!(
                visible.contains(expected),
                "missing {expected:?}: {visible:?}"
            );
        }
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::CodeBlock { text, language }
                if text == "$ rustup update stable" && language == "shellsession"
        )));
    }

    #[test]
    fn malformed_no_article_fixture_keeps_dom_semantics_and_math() {
        let html = include_str!("../tests/fixtures/simon-willison-malformed-entry.html");
        let snapshot = prepare_html_snapshot(html);
        assert_eq!(
            snapshot.title.as_deref(),
            Some("DOM notes & resilient readers")
        );
        assert_eq!(
            snapshot.base_href.as_deref(),
            Some("https://simonwillison.net/2026/Aug/13/dom-notes/")
        );
        assert!(snapshot.content.contains("<p>A browser DOM repairs"));
        assert!(!snapshot.content.contains("must not reach the reader"));

        let blocks = content_blocks(&snapshot.content, snapshot.base_href.as_deref());
        assert_eq!(
            images(&blocks),
            ["https://simonwillison.net/static/dom-1280.webp"]
        );
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Caption(text)
                if text == "A responsive image after malformed paragraph markup."
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Math { source, display: true } if source == "\\sum_{i=1}^{n} i"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::DefinitionList(items)
                if items.len() == 2
                    && items[0].term == "Repair"
                    && items[1].term == "Projection"
        )));
    }

    #[test]
    fn rss_fragment_articles_never_compete_for_readability() {
        let blocks = content_blocks(
            concat!(
                "<article><h2>First entry</h2><p>Alpha</p></article>",
                "<article><h2>Second entry</h2><p>Beta</p></article>"
            ),
            None,
        );
        let visible = all_visible_text(&blocks);
        for expected in ["First entry", "Alpha", "Second entry", "Beta"] {
            assert!(
                visible.contains(expected),
                "missing {expected:?}: {visible:?}"
            );
        }
    }

    #[test]
    fn multiple_articles_inside_main_are_kept_as_an_index() {
        let snapshot = prepare_html_snapshot(concat!(
            "<html><head><title>Index</title></head><body><main>",
            "<h1>Index</h1><p>intro</p>",
            "<article><h2><a href='/a'>A</a></h2><p>Alpha</p></article>",
            "<article><h2><a href='/b'>B</a></h2><p>Beta</p></article>",
            "</main></body></html>"
        ));
        let blocks = content_blocks(&snapshot.content, Some("https://example.com/index"));
        let visible = all_visible_text(&blocks);
        for expected in ["intro", "A", "Alpha", "B", "Beta"] {
            assert!(
                visible.contains(expected),
                "missing {expected:?}: {visible:?}"
            );
        }
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::HeadingLink { text, .. } if text == "A"))
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::HeadingLink { text, .. } if text == "B"))
        );
    }

    #[test]
    #[ignore = "live webpage smoke test; run explicitly before packaging"]
    fn martin_fowler_architecture_guide_keeps_cards_and_links() {
        let client = crate::web_clip::client().unwrap();
        let fetched =
            crate::web_clip::fetch_html(&client, "https://martinfowler.com/architecture/").unwrap();
        let snapshot = prepare_html_snapshot(&fetched.html);
        assert_eq!(
            snapshot.title.as_deref(),
            Some("Software Architecture Guide")
        );
        let blocks = content_blocks(&snapshot.content, Some(&fetched.final_url));
        let visible = all_visible_text(&blocks);
        for expected in [
            "What is architecture?",
            "Application Architecture",
            "Application Boundary",
            "Microservices Guide",
            "Enterprise Architecture",
        ] {
            assert!(
                visible.contains(expected),
                "live snapshot omitted {expected:?}"
            );
        }
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::HeadingLink { text, links }
                if text == "Application Boundary"
                    && links.iter().any(|link| link.url == "https://martinfowler.com/bliki/ApplicationBoundary.html")
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::HeadingLink { text, links }
                if text == "Microservices Guide"
                    && links.iter().any(|link| link.url == "https://martinfowler.com/microservices")
        )));
        assert!(
            blocks
                .iter()
                .filter(|block| matches!(block, Block::Image(_)))
                .count()
                >= 10
        );
    }

    #[test]
    fn duplicate_leading_h1_is_removed_but_other_h1_is_kept() {
        let duplicate = prepare_html_snapshot(
            "<html><head><title>Guide</title></head><body><main><h1>Guide</h1><p>Intro</p></main></body></html>",
        );
        let blocks = content_blocks(&duplicate.content, None);
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, Block::Heading(text) if text == "Guide"))
        );
        assert_eq!(all_visible_text(&blocks), "Intro");

        let distinct = prepare_html_snapshot(
            "<html><head><title>Site · Guide</title></head><body><main><h1>Guide</h1><p>Intro</p></main></body></html>",
        );
        assert!(
            content_blocks(&distinct.content, None)
                .iter()
                .any(|block| matches!(block, Block::Heading(text) if text == "Guide"))
        );
    }

    #[test]
    fn linked_card_heading_keeps_heading_boundary_before_abstract() {
        let snapshot = prepare_html_snapshot(concat!(
            "<main><h1>Guide</h1><div class='article-card'>",
            "<h3><a href='/x'>Application Boundary</a></h3>",
            "<p>Abstract</p></div></main>"
        ));
        let blocks = content_blocks(&snapshot.content, Some("https://example.com/guide"));
        assert!(matches!(
            &blocks[0],
            Block::HeadingLink { text, links }
                if text == "Application Boundary"
                    && links.len() == 1
                    && links[0].url == "https://example.com/x"
        ));
        assert!(matches!(&blocks[1], Block::Text(text) if text == "Abstract"));
    }

    #[test]
    fn inline_link_suffix_remains_adjacent() {
        let blocks = content_blocks(
            "<p>harder to <a href='/x'>modif</a>y, leading</p>",
            Some("https://example.com/guide"),
        );
        // Keep the anchor range exact; the GUI removes the visual separator
        // between adjacent ASCII word characters, rendering `modify` while
        // only the source anchor text `modif` remains clickable.
        assert_eq!(all_visible_text(&blocks), "harder to modif|y, leading");
        assert_eq!(blocks.len(), 2);
        assert!(matches!(
            &blocks[0],
            Block::Link { text, url, link_start, .. }
                if text == "harder to modif"
                    && url == "https://example.com/x"
                    && *link_start == "harder to ".len()
        ));
        assert!(matches!(&blocks[1], Block::Text(text) if text == "y, leading"));
    }

    #[test]
    fn normal_space_after_link_is_preserved() {
        let blocks = content_blocks(
            "<p>See <a href='/docs'>docs</a> and notes</p>",
            Some("https://example.com/guide"),
        );
        assert!(matches!(
            &blocks[0],
            Block::Link { text, space_after, .. }
                if text == "See docs" && *space_after
        ));
        assert!(matches!(&blocks[1], Block::Text(text) if text == "and notes"));
    }

    #[test]
    fn entity_space_after_link_is_preserved() {
        for entity in ["&nbsp;", "&#32;", "&#x20;"] {
            let html = format!("<p><a href='/docs'>docs</a>{entity}and notes</p>");
            let blocks = content_blocks(&html, Some("https://example.com/guide"));
            assert!(matches!(
                &blocks[0],
                Block::Link { text, space_after: true, .. } if text == "docs"
            ));
            assert!(matches!(&blocks[1], Block::Text(text) if text == "and notes"));
        }
    }

    #[test]
    fn heading_links_keep_prefix_suffix_and_exact_range() {
        let blocks = content_blocks(
            "<h2>New: <a href='/guide'>Guide</a> — updated</h2>",
            Some("https://example.com/index"),
        );
        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            Block::HeadingLink { text, links }
                if text == "New: Guide — updated"
                    && links.len() == 1
                    && links[0].url == "https://example.com/guide"
                    && &text[links[0].start..links[0].end] == "Guide"
        ));
    }

    #[test]
    fn heading_keeps_text_before_an_empty_permalink() {
        let blocks = content_blocks(
            "<h2>Visible title <a href='#permalink'></a></h2>",
            Some("https://example.com/guide"),
        );
        assert_eq!(blocks, vec![Block::Heading("Visible title".to_owned())]);
    }

    #[test]
    fn heading_keeps_multiple_links_on_one_line() {
        let blocks = content_blocks(
            "<h2><a href='/a'>Alpha</a> and <a href='/b'>Beta</a></h2>",
            Some("https://example.com/index"),
        );
        assert_eq!(blocks.len(), 1);
        let Block::HeadingLink { text, links } = &blocks[0] else {
            panic!("expected one linked heading: {blocks:?}");
        };
        assert_eq!(text, "Alpha and Beta");
        assert_eq!(links.len(), 2);
        assert_eq!(&text[links[0].start..links[0].end], "Alpha");
        assert_eq!(links[0].url, "https://example.com/a");
        assert_eq!(&text[links[1].start..links[1].end], "Beta");
        assert_eq!(links[1].url, "https://example.com/b");
    }

    #[test]
    fn multiline_list_items_keep_continuation_text_inside_item() {
        let blocks = content_blocks(
            "<ul><li>A line that\nwraps</li><li>Second\nwraps too</li></ul>",
            None,
        );
        assert_eq!(blocks.len(), 6);
        assert!(matches!(&blocks[0], Block::ListItemStart { depth: 1 }));
        assert!(matches!(&blocks[1], Block::Text(text) if text == "A line that wraps"));
        assert!(matches!(&blocks[2], Block::ListItemEnd { depth: 1 }));
        assert!(matches!(&blocks[3], Block::ListItemStart { depth: 1 }));
        assert!(matches!(&blocks[4], Block::Text(text) if text == "Second wraps too"));
        assert!(matches!(&blocks[5], Block::ListItemEnd { depth: 1 }));
    }

    #[test]
    fn list_items_keep_links_emphasis_and_images() {
        let blocks = content_blocks(
            "<ul><li>See <a href='/docs'>docs</a> and <strong>notes</strong><img src='/a.png'></li></ul>",
            Some("https://example.com/guide/"),
        );
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Link { text, url, .. }
                if text == "See docs" && url == "https://example.com/docs"
        )));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::Text(text) if text == "and"))
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::Strong(text) if text == "notes"))
        );
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Image(url) if url == "https://example.com/a.png"
        )));
    }

    #[test]
    fn rich_list_item_stops_before_following_paragraph() {
        let blocks = content_blocks(
            "<ul><li><strong>Label</strong> rest</li></ul><p>outside</p>",
            None,
        );
        assert!(matches!(&blocks[0], Block::ListItemStart { depth: 1 }));
        assert!(matches!(&blocks[1], Block::Strong(text) if text == "Label"));
        assert!(matches!(&blocks[2], Block::Text(text) if text == "rest"));
        assert!(matches!(&blocks[3], Block::ListItemEnd { depth: 1 }));
        assert!(matches!(&blocks[4], Block::Text(text) if text == "outside"));
    }

    #[test]
    fn list_item_can_start_with_a_link() {
        let blocks = content_blocks(
            "<ul><li><a href='/guide'>Guide</a> details</li></ul>",
            Some("https://example.com"),
        );
        assert!(matches!(&blocks[0], Block::ListItemStart { depth: 1 }));
        assert!(matches!(
            &blocks[1],
            Block::Link { text, url, space_after: true, .. }
                if text == "Guide" && url == "https://example.com/guide"
        ));
        assert!(matches!(&blocks[2], Block::Text(text) if text == "details"));
        assert!(matches!(&blocks[3], Block::ListItemEnd { depth: 1 }));
    }

    #[test]
    fn quoted_lists_keep_bullets() {
        let blocks = content_blocks(
            "<blockquote><ul><li>A</li><li>B</li></ul></blockquote>",
            None,
        );
        assert!(matches!(&blocks[0], Block::Quote(text) if text == "▪ A\n▪ B"));
    }

    #[test]
    fn document_base_overrides_page_url_and_normalizes_parent_segments() {
        let blocks = content_blocks(
            r#"<html><head><base href="/assets/posts/"></head><body><img src="../hero.jpg"><a href="./source">source</a></body></html>"#,
            Some("https://example.com/news/page.html"),
        );
        assert_eq!(images(&blocks), vec!["https://example.com/assets/hero.jpg"]);
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Link { url, .. } if url == "https://example.com/assets/posts/source"
        )));
    }

    #[test]
    fn splits_text_and_images_in_order() {
        let html = r#"<p>下面是 <a href="x">Dario &amp; Sam</a> 的&#8220;言论&#8221;</p><img src="https://a.com/x.png"><p>看看</p><img src="/rel.png">"#;
        let blocks = content_blocks(html, Some("https://site.com/blog/post.html"));
        // 文字块去了标签、解码了实体，图片按序穿插
        let t = texts(&blocks);
        assert!(!t.contains('<'), "still has tags: {t}");
        assert!(t.contains("Dario & Sam"));
        assert!(t.contains('\u{201c}') && t.contains("言论"));
        assert!(t.contains("看看"));
        // 绝对图 + 根相对图按 base 补全
        assert_eq!(
            images(&blocks),
            vec!["https://a.com/x.png", "https://site.com/rel.png"]
        );
        // 顺序：同一段的前缀与链接保留在一个可点击块中，图片仍按
        // 原文位置穿插。
        assert!(matches!(
            &blocks[0],
            Block::Link { text, link_start, .. }
                if text == "下面是 Dario & Sam" && *link_start == "下面是 ".len()
        ));
        assert!(blocks.iter().any(|block| matches!(block, Block::Image(_))));
    }

    #[test]
    fn resolves_relative_same_dir() {
        let html = r#"<img src="pic.jpg">"#;
        let blocks = content_blocks(html, Some("https://s.com/a/b/page.html"));
        assert_eq!(images(&blocks), vec!["https://s.com/a/b/pic.jpg"]);
    }

    #[test]
    fn supports_lazy_images_and_srcset() {
        let html = r#"
            <img src="data:image/gif;base64,x" data-lazy-src="/hero.jpg">
            <picture>
              <source srcset="/small.webp 480w, /large.webp 1280w">
              <img src="/fallback.jpg">
            </picture>
            <img src="/hero.jpg">
        "#;
        let blocks = content_blocks(html, Some("https://s.com/posts/1"));
        assert_eq!(
            images(&blocks),
            vec!["https://s.com/hero.jpg", "https://s.com/large.webp"]
        );
    }

    #[test]
    fn linked_picture_becomes_one_high_resolution_image_with_alt_text() {
        let blocks = content_blocks(
            concat!(
                "<a href='/story'><picture>",
                "<source srcset='/cover-640.webp 640w, /cover-1600.webp 1600w'>",
                "<img src='/cover-fallback.jpg' alt='Story cover'>",
                "</picture></a>"
            ),
            Some("https://example.com/index.html"),
        );

        assert_eq!(blocks.len(), 1);
        assert!(matches!(
            &blocks[0],
            Block::LinkedImage { uri, url, alt }
                if uri == "https://example.com/cover-1600.webp"
                    && url == "https://example.com/story"
                    && alt.as_deref() == Some("Story cover")
        ));
    }

    #[test]
    fn preserves_anchor_text_and_resolved_href() {
        let html = concat!(
            "<p>查看<a href=\"/docs?a=1&amp;b=2\">文档<strong>重点</strong></a>",
            "，或<a href=\"mailto:team@example.com\">发邮件</a>。</p>"
        );
        let blocks = content_blocks(html, Some("https://site.com/posts/1"));

        assert!(matches!(
            &blocks[0],
            Block::Link { text, url, link_start, .. }
                if text == "查看文档重点"
                    && *link_start == "查看".len()
                    && url == "https://site.com/docs?a=1&b=2"
        ));
        assert!(matches!(
            &blocks[1],
            Block::Link { text, url, link_start, .. }
                if text == "，或发邮件"
                    && *link_start == "，或".len()
                    && url == "mailto:team@example.com"
        ));
        assert!(matches!(&blocks[2], Block::Text(t) if t == "。"));
    }

    #[test]
    fn anchor_without_usable_href_remains_plain_text() {
        let blocks = content_blocks("<p>前<a href=\"#\">锚点</a>后</p>", None);
        assert_eq!(blocks.len(), 1);
        assert!(matches!(&blocks[0], Block::Text(t) if t == "前锚点后"));

        let blocks = content_blocks(
            "<p><a href=\"#section\">章节</a><a href=\"relative.html\">相对链接</a></p>",
            Some("https://site.com/posts/1"),
        );
        assert!(matches!(
            &blocks[0],
            Block::Link { text, url, link_start, .. }
                if text == "章节"
                    && *link_start == 0
                    && url == "https://site.com/posts/1#section"
        ));
        assert!(matches!(
            &blocks[1],
            Block::Link { text, url, link_start, .. }
                if text == "相对链接"
                    && *link_start == 0
                    && url == "https://site.com/posts/relative.html"
        ));

        let blocks = content_blocks("<a href=\"relative.html\">链接</a>", None);
        assert!(matches!(
            &blocks[0],
            Block::Link { text, url, link_start, .. }
                if text == "链接" && *link_start == 0 && url == "relative.html"
        ));
    }

    #[test]
    fn keeps_images_inside_links_in_document_order() {
        let blocks = content_blocks(
            "<p><a href=\"/cover\"><img src=\"/cover.png\">封面</a></p>",
            Some("https://site.com/posts/1"),
        );
        assert!(matches!(
            &blocks[0],
            Block::LinkedImage { uri, url, .. }
                if uri == "https://site.com/cover.png" && url == "https://site.com/cover"
        ));
        assert!(matches!(
            &blocks[1],
            Block::Link { text, url, link_start, .. }
                if text == "封面" && *link_start == 0 && url == "https://site.com/cover"
        ));
    }

    #[test]
    fn keeps_numbered_sections_and_punctuation_together() {
        let html = r#"
            <p>原因可能有下面几点。</p>
            <p>（1）工作职责范围扩大</p>
            <p>。研究人员开始维护服务器。</p>
            <p>（5）解决方法</p>
            <p>。研究人员提出，为了解决 AI 带来的职业倦怠。</p>
        "#;
        let blocks = content_blocks(html, None);
        let rendered: Vec<String> = blocks
            .iter()
            .map(|block| match block {
                Block::Text(t) => format!("T:{t}"),
                Block::Strong(t) => format!("S:{t}"),
                Block::Heading(t) => format!("H:{t}"),
                Block::HeadingWithInlineCode { text, .. } => format!("HC:{text}"),
                Block::HeadingLink { text, links } => format!(
                    "HL:{text}@{}",
                    links
                        .iter()
                        .map(|link| link.url.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                Block::Quote(t) => format!("Q:{t}"),
                Block::InlineCode(t) => format!("IC:{t}"),
                Block::Code(t) => format!("C:{t}"),
                Block::CodeBlock { text, language } => format!("C:{language}:{text}"),
                Block::Link {
                    text,
                    url,
                    link_start,
                    ..
                } => format!("L:{text}@{url}#{link_start}"),
                Block::ListItemStart { depth } => format!("LS:{depth}"),
                Block::ListItemEnd { depth } => format!("LE:{depth}"),
                Block::Image(t) => format!("I:{t}"),
                Block::LinkedImage { uri, url, .. } => format!("LI:{uri}@{url}"),
                Block::Caption(text) => format!("CAP:{text}"),
                Block::DefinitionList(items) => format!("DL:{}", items.len()),
                Block::Table { rows, .. } => format!("TB:{}", rows.len()),
                Block::Math { source, .. } => format!("M:{source}"),
            })
            .collect();
        assert!(rendered.iter().any(|s| s == "S:（1）工作职责范围扩大。"));
        assert!(rendered.iter().any(|s| s == "S:（5）解决方法。"));
        assert!(!rendered.iter().any(|s| s == "T:。"));
    }

    #[test]
    fn recognizes_plain_numbered_sections() {
        let blocks = content_blocks(
            "<p>1、</p><p>如果你是太阳，我就是黑洞。</p><p>2、AI 模型的世界就像一个城市。</p>",
            None,
        );
        assert!(matches!(&blocks[0], Block::Text(t) if t == "1、如果你是太阳，我就是黑洞。"));
        assert!(matches!(&blocks[1], Block::Strong(t) if t.starts_with("2、")));
    }

    #[test]
    fn joins_line_break_after_number_marker() {
        let blocks = content_blocks("<p>1.</p>\n<p>如果你是太阳，我就是黑洞。</p>", None);
        assert!(matches!(
        &blocks[0],
        Block::Text(t) | Block::Strong(t)
            if t == "1. 如果你是太阳，我就是黑洞。"
        ));
    }

    #[test]
    fn keeps_citation_prefix_and_following_numbered_paragraphs_distinct() {
        let html = concat!(
            "<p>1、如果你是太阳，我就是黑洞。</p>",
            "<p>---- <a href=\"https://example.com/hawking\">史蒂芬·霍金</a></p>",
            "<p>2、AI 模型的世界就像一个城市。</p>",
            "<p>-- <a href=\"https://example.com/book\">《奇点越来越近了》</a></p>",
            "<h2>往年回顾</h2>",
            "<p><a href=\"https://example.com/one\">稳定币的博弈</a>（#357）</p>",
            "<p><a href=\"https://example.com/two\">不要看重 Product Hunt</a>（#307）</p>",
        );
        let blocks = content_blocks(html, None);
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Link { text, link_start, .. }
                if text == "---- 史蒂芬·霍金" && *link_start == "---- ".len()
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Link { text, link_start, .. }
                if text == "-- 《奇点越来越近了》" && *link_start == "-- ".len()
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Text(text) | Block::Strong(text)
                if text == "2、AI 模型的世界就像一个城市。"
        )));
        let review_links = blocks
            .iter()
            .filter(|block| {
                matches!(
                    block,
                    Block::Link { text, link_start, .. }
                        if *link_start == 0
                            && (text == "稳定币的博弈" || text == "不要看重 Product Hunt")
                )
            })
            .count();
        assert_eq!(review_links, 2);
    }

    #[test]
    fn preserves_html_headings_without_promoting_inline_strong() {
        let blocks = content_blocks(
            concat!(
                "<h1>一级标题</h1>",
                "<h2>二级 <strong>重点</strong></h2>",
                "<h6>六级标题</h6>",
                "<p>正文 <strong>行内加粗</strong> 结束</p>"
            ),
            None,
        );

        assert!(matches!(&blocks[0], Block::Heading(t) if t == "一级标题"));
        assert!(matches!(&blocks[1], Block::Heading(t) if t == "二级 重点"));
        assert!(matches!(&blocks[2], Block::Heading(t) if t == "六级标题"));
        assert!(matches!(&blocks[3], Block::Text(t) if t == "正文"));
        assert!(matches!(&blocks[4], Block::Strong(t) if t == "行内加粗"));
        assert!(matches!(&blocks[5], Block::Text(t) if t == "结束"));
        assert_eq!(
            blocks
                .iter()
                .filter(|block| matches!(block, Block::Heading(_)))
                .count(),
            3
        );
    }

    #[test]
    fn preserves_blockquote_as_one_semantic_block() {
        let blocks = content_blocks(
            concat!(
                "<p>Before</p>",
                "<blockquote>",
                "<p>First <strong>important</strong> ",
                "<a href=\"https://example.com/source\">citation</a>.</p>",
                "<p>Second line<br>continued</p>",
                "</blockquote>",
                "<p>After</p>"
            ),
            None,
        );

        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], Block::Text(t) if t == "Before"));
        assert!(matches!(
            &blocks[1],
            Block::Quote(t)
                if t == "First important citation.\nSecond line\ncontinued"
        ));
        assert!(matches!(&blocks[2], Block::Text(t) if t == "After"));
    }

    #[test]
    fn keeps_images_nested_in_blockquotes_in_document_order() {
        let blocks = content_blocks(
            "<blockquote><p>Before image</p><img src=\"/quote.png\"><p>After image</p></blockquote>",
            Some("https://example.com/posts/one"),
        );

        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], Block::Quote(t) if t == "Before image"));
        assert!(matches!(
            &blocks[1],
            Block::Image(url) if url == "https://example.com/quote.png"
        ));
        assert!(matches!(&blocks[2], Block::Quote(t) if t == "After image"));
    }

    #[test]
    fn preserves_pre_code_whitespace_and_decodes_entities() {
        let blocks = content_blocks(
            "<p>Before</p><pre><code>\nfn main() {\n    println!(&quot;hi&quot;);\n    let ok = 1 &lt; 2;\n}\n</code></pre><p>After</p>",
            None,
        );

        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], Block::Text(t) if t == "Before"));
        assert!(matches!(
            &blocks[1],
            Block::Code(t)
                if t == "fn main() {\n    println!(\"hi\");\n    let ok = 1 < 2;\n}"
        ));
        assert!(matches!(&blocks[2], Block::Text(t) if t == "After"));
    }

    #[test]
    fn preserves_standalone_code_in_document_order() {
        let blocks = content_blocks(
            "<p>Run <code><span class=\"command\">cargo test --all</span></code> now.</p>",
            None,
        );

        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], Block::Text(t) if t == "Run"));
        assert!(matches!(&blocks[1], Block::InlineCode(t) if t == "cargo test --all"));
        assert!(matches!(&blocks[2], Block::Text(t) if t == "now."));
    }

    #[test]
    fn keeps_inline_code_inside_the_surrounding_sentence() {
        let blocks = content_blocks(
            "<p>If you have Rust installed via <code>rustup</code>, you can update it:</p>\
             <pre><code>$ rustup update stable</code></pre>\
             <p>Use the beta channel (<code>rustup default beta</code>) when testing.</p>",
            None,
        );

        assert!(
            !blocks.iter().any(
                |block| matches!(block, Block::Code(text) if text == "rustup" || text == "rustup default beta")
            ),
            "inline <code> must not become a standalone code block: {blocks:#?}"
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::InlineCode(text) if text == "rustup"))
        );
        assert!(blocks.iter().any(
            |block| matches!(block, Block::InlineCode(text) if text == "rustup default beta")
        ));
        assert!(
            blocks.iter().any(
                |block| matches!(block, Block::Code(text) if text == "$ rustup update stable")
            ),
            "<pre><code> must remain a standalone code block: {blocks:#?}"
        );
    }

    #[test]
    fn rust_blog_fixture_separates_inline_and_preformatted_code() {
        let blocks = content_blocks(
            include_str!("../tests/fixtures/rust-blog-inline-code.html"),
            Some("https://blog.rust-lang.org/2026/07/09/Rust-1.97.0/"),
        );

        assert!(blocks.iter().any(
            |block| matches!(block, Block::InlineCode(text) if text == "rustup default beta")
        ));
        assert!(
            blocks.iter().any(
                |block| matches!(block, Block::Code(text) if text == "$ rustup update stable")
            )
        );
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Link { text, url, .. }
                if text.contains("rustup") && url == "https://rustup.rs/"
        )));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::HeadingWithInlineCode { text, .. } if text == "New Range* types"))
        );
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::InlineCode(text) if text == "core::ops"))
        );
        assert!(
            !blocks
                .iter()
                .any(|block| matches!(block, Block::Code(text) if !text.starts_with('$')))
        );
    }

    #[test]
    fn heading_with_inline_code_remains_one_heading() {
        let blocks = content_blocks("<h2>New <code>Range*</code> types</h2>", None);

        assert!(matches!(
            blocks.as_slice(),
            [Block::HeadingWithInlineCode { text, inline_code_ranges }]
                if text == "New Range* types"
                    && inline_code_ranges.len() == 1
                    && &text[inline_code_ranges[0].start..inline_code_ranges[0].end] == "Range*"
        ));
    }

    #[test]
    fn does_not_treat_versions_or_decimals_as_numbered_headings() {
        for text in ["1.97.1", "32.6GB", "版本 1.97.1", "32.6GB 显存", "1.标题"] {
            assert!(
                !is_numbered_heading(text),
                "{text:?} must remain ordinary text"
            );
        }
        assert!(is_numbered_heading("1."));
        assert!(is_numbered_heading("1. 标题"));

        let blocks = content_blocks("<p>1.97.1 已发布</p><p>32.6GB 显存</p>", None);
        assert!(matches!(&blocks[0], Block::Text(t) if t == "1.97.1 已发布"));
        assert!(matches!(&blocks[1], Block::Text(t) if t == "32.6GB 显存"));
    }

    #[test]
    fn keeps_marker_only_text_before_inline_strong() {
        let blocks = content_blocks("<p>（1）<strong>is-</strong>：描述内容</p>", None);

        assert_eq!(blocks.len(), 3);
        assert!(matches!(&blocks[0], Block::Text(t) if t == "（1）"));
        assert!(matches!(&blocks[1], Block::Strong(t) if t == "is-"));
        assert!(matches!(&blocks[2], Block::Text(t) if t == "：描述内容"));

        for marker in ["(1)", "（1）", "1.", "1、"] {
            assert!(is_numbered_marker_only(marker), "{marker:?} is marker-only");
        }
        assert!(!is_numbered_marker_only("（1）完整标题"));

        let complete_heading = content_blocks("<p>（1）完整标题</p>", None);
        assert!(matches!(
            &complete_heading[0],
            Block::Strong(t) if t == "（1）完整标题"
        ));
    }

    #[test]
    fn martin_fowler_fixture_keeps_linked_heading_image_and_nested_lists() {
        let blocks = content_blocks(
            include_str!("../tests/fixtures/martinfowler-architecture-card.html"),
            Some("https://martinfowler.com/architecture/"),
        );

        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::LinkedImage { uri, url, alt }
                if uri == "https://martinfowler.com/articles/modularizing-react-apps/card.png"
                    && url == "https://martinfowler.com/articles/modularizing-react-apps.html"
                    && alt.as_deref() == Some("Modularizing React Applications")
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::HeadingLink { text, links }
                if text == "Modularizing React Applications"
                    && links.first().is_some_and(|link| link.url == "https://martinfowler.com/articles/modularizing-react-apps.html")
        )));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::ListItemStart { depth: 2 }))
        );
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Image(uri) if uri == "https://martinfowler.com/images/sketch.png"
        )));
    }

    #[test]
    fn beekka_fixture_keeps_table_code_language_and_list_media() {
        let blocks = content_blocks(
            include_str!("../tests/fixtures/beekka-weekly-rich-content.html"),
            Some("https://www.ruanyifeng.com/blog/2026/08/weekly-issue-407.html"),
        );

        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Table { rows, header_rows, .. }
                if *header_rows == 1
                    && rows.len() == 3
                    && rows[1].iter().map(|cell| cell.text.as_str()).collect::<Vec<_>>()
                        == ["RTX 5090", "104.8 TFLOPS"]
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::CodeBlock { text, language }
                if language == "bash" && text.contains("rg --files")
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::LinkedImage { uri, url, .. }
                if uri.ends_with("/asset/demo.webp") && url == "https://example.com/original"
        )));
        assert!(
            blocks
                .iter()
                .any(|block| matches!(block, Block::ListItemStart { depth: 2 }))
        );
    }

    #[test]
    fn wikipedia_fixture_keeps_math_table_and_clickable_footnotes() {
        let blocks = content_blocks(
            include_str!("../tests/fixtures/wikipedia-math-footnotes-table.html"),
            Some("https://zh.wikipedia.org/wiki/欧拉恒等式"),
        );

        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Math { source, display: true } if source == "e^{i\\pi}+1=0"
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Math { source, display: true } if source.contains("\\int_0^1")
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Link { text, url, link_start, .. }
                if text.ends_with("[1]")
                    && &text[*link_start..] == "[1]"
                    && url.ends_with("#cite_note-1")
        )));
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Table { rows, header_rows, .. }
                if *header_rows == 1 && rows.len() == 2 && rows[1][0].text == "π"
        )));
    }

    #[test]
    fn responsive_fixture_keeps_picture_caption_definitions_and_spans() {
        let snapshot =
            prepare_html_snapshot(include_str!("../tests/fixtures/responsive-semantics.html"));
        let blocks = content_blocks(
            &snapshot.content,
            Some("https://fixture.example/articles/semantic.html"),
        );

        assert_eq!(
            images(&blocks),
            ["https://fixture.example/media/hero-1280.jpg"]
        );
        assert!(matches!(
            blocks.iter().find(|block| matches!(block, Block::Caption(_))),
            Some(Block::Caption(text)) if text == "图 1：不同屏幕共用一条说明文字。"
        ));
        assert!(matches!(
            blocks.iter().find(|block| matches!(block, Block::DefinitionList(_))),
            Some(Block::DefinitionList(items))
                if items.len() == 2
                    && items[0].term == "渐进增强"
                    && items[0].definitions.len() == 2
        ));
        assert!(matches!(
            blocks.iter().find(|block| matches!(block, Block::Table { .. })),
            Some(Block::Table { rows, column_count, .. })
                if *column_count == 3
                    && rows[0][0].row_span == 2
                    && rows[0][1].col_span == 2
                    && rows[3][0].col_span == 2
        ));
    }

    #[test]
    fn readability_falls_back_when_math_semantics_would_be_lost() {
        let snapshot = prepare_html_snapshot(concat!(
            "<!doctype html><html><head><title>Math note</title></head><body>",
            "<main><article><h1>Math note</h1><p>Equation follows.</p>",
            "<script type='math/tex; mode=display'>x^2 + y^2</script>",
            "<p>Explanation remains.</p></article></main></body></html>"
        ));
        let blocks = content_blocks(&snapshot.content, None);
        assert!(blocks.iter().any(|block| matches!(
            block,
            Block::Math { source, display: true } if source == "x^2 + y^2"
        )));
        assert!(all_visible_text(&blocks).contains("Explanation remains."));
    }

    #[test]
    fn html5_dom_and_readability_candidates_are_fixture_gated() {
        let html = include_str!("../tests/fixtures/responsive-semantics.html");

        // scraper is backed by html5ever. This proves a browser-grade DOM can
        // recover the nested semantics we currently preserve by hand.
        let document = scraper::Html::parse_document(html);
        let selector = scraper::Selector::parse(
            "picture source, figure figcaption, dl dt, dl dd, td[rowspan], th[colspan]",
        )
        .expect("valid fixture selector");
        assert_eq!(document.select(&selector).count(), 10);

        // Readability is evaluated as a *content selection* stage. Feed
        // fragments and guide/index pages still bypass it because selecting
        // one dominant article would discard useful cards and list entries.
        let scope = html5_reading_scope(html);
        assert_eq!(scope.kind, CompletePageKind::SingleArticle);
        let source = strip_ignored_elements(&scope.content);
        let gated = guarded_readability_content(
            html,
            Some("https://fixture.example/articles/semantic.html"),
            &source,
        )
        .expect("semantic fixture should pass the production Readability gate");
        assert!(gated.contains("rowspan=\"2\""));

        let mut readability = dom_smoothie::Readability::new(
            html,
            Some("https://fixture.example/articles/semantic.html"),
            Some(dom_smoothie::Config {
                char_threshold: 0,
                ..Default::default()
            }),
        )
        .expect("fixture should form a valid HTML5 document");
        let article = readability.parse().expect("fixture should be readable");
        assert!(article.content.contains("渐进增强"));
        assert!(article.content.contains("rowspan=\"2\""));
    }

    fn semantic_snapshot_svg(blocks: &[Block]) -> String {
        use std::fmt::Write as _;

        let mut body = String::new();
        let mut y = 20usize;
        for block in blocks {
            match block {
                Block::Heading(text) => {
                    let _ = writeln!(
                        body,
                        "  <text class=\"heading\" x=\"20\" y=\"{}\">{}</text>",
                        y + 24,
                        escape_html_attribute(text)
                    );
                    y += 52;
                }
                Block::Image(uri) | Block::LinkedImage { uri, .. } => {
                    let _ = writeln!(
                        body,
                        "  <g class=\"image\"><rect x=\"20\" y=\"{y}\" width=\"720\" height=\"100\"/><text x=\"36\" y=\"{}\">{}</text></g>",
                        y + 56,
                        escape_html_attribute(uri)
                    );
                    y += 116;
                }
                Block::Caption(text) => {
                    let _ = writeln!(
                        body,
                        "  <text class=\"caption\" x=\"380\" y=\"{}\" text-anchor=\"middle\">{}</text>",
                        y + 18,
                        escape_html_attribute(text)
                    );
                    y += 34;
                }
                Block::DefinitionList(items) => {
                    let line_count: usize =
                        items.iter().map(|item| 1 + item.definitions.len()).sum();
                    let height = 20 + line_count * 24;
                    let _ = writeln!(
                        body,
                        "  <g class=\"definition-list\"><rect x=\"20\" y=\"{y}\" width=\"720\" height=\"{height}\"/>"
                    );
                    let mut line_y = y + 26;
                    for item in items {
                        let _ = writeln!(
                            body,
                            "    <text class=\"term\" x=\"36\" y=\"{line_y}\">{}</text>",
                            escape_html_attribute(&item.term)
                        );
                        line_y += 24;
                        for definition in &item.definitions {
                            let _ = writeln!(
                                body,
                                "    <text class=\"definition\" x=\"58\" y=\"{line_y}\">— {}</text>",
                                escape_html_attribute(definition)
                            );
                            line_y += 24;
                        }
                    }
                    body.push_str("  </g>\n");
                    y += height + 16;
                }
                Block::Table {
                    rows, column_count, ..
                } => {
                    let unit = 720.0 / (*column_count).max(1) as f32;
                    let layout = table_cell_columns(rows, *column_count);
                    let _ = writeln!(body, "  <g class=\"table\">");
                    for (row_index, row) in rows.iter().enumerate() {
                        for (column, cell_index) in &layout[row_index] {
                            let cell = &row[*cell_index];
                            let x = 20.0 + *column as f32 * unit;
                            let width = cell.col_span as f32 * unit;
                            let cell_y = y + row_index * 44;
                            let _ = writeln!(
                                body,
                                "    <g class=\"cell\" data-rowspan=\"{}\" data-colspan=\"{}\"><rect x=\"{x:.0}\" y=\"{cell_y}\" width=\"{width:.0}\" height=\"44\"/><text x=\"{:.0}\" y=\"{}\">{}</text></g>",
                                cell.row_span,
                                cell.col_span,
                                x + 10.0,
                                cell_y + 27,
                                escape_html_attribute(&cell.text)
                            );
                        }
                    }
                    body.push_str("  </g>\n");
                    y += rows.len() * 44 + 20;
                }
                _ => {}
            }
        }
        format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 760 {}\">\n<style>.heading,.term{{font-weight:700}}.image rect,.definition-list rect,.cell rect{{fill:#f8f8f8;stroke:#ddd}}text{{font:15px sans-serif;fill:#333}}.caption{{fill:#888}}</style>\n{body}</svg>\n",
            y + 20
        )
    }

    #[test]
    fn semantic_blocks_match_visual_svg_snapshot() {
        let blocks = content_blocks(
            include_str!("../tests/fixtures/responsive-semantics.html"),
            Some("https://fixture.example/articles/semantic.html"),
        );
        let actual = semantic_snapshot_svg(&blocks);
        let expected = include_str!("../tests/snapshots/semantic-blocks.svg");
        assert_eq!(actual, expected, "semantic visual snapshot changed");
    }
}
