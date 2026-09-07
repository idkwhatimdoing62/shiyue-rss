//! Small, site-specific adapter for 阮一峰's monthly archive pages.

use anyhow::{Context, Result, bail};
use scraper::{Html, Selector};

pub(crate) const DEFAULT_ARCHIVE_URL: &str = "https://www.ruanyifeng.com/blog/archives.html";
pub(crate) const BATCH_SIZE: usize = 50;

pub(crate) fn is_supported_feed_url(url: &str) -> bool {
    let normalized = url.trim().to_ascii_lowercase();
    normalized.contains("ruanyifeng.com") || normalized.contains("feeds.feedburner.com/ruanyifeng")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchiveEntry {
    pub(crate) url: String,
    pub(crate) title: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ArchivePage {
    pub(crate) entries: Vec<ArchiveEntry>,
    pub(crate) month_url: Option<String>,
    pub(crate) previous_month: Option<String>,
}

pub(crate) fn parse_archive_page(html: &str, base_url: &str) -> Result<ArchivePage> {
    let document = Html::parse_document(html);
    let links = Selector::parse("a[href]").expect("static selector");
    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let base = reqwest::Url::parse(base_url).context("归档页面地址无效")?;
    for link in document.select(&links) {
        let Some(href) = link.value().attr("href") else {
            continue;
        };
        let Ok(url) = base.join(href) else { continue };
        let path = url.path();
        if !path.starts_with("/blog/")
            || !path.ends_with(".html")
            || path.ends_with("/index.html")
            || path.ends_with("/archives.html")
        {
            continue;
        }
        let url = url.to_string();
        if !seen.insert(url.clone()) {
            continue;
        }
        let title = link
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() {
            continue;
        }
        entries.push(ArchiveEntry { url, title });
    }
    let month_url = document
        .select(&links)
        .filter_map(|link| link.value().attr("href"))
        .filter_map(|href| base.join(href).ok())
        .find_map(|mut url| {
            let path = url.path().to_owned();
            let segments = path.trim_matches('/').split('/').collect::<Vec<_>>();
            let is_month = (segments.len() == 3
                && segments[0] == "blog"
                && segments[1].len() == 4
                && segments[2].len() == 2)
                || (segments.len() == 4
                    && segments[0] == "blog"
                    && segments[1].len() == 4
                    && segments[2].len() == 2
                    && segments[3] == "index.html");
            if !is_month {
                return None;
            }
            if !url.path().ends_with("/index.html") {
                url.set_path(&format!("{}/index.html", url.path().trim_end_matches('/')));
            }
            Some(url.to_string())
        });
    let previous_month = document
        .select(&links)
        .find(|link| link.text().any(|text| text.contains("上月")))
        .and_then(|link| link.value().attr("href"))
        .and_then(|href| base.join(href).ok())
        .map(|url| url.to_string());
    if entries.is_empty() && previous_month.is_none() && month_url.is_none() {
        bail!("归档页面没有发现文章链接");
    }
    Ok(ArchivePage {
        entries,
        month_url,
        previous_month,
    })
}

#[allow(dead_code)]
pub(crate) fn next_batch(
    entries: &[ArchiveEntry],
    cursor: &mut usize,
    batch_size: usize,
) -> Vec<ArchiveEntry> {
    let start = (*cursor).min(entries.len());
    let end = start.saturating_add(batch_size).min(entries.len());
    *cursor = end;
    entries[start..end].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_official_and_feedburner_subscriptions() {
        assert!(is_supported_feed_url(
            "https://www.ruanyifeng.com/blog/atom.xml"
        ));
        assert!(is_supported_feed_url(
            "http://feeds.feedburner.com/ruanyifeng"
        ));
        assert!(!is_supported_feed_url("https://example.com/feed.xml"));
    }

    #[test]
    fn parses_and_deduplicates_article_links_and_previous_month() {
        let html = r#"
          <a href="/blog/2026/09/a.html">第一篇</a>
          <a href="/blog/2026/09/a.html">重复</a>
          <a href="/blog/2026/09/index.html">月份</a>
          <a href="/blog/2026/08/index.html">上月</a>
        "#;
        let page =
            parse_archive_page(html, "https://www.ruanyifeng.com/blog/2026/09/index.html").unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(
            page.month_url.as_deref(),
            Some("https://www.ruanyifeng.com/blog/2026/09/index.html")
        );
        assert_eq!(
            page.entries[0].url,
            "https://www.ruanyifeng.com/blog/2026/09/a.html"
        );
        assert_eq!(
            page.previous_month.as_deref(),
            Some("https://www.ruanyifeng.com/blog/2026/08/index.html")
        );
    }

    #[test]
    fn batches_at_requested_size() {
        let entries = (0..120)
            .map(|i| ArchiveEntry {
                url: format!("https://x/{i}.html"),
                title: i.to_string(),
            })
            .collect::<Vec<_>>();
        let mut cursor = 0;
        assert_eq!(next_batch(&entries, &mut cursor, 50).len(), 50);
        assert_eq!(next_batch(&entries, &mut cursor, 50).len(), 50);
        assert_eq!(next_batch(&entries, &mut cursor, 50).len(), 20);
        assert!(next_batch(&entries, &mut cursor, 50).is_empty());
    }

    #[test]
    fn recognizes_month_directory_links() {
        let page = parse_archive_page(
            r#"<a href="/blog/2026/09/">按日期排列</a>"#,
            "https://www.ruanyifeng.com/blog/archives.html",
        )
        .unwrap();
        assert_eq!(
            page.month_url.as_deref(),
            Some("https://www.ruanyifeng.com/blog/2026/09/index.html")
        );
    }
}
