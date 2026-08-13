//! 抓取 + 解析（ADR-5 reqwest / ADR-6 feed-rs）。

use anyhow::{Context, Result};
use std::time::Duration;

use crate::model::NewArticle;

pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(concat!("Shiyue/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()?)
}

/// 拉取一个源并解析成统一条目。返回 (源标题, 条目列表)。
pub async fn fetch(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Option<String>, Vec<NewArticle>)> {
    let bytes = client
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let feed = feed_rs::parser::parse(bytes.as_ref()).context("解析订阅源失败")?;
    let title = feed.title.map(|t| t.content);
    let articles = feed.entries.into_iter().map(entry_to_article).collect();
    Ok((title, articles))
}

/// feed-rs 的 Entry → 待入库 NewArticle。entry_id 优先 guid/id，回退链接、再回退标题（ADR-8）。
fn entry_to_article(e: feed_rs::model::Entry) -> NewArticle {
    let url = e
        .links
        .iter()
        .find(|l| l.rel.as_deref().is_none_or(|rel| rel == "alternate"))
        .or_else(|| e.links.first())
        .map(|l| l.href.clone());
    let entry_id = if !e.id.is_empty() {
        e.id.clone()
    } else if let Some(u) = &url {
        u.clone()
    } else {
        e.title
            .as_ref()
            .map(|t| t.content.clone())
            .unwrap_or_default()
    };
    let title = e.title.as_ref().map(|t| t.content.clone());
    let author = e.authors.first().map(|p| p.name.clone());
    let published = e.published.or(e.updated).map(|d| d.timestamp());
    let mut content = e
        .content
        .and_then(|c| c.body)
        .or_else(|| e.summary.map(|t| t.content));

    // MediaRSS and enclosure images often live outside content:encoded. Preserve them
    // as ordinary image tags so the reader can display the complete article gallery.
    let mut media_images = Vec::new();
    for media in &e.media {
        media_images.extend(media.thumbnails.iter().map(|t| t.image.uri.clone()));
        media_images.extend(media.content.iter().filter_map(|item| {
            let is_image = item
                .content_type
                .as_ref()
                .is_some_and(|kind| kind.to_string().starts_with("image/"));
            (is_image || item.content_type.is_none())
                .then(|| item.url.as_ref().map(ToString::to_string))
                .flatten()
        }));
    }
    media_images.extend(e.links.iter().filter_map(|link| {
        link.media_type
            .as_deref()
            .is_some_and(|kind| kind.starts_with("image/"))
            .then(|| link.href.clone())
    }));
    if !media_images.is_empty() {
        let html = content.get_or_insert_with(String::new);
        for image in media_images {
            if !html.contains(&image) {
                html.push_str(&format!(r#"<img src="{image}">"#));
            }
        }
    }
    NewArticle {
        entry_id,
        url,
        title,
        author,
        published,
        content,
    }
}
