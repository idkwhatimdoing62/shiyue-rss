# Domain Context

## Article Bookmark / 文章收藏

An article-level saved state for durable reference. It is independent of read state, archive state, excerpts, and the read-later queue.

## Read Later / 稍后读

A temporary queue state that says the user intends to return to an article. Adding an article does not mark it unread, bookmark it, or prevent archival.

## Tag / 标签

A user-defined normalized label that can be attached to multiple articles. Editing an Article's Tags replaces its complete Tag set; empty names are discarded and names are unique without regard to letter case.

## Stable Excerpt Anchor / 稳定摘录锚点

The selected quote plus nearby prefix and suffix context and its last known character offsets. Exact offsets are preferred; context disambiguates repeated quotes after the article body changes.

## Excerpt / 摘录

A durable passage selected from one Article, identified within that Article by its Stable Excerpt Anchor. Repeating an action on the same Article and Anchor enriches the same Excerpt rather than creating a duplicate; identical text selected at different anchored locations remains distinct.

An Excerpt is independent of bookmark, read-later, archive, read, and Tag states. It remains available when its Article is archived or no longer bookmarked, and is removed only when explicitly deleted or when its Article is permanently deleted.

Creating a Thought for a newly selected passage also creates and retains its Excerpt. An Excerpt whose Anchor no longer resolves after the Article body changes retains its quoted passage and Thought and remains available to Library Search; only navigation to the current body is unavailable.

Deleting an Excerpt also deletes its Thought. Deleting only the Thought preserves the Excerpt. Excerpts in the complete saved collection are ordered by their latest meaningful change, including a change to their Thought, with stable identity resolving equal times.

## Thought / 想法

The single optional personal note attached to an Excerpt. A Thought has no identity independent of its Excerpt and can be edited or removed without replacing the quoted passage or its Stable Excerpt Anchor. Writing again replaces the current Thought rather than appending or creating another one. Only its current content and latest change time are retained.

## Excerpt Resolution / 摘录定位状态

Whether an Excerpt's Stable Excerpt Anchor can identify its passage in the current Article body. A resolved Excerpt can navigate to that passage; an unresolved Excerpt remains durable and searchable but cannot promise automatic navigation.

## Legacy Excerpt / 历史摘录

An Excerpt preserved from data created before same-Anchor reuse became authoritative. Exact-Anchor duplicates remain distinct when automatically combining them could overwrite or rewrite a person's Thoughts. New lifecycle changes do not create additional duplicates. An Excerpt & Thought Projection distinguishes Legacy Excerpts so a person can understand and individually maintain them without being forced to merge them.

## Excerpt & Thought Projection / 摘录与想法投影

An authoritative saved-library view for either one Article or the complete Excerpt and Thought collection. It contains the applicable Excerpts, their optional Thoughts, Resolution and managed-or-Legacy identity kind, collection counts, and the material required to navigate to their source Articles. The primary saved-collection count counts Excerpts; a Thought count is supplementary and never counts the same Excerpt twice.

## Excerpt & Thought Lifecycle / 摘录与想法生命周期

The authoritative lifecycle for creating, enriching, editing, querying, and deleting Excerpts and Thoughts, including their saved-library counts, Article navigation material, and Library Search visibility. It owns durable Excerpt and Thought meaning but not text-selection gestures, Popovers, Modals, Notices, or other Desktop Interaction state.

A successful durable change and its Library Search visibility take effect together and return an Excerpt & Thought Projection from the same consistent state. New changes are unavailable during a Data Maintenance Window. The lifecycle is reusable by desktop and command-line adapters, while adding new command-line interactions is not itself part of the lifecycle.

Lifecycle changes are short, immediate operations rather than queued work. Failures are returned to the initiator and are never retried automatically.

Lifecycle changes are idempotent where the requested state already exists. An unchanged Excerpt or Thought does not receive a new latest-change time and does not move in the saved collection order.

## Batch Article Action / 批量文章操作

One explicit state change applied atomically to a user-selected set of articles. Batch actions set a target state, never infer per-row toggles, and change no Article when any selected Article no longer exists.

## Article Archive / 文章归档

An independent state that temporarily removes an Article from normal Feed, Article Bookmark, and Read Later collections without clearing its bookmark, read-later, read, or Tag states. Restoring the Article makes those retained states visible again.

## Web Clipping / 网页收藏

An immutable locally saved Article captured from a public HTTP(S) page or pasted HTML. Repeated captures remain distinct, and URL captures retain both the user-supplied source URL and the final resolved URL.
_Avoid_: Resource Snapshot, saved page

## Web Clipping Lifecycle / 网页收藏生命周期

The authoritative lifecycle for capturing and permanently deleting a Web Clipping, returning the affected Article Library Projection after each durable change. Deletion is unavailable during active Article Knowledge Processing and otherwise removes attached material while retaining and detaching any independently curated Resource.

