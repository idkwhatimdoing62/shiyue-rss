mod cli;
mod config;
mod daemon;
mod db;
mod fetch;
mod gui;
mod model;
mod notify;
mod text;

use anyhow::Result;
use chrono::Utc;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::Paths;
use crate::db::Db;

fn main() -> Result<()> {
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
                Some(e) => println!("已添加 #{id}，但首次抓取失败: {e}"),
                None => println!("已添加 #{id}: {}", feed.title.unwrap_or(feed.url)),
            }
        }
        Some(Command::Rm { target }) => {
            let n = db.remove_feed(&target)?;
            println!(
                "{}",
                if n > 0 {
                    "已删除"
                } else {
                    "未找到该源"
                }
            );
        }
        Some(Command::List) => list_feeds(&db)?,
        Some(Command::SetInterval { id, interval }) => {
            let secs = config::parse_duration(&interval)?;
            if db.set_interval(id, secs)? > 0 {
                println!("#{id} 间隔已设为 {secs}s");
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
            let n = async_runtime()?.block_on(daemon::update_once(&db, &cfg))?;
            println!("新增 {n} 篇");
        }
        None => {
            drop(db); // GUI 自己开连接（UI 线程 + 后台调度线程各一个），主线程这个用不上
            gui::run(paths, cfg)?;
        }
    }
    Ok(())
}

/// CLI fetch commands need Tokio, while the desktop window must stay outside
/// an entered async runtime because it owns a `reqwest::blocking::Client` for
/// image downloads. Building the runtime only in the relevant CLI branches
/// keeps both execution models on their supported threads.
fn async_runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(Into::into)
}

fn list_feeds(db: &Db) -> Result<()> {
    let feeds = db.feeds_with_unread()?;
    if feeds.is_empty() {
        println!("还没有订阅源，用 `shiyue add <url>` 添加。");
        return Ok(());
    }
    for (f, unread) in feeds {
        let status = if f.disabled {
            "[已禁用]"
        } else if f.fail_count > 0 {
            "[⚠]"
        } else {
            ""
        };
        let title = f.title.unwrap_or_else(|| f.url.clone());
        println!("#{:<3} 未读 {:<4} {status} {title}", f.id, unread);
    }
    Ok(())
}

/// daemon 与命令的日志都追加到 rrss.log（ADR-7 兜底）。
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
