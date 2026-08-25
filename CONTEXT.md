# Domain Context

## Article Bookmark / 文章收藏

An article-level saved state for durable reference. It is independent of read state, archive state, excerpts, and the read-later queue.

## Read Later / 稍后读

A temporary queue state that says the user intends to return to an article. Adding an article does not mark it unread, bookmark it, or prevent archival.

## Tag / 标签

A user-defined normalized label that can be attached to multiple articles. Editing an Article's Tags replaces its complete Tag set; empty names are discarded and names are unique without regard to letter case.

## Stable Excerpt Anchor / 稳定摘录锚点

The selected quote plus nearby prefix and suffix context and its last known character offsets. Exact offsets are preferred; context disambiguates repeated quotes after the article body changes.

## Batch Article Action / 批量文章操作

One explicit state change applied atomically to a user-selected set of articles. Batch actions set a target state, never infer per-row toggles, and change no Article when any selected Article no longer exists.

## Article Archive / 文章归档

An independent state that temporarily removes an Article from normal Feed, Article Bookmark, and Read Later collections without clearing its bookmark, read-later, read, or Tag states. Restoring the Article makes those retained states visible again.

## Article Library Lifecycle / 文章资料生命周期

The authoritative lifecycle for an Article's bookmark, read-later, archive, read, and Tag states, including Batch Article Actions and their effects on library collections and counts. It does not own Web Clipping intake or deletion, Excerpts and Thoughts, Knowledge Processing, or desktop interaction state.

Its external interface accepts only explicit lifecycle changes and Article Library scopes. A successful change returns an authoritative SQLite projection for the requested scope together with library counts and Feed unread counts; callers do not infer or optimistically patch those values. Route, Modal, Panel, Popover, and Notice remain Desktop Interaction concepts outside this module.

The module uses the concrete local SQLite database and its writer permit directly. SQLite transactions, schema details, Web Clipping fixed membership, Tag normalization, collection visibility, and count derivation remain hidden implementation knowledge rather than a repository interface.

An Article Library Projection is a SQLite-backed consistent snapshot for one Article Library scope. It contains the scoped Articles, their lifecycle states and normalized Tags, the Article Bookmark, Read Later, and Archive counts, and Feed unread counts. A projection for one Article must distinguish a missing Article from an Article with no Tags.

Lifecycle changes are idempotent and use last-successful-commit semantics. A successful change reports whether any target changed and returns the projection read inside the same transaction; a later writer may supersede it. Input, missing-Article, Data Maintenance, and storage failures never authorize callers to patch their existing projection.

## Search History / 搜索历史

A deduplicated record of completed non-empty library searches, ordered by most recent use and carrying use count and last result count.

## Resource / 资源

A website, page, article, tool, or reference intentionally kept so that a future search can return it when it fits the user's need. A Resource remains distinct from an Article even when it reuses an Article's saved content.

## Resource Identity / 资源身份

One canonical HTTP(S) URL identifies one Resource, including its path and meaningful query. The identity is immutable: changing the URL creates a different Resource, while redirect destinations belong to Resource Snapshots. Adding the same identity is idempotent and never overwrites manual fields, changes curation state, or implicitly retries processing.

## Resource Curation State / 资源收录状态

The user's durable decision about whether a Resource is awaiting review, active in the library, or archived. A user-added or explicitly imported Resource is active, an agent-added Resource awaits review, and Knowledge Processing never changes this state.
_Avoid_: Resource status, enrichment status

## Resource Health / 资源健康状态

The known condition of a Resource's source: unknown, healthy, or broken. A transient failure does not make it broken, while a later successful fetch restores healthy independently of curation and processing state.
_Avoid_: Resource status, task status

## Resource Library Lifecycle / 资源资料生命周期

The authoritative lifecycle for human-managed Resource creation, editing, confirmation, archiving, restoration, deletion, classification, tagging, and Web Clipping import. It preserves the Resource before requesting Knowledge Processing and does not own processing execution, Article lifecycle, desktop interaction, search ranking, or caller-specific presentation.

Each successful lifecycle change validates current Resource and processing state, commits one SQLite transaction, and returns the authoritative projection read in that transaction. Knowledge Processing handoff occurs only after commit; a deferred handoff is reported without rolling back the Resource. Timestamps come from the lifecycle's clock rather than from callers.

Web Clipping import is one all-or-nothing durable change over the selected clipping identities. Existing imports are reported idempotently, invalid membership rolls back the import, and post-commit processing handoffs may independently be queued or deferred for each newly created Resource.

Manual editing atomically replaces the complete editable Resource description and Resource Classification after normalization. The Resource identity cannot be edited, and any invalid field leaves the Resource unchanged. Permanent deletion is also atomic: it is rejected while Knowledge Processing is queued or running and otherwise removes terminal processing history together with all Resource-owned data.