Its external interface issues one non-reusable Capture Lease for the single active Capture. The desktop submits raw capture input, observes lease snapshots, requests cancellation, and adopts terminal projections; it does not interpret the input or sequence fetching, preparation, and persistence. Closing the capture Modal does not discard a Capture that has entered Committing: desktop root state consumes the module's recent terminal snapshot by revision and presents the outcome independently of the closed Modal.

Capture and deletion use the concrete local SQLite database and return the Article Library Projection read in the same transaction as the durable change. The module owns Web Clipping provenance, while public-page fetching is isolated behind a narrow internal seam with production HTTP and deterministic test adapters. It does not expose a generalized repository interface or a persistent task queue.

## Web Clipping Capture / 网页收藏捕获

One session-bound attempt to turn a URL or pasted HTML into a Web Clipping, observed through Fetching, Preparing, Committing, and terminal stages. At most one Capture is active; cancellation or a Data Maintenance Window before Committing guarantees no write, while failures are never resumed or retried automatically.
_Avoid_: Knowledge Processing Task, download job

## Capture Lease / 捕获租约

The non-reusable capability returned for one admitted Web Clipping Capture. It exposes only identity, revisioned snapshots, and cancellation; the normal desktop close path requests cancellation explicitly, while dropping a lease is only a best-effort fallback. Once Committing wins the linearization race, cancellation is too late and the short transaction completes.

## Web Clipping Provenance / 网页收藏来源信息

Capture-owned metadata recording input kind, capture time, and the information required to explain how a Web Clipping was produced. A URL capture records normalized original and final resolved URLs; pasted HTML records its optional base URL. Legacy records preserve only facts that can be recovered without inference, leaving unknown final and base URLs empty.

## Article Library Lifecycle / 文章资料生命周期

The authoritative lifecycle for an Article's bookmark, read-later, archive, read, and Tag states, including Batch Article Actions and their effects on library collections and counts. It does not own Web Clipping intake or deletion, Excerpts and Thoughts, Knowledge Processing, or desktop interaction state.

Its external interface accepts only explicit lifecycle changes and Article Library scopes. A successful change returns an authoritative SQLite projection for the requested scope together with library counts and Feed unread counts; callers do not infer or optimistically patch those values. Route, Modal, Panel, Popover, and Notice remain Desktop Interaction concepts outside this module.

The module uses the concrete local SQLite database and its writer permit directly. SQLite transactions, schema details, Web Clipping fixed membership, Tag normalization, collection visibility, and count derivation remain hidden implementation knowledge rather than a repository interface.

An Article Library Projection is a SQLite-backed consistent snapshot for one Article Library scope. It contains the scoped Articles, their lifecycle states and normalized Tags, the Article Bookmark, Read Later, and Archive counts, and Feed unread counts. A projection for one Article must distinguish a missing Article from an Article with no Tags.

Lifecycle changes are idempotent and use last-successful-commit semantics. A successful change reports whether any target changed and returns the projection read inside the same transaction; a later writer may supersede it. Input, missing-Article, Data Maintenance, and storage failures never authorize callers to patch their existing projection.

## Search History / 搜索历史

A deduplicated record of completed non-empty Library Searches explicitly initiated by a person, ordered by most recent use and carrying use count and last result count. Agent-initiated searches never enter Search History.

## Library Search / 资料库搜索

A user or agent intent to retrieve one relevance-ordered view across Active Resources, Article Bookmarks, Web Clippings, Excerpts, and Thoughts. The complete RSS stream and archived primary material are excluded unless explicitly requested, while Excerpts and Thoughts remain searchable after their Article is archived.
_Avoid_: Resource search, global search, full-text search

## Library Search Scope / 资料库搜索范围

The declared material set for a Library Search: Curated searches the normal saved library, All Articles adds the unarchived RSS stream, and Archive searches archived primary material. A scope changes eligibility, not the meaning of relevance.

## Library Search Result / 资料库搜索结果

One ranked primary Resource or Article identity together with its related saved forms, navigation targets, privacy and health facts, and Search Evidence. An eligible Resource is primary over related Articles; explicit links precede canonical URL grouping, and related saved forms enrich one result instead of consuming separate ranking positions.

## Search Evidence / 搜索依据

A bounded piece of matched material that explains why a Library Search Result was returned and identifies its source field or saved form. Internal retrieval scores are not Search Evidence.

## Library Search Relevance / 资料库搜索相关性

The ordering value led by a result's strongest Search Evidence, with bounded corroboration from additional evidence and limited influence from manual rating and Resource Health. Recency only resolves otherwise comparable results, and duplicate saved forms cannot accumulate unbounded relevance.

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

