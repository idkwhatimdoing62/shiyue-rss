# 术语表（Glossary）

- **Resource / 资源**：用户希望长期保存、以后可由 AI 检索并推荐的网站、页面或文章引用；与 RSS Article 是不同领域实体，允许通过 `linked_article_id` 关联但不复制或接管 Article（ADR-0007）。
- **Resource Library Lifecycle / 资源库生命周期**：资源创建、完整人工编辑、整理状态转换、永久删除、网页收藏导入和集合投影的唯一应用边界（ADR-0007）。
- **Resource Curation State / 资源整理状态**：人的资料维护状态：待确认、可用或已归档。它与来源健康状态彼此独立（ADR-0007）。
- **Resource Health / 资源健康状态**：来源可访问性的运行状态：未知、健康或失效。失效资源仍可处于可用整理状态，因此会同时出现在“我的资源”和“失效”视图（ADR-0007）。
- **Resource Library Projection / 资源库投影**：同一 SQLite 一致快照中的作用域资源、详情、完整集合计数和稳定分页游标；GUI 必须整体采用（ADR-0007）。
- **Processing Handoff / 处理交接**：资源事务提交后向 Knowledge Processing 请求后台补全。交接失败不回滚已保存资源，而是返回可观察、可重试的 Deferred 结果（ADR-0007）。

- **Feed / 源**：一个订阅 URL 及其元数据（`feeds` 表一行）。
- **Feed Subscription / RSS 订阅**：用户持续关注一个 Feed 的持久意图，包含规范化 URL、启用状态和可选单源刷新间隔；删除时连同该 Feed 的本地文章永久移除（ADR-0005）。
- **Feed Subscription Lifecycle / 订阅生命周期**：GUI 与 CLI 修改订阅的唯一应用边界，统一增删、启停、间隔、幂等 URL、首次刷新和维护期错误语义（ADR-0005）。
- **Entry / Article / 条目**：源里的一篇文章（`articles` 表一行）。
- **Article Bookmark / 文章收藏**：Article 的长期保留状态；与已读、稍后读、归档、标签和摘录相互独立。网页收藏 Article 固定属于该集合（ADR-0006）。
- **Read Later / 稍后读**：用户打算稍后返回 Article 的临时队列状态；不会隐式改变已读、收藏或归档状态（ADR-0006）。
- **Article Archive / 文章归档**：暂时从 Feed、文章收藏和稍后读集合隐藏 Article，但保留其收藏、稍后读、已读和标签状态；恢复后重新显露（ADR-0006）。
- **Article Library Lifecycle / 文章资料生命周期**：文章收藏、稍后读、归档、已读、标签和批量操作的唯一应用边界，统一事务、维护期、固定网页收藏和错误语义（ADR-0006）。
- **Article Library Projection / 文章资料投影**：从同一 SQLite 一致快照返回的作用域文章、标签、固定收藏标识、集合计数和 Feed 未读数；GUI 必须整体采用，不能自行推断或乐观修改（ADR-0006）。
- **guid / id**：条目的规范唯一标识（RSS `<guid>` / Atom `<id>`）；去重键，缺失时回退用文章 URL。
- **RSS Refresh Run / RSS 刷新运行**：一次会话内对确定 Feed 集合执行的「拉取 → 解析 → 独立提交」。运行本身不跨重启恢复；文章、下次刷新时间和最近失败由 Feed 数据持久保存（ADR-0004）。
- **due / 到期**：`now >= next_fetch` 且未禁用的源，本轮需要抓。
- **backoff / 退避**：源失败后按 `base * 2^fail_count` 拉长下次抓取间隔，封顶。
- **disabled / 禁用**：连续失败超阈值后停抓，需 `rrss enable` 恢复。
- **unread / 未读**：Article 的阅读状态；打开 Article 后的标已读失败不会阻止正文阅读。
- **RSS Refresh Workflow / RSS 刷新工作流**：统一接收 GUI、CLI 和新增订阅的刷新意图，内部负责到期调度、最多 8 路抓取、独立提交、可观察状态与资料维护中断；调用方不再自行编排抓取。
- **daemon / 守护进程**：已退役的独立常驻命令。定时刷新现由桌面进程持有的 RSS Refresh Workflow 执行；`shiyue-cli update` 通过同一模块执行一次性 Run。
- **tray / 托盘**：关窗后 app 缩到系统托盘继续后台抓取/通知，托盘菜单才真正退出（ADR-15）。
- **block / 正文块**：正文按 HTML 解析成的有序单元，`文字块` 或 `图片块`，按原文位置穿插渲染（ADR-16）。
- **global default interval / 全局默认间隔**：`config.toml` 中的抓取间隔，源未单独设置时采用。
