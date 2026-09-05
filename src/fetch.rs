//! 抓取 + 解析（ADR-5 reqwest / ADR-6 feed-rs）。

use anyhow::{Context, Result, bail};
use std::time::Duration;

use crate::model::NewArticle;

const MAX_ATTEMPTS: usize = 3;
const MAX_FEED_BYTES: usize = 16 * 1024 * 1024;
const MAX_FEED_ENTRIES: usize = 2_000;
const MAX_ARTICLE_CONTENT_BYTES: usize = 2 * 1024 * 1024;
const RETRY_DELAYS: [Duration; MAX_ATTEMPTS - 1] =
    [Duration::from_millis(250), Duration::from_secs(1)];

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
    let mut attempt = 0;
    loop {
        match fetch_once(client, url).await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < RETRY_DELAYS.len() && is_retryable_request_error(&error) => {
                tokio::time::sleep(RETRY_DELAYS[attempt]).await;
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn fetch_once(
    client: &reqwest::Client,
    url: &str,
) -> Result<(Option<String>, Vec<NewArticle>)> {
    let response = client.get(url).send().await?.error_for_status()?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_FEED_BYTES as u64)
    {
        bail!(
            "订阅源响应超过大小限制（最多 {} MB）",
            MAX_FEED_BYTES / (1024 * 1024)
        );
    }
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > MAX_FEED_BYTES.saturating_sub(bytes.len()) {
            bail!(
                "订阅源响应超过大小限制（最多 {} MB）",
                MAX_FEED_BYTES / (1024 * 1024)
            );
        }
        bytes.extend_from_slice(&chunk);
    }
    let feed = feed_rs::parser::parse(std::io::Cursor::new(bytes)).context("解析订阅源失败")?;
    let title = feed.title.map(|t| t.content);
    let articles = feed
        .entries
        .into_iter()
        .take(MAX_FEED_ENTRIES)
        .map(entry_to_article)
        .collect();
    Ok((title, articles))
}

fn is_retryable_request_error(error: &anyhow::Error) -> bool {
    let Some(request) = error.downcast_ref::<reqwest::Error>() else {
        return false;
    };
    request.is_timeout()
        || request.is_connect()
        || request.is_request()
        || request.status().is_some_and(|status| {
            status == reqwest::StatusCode::REQUEST_TIMEOUT
                || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || status.is_server_error()
        })
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
    media_images.extend(
        e.links
            .iter()
            .filter(|&link| {
                link.media_type
                    .as_deref()
                    .is_some_and(|kind| kind.starts_with("image/"))
            })
            .map(|link| link.href.clone()),
    );
    if !media_images.is_empty() {
        let html = content.get_or_insert_with(String::new);
        for image in media_images {
            if !html.contains(&image) {
                html.push_str(&format!(r#"<img src="{image}">"#));
            }
        }
    }
    if let Some(value) = &mut content {
        truncate_utf8(value, MAX_ARTICLE_CONTENT_BYTES);
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

fn truncate_utf8(value: &mut String, max_bytes: usize) {
    if value.len() <= max_bytes {
        return;
    }
    let end = value
        .char_indices()
        .take_while(|(index, character)| *index + character.len_utf8() <= max_bytes)
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    value.truncate(end);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::thread;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retries_transient_request_failures_with_a_bounded_attempt_count() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            for attempt in 0..MAX_ATTEMPTS {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request);
                if attempt + 1 < MAX_ATTEMPTS {
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                let body = br#"<?xml version="1.0"?><rss version="2.0"><channel><title>Retry Feed</title></channel></rss>"#;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });

        let client = client().unwrap();
        let (title, articles) = fetch(&client, &format!("http://{address}/feed"))
            .await
            .unwrap();
        assert_eq!(title.as_deref(), Some("Retry Feed"));
        assert!(articles.is_empty());
        server.join().unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_feed_responses_larger_than_the_memory_budget() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request);
            writeln!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                MAX_FEED_BYTES + 1
            )
            .unwrap();
        });

        let error = fetch(&client().unwrap(), &format!("http://{address}/feed"))
            .await
            .expect_err("oversized feed must be rejected");
        assert!(error.to_string().contains("超过大小限制"));
        server.join().unwrap();
    }

    #[test]
    fn content_limit_keeps_utf8_boundaries() {
        let mut value = "😀abc".to_owned();
        truncate_utf8(&mut value, 4);
        assert_eq!(value, "😀");
        truncate_utf8(&mut value, 3);
        assert!(value.is_empty());
    }
}
