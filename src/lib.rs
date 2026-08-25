mod article_library_lifecycle;
mod backup;
mod cli;
mod config;
mod db;
mod feed_subscription;
mod fetch;
mod gui;
mod gui_modal;
mod gui_state;
mod gui_theme;
mod image_store;
mod knowledge_workflow;
mod local_data_maintenance;
mod model;
mod notify;
mod resource_enrichment;
mod resource_library_lifecycle;
mod rss_refresh_workflow;
mod text;
mod web_clip;

use anyhow::Result;
use clap::{CommandFactory, Parser};

use crate::cli::{Cli, Command, ResourceCommand, ResourceResultType, ResourceScope};
use crate::config::Paths;
use crate::db::Db;
use crate::feed_subscription::{
    ChangeDisposition, FeedSubscriptions, InitialRefreshOutcome, SubscriptionChange,
};
use crate::rss_refresh_workflow::{RefreshRunSnapshot, RefreshRunStatus, RssRefreshWorkflow};

/// 启动不带控制台窗口的拾阅桌面界面。
pub fn run_gui() -> Result<()> {
    let paths = Paths::resolve()?;
    init_logging(&paths);
    let cfg = config::load(&paths)?;
    gui::run(paths, cfg)
}

/// 运行独立的命令行管理工具。
pub fn run_cli() -> Result<i32> {
    let paths = Paths::resolve()?;
    init_logging(&paths);
    let cfg = config::load(&paths)?;

    match Cli::parse().command {
        Some(Command::Resource { command }) => {
            return run_resource_cli(&paths, &cfg, command);
        }
        Some(Command::Add { url }) => {
            let outcome = FeedSubscriptions::one_shot(paths.db_file.clone(), &cfg)
                .apply(SubscriptionChange::Add { url })?;
            let feed = outcome
                .subscription
                .expect("add subscription returns the durable subscription");
            let id = feed.id;
            match &outcome.refresh {
                Some(InitialRefreshOutcome::Degraded { technical_detail }) => {
                    let error = technical_detail
                        .as_deref()
                        .or(feed.last_error.as_deref())
                        .unwrap_or("首次抓取失败");
                    println!("已添加 #{id}，但首次抓取失败: {error}");
                    eprintln!("刷新失败 #{id} {}: {error}", feed.url);
                }
                _ => println!("已添加 #{id}: {}", feed.title.unwrap_or(feed.url)),
            }
            return Ok(subscription_refresh_exit_code(outcome.refresh.as_ref()));
        }
        Some(Command::Rm { target }) => {
            let outcome = FeedSubscriptions::one_shot(paths.db_file.clone(), &cfg)
                .apply(SubscriptionChange::Delete { target })?;
            println!(
                "{}",
                if outcome.disposition == ChangeDisposition::Deleted {
                    "已删除"
                } else {
                    "未找到该源"
                }
            );
        }
        Some(Command::List) => {
            let feeds = FeedSubscriptions::one_shot(paths.db_file.clone(), &cfg).list()?;
            list_feeds(feeds);
        }
        Some(Command::SetInterval { id, interval }) => {
            let seconds = config::parse_duration(&interval)?;
            let outcome = FeedSubscriptions::one_shot(paths.db_file.clone(), &cfg)
                .apply(SubscriptionChange::SetInterval { id, seconds })?;
            if outcome.disposition != ChangeDisposition::NotFound {
                println!("#{id} 间隔已设为 {seconds}s");
            } else {
                println!("未找到 #{id}");
            }
        }
        Some(Command::Enable { id }) => {
            let outcome = FeedSubscriptions::one_shot(paths.db_file.clone(), &cfg)
                .apply(SubscriptionChange::Enable { id })?;
            println!(
                "{}",
                if outcome.disposition == ChangeDisposition::NotFound {
                    "未找到"
                } else {
                    "已启用"
                }
            );
            if outcome.disposition != ChangeDisposition::NotFound {
                if let Some(InitialRefreshOutcome::Degraded { technical_detail }) = &outcome.refresh
                {
                    eprintln!(
                        "刷新失败 #{id}: {}",
                        technical_detail.as_deref().unwrap_or("订阅刷新失败")
                    );
                }
                return Ok(subscription_refresh_exit_code(outcome.refresh.as_ref()));
            }
        }
        Some(Command::Disable { id }) => {
            let outcome = FeedSubscriptions::one_shot(paths.db_file.clone(), &cfg)
                .apply(SubscriptionChange::Disable { id })?;
            println!(
                "{}",
                if outcome.disposition == ChangeDisposition::NotFound {
                    "未找到"
                } else {
                    "已禁用"
                }
            );
        }
        Some(Command::Update) => {
            let run = RssRefreshWorkflow::run_once_all(&paths.db_file, &cfg)?;
            println!(
                "刷新 {} 个订阅，完成 {} 个，失败 {} 个，新增 {} 篇",
                run.target_count, run.completed_count, run.failed_feed_count, run.new_article_count
            );
            print_refresh_failures(&run);
            return Ok(refresh_exit_code(run.status));
        }
        None => {
            Cli::command().print_help()?;
            println!();
        }
    }
    Ok(0)
}

