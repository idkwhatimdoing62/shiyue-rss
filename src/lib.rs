mod backup;
mod cli;
mod config;
mod daemon;
mod db;
mod fetch;
mod gui;
mod image_store;
mod model;
mod notify;
mod text;
mod web_clip;

use anyhow::Result;
use chrono::Utc;
use clap::{CommandFactory, Parser};

use crate::cli::{Cli, Command};
use crate::config::Paths;
use crate::db::Db;

/// 启动不带控制台窗口的拾阅桌面界面。
pub fn run_gui() -> Result<()> {
    let paths = Paths::resolve()?;
    init_logging(&paths);
    let cfg = config::load(&paths)?;
    gui::run(paths, cfg)
}

/// 运行独立的命令行管理工具。
pub fn run_cli() -> Result<()> {
    let paths = Paths::resolve()?;
    init_logging(&paths);
    let cfg = config::load(&paths)?;
    let db = Db::open(&paths.db_file)?;

    match Cli::parse().command {
        Some(Command::Add { url }) => {
            let now = Utc::now().timestamp();
            let id = db.add_feed(&url, now)?;
            let client = fetch::client()?;
            let feed = db.get_feed(id)?;
            async_runtime()?.block_on(daemon::fetch_feeds(&db, &cfg, &client, vec![feed]))?;
            let feed = db.get_feed(id)?;
            match feed.last_error {
                Some(error) => println!("已添加 #{id}，但首次抓取失败: {error}"),
                None => println!("已添加 #{id}: {}", feed.title.unwrap_or(feed.url)),
            }
        }
        Some(Command::Rm { target }) => {
            let count = db.remove_feed(&target)?;
            println!(
                "{}",
                if count > 0 {
                    "已删除"
                } else {
                    "未找到该源"
                }
            );
        }
        Some(Command::List) => list_feeds(&db)?,
        Some(Command::SetInterval { id, interval }) => {
            let seconds = config::parse_duration(&interval)?;
            if db.set_interval(id, seconds)? > 0 {
                println!("#{id} 间隔已设为 {seconds}s");
            } else {
                println!("未找到 #{id}");
            }
        }
        Some(Command::Enable { id }) => {
            let now = Utc::now().timestamp();
            println!(
                "{}",
                if db.set_disabled(id, false, now)? > 0 {
                    "已启用"
                } else {
                    "未找到"
                }
            );
        }
        Some(Command::Disable { id }) => {
            let now = Utc::now().timestamp();
            println!(
                "{}",
                if db.set_disabled(id, true, now)? > 0 {
                    "已禁用"
                } else {
                    "未找到"
                }
            );
        }
        Some(Command::Update) => {
            let count = async_runtime()?.block_on(daemon::update_once(&db, &cfg))?;
            println!("新增 {count} 篇");
        }
        None => {
            Cli::command().print_help()?;
            println!();
        }
    }
    Ok(())
}

/// CLI 抓取命令需要 Tokio；GUI 必须留在未进入异步运行时的主线程。
fn async_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}

fn list_feeds(db: &Db) -> Result<()> {
    let feeds = db.feeds_with_unread()?;
    if feeds.is_empty() {
        println!("还没有订阅源，用 `shiyue-cli add <url>` 添加。");
        return Ok(());
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
    Ok(())
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
