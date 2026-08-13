//! 桌面通知（ADR-7 N1）。失败只记日志，不影响 daemon。
//! ponytail: Windows toast 依赖系统的 AppUserModelID，个别环境不弹属正常，日志兜底。

pub fn notify_new(feeds: usize, articles: usize) {
    let body = format!("{feeds} 个源共 {articles} 篇新文章");
    if let Err(e) = notify_rust::Notification::new()
        .summary("拾阅")
        .body(&body)
        .show()
    {
        tracing::warn!("弹通知失败: {e}");
    }
}