## Resource Library Projection / 资源资料投影

A consistent Resource Library snapshot containing scoped Resources, their curation and health states, classifications, Tags, manual provenance, and authoritative collection counts. Knowledge Processing state, cross-library search ranking, and caller-specific presentation are not part of this projection.

Collection projections use stable cursor pages ordered by most recent change and Resource identity, while one-Resource projections provide complete detail. Broken Resources remain visible in their curation collection and also appear in the overlapping Broken collection.

## Resource Privacy / 资源隐私

A Public Resource may enter a configured cloud provider only through Knowledge Processing, while a Private Resource and its derived content remain local. A Resource cannot change from Public to Private during a running processing Attempt because remote disclosure may already have begun.

## Resource Classification / 资源分类

The complete Category and Tag sets attached to a Resource, with independent AI or Manual provenance for each set. Saving a set manually, including an empty set, prevents later Knowledge Processing from changing that set.

## Resource Processing Handoff / 资源处理交接

The post-commit request from Resource Library Lifecycle to Knowledge Processing for an eligible Resource. A failed or deferred handoff never undoes the durable Resource change, and agent-added Resources are not handed off before human confirmation.

## Resource Snapshot / 资源快照

Fetched source material retained for a Resource. A successful Snapshot can be reused by later Knowledge Processing Attempts even when organizing the Resource fails.

A successful fetch makes Resource Health healthy and clears eligible source-failure history even when later AI organization fails. Confirmed permanent source failures make it broken immediately; three consecutive eligible non-transient source failures also make it broken. Timeouts, rate limits, and temporary network failures do not contribute to that threshold.

## Knowledge Processing Task / 知识处理任务

One user-intended background outcome that turns saved or fetched material into useful knowledge, such as completing a Resource description or summarizing and translating an Article. Closing its page does not abandon the outcome.

## Task Stage / 任务阶段

The current meaningful step of a Knowledge Processing Task, such as fetching source material, organizing a Resource, summarizing an Article, or testing an AI connection. Stages occur in the order required by the task kind.

## Task Attempt / 任务尝试

One execution of a Knowledge Processing Task. Retrying creates a new Attempt and preserves earlier failures and technical details instead of overwriting them.

User-facing failure information states the incomplete outcome, durability, and retryability separately from bounded technical details. Credentials, private notes, full captured content, and unbounded provider output are never retained as technical details.

## Desktop Interaction / 桌面交互

**Route / 主页面**:
The one mutually exclusive workspace the desktop is currently showing, including the article collection that supplies an Article workspace. Only a stable Route is restored after restart.
_Avoid_: Page mode, content mode

**Modal / 阻断任务**:
The one temporary task that blocks interaction with the current Route and owns all of its draft, confirmation, error, and request identity until it is completed or discarded.
_Avoid_: Popup, dialog flag

**Panel / 页面编辑区**:
A non-blocking editor owned by the current Route, such as the Resource editor on the Resource Route. A Panel cannot survive leaving its owning Route.
_Avoid_: Modal, secondary window

**Popover / 锚定操作层**:
A short-lived set of actions attached to an article selection or another on-screen anchor. A Popover has no recoverable draft and closes when its anchor or Route changes.
_Avoid_: Modal, Panel

**Notice / 操作反馈**:
A short-lived user-facing result of an interaction that does not block the Route or retain a draft.
_Avoid_: Error dialog, Modal

## Feed Subscription / RSS 订阅

A durable user intent to follow one RSS or Atom Feed. It owns the Feed URL, enabled state, and optional per-Feed refresh interval. Adding the same normalized HTTP(S) URL is idempotent and expresses a new refresh intent; failure of that refresh does not remove the Subscription. Disabling a Subscription is reversible. Deleting it permanently removes its Feed and all locally stored Articles from that Feed.

## RSS Refresh / RSS 刷新

**RSS Refresh Run / RSS 刷新运行**:
One session-bound aggregate attempt to refresh a determined set of subscribed Feeds, triggered by the due schedule or an explicit user request. A Run is not restored after process exit; each Feed's articles, next refresh time, and latest failure remain durable independently.
_Avoid_: Task, job, update command

## Local Data Maintenance / 本地资料维护

**Data Maintenance Window / 资料维护窗口**:
A user-initiated period when local reading data is temporarily read-only so an exclusive maintenance operation can run safely. Reading may continue, but new changes are rejected rather than queued.
_Avoid_: Program freeze, task cancellation, maintenance flag

**Data Maintenance Run / 资料维护运行**:
One exclusive execution of a database restore or compaction, observed through meaningful stages from waiting for writers through validation and resumption. At most one Run may be active for a local library across all cooperating processes.
_Avoid_: Dialog, generic background task, database command
