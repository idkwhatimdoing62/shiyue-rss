//! Bounded, site-independent archive discovery and feed paging.
use std::collections::{HashSet, VecDeque};

use anyhow::{Result, anyhow};
use reqwest::Url;
use scraper::{Html, Selector};

use crate::{fetch::entry_to_article, model::NewArticle, web_clip::FetchedWebClip};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    Feed,
    Archive,
}

#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub(crate) article: NewArticle,
    pub(crate) fetch_body: bool,
}

pub(crate) struct Pager {
    pub(crate) kind: SourceKind,
    pub(crate) description: String,
    pending: VecDeque<String>,
    visited: HashSet<String>,
}

struct Page {
    entries: Vec<Entry>,
    pages: Vec<String>,
}

/// The fetch adapter enforces network policy; fixture adapters make discovery testable.
pub(crate) fn discover(
    feed_url: &str,
    fetch: &mut impl FnMut(&str, SourceKind) -> Result<FetchedWebClip>,
) -> Result<(Pager, Vec<Entry>)> {
    let document = fetch(feed_url, SourceKind::Feed)?;
    let feed = feed_rs::parser::parse(document.html.as_bytes())?;
    let base = Url::parse(&document.final_url)?;
    let home = feed
        .links
        .iter()
        .find(|l| {
            l.rel.as_deref().is_none_or(|r| r == "alternate")
                && l.media_type.as_deref().is_none_or(|t| t.contains("html"))
        })
        .and_then(|l| web_url(&base, &l.href));
    let feed_page = parse_feed(&document)?;
    let samples: HashSet<String> = feed_page
        .entries
        .iter()
        .filter_map(|e| e.article.url.clone())
        .collect();
    // A feed proxy's own homepage is not evidence of the publisher's archive.
    let home = home
        .or_else(|| {
            feed_page
                .entries
                .first()
                .and_then(|e| e.article.url.as_deref())
                .and_then(|u| Url::parse(u).ok())
                .and_then(|u| u.join("/").ok())
                .map(|u| u.to_string())
        })
        .unwrap_or_else(|| base.join("/").unwrap().to_string());
    let mut candidates = VecDeque::new();
    let mut checked = HashSet::new();
    let mut detection_failed = false;
    match fetch(&home, SourceKind::Archive) {
        Ok(homepage) => candidates.extend(archive_candidates(&homepage.html, &homepage.final_url)),
        Err(_) => detection_failed = true,
    }
    // Some publishers block their home page while leaving the static archive
    // reachable. Probe a small, same-site set of conventional paths; every
    // candidate still has to pass the article-list validation below.
    let mut hosts = Vec::new();
    if let Some(article) = feed_page
        .entries
        .first()
        .and_then(|e| e.article.url.as_deref())
    {
        if let Ok(article) = Url::parse(article) {
            if !is_feed_mirror(&article) {
                hosts.push(article);
            }
        }
    }
    if !is_feed_mirror(&base) && !hosts.iter().any(|host| host.host_str() == base.host_str()) {
        hosts.push(base.clone());
    }
    // If the source exposes only a mirror URL, still use the source's final
    // host as a last resort; all candidates must pass page validation.
    if hosts.is_empty() {
        hosts.push(base.clone());
    }
    for host in hosts {
        for path in [
            "/archives.html",
            "/archive.html",
            "/archives",
            "/archive",
            "/blog/archives.html",
        ] {
            if let Ok(candidate) = host.join(path) {
                let candidate = candidate.to_string();
                if same_site(&base, &candidate) && !candidates.contains(&candidate) {
                    candidates.push_back(candidate);
                }
            }
        }
    }
    // Discovery never guesses a fixed path or recursively crawls an entire site.
    for _ in 0..6 {
        let Some(candidate) = candidates.pop_front() else {
            break;
        };
        if !checked.insert(candidate.clone()) {
            continue;
        }
        let document = match fetch(&candidate, SourceKind::Archive) {
            Ok(page) => page,
            Err(_) => {
                detection_failed = true;
                continue;
            }
        };
        if !same_site(&Url::parse(&candidate)?, &document.final_url) {
            detection_failed = true;
            continue;
        }
        let Ok(page) = parse_archive(&document, &samples) else {
            continue;
        };
        if page.entries.len() >= 2 {
            let pager = Pager {
                kind: SourceKind::Archive,
                description: format!("自动检测：使用归档页 {}", document.final_url),
                pending: page.pages.into_iter().chain(candidates).collect(),
                visited: HashSet::from([document.final_url, candidate]),
            };
            return Ok((pager, page.entries));
        }
        for next in page.pages.into_iter().rev() {
            candidates.push_front(next);
        }
    }
    let reason = if detection_failed {
        "归档页检测未完成或不可访问"
    } else {
        "未发现可验证的归档页"
    };
    let description = if feed_page.pages.is_empty() {
        format!("{reason}；订阅源未提供历史分页，只能读取当前条目。")
    } else {
        format!("{reason}；使用 RSS/Atom 历史分页。")
    };
    Ok((
        Pager {
            kind: SourceKind::Feed,
            description,
            pending: feed_page.pages.into(),
            visited: HashSet::from([feed_url.to_owned(), document.final_url]),
        },
        feed_page.entries,
    ))
}

