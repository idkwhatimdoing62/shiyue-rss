//! 抓取轮次（ADR-4/14）。`update` 与 GUI 后台调度线程共用 `fetch_feeds`。
//! 调度*循环*本身现在住在 GUI 后台线程里（见 gui.rs），这里只留可复用的抓取原语。

use anyhow::Result;
use chrono::Utc;
use tokio::task::JoinSet;

use crate::config::Config;
use crate::db::Db;
use crate::fetch;

/// 并发抓一批源、落库。返回 (有新文章的源数, 新文章总数)。
pub async fn fetch_feeds(
    db: &Db,
    cfg: &Config,
    client: &reqwest::Client,
    feeds: Vec<crate::model::Feed>,
) -> Result<(usize, usize)> {
    let now = Utc::now().timestamp();
    let mut set = JoinSet::new();
    for feed in feeds {
        let client = client.clone();
        set.spawn(async move {
            let res = fetch::fetch(&client, &feed.url).await;
            (feed, res)
        });
    }

    let (mut feeds_with_new, mut total_new) = (0usize, 0usize);
    while let Some(joined) = set.join_next().await {
        let (feed, res) = joined?; // JoinError（任务 panic）向上抛
        match res {
            Ok((title, arts)) => {
                let n = db.record_success(&feed, now, cfg, title, &arts)?;
                if n > 0 {
                    feeds_with_new += 1;
                    total_new += n;
                }
            }
            Err(e) => {
                db.record_failure(&feed, now, cfg, &e.to_string())?;
                tracing::warn!("抓取失败 {}: {e}", feed.url);
            }
        }
    }
    Ok((feeds_with_new, total_new))
}

/// 前台抓一轮所有启用的源（`rrss update`）。返回新文章数。
pub async fn update_once(db: &Db, cfg: &Config) -> Result<usize> {
    let client = fetch::client()?;
    let feeds = db.enabled_feeds()?;
    let (_, total_new) = fetch_feeds(db, cfg, &client, feeds).await?;
    Ok(total_new)
}
