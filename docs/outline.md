# 拾阅（原 rrss）—— RSS 订阅器设计大纲

> ⚠ 本文是 2026-07-11 的初版规划稿，形态已演进。**当前架构以 [adr.md](adr.md) 的 ADR-13～16 为准**：
> `shiyue`（无参）进桌面 GUI（`egui`），GUI 内置后台线程做抓取+调度，`daemon`/`tui` 命令已删。
> 下面涉及 TUI / `rrss daemon` / `tui/` 目录的段落均为历史设计，保留作背景。

一句话定位（现）：**一个 Rust 桌面应用（egui），无参启动即开窗阅读；进程内后台线程定时并发抓取订阅源、发现新文章写库并弹桌面通知（关窗缩到系统托盘继续）。CLI 子命令 `add/rm/list/update/...` 仍在。**

决策理由见 [adr.md](adr.md)，术语见 [glossary.md](glossary.md)。

## 依赖清单（Cargo.toml）

```
tokio            # async 运行时（rt-multi-thread, macros, time）
reqwest          # HTTP（rustls-tls, gzip）
feed-rs          # RSS/Atom/JSON Feed 解析
rusqlite         # SQLite（bundled 特性，免装系统库）
ratatui + crossterm   # TUI
notify-rust      # 桌面 toast
directories      # 平台标准目录
clap             # 参数解析（derive）
serde + toml     # 读 config.toml
anyhow + thiserror    # 错误
tracing + tracing-subscriber   # 日志（写文件 + 控制台）
```

## 项目结构

```
rrss/
├─ Cargo.toml
├─ docs/
│  ├─ outline.md
│  ├─ adr.md
│  └─ glossary.md
└─ src/
   ├─ main.rs      // #[tokio::main]，clap 分发子命令
   ├─ cli.rs       // clap 参数定义
   ├─ config.rs    // directories 定位 + TOML 加载（全局间隔/退避/阈值/开关）
   ├─ db.rs        // rusqlite：建表、feed/article 的增删查改
   ├─ model.rs     // Feed / Article 领域类型
   ├─ fetch.rs     // reqwest 拉取 + feed-rs 解析 → 归一 Entry
   ├─ daemon.rs    // tokio 调度循环：到期检查、并发抓、退避、禁用、触发通知
   ├─ notify.rs    // notify-rust 封装
   └─ tui/
      ├─ mod.rs    // 事件循环（crossterm 读键）
      ├─ app.rs    // TUI 状态（当前选中源/文章/焦点）
      └─ ui.rs     // ratatui 布局：源列表 | 文章列表 | 正文
```

> 注：`main` 用 `#[tokio::main]`；daemon 走 async；TUI 事件循环是同步的，跑在 runtime 上但不 `.await`，互不干扰。

## 数据库 schema

```sql
CREATE TABLE feeds (
  id           INTEGER PRIMARY KEY,
  url          TEXT NOT NULL UNIQUE,
  title        TEXT,
  interval_secs INTEGER,          -- NULL = 用全局默认（ADR-9）
  last_fetch   INTEGER,           -- unix 秒
  next_fetch   INTEGER,           -- 调度依据；退避时往后推
  last_error   TEXT,              -- ADR-11
  fail_count   INTEGER NOT NULL DEFAULT 0,   -- 退避 / 禁用计数
  disabled     INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE articles (
  id         INTEGER PRIMARY KEY,
  feed_id    INTEGER NOT NULL REFERENCES feeds(id) ON DELETE CASCADE,
  entry_id   TEXT NOT NULL,       -- guid/id，回退为 url（ADR-8）
  url        TEXT,
  title      TEXT,
  author     TEXT,
  published  INTEGER,
  content    TEXT,
  is_read    INTEGER NOT NULL DEFAULT 0,
  starred    INTEGER NOT NULL DEFAULT 0,
  fetched_at INTEGER NOT NULL,
  UNIQUE(feed_id, entry_id)       -- 去重的核心，配合 INSERT OR IGNORE
);
```

设置（全局默认间隔、退避基数/上限、失败禁用阈值、通知开关）都在 `config.toml`，不进库。

## CLI 命令面

```
rrss add <url>              添加源（首次抓一次拿标题）
rrss rm <id|url>            删除源
rrss list                   列出源：未读数 / 状态(⚠/禁用) / 上次抓取
rrss set-interval <id> <dur>  单源间隔（如 5m / 6h），覆盖全局
rrss enable <id>            解除禁用（清 fail_count）
rrss disable <id>           手动禁用
rrss update                 前台抓一轮就退出（手动/调试用）
rrss daemon                 常驻：按到期调度抓取 + 通知
rrss                        无参 → 进 TUI 阅读
```

## 两个核心循环

**daemon.rs（伪码）**
```
loop {
  now = 当前时间
  due = feeds where !disabled && now >= next_fetch
  results = join_all(due.map(fetch_one))          // reqwest + feed-rs 并发
  for (feed, res) in results:
    match res:
      Ok(entries):
        new = INSERT OR IGNORE 所有 entry；changes() 即新增数   // ADR-8
        fail_count=0, last_error=NULL
        next_fetch = now + (interval_secs ?? 全局默认)          // ADR-9
        累计 new 到通知批次
      Err(e):
        fail_count++, last_error=e                              // ADR-11
        backoff = min(base * 2^fail_count, cap)
        next_fetch = now + backoff
        if fail_count >= 阈值: disabled=1
  if 通知批次非空: notify_rust 弹 toast（"3 个源共 12 篇新文章"）  // ADR-7
  sleep 到最近的 next_fetch（或最小 tick，取大）
}
```

**tui/mod.rs（伪码）**
```
初始化 crossterm 进 raw+alt-screen
loop {
  从 db 读源列表(带未读数) + 当前源的文章列表 + 选中文章正文
  ui::draw(布局: 三栏)
  match 读键:
    j/k       上下移动
    Tab       切换焦点(源↔文章)
    Enter     打开文章正文
    r         标已读（写 is_read）
    s         星标
    R         手动触发一次 update（可选）
    q         退出并恢复终端
}
```

## 分阶段实现路线（里程碑）

1. **骨架**：`cargo new` → config+路径(`directories`) → db 建表 → `add`/`list`/`rm`。能把源存进库。
2. **抓取**：`fetch.rs`（reqwest+feed-rs）→ `rrss update` 前台抓一轮、去重入库。链路打通。
3. **守护**：`daemon.rs` tokio 调度 + 每源到期检查 + 退避/禁用 + `notify-rust` toast。
4. **阅读**：`tui/` ratatui 三栏、标已读/星标、⚠ 状态显示。
5. **打磨**：`tracing` 日志、错误展示；**外部推送（Telegram/邮件等）** 留作后续扩展。

## 搭建命令

```powershell
cd <你的项目目录>
cargo add tokio --features rt-multi-thread,macros,time
cargo add reqwest --features rustls-tls,gzip --no-default-features
cargo add feed-rs
cargo add rusqlite --features bundled
cargo add ratatui crossterm notify-rust directories clap serde toml anyhow thiserror tracing tracing-subscriber
```