impl Pager {
    pub(crate) fn has_more(&self) -> bool {
        self.pending.iter().any(|url| !self.visited.contains(url))
    }

    pub(crate) fn next_page(
        &mut self,
        fetch: &mut impl FnMut(&str, SourceKind) -> Result<FetchedWebClip>,
    ) -> Result<Vec<Entry>> {
        while self
            .pending
            .front()
            .is_some_and(|u| self.visited.contains(u))
        {
            self.pending.pop_front();
        }
        let Some(url) = self.pending.front().cloned() else {
            return Ok(Vec::new());
        };
        let document = fetch(&url, self.kind)?;
        if self.kind == SourceKind::Archive && !same_site(&Url::parse(&url)?, &document.final_url) {
            return Err(anyhow!("归档页跳转到了其他网站，已停止继续回补"));
        }
        if self.visited.contains(&document.final_url) {
            self.pending.pop_front();
            self.visited.insert(url);
            return Ok(Vec::new());
        }
        let page = match self.kind {
            SourceKind::Feed => parse_feed(&document)?,
            SourceKind::Archive => parse_archive(&document, &HashSet::new())?,
        };
        // Advance only after successful fetch and parse, so a failure remains retryable.
        self.pending.pop_front();
        self.visited.insert(url);
        self.visited.insert(document.final_url);
        for url in page.pages {
            if !self.visited.contains(&url) && !self.pending.contains(&url) {
                self.pending.push_back(url);
            }
        }
        Ok(page.entries)
    }
}

fn web_url(base: &Url, href: &str) -> Option<String> {
    if href.trim().is_empty() || href.starts_with('#') {
        return None;
    }
    let mut url = base.join(href).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    url.set_fragment(None);
    Some(url.to_string())
}

