# AI 资源记忆库规格

Status: approved-for-implementation
Target repository: `C:\yangxiaochen\rrss`
Product scope: personal, local-first, Windows desktop

## 1. Outcome

在 rrss 中加入独立的个人资源库。用户收藏网站、具体页面或文章后，系统抓取快照并生成结构化用途描述；本机 AI 通过稳定 CLI 搜索这些资源，获得链接、匹配证据、适用场景、限制与数据时间。

首版完成的判断标准不是“页面和命令存在”，而是：用 20～50 个真实收藏和 10 条真实查询组成回归集，每条查询的前 5 个结果中至少有 1 个用户认可的结果。

## 2. 已确认的产品边界

- 只服务单个本机用户；表结构无需预埋多用户租户层。
- Resource 是独立领域实体；Article 保持现状，两者可以关联但不互相伪装。
- 默认统一搜索 Resource 与经过筛选的 Article，并允许按类型过滤。
- Article 默认范围为 `starred = true` 或来自网页收藏；只有显式请求才搜索全部 RSS Article。
- GUI 首版支持粘贴 URL；浏览器扩展是首版验证后的下一项能力。
- 本机 Agent 通过 CLI 使用资源库。MCP、远程 HTTP 服务和 ChatGPT 网页直连不在首版。
- DeepSeek 是主要模型提供方，但实现采用 OpenAI-compatible 配置，不把供应商写死在业务代码中。
- 私密资源只做本地抓取、编辑和 FTS 检索；其 URL、正文和备注均不得发送云端。
- 网页内容是不可信输入，不能成为指令来源。

## 3. 现有系统约束

实现前先核对这些事实仍然成立：

- Cargo package 是 `rrss`，桌面二进制是 `shiyue`，CLI 二进制是 `shiyue-cli`。
- SQLite schema 当前由 `src/db.rs` 中幂等 `CREATE TABLE IF NOT EXISTS` 语句维护。
- 网页收藏当前作为 `shiyue://web-clippings` 隐藏 Feed 下的 Article 保存。
- `library_fts` 是 SQLite FTS5 trigram 表，现有 kind `0..3` 表示 Article、WebClipping、Excerpt、Thought。
- `web_clip.rs` 已实施 HTTP(S) 限制、SSRF/内网阻断、重定向检查和响应体上限；Resource 抓取复用同一安全路径。
- `text.rs` 已提供标题与正文抽取；Resource 不另写第二套 HTML 清洗器。
- `lib.rs` 中模块当前均为私有；CLI 与 GUI 应调用同一内部 service，而不是各复制一套流程。

规格中的命令使用实际二进制名 `shiyue-cli`。重命名可执行文件不属于本功能。

## 4. 领域模型

### 4.1 Resource

`resources` 至少包含：

| 字段 | 规则 |
| --- | --- |
| `id` | SQLite INTEGER 主键；CLI 以字符串输出以便未来更换标识方案 |
| `url` | 用户输入 URL，唯一必填输入 |
| `canonical_url` | 规范化后用于去重；不得仅按域名合并 |
| `parent_resource_id` | 可空；具体页面可指向所属网站 Resource |
| `linked_article_id` | 可空；指向已有网页收藏或 Article 快照，避免复制正文 |
| `kind` | `site`、`page`、`article` 三选一 |
| `title` | 原始标题；抓取失败时可空 |
| `purpose_zh` | 中文一句话用途，可空 |
| `use_when_zh` | 中文适用场景，可空 |
| `capabilities` | JSON 字符串数组；每项是短语 |
| `limitations` | JSON 字符串数组；无证据时为空而非猜测 |
| `pricing` | 可空；`free`、`freemium`、`paid`、`unknown` |
| `requires_login` | 可空布尔值；未知保持 null |
| `languages` | JSON 字符串数组 |
| `private_note` | 用户备注，可空 |
| `privacy` | `public` 或 `private`；默认 `public` |
| `status` | 见状态机 |
| `manual_rating` | 可空整数 1～5 |
| `latest_snapshot_id` | 可空；指向最新成功快照 |
| `last_checked_at` | 可空 Unix 秒 |
| `created_at` / `updated_at` | Unix 秒 |

SQLite 不原生约束 JSON 结构时，Rust service 必须在写入前验证枚举、数组和评分范围。

### 4.2 分类和标签

- `kind` 表达资源形态且只能有一个。
- `categories` 表达用途且允许多个，首版词表为：`tool`、`asset-library`、`docs`、`blog`、`inspiration`、`service`、`repository`、`other`。
- `tags` 表达具体能力，可自由扩展并同时保存中文和英文词。
- `resource_categories(resource_id, category)` 使用复合主键。
- 新建 `resource_tags`，不要复用 `article_tags` 关联表。
- 标签记录 `source = manual|ai`。同名手工标签优先；AI 整理不得删除或覆盖手工标签。

