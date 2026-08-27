mod article_document_presentation;
mod article_library_lifecycle;
mod backup;
mod cli;
mod config;
mod db;
mod desktop_library_projection;
mod desktop_runtime;
mod excerpt_thought_lifecycle;
mod feed_subscription;
mod fetch;
mod gui;
mod gui_icons;
mod gui_modal;
mod gui_state;
mod gui_theme;
mod image_store;
mod knowledge_workflow;
mod library_projection_revision;
mod library_search;
mod local_data_maintenance;
mod model;
mod resource_enrichment;
mod resource_library_lifecycle;
mod rss_refresh_workflow;
mod schema_evolution;
mod web_clip;
mod web_clipping_lifecycle;

use anyhow::Result;
use clap::{CommandFactory, Parser};

use crate::cli::{Cli, Command, ResourceCommand, ResourceResultType, ResourceScope};
use crate::db::Db;
use crate::desktop_runtime::Paths;
use crate::feed_subscription::{
    ChangeDisposition, FeedSubscriptions, InitialRefreshOutcome, SubscriptionChange,
};
use crate::rss_refresh_workflow::{RefreshRunSnapshot, RefreshRunStatus, RssRefreshWorkflow};

/// 启动不带控制台窗口的拾阅桌面界面。
pub fn run_gui() -> Result<()> {
    desktop_runtime::launch()
}

/// 运行独立的命令行管理工具。
pub fn run_cli() -> Result<i32> {
    let environment = desktop_runtime::command_environment()?;
    let paths = environment.paths;
    let cfg = environment.settings;

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
    if let ResourceCommand::Search {
        query,
        r#type,
        scope,
        limit,
        agent,
        ..
    } = command
    {
        return run_library_search_cli(paths, query, r#type, scope, limit, agent);
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
            ResourceCommand::Search { .. } => unreachable!("search handled by Library Search"),
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

fn run_library_search_cli(
    paths: &Paths,
    query: String,
    result_type: ResourceResultType,
    scope: ResourceScope,
    limit: usize,
    agent: bool,
) -> Result<i32> {
    use library_search::{
        LibrarySearch, PrimaryIdentity, ResultType, SearchOrigin, SearchRequest, SearchScope,
        SearchWarning,
    };

    let db = match Db::open(&paths.db_file) {
        Ok(db) => db,
        Err(error) => {
            let technical_detail = error.to_string();
            let maintenance = technical_detail.contains("migration")
                || technical_detail.contains("schema")
                || technical_detail.contains("user_version");
            println!(
                "{}",
                serde_json::json!({
                    "schema_version": 2,
                    "ok": false,
                    "error": {
                        "code": if maintenance { "LIBRARY_SEARCH_MAINTENANCE" } else { "LIBRARY_SEARCH_STORAGE" },
                        "message": if maintenance { "资料库正在维护或需要迁移" } else { "无法打开本地资料库" },
                        "technical_detail": technical_detail,
                        "retryable": true,
                    }
                })
            );
            return Ok(1);
        }
    };
    let request = SearchRequest {
        query,
        scope: match scope {
            ResourceScope::Curated => SearchScope::Curated,
            ResourceScope::All => SearchScope::AllArticles,
            ResourceScope::Archive => SearchScope::Archive,
        },
        result_type: match result_type {
            ResourceResultType::All => ResultType::All,
            ResourceResultType::Resource => ResultType::Resource,
            ResourceResultType::Article => ResultType::Article,
        },
        origin: if agent {
            SearchOrigin::Agent
        } else {
            SearchOrigin::Human
        },
        limit,
    };
    match LibrarySearch::new(&db).search(request) {
        Ok(outcome) => {
            let data = outcome
                .results
                .into_iter()
                .map(|result| {
                    let (result_type, id) = match result.primary {
                        PrimaryIdentity::Resource(id) => ("resource", id),
                        PrimaryIdentity::Article(id) => ("article", id),
                    };
                    serde_json::json!({
                        "primary_identity":{"type":result_type,"id":id.to_string()},
                        "title":result.title,
                        "url":result.url,
                        "privacy":result.privacy,
                        "health":result.health,
                        "archived":result.archived,
                        "updated_at":result.updated_at,
                        "evidence":result.evidence.into_iter().map(|evidence| serde_json::json!({
                            "kind":evidence.kind.as_str(),
                            "source_id":evidence.source_id.to_string(),
                            "article_id":evidence.article_id.map(|id| id.to_string()),
                            "field":evidence.field.as_str(),
                            "text":evidence.text,
                        })).collect::<Vec<_>>(),
                        "score_factors":result.factors.into_iter().map(|factor| factor.as_str()).collect::<Vec<_>>(),
                        "article_targets":result.article_targets.into_iter().map(|target| serde_json::json!({
                            "article_id":target.article_id.to_string(),
                            "feed_id":target.feed_id.to_string(),
                            "selection_id":target.selection_id.map(|id| id.to_string()),
                            "archived":target.archived,
                            "web_clipping":target.web_clipping,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>();
            let warnings = outcome
                .warnings
                .into_iter()
                .map(|warning| match warning {
                    SearchWarning::HistoryNotRecorded { technical_detail } => serde_json::json!({
                        "code":"SEARCH_HISTORY_NOT_RECORDED",
                        "message":"搜索结果有效，但历史记录没有保存",
                        "technical_detail":technical_detail,
                    }),
                })
                .collect::<Vec<_>>();
            println!(
                "{}",
                serde_json::json!({"schema_version":2,"ok":true,"query":outcome.query,"data":data,"warnings":warnings})
            );
            Ok(0)
        }
        Err(failure) => {
            let code = match failure.kind {
                library_search::FailureKind::Input => "LIBRARY_SEARCH_INPUT",
                library_search::FailureKind::Maintenance => "LIBRARY_SEARCH_MAINTENANCE",
                library_search::FailureKind::Storage => "LIBRARY_SEARCH_STORAGE",
                library_search::FailureKind::Index => "LIBRARY_SEARCH_INDEX",
            };
            println!(
                "{}",
                serde_json::json!({"schema_version":2,"ok":false,"error":{"code":code,"message":failure.user_message,"technical_detail":failure.technical_detail,"retryable":matches!(failure.kind,library_search::FailureKind::Maintenance | library_search::FailureKind::Storage)}})
            );
            Ok(if failure.kind == library_search::FailureKind::Input {
                2
            } else {
                1
            })
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
