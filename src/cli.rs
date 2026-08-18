//! 独立命令行管理工具（见 docs/outline.md）。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "shiyue-cli", version, about = "拾阅 RSS 命令行管理工具")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// 搜索和管理本地 AI 资源记忆库
    Resource {
        #[command(subcommand)]
        command: ResourceCommand,
    },
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

#[derive(Subcommand)]
pub enum ResourceCommand {
    Search {
        query: String,
        #[arg(long, value_enum, default_value = "all")]
        r#type: ResourceResultType,
        #[arg(long, value_enum, default_value = "curated")]
        scope: ResourceScope,
        #[arg(long, default_value_t = 5)]
        limit: usize,
        #[arg(long, required = true)]
        json: bool,
    },
    Get {
        id: i64,
        #[arg(long, required = true)]
        json: bool,
    },
    Recent {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, required = true)]
        json: bool,
    },
    Pending {
        #[arg(long, required = true)]
        json: bool,
    },
    Retry {
        id: i64,
        #[arg(long, required = true)]
        json: bool,
    },
    Add {
        url: String,
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        private: bool,
        #[arg(long, required = true)]
        json: bool,
    },
}

#[derive(clap::ValueEnum, Clone, Copy)]
pub enum ResourceResultType {
    All,
    Resource,
    Article,
}

#[derive(clap::ValueEnum, Clone, Copy)]
pub enum ResourceScope {
    Curated,
    All,
}