### 4.3 Snapshot

`resource_snapshots` 至少包含：

- `id`、`resource_id`
- `content_hash`
- `fetched_url`、`http_status`
- `title`、`cleaned_content`
- `fetched_at`
- `fetch_error`（失败检查可以无正文）

只有清洗正文或关键元数据的 hash 变化时才创建新成功版本。`resources.latest_snapshot_id` 指向最新版。首版 UI 只展示最新版，不实现版本 diff。

关联了 Article 的 Resource 可以把 Article 作为初始内容来源；不得复制相同正文进入 Snapshot，除非之后主动刷新形成 Resource 自己的版本。

### 4.4 Enrichment run

`resource_enrichment_runs` 保存：

- `resource_id`、可空 `snapshot_id`
- `provider`、`model`
- `prompt_version`、`schema_version`
- `started_at`、`finished_at`
- `status = pending|running|succeeded|failed`
- 可空、去敏后的 `error_code` 和 `error_message`

不保存 API Key、完整请求或完整模型响应。日志同样执行去敏。

### 4.5 Usage event

记录 `returned` 与 `confirmed_used` 事件及时间，供以后分析。首版排序只使用 `manual_rating`，usage event 不改变排名。

## 5. 状态机

Resource 状态为：

- `pending_review`：由 AI CLI 添加，尚未人工确认；默认搜索排除。
- `enrichment_pending`：用户已保存，但抓取或模型整理未完成；允许在 GUI 中重试。
- `active`：人工确认可用于默认检索。
- `broken`：检查确认 URL 失效；默认搜索排除，历史快照保留。
- `archived`：用户主动弃用；默认搜索排除。

允许的核心转换：

```text
AI add -> pending_review
GUI add -> enrichment_pending -> active
pending_review -> active | archived | physical delete
enrichment_pending -> active | enrichment_pending
active -> broken | archived
broken -> active | archived
archived -> active | physical delete
```

模型整理成功不能自动把 `pending_review` 变成 `active`。物理删除只在 GUI 二次确认后执行，并级联删除 Resource 专属数据；关联 Article 不随之删除。

## 6. 收藏与整理流程

### 6.1 GUI 添加

1. 复用现有网页收藏对话框的 URL/HTML 输入能力，增加明确的保存类型选择：`文章` 或 `资源`，默认记住上一次选择。
2. 选择资源时，URL 是唯一必填项；可选输入私人备注和隐私级别。
3. 立即创建 Resource，使“保存成功”不依赖网络或模型。
4. 后台通过现有安全抓取器获取页面，并通过现有正文抽取器生成 cleaned content。
5. 抓取成功后创建 Snapshot；失败则保留 Resource、记录错误并置为 `enrichment_pending`。
6. public 资源进入模型整理；private 资源跳过云模型并保留本地可编辑字段。
7. GUI 展示整理结果供快速确认：标题、用途、分类、隐私和手工评分。

完成条件：断网、抓取失败和 DeepSeek 不可用三种情况下，URL 均已持久化且 GUI 能重试。

### 6.2 CLI 添加

`shiyue-cli resource add <url> [--note <text>] [--private] --json`

- 复用相同 service 和安全抓取队列。
- 初始状态固定为 `pending_review`。
- 返回成功表示 Resource 已持久化，不表示抓取或整理成功。
- 记录添加来源为 `cli_agent`。
- CLI 不提供 AI 可调用的 edit、tag 或 delete 命令。

### 6.3 模型输入

public 资源发送：标题、页面元数据、用户备注和有明确上限的清洗正文。默认上限应位于配置中并有保守默认值；超长正文按顺序分块摘要，再以摘要生成最终结构。

模型输出必须匹配固定 JSON schema，至少包含：

- `purpose_zh`
- `use_when_zh`
- `capabilities[]`
- `limitations[]`
- `categories[]`
- `tags_zh[]`、`tags_en[]`
- `pricing`、`requires_login`、`languages[]`
- 每个事实字段的 `evidence[]` 或 `inferred` 标志

无证据的价格、授权、登录要求保持 `unknown/null`。网页中的命令式文本只作为页面内容，不进入 system/developer 指令位置。模型响应先反序列化并验证，再进入事务写库。

## 7. Provider 与凭据

配置文件只保存非秘密项：

```toml
[resource_enrichment]
enabled = true
provider = "openai-compatible"
base_url = "https://api.deepseek.com"
model = "<user-configured-model>"
max_input_chars = 60000
prompt_version = "resource-v1"
schema_version = "1"
```

API Key 查找顺序：

1. 专用环境变量覆盖值，供开发和临时运行使用。
2. Windows Credential Manager 中 rrss 专用凭据。
3. 无凭据：跳过云整理并保留 `enrichment_pending`，给出可操作错误。

