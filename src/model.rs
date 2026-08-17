//! 领域类型：源与文章。术语见 docs/glossary.md。

/// 一个订阅源（feeds 表一行）。
/// 部分字段（last_fetch/next_fetch 等）只经 SQL 读写，Rust 侧不直接读，故 allow。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Feed {
    pub id: i64,
    pub url: String,
    pub title: Option<String>,
    /// 单源抓取间隔（秒）；None = 用全局默认（ADR-9）。
    pub interval_secs: Option<i64>,
    pub last_fetch: Option<i64>,
    pub next_fetch: i64,
    pub last_error: Option<String>,
    pub fail_count: i64,
    pub disabled: bool,
}

/// 一篇已入库的文章（articles 表一行）。
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Article {
    pub id: i64,
    pub feed_id: i64,
    pub entry_id: String,
    pub url: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub published: Option<i64>,
    pub content: Option<String>,
    pub is_read: bool,
    pub starred: bool,
    pub read_later: bool,
    pub archived: bool,
    pub fetched_at: i64,
}

/// 解析出来、准备入库的条目（还没有 db id）。
#[derive(Debug, Clone)]
pub struct NewArticle {
    pub entry_id: String,
    pub url: Option<String>,
    pub title: Option<String>,
    pub author: Option<String>,
    pub published: Option<i64>,
    pub content: Option<String>,
}

/// 用户在文章正文中选中的一段文字。
///
/// 一条选区既可以只作为收藏，也可以附带评论；这样同一段文字的显示、
/// 评论编辑和收藏切换都能由同一条记录承载。`start_offset`/`end_offset`
/// 是可选的字符偏移，UI 无法可靠取得偏移时可以传 `None`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleSelection {
    pub id: i64,
    pub article_id: i64,
    pub selected_text: String,
    pub start_offset: Option<i64>,
    pub end_offset: Option<i64>,
    pub anchor_prefix: String,
    pub anchor_suffix: String,
    pub comment: Option<String>,
    pub is_favorite: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

/// A quote's durable position inside article text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextAnchor {
    pub start_offset: Option<i64>,
    pub end_offset: Option<i64>,
    pub prefix: String,
    pub suffix: String,
}

impl TextAnchor {
    pub fn capture(document: &str, start: usize, end: usize, context_chars: usize) -> Self {
        let chars = document.chars().collect::<Vec<_>>();
        let start = start.min(chars.len());
        let end = end.clamp(start, chars.len());
        let prefix_start = start.saturating_sub(context_chars);
        let suffix_end = end.saturating_add(context_chars).min(chars.len());
        Self {
            start_offset: Some(start as i64),
            end_offset: Some(end as i64),
            prefix: chars[prefix_start..start].iter().collect(),
            suffix: chars[end..suffix_end].iter().collect(),
        }
    }
}

/// Locate a saved quote after the article text may have changed.
///
/// Exact historical offsets win when still valid. Otherwise every exact quote
/// occurrence is scored by the surrounding prefix/suffix context; the old
/// offset is only a final tie breaker.
pub fn resolve_excerpt_anchor(
    document: &str,
    quote: &str,
    anchor: &TextAnchor,
) -> Option<std::ops::Range<usize>> {
    let document = document.chars().collect::<Vec<_>>();
    let quote = quote.chars().collect::<Vec<_>>();
    if quote.is_empty() || quote.len() > document.len() {
        return None;
    }

    if let (Some(start), Some(end)) = (anchor.start_offset, anchor.end_offset) {
        let (start, end) = (usize::try_from(start).ok()?, usize::try_from(end).ok()?);
        if end <= document.len() && start <= end && document[start..end] == quote {
            return Some(start..end);
        }
    }

    let prefix = anchor.prefix.chars().collect::<Vec<_>>();
    let suffix = anchor.suffix.chars().collect::<Vec<_>>();
    let old_start = anchor
        .start_offset
        .and_then(|value| usize::try_from(value).ok());
    let mut best: Option<(usize, usize, usize)> = None;
    for start in 0..=document.len() - quote.len() {
        let end = start + quote.len();
        if document[start..end] != quote {
            continue;
        }
        let prefix_score = (0..prefix.len().min(start))
            .take_while(|offset| prefix[prefix.len() - 1 - offset] == document[start - 1 - offset])
            .count();
        let suffix_score = (0..suffix.len().min(document.len() - end))
            .take_while(|offset| suffix[*offset] == document[end + offset])
            .count();
        let context_score = prefix_score + suffix_score;
        let distance = old_start.map_or(0, |old| old.abs_diff(start));
        match best {
            None => best = Some((start, context_score, distance)),
            Some((_, best_score, best_distance))
                if context_score > best_score
                    || (context_score == best_score && distance < best_distance) =>
            {
                best = Some((start, context_score, distance));
            }
            _ => {}
        }
    }
    best.map(|(start, _, _)| start..start + quote.len())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub id: i64,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArticleBatchAction {
    Archive,
    Bookmark,
    ReadLater,
}

/// 全局搜索的一条命中。文章标题、作者、正文与网址产生 `Article` 命中；
/// 摘录原文和想法分别产生对应命中。所有结果都保留文章 id，供界面直接跳回原文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchHitKind {
    Article,
    WebClipping,
    Excerpt,
    Thought,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub kind: SearchHitKind,
    pub article_id: i64,
    pub selection_id: Option<i64>,
    pub feed_id: i64,
    pub article_title: Option<String>,
    pub snippet: String,
    pub timestamp: i64,
    pub archived: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHistoryEntry {
    pub query: String,
    pub last_used_at: i64,
    pub use_count: i64,
    pub result_count: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_anchor_survives_insertions_before_quote() {
        let original = "开头。重复句。中间。重复句。结尾。";
        let start = original.find("重复句").unwrap();
        let start = original[..start].chars().count();
        let end = start + "重复句".chars().count();
        let anchor = TextAnchor::capture(original, start, end, 8);
        let updated = "新增内容。开头。重复句。中间。重复句。结尾。";
        let range = resolve_excerpt_anchor(updated, "重复句", &anchor).unwrap();
        let selected = updated
            .chars()
            .skip(range.start)
            .take(range.len())
            .collect::<String>();
        assert_eq!(selected, "重复句");
        assert_eq!(range.start, "新增内容。开头。".chars().count());
    }
}
