# 术语表（Glossary）

- **Feed / 源**：一个订阅 URL 及其元数据（`feeds` 表一行）。
- **Entry / Article / 条目**：源里的一篇文章（`articles` 表一行）。
- **guid / id**：条目的规范唯一标识（RSS `<guid>` / Atom `<id>`）；去重键，缺失时回退用文章 URL。
- **poll / 抓取一轮**：daemon 对到期源执行一次「拉取 → 解析 → 入库」。
- **due / 到期**：`now >= next_fetch` 且未禁用的源，本轮需要抓。
- **backoff / 退避**：源失败后按 `base * 2^fail_count` 拉长下次抓取间隔，封顶。
- **disabled / 禁用**：连续失败超阈值后停抓，需 `rrss enable` 恢复。
- **unread / 未读**、**starred / 星标**：条目的阅读态与收藏态。
- **daemon / 守护进程**：~~`rrss daemon` 常驻~~ 命令已退役（ADR-14）。定时抓取+通知现由 GUI 进程内的**后台调度线程**承担；`rrss update` 仍可一次性抓一轮。
- **scheduler / 调度循环**：按每源 `next_fetch` 到期「拉取→解析→入库」的常驻循环，现跑在 GUI 后台线程（tokio 运行时）里，抓完发信号叫 UI 刷新。
- **tray / 托盘**：关窗后 app 缩到系统托盘继续后台抓取/通知，托盘菜单才真正退出（ADR-15）。
- **block / 正文块**：正文按 HTML 解析成的有序单元，`文字块` 或 `图片块`，按原文位置穿插渲染（ADR-16）。
- **global default interval / 全局默认间隔**：`config.toml` 中的抓取间隔，源未单独设置时采用。