Route、Modal、Panel、Popover 和 Notice 等用户交互状态。它不拥有原生托盘、窗口进程生命周期、设置持久化或系统通知；这些由 Desktop Runtime & Settings 通过 `DesktopSession` Interface 提供（ADR-0013）。

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

## Desktop Library Projection / 桌面资料投影

The last successfully adopted desktop view of Article, Resource, Excerpt, and Knowledge Processing material supplied by their authoritative lifecycles. It may remain visible while ordinary changes are being refreshed, but it is not durable truth and cannot survive a replaced library generation.

The current Desktop Projection Frame is the desktop presentation authority. GUI state keeps interaction intent only and must reconcile selection, pending navigation, and drafts against the frame; it must not retain a second Article or Excerpt projection, derived metadata maps, or count mirrors. Lifecycle outcomes enter presentation through projection adoption and become visible from a subsequent frame.
_Avoid_: GUI cache, read model truth, local mirror

## Library Projection Revision / 资料投影修订号

A monotonic durable marker for one Article, Resource, or Excerpt projection family. A durable change advances every affected family together so another desktop process can discover which Desktop Library Projections are no longer current.
_Avoid_: event log, updated timestamp, cache version

## Desktop Projection Freshness / 桌面投影新鲜度

The observable availability of one demanded Desktop Library Projection: Loading, Current, Refreshing with a last success, Failed with an optional last success, or unavailable during Data Maintenance. Ordinary refresh may retain the last success, while a replaced library generation never may.
_Avoid_: cache state, spinner state, database status

## Desktop Runtime & Settings / 桌面运行时与设置

The deep Module that owns desktop startup, standard paths, logging, versioned non-secret settings, atomic persistence, native tray/window lifecycle, fonts/style/zoom, focus-aware notification policy, and shutdown direction. GUI receives a `DesktopSession` and semantic intents; CLI receives only a validated command-environment projection. It does not own RSS, Knowledge Processing, maintenance, library, Route, Modal, Panel, Popover, or Notice semantics.

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

**Maintenance Fence / 资料维护栅栏**:
The sole authority by which normal code observes a Data Maintenance Window and commits local-library mutations. A connection fence keeps exclusive maintenance aware of a live database handle; a generation witness prevents results produced before a restore from entering the replacement library; a fenced transaction revalidates maintenance activity and generation immediately before commit. A maintenance-drain transaction is reserved for participant interrupt and lease-release bookkeeping needed to acknowledge a safe point while the old epoch is still locked.
_Avoid_: GUI maintenance flag, raw writer permit, caller-side preflight check

## Versioned Schema Evolution / 版本化资料库演进

The sole authority that creates a new local library schema or advances an existing library through every supported historical version. A caller receives either a library whose schema is fully ready for normal use or an explicit failure; no caller may depend on or continue using a partially evolved schema.

Every previously released schema version remains upgradeable. A library created by a newer unsupported version is rejected without modification; it is never automatically downgraded or repaired by an older application.
_Avoid_: startup patch, best-effort migration, automatic downgrade

**Schema Transition / 资料库版本转换**:
One atomic advance from one supported schema version to its immediate successor. New libraries follow the same ordered Transitions as existing libraries rather than using a separate latest-schema path.

Each completed Transition leaves a valid historical schema. If a later Transition fails, normal library use remains unavailable, the last fully completed version remains identifiable, and a later evolution attempt may continue from it. Automatic evolution runs only inside a Data Maintenance Window after a safety copy has been created.
_Avoid_: partial patch, skipped version, latest-schema shortcut

**Schema Drift / 资料库结构漂移**:
A mismatch between a library's declared schema version and the required structure or derived indexes for that version. Schema Drift is not treated as an older version and is never repaired as a side effect of opening the library; detection leaves the library unchanged and requires an explicit maintenance or recovery decision.
_Avoid_: missing optional index, harmless schema difference, automatic repair opportunity

**Schema Readiness / 资料库结构就绪状态**:
The read-only classification of a local library as uninitialized, ready for normal use, requiring supported evolution, drifted from its declared version, or created by a newer unsupported application. A library with no Shiyue-owned material structure is Uninitialized, while an unversioned library containing historical core material requires evolution from version zero.

A readiness observation never changes the library and may become stale before an evolution attempt acquires exclusive maintenance ownership. Evolution and normal opening must establish current readiness again while holding their applicable maintenance or writer ownership.
_Avoid_: migration progress, repair result, cached open permission

**Schema Evolution Report / 资料库演进报告**:
The bounded result of one Versioned Schema Evolution attempt. It identifies the initial readiness, target version, completed Schema Transitions, final readiness and verification summary; a failure additionally identifies its stage, failed Transition and last complete version without retaining private library content or credentials.
_Avoid_: migration log dump, library content snapshot, provider output
