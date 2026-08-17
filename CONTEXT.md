# Domain Context

## Article Bookmark / 文章收藏

An article-level saved state for durable reference. It is independent of read state, archive state, excerpts, and the read-later queue.

## Read Later / 稍后读

A temporary queue state that says the user intends to return to an article. Adding an article does not mark it unread, bookmark it, or prevent archival.

## Tag / 标签

A user-defined normalized label that can be attached to multiple articles. Tag names are unique without regard to letter case.

## Stable Excerpt Anchor / 稳定摘录锚点

The selected quote plus nearby prefix and suffix context and its last known character offsets. Exact offsets are preferred; context disambiguates repeated quotes after the article body changes.

## Batch Article Action / 批量文章操作

One explicit state change applied atomically to a user-selected set of articles. Batch actions set a target state and never infer per-row toggles.

## Search History / 搜索历史

A deduplicated record of completed non-empty library searches, ordered by most recent use and carrying use count and last result count.