凭据读取封装成 provider-neutral trait。Windows 凭据操作放在独立模块；测试以假实现注入，不访问开发机真实凭据。

## 8. 搜索

### 8.1 收录范围

默认 `scope=curated`：

- `status=active` 的 Resource
- `starred=true` 的 Article
- 现有网页收藏 Article

显式 `scope=all` 才加入全部未归档 Article。`pending_review`、`enrichment_pending`、`broken`、`archived` 只有对应状态过滤器明确指定时才返回。

### 8.2 首版召回和排序

扩展或新增 FTS 表以索引：

- Resource 标题、URL、中文用途、适用场景
- capabilities、limitations、categories
- AI 与手工标签
- 私人备注
- 最新 Snapshot 的 cleaned content
- 现有 Article、摘录和想法

不要继续用难以扩展的裸整数表达新 kind；Rust 层定义稳定枚举映射并为迁移写测试。

搜索流水线：

1. 规范化用户查询。
2. 可选接收调用方提供的中英文同义词；数据库搜索本身保持确定性，不在每次 CLI 查询中强制调用云模型。
3. 用 FTS5/trigram 和结构化过滤召回候选。
4. 按文本相关性排序。
5. 对 Resource 应用有限的手工评分 boost；boost 不得使完全无文本匹配的项目进入结果。
6. 去除同一 Resource/Snapshot 的重复命中，保留最佳证据片段。

第一版不引入 embedding。只有回归集显示稳定语义漏检，且查询扩展无法解决时，才另写设计决策加入多语言向量。

### 8.3 搜索结果证据

每条结果至少返回：

- 稳定 `id` 与 `result_type`
- URL、标题、kind、categories、tags
- `purpose_zh`、`use_when_zh`
- capabilities、limitations、pricing、requires_login
- private note（仅本机 CLI；标明来源）
- `matched_fields[]`
- `evidence_snippets[]`，每项含来源类型和可空 snapshot/article id
- `updated_at`、`last_checked_at`、status
- 数值 `score` 与可读的 `score_factors[]`

CLI 不生成自然语言结论。调用方 AI 根据结构化证据解释“为什么匹配”。

## 9. CLI 契约

### 9.1 命令

```text
shiyue-cli resource search <query> --type all|resource|article --scope curated|all --limit 5 --json
shiyue-cli resource get <id> --json
shiyue-cli resource recent --limit 20 --json
shiyue-cli resource pending --json
shiyue-cli resource retry <id> --json
shiyue-cli resource add <url> [--note <text>] [--private] --json
```

所有 resource 子命令支持 `--json`；供 Agent 使用时必须传它。JSON 写 stdout，诊断日志写 stderr。

### 9.2 Envelope

成功：

```json
{
  "schema_version": 1,
  "ok": true,
  "data": {},
  "warnings": []
}
```

失败：

```json
{
  "schema_version": 1,
  "ok": false,
  "error": {
    "code": "RESOURCE_NOT_FOUND",
    "message": "Resource 42 was not found",
    "retryable": false
  }
}
```

约束：

- JSON 字段使用 `snake_case`。
- 时间使用 UTC RFC 3339；数据库内部可继续使用 Unix 秒。
- ID 在 JSON 中使用字符串。
- 新增可选字段保持向后兼容；删除或改变含义必须提升 `schema_version`。
- `--json` 模式即使失败也尽力输出一个合法 envelope。

退出码：`0` 成功，`2` 参数错误，`3` 未找到，`4` 配置/凭据错误，`5` 临时网络或 provider 错误，`1` 其他内部错误。

完成条件：集成测试逐字解析 stdout 为 JSON，并证明 stderr 中的日志不会污染 stdout。

## 10. GUI

首版新增“资源库”主视图：

- active、待确认、待整理、失效、归档过滤器
- 关键词搜索与 kind/category/tag 过滤
- 添加 URL
- 详情与编辑
- 确认 pending_review
- 重试抓取/整理
- 设置 1～5 手工评分
- 归档、恢复、二次确认物理删除
- 打开原始 URL 和查看最新快照

详情页区分 AI 字段与手工字段。模型重新整理只能覆盖上一次 AI 生成值；手工修改值必须有 provenance 标记并受到保护。

首版不实现内置聊天、复杂快照 diff、批量自动刷新或浏览器扩展。

## 11. 旧数据导入

提供一次性、可重复预览的导入流程：

1. 扫描隐藏网页收藏 Feed 下的 Article。
2. 以 canonical URL 和现有 Resource 关联检查去重。
3. 展示候选列表，允许逐项或批量选中。
4. 选中项创建 Resource，`linked_article_id` 指向原 Article；不复制正文。
5. 原 Article、标签、摘录和收藏状态保持不变。
6. 加星 RSS Article 不转成 Resource，只自动进入 curated 搜索范围。