fn run_resource_cli(paths: &Paths, cfg: &config::Config, command: ResourceCommand) -> Result<i32> {
    use resource_library_lifecycle::{
        CreateResource, NoProcessingHandoff, ProjectionScope, ResourceCollection, ResourceKind,
        ResourceLibraryLifecycle, ResourceLifecycleChange, ResourcePrivacy, ResourceSource,
        SystemClock,
    };
    if let ResourceCommand::Retry {
        id,
        no_wait,
        timeout,
        ..
    } = command
    {
        return run_resource_retry_cli(paths, cfg, id, no_wait, &timeout);
    }
    let db = Db::open(&paths.db_file)?;
    let lifecycle = ResourceLibraryLifecycle::new(&db, &NoProcessingHandoff, &SystemClock);
    let result: Result<serde_json::Value> = (|| {
        Ok(match command {
            ResourceCommand::Add {
                url, note, private, ..
            } => {
                let outcome = lifecycle.apply(
                    ResourceLifecycleChange::Create(CreateResource {
                        url,
                        parent_resource_id: None,
                        linked_article_id: None,
                        kind: ResourceKind::Page,
                        title: None,
                        private_note: note,
                        privacy: if private {
                            ResourcePrivacy::Private
                        } else {
                            ResourcePrivacy::Public
                        },
                        source: ResourceSource::CliAgent,
                        manual_rating: None,
                    }),
                    ProjectionScope::collection(ResourceCollection::PendingReview),
                )?;
                let id = outcome.affected_resource_ids[0];
                let resource = lifecycle
                    .project(ProjectionScope::Resource(id))?
                    .detail
                    .expect("resource projection contains detail")
                    .resource;
                resource_library_lifecycle::resource_json(
                    &db,
                    &resource,
                    "url",
                    resource.url.clone(),
                    1.0,
                )?
            }
            ResourceCommand::Get { id, .. } => {
                let resource = lifecycle
                    .project(ProjectionScope::Resource(id))?
                    .detail
                    .expect("resource projection contains detail")
                    .resource;
                resource_library_lifecycle::resource_json(&db, &resource, "id", String::new(), 1.0)?
            }
            ResourceCommand::Recent { limit, .. } => serde_json::Value::Array(
                lifecycle
                    .project(ProjectionScope::Collection {
                        collection: ResourceCollection::Active,
                        after: None,
                        limit,
                    })?
                    .resources
                    .iter()
                    .map(|resource| {
                        resource_library_lifecycle::resource_json(
                            &db,
                            resource,
                            "recent",
                            String::new(),
                            1.0,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            ResourceCommand::Pending { .. } => serde_json::Value::Array(
                lifecycle
                    .project(ProjectionScope::collection(
                        ResourceCollection::PendingReview,
                    ))?
                    .resources
                    .iter()
                    .map(|resource| {
                        resource_library_lifecycle::resource_json(
                            &db,
                            resource,
                            "curation_state",
                            String::new(),
                            1.0,
                        )
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
            ResourceCommand::Retry { .. } => unreachable!("retry handled before generic commands"),
            ResourceCommand::Search {
                query,
                r#type,
                scope,
                limit,
                ..
            } => serde_json::Value::Array(resource_library_lifecycle::legacy_search_json(
                &db,
                &query,
                !matches!(r#type, ResourceResultType::Article),
                !matches!(r#type, ResourceResultType::Resource),
                matches!(scope, ResourceScope::All),
                limit,
            )?),
        })
    })();
    match result {
        Ok(data) => {
            println!(
                "{}",
                serde_json::json!({"schema_version":1,"ok":true,"data":data,"warnings":[]})
            );
            Ok(0)
        }
        Err(error) => {
            let not_found = error
                .downcast_ref::<resource_library_lifecycle::LifecycleFailure>()
                .is_some_and(|failure| {
                    failure.kind == resource_library_lifecycle::FailureKind::NotFound
                });
            println!(
                "{}",
                serde_json::json!({"schema_version":1,"ok":false,"error":{"code":if not_found{"RESOURCE_NOT_FOUND"}else{"RESOURCE_ERROR"},"message":error.to_string(),"retryable":false}})
            );
            Ok(if not_found { 3 } else { 1 })
        }
    }
}

fn run_resource_retry_cli(
    paths: &Paths,
    cfg: &config::Config,
    id: i64,
    no_wait: bool,
    timeout: &str,
) -> Result<i32> {
    use crate::knowledge_workflow::{
        ErrorKind, KnowledgeEngine, RequestDisposition, TaskKey, TaskKind, TaskStatus,
    };
    use crate::resource_library_lifecycle::{
        NoProcessingHandoff, ProjectionScope, ResourceLibraryLifecycle, SystemClock,
    };

    let resource_json = match (|| -> Result<serde_json::Value> {
        let db = Db::open(&paths.db_file)?;
        let lifecycle = ResourceLibraryLifecycle::new(&db, &NoProcessingHandoff, &SystemClock);
        let resource = lifecycle
            .project(ProjectionScope::Resource(id))?
            .detail
            .expect("resource projection contains detail")
            .resource;
        resource_library_lifecycle::resource_json(&db, &resource, "id", String::new(), 1.0)
    })() {
        Ok(resource) => resource,
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 1,
                    "ok": false,
                    "error": {"code":"RESOURCE_NOT_FOUND","message":error.to_string(),"retryable":false}
                })
            );
            return Ok(3);
        }
    };
    let engine = if no_wait {
        KnowledgeEngine::start_client(paths.db_file.clone(), cfg.resource_enrichment.clone())?
    } else {
        KnowledgeEngine::start(paths.db_file.clone(), cfg.resource_enrichment.clone())?
    };
    let key = TaskKey::new(TaskKind::ResourceCompletion, id);
    let receipt = match engine.request(key) {
        Ok(receipt) => receipt,
        Err(error) => {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version":1,"ok":false,
                    "error":{"code":"WORKFLOW_REQUEST_FAILED","message":error.to_string(),"retryable":false}
                })
            );
            return Ok(1);
        }
    };
    let disposition = match receipt.disposition {
        RequestDisposition::Created => "created",
        RequestDisposition::Existing => "existing",
        RequestDisposition::Retried => "retried",
    };
    if no_wait {
        println!(
            "{}",
            serde_json::json!({
                "schema_version":1,"ok":true,
                "data":{"resource":resource_json,"queued":true,"disposition":disposition},
                "warnings":[]
            })
        );
        return Ok(0);
    }

    let timeout = std::time::Duration::from_secs(config::parse_duration(timeout)? as u64);
    let snapshot = match engine.wait_terminal(key, timeout) {
        Ok(snapshot) => snapshot,
        Err(error) if error.to_string().contains("WORKFLOW_WAIT_TIMEOUT") => {
            println!(
                "{}",
                serde_json::json!({
                    "schema_version":1,"ok":false,
                    "error":{"code":"WORKFLOW_WAIT_TIMEOUT","message":"等待后台处理超时；任务仍会继续执行","retryable":true,"technical_detail":error.to_string()}
                })
            );
            return Ok(5);
        }
        Err(error) => return Err(error),
    };
    if snapshot.status == TaskStatus::Succeeded {
        let db = Db::open(&paths.db_file)?;
        let lifecycle = ResourceLibraryLifecycle::new(&db, &NoProcessingHandoff, &SystemClock);
        let resource = lifecycle
            .project(ProjectionScope::Resource(id))?
            .detail
            .expect("resource projection contains detail")
            .resource;
        println!(
            "{}",
            serde_json::json!({
                "schema_version":1,"ok":true,
                "data":{
                    "resource":resource_library_lifecycle::resource_json(&db,&resource,"id",String::new(),1.0)?,
                    "task":{"status":"succeeded","attempt":snapshot.attempt_number,"disposition":disposition}
                },
                "warnings":[]
            })
        );
        return Ok(0);
    }

    let kind = snapshot.error_kind.unwrap_or(ErrorKind::Storage);
    let retryable = kind == ErrorKind::Transient;
    println!(
        "{}",
        serde_json::json!({
            "schema_version":1,"ok":false,
            "error":{
                "code":kind.code(),
                "message":snapshot.user_message.unwrap_or_else(|| "知识处理失败".into()),
                "retryable":retryable,
                "technical_detail":snapshot.technical_detail
            }
        })
    );
    Ok(match kind {
        ErrorKind::Input => 3,
        ErrorKind::Authentication | ErrorKind::Security => 4,
        ErrorKind::Transient => 5,
        ErrorKind::ProviderOutput | ErrorKind::Storage | ErrorKind::Interrupted => 1,
    })
}

fn refresh_exit_code(status: RefreshRunStatus) -> i32 {
    match status {
        RefreshRunStatus::Succeeded => 0,
        RefreshRunStatus::Degraded => 2,
        RefreshRunStatus::Failed | RefreshRunStatus::Interrupted => 1,
        RefreshRunStatus::Fetching | RefreshRunStatus::Committing => 1,
    }
}

fn subscription_refresh_exit_code(refresh: Option<&InitialRefreshOutcome>) -> i32 {
    match refresh {
        Some(InitialRefreshOutcome::Degraded { .. }) => 2,
        Some(InitialRefreshOutcome::Queued)
        | Some(InitialRefreshOutcome::Succeeded { .. })
        | Some(InitialRefreshOutcome::Deferred)
        | None => 0,
    }
}

fn print_refresh_failures(run: &RefreshRunSnapshot) {
    for feed in run.feeds.iter().filter(|feed| feed.failure.is_some()) {
        let failure = feed.failure.as_ref().expect("filtered failure exists");
        let label = feed.title.as_deref().unwrap_or(&feed.url);
        eprintln!(
            "刷新失败 #{} {} [{:?}]: {}",
            feed.feed_id, label, failure.kind, failure.technical_detail
        );
    }
    if let Some(failure) = &run.module_failure {
        eprintln!(
            "刷新模块失败 [{:?}]: {}",
            failure.kind, failure.technical_detail
        );
    }
}

fn list_feeds(feeds: Vec<(crate::model::Feed, i64)>) {
    if feeds.is_empty() {
        println!("还没有订阅源，用 `shiyue-cli add <url>` 添加。");
        return;
    }
    for (feed, unread) in feeds {
        let status = if feed.disabled {
            "[已禁用]"
        } else if feed.fail_count > 0 {
            "[⚠]"
        } else {
            ""
        };
        let title = feed.title.unwrap_or_else(|| feed.url.clone());
        println!("#{:<3} 未读 {:<4} {status} {title}", feed.id, unread);
    }
}

/// 后台调度与命令行的日志都追加到兼容路径 rrss.log。
fn init_logging(paths: &Paths) {
    let path = paths.log_file.clone();
    let make = move || {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap_or_else(|_| panic!("无法打开日志文件"))
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(make)
        .with_ansi(false)
        .try_init();
}
