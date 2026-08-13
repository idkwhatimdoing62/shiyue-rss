//! 命令面（见 docs/outline.md）。无子命令 → 进桌面 GUI（内置抓取+调度）。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "shiyue", version, about = "拾阅 RSS 阅读器")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// 添加订阅源并立即抓一次
    Add { url: String },
    /// 删除源（按 id 或 url）
    Rm { target: String },
    /// 列出所有源：未读数 / 状态 / 上次抓取
    List,
    /// 设置单源抓取间隔（如 5m / 6h），覆盖全局
    SetInterval { id: i64, interval: String },
    /// 解除禁用（清空失败状态）
    Enable { id: i64 },
    /// 手动禁用某源
    Disable { id: i64 },
    /// 前台抓一轮就退出（脚本/调试用；日常抓取由 GUI 后台线程负责）
    Update,
}