fn parse_feed(document: &FetchedWebClip) -> Result<Page> {
    let feed = feed_rs::parser::parse(document.html.as_bytes())?;
    let base = Url::parse(&document.final_url)?;
    let pages = ["prev-archive", "next"]
        .into_iter()
        .find_map(|rel| {
            feed.links
                .iter()
                .find(|l| l.rel.as_deref() == Some(rel))
                .and_then(|l| web_url(&base, &l.href))
        })
        .into_iter()
        .collect();
    let entries = feed
        .entries
        .into_iter()
        .take(2000)
        .map(|entry| {
            let mut article = entry_to_article(entry);
            article.url = article.url.as_deref().and_then(|url| web_url(&base, url));
            Entry {
                article,
                fetch_body: false,
            }
        })
        .collect();
    Ok(Page { entries, pages })
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::bail;

    fn doc(url: &str, html: &str) -> FetchedWebClip {
        FetchedWebClip {
            original_url: url.into(),
            final_url: url.into(),
            html: html.into(),
        }
    }

    fn atom(next: &str) -> String {
        format!(
            r#"<feed xmlns="http://www.w3.org/2005/Atom">
          <id>test</id><title>Blog</title><updated>2026-09-01T00:00:00Z</updated>
          <link rel="alternate" href="https://blog.test/"/>{next}
          <entry><id>entry-one</id><title>One</title><link href="/2026/09/one.html"/>
          <published>2026-09-01T00:00:00Z</published><updated>2026-09-01T00:00:00Z</updated>
          <author><name>Alice</name></author><content type="html">Full body</content></entry>
        </feed>"#
        )
    }

    fn archive() -> &'static str {
        r#"<main><h1>Archives</h1><a href="/2026/09/one.html#comments">One</a>
        <a href="/2026/08/two.html">Two</a>
        <aside><a href="/2026/08/ad.html">Advertisement</a></aside>
        <nav><a href="/archives?page=2" rel="next">下一页</a></nav></main>"#
    }

    #[test]
    fn discovers_arbitrary_site_archive_and_rejects_blogroll() {
        let (mut pager, entries) = discover("https://blog.test/feed", &mut |url, kind| {
            let html = match url {
                "https://blog.test/feed" => {
                    assert_eq!(kind, SourceKind::Feed);
                    atom("")
                }
                "https://blog.test/" => r#"<a href="/archives">文章归档</a>"#.into(),
                "https://blog.test/archives" => archive().into(),
                "https://blog.test/archives?page=2" => archive().replace("one.html", "three.html"),
                _ => "<h1>Not an archive</h1>".into(),
            };
            Ok(doc(url, &html))
        })
        .unwrap();
        assert_eq!(pager.kind, SourceKind::Archive);
        assert_eq!(entries.len(), 2);
        assert!(entries.iter().all(|entry| entry.fetch_body));
        assert!(!entries[0].article.url.as_ref().unwrap().contains('#'));
        assert!(pager.has_more());
        while pager.has_more() {
            pager
                .next_page(&mut |url, _| Ok(doc(url, archive())))
                .unwrap();
        }
        assert!(!pager.has_more(), "self-loop must terminate");
    }

    #[test]
    fn feedburner_uses_publisher_home_and_date_archive_without_domain_rule() {
        let mut requests = Vec::new();
        let (pager, entries) = discover("https://proxy.test/source", &mut |url, _| {
            requests.push(url.to_owned());
            let html = match url {
                "https://proxy.test/source" => atom(""),
                "https://blog.test/" => r#"<a href="/all">按日期排列</a>"#.into(),
                "https://blog.test/all" => {
                    r#"<a href="/2026/09/">九月</a><a href="/2026/08/">八月</a>"#.into()
                }
                "https://blog.test/2026/09/" => archive().into(),
                _ => "<h1>Not an archive</h1>".into(),
            };
            Ok(doc(url, &html))
        })
        .unwrap();
        assert_eq!(pager.kind, SourceKind::Archive);
        assert_eq!(entries.len(), 2);
        assert!(pager.pending.contains(&"https://blog.test/2026/08/".into()));
        assert!(!requests.contains(&"https://proxy.test/".into()));
    }

    #[test]
    fn falls_back_to_relative_feed_paging_and_preserves_metadata() {
        let (mut pager, entries) = discover("https://blog.test/feed", &mut |url, _| {
            Ok(doc(
                url,
                &if url.ends_with("/feed") {
                    atom(r#"<link rel="next" href="?page=2"/>"#)
                } else {
                    "<a href='/about'>About</a>".into()
                },
            ))
        })
        .unwrap();
        assert_eq!(pager.kind, SourceKind::Feed);
        assert!(!entries[0].fetch_body);
        assert_eq!(entries[0].article.author.as_deref(), Some("Alice"));
        assert!(entries[0].article.published.is_some());
        assert_eq!(entries[0].article.content.as_deref(), Some("Full body"));
        let mut attempts = 0;
        assert!(
            pager
                .next_page(&mut |url, _| {
                    assert_eq!(url, "https://blog.test/feed?page=2");
                    attempts += 1;
                    bail!("temporary failure")
                })
                .is_err()
        );
        assert!(pager.has_more());
        pager
            .next_page(&mut |url, _| {
                attempts += 1;
                Ok(doc(url, &atom("")))
            })
            .unwrap();
        assert_eq!(attempts, 2);
        assert!(!pager.has_more());
    }

    #[test]
    fn rss_atom_namespace_paging_and_archive_relations_are_supported() {
        let rss = r#"<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom"><channel>
          <title>Blog</title><link>https://blog.test/</link><description>Blog</description>
          <atom:link rel="next" href="?page=2"/>
          <item><guid>one</guid><title>One</title><link>https://blog.test/one</link></item>
        </channel></rss>"#;
        let page = parse_feed(&doc("https://blog.test/feed", rss)).unwrap();
        assert_eq!(page.pages, vec!["https://blog.test/feed?page=2"]);
        let page = parse_feed(&doc(
            "https://blog.test/feed",
            &atom(r#"<link rel="prev-archive" href="old.atom"/>"#),
        ))
        .unwrap();
        assert_eq!(page.pages, vec!["https://blog.test/old.atom"]);
    }

    #[test]
    fn bad_archive_and_unreachable_home_do_not_claim_full_history() {
        for home_fails in [false, true] {
            let (pager, _) = discover("https://blog.test/feed", &mut |url, _| {
                if url.ends_with("/feed") {
                    return Ok(doc(url, &atom("")));
                }
                if home_fails {
                    bail!("timeout")
                }
                Ok(doc(
                    url,
                    if url.ends_with("/archives") {
                        "<h1>Login required</h1>"
                    } else {
                        r#"<a href="/archives">Archives</a>"#
                    },
                ))
            })
            .unwrap();
            assert_eq!(pager.kind, SourceKind::Feed);
            assert!(!pager.has_more());
            assert!(pager.description.contains("只能读取当前条目"));
        }
    }

    #[test]
    fn archive_discovery_stays_same_site_and_rejects_unsafe_schemes() {
        let links = archive_candidates(
            r#"<a href="https://evil.test/archives">Archives</a>
            <a href="javascript:alert(1)">Archives</a><a href="/archives">Archives</a>
            <a href="/archives#x">Archives</a>"#,
            "https://blog.test/",
        );
        assert_eq!(links, vec!["https://blog.test/archives"]);
    }

    #[test]
    fn keeps_the_single_article_on_a_final_archive_page() {
        let page = parse_archive(
            &doc(
                "https://blog.test/archives?page=99",
                r#"<main><h2><a href="/very-first-post">First post</a></h2></main>"#,
            ),
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert!(page.pages.is_empty());
    }
}

fn archive_label(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    [
        "归档",
        "历史文章",
        "按日期",
        "archive",
        "所有文章",
        "全部文章",
        "往期文章",
        "all posts",
    ]
    .iter()
    .any(|word| t.contains(word))
}

fn archive_candidates(html: &str, base: &str) -> Vec<String> {
    let Ok(base) = Url::parse(base) else {
        return Vec::new();
    };
    let document = Html::parse_document(html);
    let mut seen = HashSet::new();
    document
        .select(&Selector::parse("a[href]").unwrap())
        .filter(|a| {
            archive_label(&a.text().collect::<String>())
                || a.value().attr("rel") == Some("archives")
        })
        .filter_map(|a| web_url(&base, a.value().attr("href")?))
        .filter(|u| same_site(&base, u) && seen.insert(u.clone()))
        .take(6)
        .collect()
}

fn same_site(base: &Url, url: &str) -> bool {
    Url::parse(url).ok().is_some_and(|u| {
        u.host_str().map(|h| h.trim_start_matches("www."))
            == base.host_str().map(|h| h.trim_start_matches("www."))
    })
}

fn is_feed_mirror(url: &Url) -> bool {
    url.host_str().is_some_and(|host| {
        let host = host.trim_start_matches("www.");
        host == "feeds.feedburner.com" || host.starts_with("feeds.") || host.contains("feedburner")
    })
}

fn date_path(url: &Url) -> (bool, bool) {
    let parts: Vec<_> = url.path().trim_matches('/').split('/').collect();
    let date = parts.iter().position(|part| {
        part.len() == 4
            && part
                .parse::<u16>()
                .is_ok_and(|year| (1900..=2200).contains(&year))
    });
    let Some(i) = date else { return (false, false) };
    let tail = &parts[i + 1..];
    let index = tail.iter().all(|part| {
        part.is_empty() || *part == "index.html" || (part.len() <= 2 && part.parse::<u8>().is_ok())
    });
    (true, index)
}

fn parse_archive(document: &FetchedWebClip, samples: &HashSet<String>) -> Result<Page> {
    let base = Url::parse(&document.final_url)?;
    let html = Html::parse_document(&document.html);
    let anchors = Selector::parse("a[href]").unwrap();
    let mut entries = Vec::new();
    let mut pages = Vec::new();
    let mut seen = HashSet::new();
    for a in html.select(&anchors) {
        let Some(url) = web_url(&base, a.value().attr("href").unwrap_or("")) else {
            continue;
        };
        if !same_site(&base, &url) || url == document.final_url || !seen.insert(url.clone()) {
            continue;
        }
        let title = a
            .text()
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if title.is_empty() {
            continue;
        }
        let parsed = Url::parse(&url)?;
        let (dated, date_index) = date_path(&parsed);
        let older = matches!(a.value().attr("rel"), Some("next" | "prev"))
            || [
                "上月",
                "上一月",
                "下一页",
                "更早",
                "older",
                "previous month",
                "next page",
            ]
            .iter()
            .any(|label| title.to_lowercase().contains(label));
        if older || date_index {
            pages.push(url);
            continue;
        }
        // Pagination can live in nav; article candidates cannot.
        if a.ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .any(|e| matches!(e.value().name(), "nav" | "footer" | "aside" | "header"))
        {
            continue;
        }
        if parsed
            .path()
            .split('/')
            .any(|part| matches!(part, "tag" | "tags" | "category" | "categories" | "search"))
        {
            continue;
        }
        let structured_title = a
            .ancestors()
            .filter_map(scraper::ElementRef::wrap)
            .any(|e| {
                matches!(e.value().name(), "h2" | "h3")
                    || e.value().classes().any(|c| {
                        matches!(
                            c,
                            "post-title"
                                | "entry-title"
                                | "archive-item"
                                | "archive__item-title"
                                | "post-link"
                                | "archive-article-title"
                                | "archive-post-title"
                        )
                    })
            });
        if !(dated || samples.contains(&url) || structured_title) {
            continue;
        }
        if parsed.path().ends_with("/archives.html") {
            continue;
        }
        entries.push(Entry {
            article: NewArticle {
                entry_id: url.clone(),
                url: Some(url),
                title: Some(title),
                author: None,
                published: None,
                content: None,
            },
            fetch_body: true,
        });
    }
    pages.sort_by(|a, b| b.cmp(a));
    pages.dedup();
    if entries.is_empty() && pages.is_empty() {
        return Err(anyhow!("未发现历史条目"));
    }
    Ok(Page { entries, pages })
}