导入流程必须可安全重跑；同一 Article 最多关联一个由本导入流程创建的 Resource。迁移前复用现有备份机制创建数据库备份。

## 12. 安全与隐私不变量

- 所有 URL 抓取复用 `web_clip.rs` 的协议、DNS/IP、重定向和响应大小防护。
- private 资源的 URL、标题、正文、备注及衍生文本均不进入云端 provider 请求。
- 页面 HTML、正文、标题和元数据都作为不可信 data 传给模型。
- provider 仅接收固定 system 指令和被明确标记的数据段。
- 模型输出经过 JSON schema、枚举、长度与 URL 验证；写库使用事务和参数化 SQL。
- API Key 只从 Credential Manager 或环境变量进入内存，不进入 SQLite、配置、日志、备份或错误 envelope。
- CLI 的 private note 仅在本机返回；未来若增加网络接口，必须另行设计授权和字段裁剪。

## 13. 测试与验收

### 13.1 自动化测试

至少覆盖：

- schema 从现有数据库幂等升级，原 feeds/articles/FTS 数据不丢失
- Resource 枚举、评分和 provenance 验证
- canonical URL 去重不误合并同域不同页面
- 父网站与子页面关系
- Snapshot 只在内容变化时新增
- private 资源永不调用 provider fake
- provider 返回非法 JSON、未知枚举、超长字段和 prompt injection 文本时安全失败
- 手工字段不被重新整理覆盖
- pending/broken/archived 默认不进入搜索
- curated 与 all Article 范围差异
- 中文用途/标签可以命中英文网站 Resource
- 手工评分只对已有文本候选做有限 boost
- CLI envelope、退出码及 stdout/stderr 隔离
- CLI add 落库后即成功，后台失败可重试
- 旧网页收藏导入可重跑且不重复
- 删除 Resource 不删除关联 Article

### 13.2 真实回归集

建立一个不含秘密的测试 fixture 或本地测试数据集：20～50 个真实资源，包含网站、具体页面、加星文章、中英文页面、失效链接和至少一个 private fixture。

维护 10 条真实查询及认可结果 ID，例如：

- 找可以制作或下载 App icon 的网站
- 找无需登录的 SVG 工具
- 找允许商业使用的图标资源
- 找讲 Rust GUI 的文章
- 找之前收藏的设计灵感网站

验收门槛：每条查询前 5 名至少含一个认可结果；所有失败查询必须记录为排序/召回缺口，不能通过修改认可列表掩盖回归。

## 14. 实施阶段

### Phase 1：领域与持久化

新增模型、schema、repository/service、状态转换、分类/标签 provenance、Snapshot 与迁移测试。

完成条件：现有数据库副本升级后通过 integrity check；旧功能测试全部通过；Resource CRUD 和状态机测试齐全。

### Phase 2：确定性搜索与 CLI

接入 Resource FTS、curated/all 范围、结果证据和版本化 JSON 命令。

完成条件：CLI 集成测试通过，手工构造的中英文 fixture 能按过滤器稳定返回预期证据。

### Phase 3：GUI 收藏与管理

增加保存类型选择、资源库视图、待确认/待整理队列、编辑、评分、归档、恢复和删除。

完成条件：断网情况下仍能保存 URL；用户可从 GUI 完成 Resource 全生命周期而无需改数据库。

### Phase 4：DeepSeek 整理

实现 provider-neutral OpenAI-compatible client、Credential Manager、结构化输出验证、隐私隔离和重试。

完成条件：fake provider 覆盖成功与所有失败分支；真实 DeepSeek smoke test 由用户显式提供凭据后运行；private fixture 的 provider 调用次数为零。

### Phase 5：旧数据导入与产品验收

实现预览导入，建立真实查询回归集并调校确定性排序。

完成条件：导入可重复执行且无重复；10 条真实查询全部达到 top-5 门槛。

## 15. 首版非目标

- 浏览器扩展
- MCP、远程 HTTP API、Secure MCP Tunnel 或 ChatGPT 网页直连
- embedding 或向量数据库
- 多用户、账户、登录、跨设备同步
- rrss 内置聊天
- 自动定时刷新全部网站
- 复杂快照差异 UI
- CLI 可执行文件重命名
- 允许 Agent 自动 edit、tag、archive 或 delete

## 16. 后续决策触发器

- 真实回归集中出现稳定语义漏检：评估多语言 embedding。
- CLI 搜索价值通过验收：优先开发浏览器扩展。
- 需要 ChatGPT 网页直接访问：单独设计远程 MCP、隧道、鉴权和 private 字段隔离。
- 多入口采集导致 GUI/CLI service 边界吃力：评估独立本地后台服务。
- usage event 数据足够且能区分“返回”与“采用”：再决定是否纳入排序。
