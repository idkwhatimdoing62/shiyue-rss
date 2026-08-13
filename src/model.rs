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
    pub comment: Option<String>,
    pub is_favorite: bool,
    pub created_at: i64,
    pub updated_at: i64,
}
