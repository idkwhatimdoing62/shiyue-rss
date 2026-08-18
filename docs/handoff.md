# rrss AI 资源记忆库实施交接

## 交接目标

在 `C:\yangxiaochen\rrss` 中实施“AI 资源记忆库”。产品决策已完成并获用户确认，唯一产品规格是：

`C:\Users\xingr\Documents\囤囤鼠\.scratch\ai-resource-memory\spec.md`

完整读取该规格后再设计或修改代码。规格是产品范围、数据契约、安全规则、测试要求和阶段完成条件的单一事实来源；本交接只规定接手顺序。

## 当前现场

- 目标仓库：`C:\yangxiaochen\rrss`
- 当前分支：`main`
- 检查时 HEAD：`fa92106 feat: improve article favorites and web guide imports`
- 工作区已有大量未提交修改和未跟踪文件，且 `src/db.rs`、`src/model.rs`、`src/gui.rs`、`src/lib.rs`、`src/config.rs` 等与本功能可能重叠。
- 这些现场改动属于用户或前序 Agent。保留并理解它们，在当前设计上增量实现；不得 reset、checkout、清理或覆盖。
- 现有 `CONTEXT.md` 定义 Article Bookmark、Read Later、Tag、Stable Excerpt Anchor、Batch Article Action 和 Search History。新增术语要与它保持一致；Resource 与 Article 是不同领域实体。

## 接手步骤

### 1. 恢复上下文

读取目标仓库的 `CONTEXT.md`、相关 ADR、当前 Git diff、Cargo 配置，以及规格第 3 节列出的关键模块。将未提交改动按“已完成能力、进行中能力、与 Resource 实施的重叠风险”分类。

完成条件：能指出所有计划修改文件中已有的现场改动，并给出保留方式；没有把工作区差异误判成应清理的旧文件。

### 2. 对齐实施方案

以规格的五个 Phase 为顺序，给出文件级计划、数据库迁移策略和测试入口。发现规格与当前代码事实冲突时，先展示证据并请求用户裁决；实现细节可自行决定，但产品边界、CLI JSON、隐私不变量和验收门槛需要显式变更批准。

完成条件：计划覆盖 Phase 1 的每项交付和测试，并列出后续 Phase 的依赖；所有现场重叠都有处理方案。

### 3. 只实施 Phase 1

第一批工作只实现规格“Phase 1：领域与持久化”：Resource 模型、SQLite schema、repository/service、状态机、分类和标签 provenance、Snapshot、Enrichment run、Usage event 及迁移测试。

建立 GUI、DeepSeek、Credential Manager、完整搜索或 CLI 命令所需的接口边界可以进入 Phase 1；其业务实现留在对应后续 Phase。

完成条件采用规格 Phase 1 的原文：现有数据库副本升级后通过 integrity check；旧功能测试全部通过；Resource CRUD 和状态机测试齐全。

### 4. 验证并停在阶段门

运行与风险相称的格式化、单元测试、迁移测试和现有回归测试。报告修改文件、schema 变化、测试命令及结果、仍有风险和 Phase 2 计划，然后等待用户确认再继续。

完成条件：Phase 1 的证据可复现，用户能在不阅读全部 diff 的情况下判断是否准入 Phase 2。

## 硬边界

- 首版 CLI 使用现有二进制 `shiyue-cli resource ...`；可执行文件重命名不在范围内。
- Resource 保持独立实体；关联 Article 时复用内容来源，不把 Resource 塞回隐藏 Feed 模型。
- private 资源的数据流保持纯本地。
- 页面内容按不可信数据处理，模型输出必须经过固定 schema 验证。
- 第一版使用 FTS 与结构化字段，不引入 embedding、MCP、公网服务、多用户或同步。
- API Key 只来自 Windows Credential Manager 或环境变量。
- Agent 的 CLI 写能力止于 `resource add`；edit、tag、archive 和 delete 由 GUI 中的人执行。

## 可直接发送给 rrss Agent 的消息

```text
继续负责 C:\yangxiaochen\rrss 项目。用户已经完成并确认“AI 资源记忆库”的产品访谈。

先完整读取交接文档：
C:\Users\xingr\Documents\囤囤鼠\.scratch\ai-resource-memory\handoff.md

再完整读取唯一产品规格：
C:\Users\xingr\Documents\囤囤鼠\.scratch\ai-resource-memory\spec.md

目标仓库当前 main 工作区有大量未提交改动，它们属于现有现场；保留并审计，禁止 reset、checkout 或覆盖。先检查 CONTEXT.md、相关 ADR、git diff 和现有测试。

先提交文件级实施计划和重叠风险，然后只实施规格 Phase 1。完成 Phase 1 的迁移、测试和完整验证后停下汇报，等我确认再进入 Phase 2。若规格与当前代码冲突，用文件和行号给出证据后问我，不要静默改变已确认的产品决策。
```

