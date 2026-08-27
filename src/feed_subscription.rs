//! Feed Subscription lifecycle.
//!
//! GUI and CLI cross this module's external seam for every durable Feed
//! Subscription change. Persistence ordering and the initial RSS Refresh
//! intent stay behind the interface recorded by ADR-0005.

use chrono::Utc;
use std::path::PathBuf;

use crate::config::Config;
use crate::db::Db;
use crate::model::Feed;
use crate::rss_refresh_workflow::{
    RefreshRunSnapshot, RefreshRunStatus, RefreshWorkflowStatus, RssRefreshWorkflow,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionErrorKind {
    Input,
    Maintenance,
    Storage,
    RefreshDispatch,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{user_message}")]
pub(crate) struct SubscriptionError {
    pub(crate) kind: SubscriptionErrorKind,
    pub(crate) user_message: String,
    pub(crate) technical_detail: String,
}

impl SubscriptionError {
    fn storage(error: impl std::fmt::Display) -> Self {
        let detail = error.to_string();
        let kind = if detail.contains("MAINTENANCE_IN_PROGRESS")
            || detail.contains("STALE_LIBRARY_EPOCH")
        {
            SubscriptionErrorKind::Maintenance
        } else {
            SubscriptionErrorKind::Storage
        };
        Self {
            kind,
            user_message: match kind {
                SubscriptionErrorKind::Maintenance => "资料维护期间不能修改订阅".into(),
                _ => "无法保存订阅变更".into(),
            },
            technical_detail: detail,
        }
    }

    fn input(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            kind: SubscriptionErrorKind::Input,
            user_message: message.clone(),
            technical_detail: message,
        }
    }

    fn refresh(error: impl std::fmt::Display) -> Self {
        Self {
            kind: SubscriptionErrorKind::RefreshDispatch,
            user_message: "订阅变更已保存，但无法启动刷新".into(),
            technical_detail: sanitize_detail(&error.to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InitialRefreshOutcome {
    Queued,
    Succeeded { new_articles: usize },
    Degraded { technical_detail: Option<String> },
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChangeDisposition {
    Created,
    Existing,
    Changed,
    Unchanged,
    Deleted,
    NotFound,
}

#[derive(Debug, Clone)]
pub(crate) struct SubscriptionOutcome {
    pub(crate) disposition: ChangeDisposition,
    pub(crate) subscription: Option<Feed>,
    pub(crate) refresh: Option<InitialRefreshOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SubscriptionChange {
    Add { url: String },
    Enable { id: i64 },
    Disable { id: i64 },
    SetInterval { id: i64, seconds: i64 },
    Delete { target: String },
}

trait InitialRefreshAdapter {
    fn start(
        &self,
        database: &std::path::Path,
        feed_id: i64,
    ) -> Result<InitialRefreshOutcome, SubscriptionError>;
}

enum RefreshAdapter<'a> {
    Session(&'a RssRefreshWorkflow),
    OneShot(&'a Config),
    #[cfg(test)]
    Test(&'a dyn InitialRefreshAdapter),
}

impl InitialRefreshAdapter for RefreshAdapter<'_> {
    fn start(
        &self,
        database: &std::path::Path,
        feed_id: i64,
    ) -> Result<InitialRefreshOutcome, SubscriptionError> {
        match self {
            Self::Session(workflow) => {
                if workflow.snapshot().status == RefreshWorkflowStatus::PausedForMaintenance {
                    return Ok(InitialRefreshOutcome::Deferred);
                }
                workflow
                    .request_feed(feed_id)
                    .map(|()| InitialRefreshOutcome::Queued)
                    .map_err(SubscriptionError::refresh)
            }
            Self::OneShot(config) => {
                let run = RssRefreshWorkflow::run_once_feed(database, config, feed_id);
                match run {
                    Ok(run) => one_shot_outcome(&run),
                    Err(error) => Err(SubscriptionError::refresh(error)),
                }
            }
            #[cfg(test)]
            Self::Test(adapter) => adapter.start(database, feed_id),
        }
    }
}

pub(crate) struct FeedSubscriptions<'a> {
    database: PathBuf,
    refresh: RefreshAdapter<'a>,
}

impl<'a> FeedSubscriptions<'a> {
    pub(crate) fn session(database: PathBuf, refresh: &'a RssRefreshWorkflow) -> Self {
        Self {
            database,
            refresh: RefreshAdapter::Session(refresh),
        }
    }

    pub(crate) fn one_shot(database: PathBuf, config: &'a Config) -> Self {
        Self {
            database,
            refresh: RefreshAdapter::OneShot(config),
        }
    }

    #[cfg(test)]
    fn with_refresh(database: PathBuf, refresh: &'a dyn InitialRefreshAdapter) -> Self {
        Self {
            database,
            refresh: RefreshAdapter::Test(refresh),
        }
    }

    pub(crate) fn apply(
        &self,
        change: SubscriptionChange,
    ) -> Result<SubscriptionOutcome, SubscriptionError> {
        match change {
            SubscriptionChange::Add { url } => self.add(&url),
            SubscriptionChange::Enable { id } => self.set_disabled(id, false),
            SubscriptionChange::Disable { id } => self.set_disabled(id, true),
            SubscriptionChange::SetInterval { id, seconds } => self.set_interval(id, seconds),
            SubscriptionChange::Delete { target } => self.delete(&target),
        }
    }

    pub(crate) fn list(&self) -> Result<Vec<(Feed, i64)>, SubscriptionError> {
        Db::open(&self.database)
            .and_then(|db| db.feeds_with_unread())
            .map_err(SubscriptionError::storage)
    }

    pub(crate) fn get(&self, id: i64) -> Result<Option<Feed>, SubscriptionError> {
        Db::open(&self.database)
            .and_then(|db| db.find_feed(id))
            .map_err(SubscriptionError::storage)
    }

    fn add(&self, input: &str) -> Result<SubscriptionOutcome, SubscriptionError> {
        let raw = input.trim();
        let url = normalize_feed_url(raw)?;
        let db = Db::open(&self.database).map_err(SubscriptionError::storage)?;
        let now = Utc::now().timestamp();
        let legacy = db
            .find_feed_by_url(raw)
            .map_err(SubscriptionError::storage)?;
        let (id, created) = if let Some(existing) = legacy {
            (existing.id, false)
        } else {
            db.add_feed_with_disposition(&url, now)
                .map_err(SubscriptionError::storage)?
        };
        if !created {
            db.request_subscription_refresh(id, now)
                .map_err(SubscriptionError::storage)?;
        }
        let subscription = db.get_feed(id).map_err(SubscriptionError::storage)?;
        drop(db);
        let refresh = self.refresh.start(&self.database, id)?;
        let subscription = self.get(id)?.unwrap_or(subscription);
        Ok(SubscriptionOutcome {
            disposition: if created {
                ChangeDisposition::Created
            } else {
                ChangeDisposition::Existing
            },
            subscription: Some(subscription),
            refresh: Some(refresh),
        })
    }

    fn set_disabled(
        &self,
        id: i64,
        disabled: bool,
    ) -> Result<SubscriptionOutcome, SubscriptionError> {
        let db = Db::open(&self.database).map_err(SubscriptionError::storage)?;
        let Some(before) = db.find_feed(id).map_err(SubscriptionError::storage)? else {
            return Ok(not_found());
        };
        if before.disabled == disabled {
            return Ok(SubscriptionOutcome {
                disposition: ChangeDisposition::Unchanged,
                subscription: Some(before),
                refresh: None,
            });
        }
        db.set_disabled(id, disabled, Utc::now().timestamp())
            .map_err(SubscriptionError::storage)?;
        let subscription = db.get_feed(id).map_err(SubscriptionError::storage)?;
        drop(db);
        let refresh = if disabled {
            None
        } else {
            Some(self.refresh.start(&self.database, id)?)
        };
        Ok(SubscriptionOutcome {
            disposition: ChangeDisposition::Changed,
            subscription: Some(subscription),
            refresh,
        })
    }

    fn set_interval(
        &self,
        id: i64,
        seconds: i64,
    ) -> Result<SubscriptionOutcome, SubscriptionError> {
        if seconds <= 0 {
            return Err(SubscriptionError::input("刷新间隔必须大于 0 秒"));
        }
        let db = Db::open(&self.database).map_err(SubscriptionError::storage)?;
        let Some(before) = db.find_feed(id).map_err(SubscriptionError::storage)? else {
            return Ok(not_found());
        };
        if before.interval_secs == Some(seconds) {
            return Ok(SubscriptionOutcome {
                disposition: ChangeDisposition::Unchanged,
                subscription: Some(before),
                refresh: None,
            });
        }
        db.set_subscription_interval(id, seconds, Utc::now().timestamp())
            .map_err(SubscriptionError::storage)?;
        let subscription = db.get_feed(id).map_err(SubscriptionError::storage)?;
        Ok(SubscriptionOutcome {
            disposition: ChangeDisposition::Changed,
            subscription: Some(subscription),
            refresh: None,
        })
    }

    fn delete(&self, target: &str) -> Result<SubscriptionOutcome, SubscriptionError> {
        let db = Db::open(&self.database).map_err(SubscriptionError::storage)?;
        let subscription = if let Ok(id) = target.trim().parse::<i64>() {
            db.find_feed(id).map_err(SubscriptionError::storage)?
        } else {
            let raw = target.trim();
            let url = normalize_feed_url(raw)?;
            db.find_feed_by_url(raw)
                .map_err(SubscriptionError::storage)?
                .or(db
                    .find_feed_by_url(&url)
                    .map_err(SubscriptionError::storage)?)
        };
        let Some(subscription) = subscription else {
            return Ok(not_found());
        };
        let removed = db
            .remove_feed(&subscription.id.to_string())
            .map_err(SubscriptionError::storage)?;
        if removed == 0 {
            return Ok(not_found());
        }
        Ok(SubscriptionOutcome {
            disposition: ChangeDisposition::Deleted,
            subscription: Some(subscription),
            refresh: None,
        })
    }
}

fn one_shot_outcome(run: &RefreshRunSnapshot) -> Result<InitialRefreshOutcome, SubscriptionError> {
    match run.status {
        RefreshRunStatus::Succeeded => Ok(InitialRefreshOutcome::Succeeded {
            new_articles: run.new_article_count,
        }),
        RefreshRunStatus::Degraded => Ok(InitialRefreshOutcome::Degraded {
            technical_detail: run
                .feeds
                .iter()
                .find_map(|feed| feed.failure.as_ref())
                .map(|failure| sanitize_detail(&failure.technical_detail)),
        }),
        RefreshRunStatus::Failed | RefreshRunStatus::Interrupted => {
            let detail = run
                .module_failure
                .as_ref()
                .map(|failure| failure.technical_detail.as_str())
                .unwrap_or("RSS refresh did not complete");
            Err(SubscriptionError::refresh(detail))
        }
        RefreshRunStatus::Fetching | RefreshRunStatus::Committing => Err(
            SubscriptionError::refresh("RSS refresh returned a non-terminal result"),
        ),
    }
}

fn sanitize_detail(detail: &str) -> String {
    detail
        .replace(['\r', '\n'], " ")
        .replace("Bearer ", "Bearer [redacted]")
        .replace("sk-", "[redacted-key-prefix]")
        .chars()
        .take(1_024)
        .collect()
}

fn not_found() -> SubscriptionOutcome {
    SubscriptionOutcome {
        disposition: ChangeDisposition::NotFound,
        subscription: None,
        refresh: None,
    }
}

fn normalize_feed_url(input: &str) -> Result<String, SubscriptionError> {
    let parsed = reqwest::Url::parse(input.trim())
        .map_err(|_| SubscriptionError::input("请输入完整的 HTTP(S) 订阅地址"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(SubscriptionError::input("订阅地址只支持 HTTP 或 HTTPS"));
    }
    Ok(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeRefresh {
        requested: Mutex<Vec<i64>>,
    }

    impl InitialRefreshAdapter for FakeRefresh {
        fn start(
            &self,
            _database: &std::path::Path,
            feed_id: i64,
        ) -> Result<InitialRefreshOutcome, SubscriptionError> {
            self.requested.lock().unwrap().push(feed_id);
            Ok(InitialRefreshOutcome::Queued)
        }
    }

    fn test_database(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "shiyue-feed-subscription-{name}-{}-{}.db",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    #[test]
    fn adding_the_same_normalized_url_is_idempotent_and_requests_refresh_again() {
        let path = test_database("idempotent-add");
        let refresh = FakeRefresh::default();
        let subscriptions = FeedSubscriptions::with_refresh(path.clone(), &refresh);

        let created = subscriptions
            .apply(SubscriptionChange::Add {
                url: "  https://example.com  ".into(),
            })
            .unwrap();
        assert_eq!(created.disposition, ChangeDisposition::Created);
        assert_eq!(created.refresh, Some(InitialRefreshOutcome::Queued));
        let created_feed = created.subscription.unwrap();
        assert_eq!(created_feed.url, "https://example.com/");

        let existing = subscriptions
            .apply(SubscriptionChange::Add {
                url: "https://example.com/".into(),
            })
            .unwrap();
        assert_eq!(existing.disposition, ChangeDisposition::Existing);
        assert_eq!(existing.subscription.unwrap().id, created_feed.id);
        assert_eq!(
            *refresh.requested.lock().unwrap(),
            vec![created_feed.id, created_feed.id]
        );

        drop(subscriptions);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn settings_changes_are_typed_and_only_enable_requests_refresh() {
        let path = test_database("settings");
        let refresh = FakeRefresh::default();
        let subscriptions = FeedSubscriptions::with_refresh(path.clone(), &refresh);
        let feed_id = subscriptions
            .apply(SubscriptionChange::Add {
                url: "https://example.com/feed.xml".into(),
            })
            .unwrap()
            .subscription
            .unwrap()
            .id;

        assert_eq!(
            subscriptions
                .apply(SubscriptionChange::Disable { id: feed_id })
                .unwrap()
                .disposition,
            ChangeDisposition::Changed
        );
        assert_eq!(
            subscriptions
                .apply(SubscriptionChange::Disable { id: feed_id })
                .unwrap()
                .disposition,
            ChangeDisposition::Unchanged
        );
        let enabled = subscriptions
            .apply(SubscriptionChange::Enable { id: feed_id })
            .unwrap();
        assert_eq!(enabled.disposition, ChangeDisposition::Changed);
        assert_eq!(enabled.refresh, Some(InitialRefreshOutcome::Queued));
        assert_eq!(
            subscriptions
                .apply(SubscriptionChange::SetInterval {
                    id: feed_id,
                    seconds: 3_600,
                })
                .unwrap()
                .disposition,
            ChangeDisposition::Changed
        );
        assert_eq!(
            subscriptions
                .apply(SubscriptionChange::SetInterval {
                    id: feed_id,
                    seconds: 3_600,
                })
                .unwrap()
                .disposition,
            ChangeDisposition::Unchanged
        );

        let rows = subscriptions.list().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0.interval_secs, Some(3_600));
        assert_eq!(*refresh.requested.lock().unwrap(), vec![feed_id, feed_id]);

        drop(subscriptions);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn deleting_a_subscription_is_typed_and_idempotent() {
        let path = test_database("delete");
        let refresh = FakeRefresh::default();
        let subscriptions = FeedSubscriptions::with_refresh(path.clone(), &refresh);
        let feed_id = subscriptions
            .apply(SubscriptionChange::Add {
                url: "https://example.com/feed.xml".into(),
            })
            .unwrap()
            .subscription
            .unwrap()
            .id;

        let deleted = subscriptions
            .apply(SubscriptionChange::Delete {
                target: feed_id.to_string(),
            })
            .unwrap();
        assert_eq!(deleted.disposition, ChangeDisposition::Deleted);
        assert_eq!(deleted.subscription.unwrap().id, feed_id);
        assert!(subscriptions.list().unwrap().is_empty());

        let missing = subscriptions
            .apply(SubscriptionChange::Delete {
                target: feed_id.to_string(),
            })
            .unwrap();
        assert_eq!(missing.disposition, ChangeDisposition::NotFound);

        drop(subscriptions);
        let _ = std::fs::remove_file(path);
    }
}
