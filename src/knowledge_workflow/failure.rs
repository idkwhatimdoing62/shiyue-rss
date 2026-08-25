use super::ErrorKind;

pub(super) fn classify_error(error: &anyhow::Error) -> (ErrorKind, &'static str) {
    let detail = format!("{error:#}").to_ascii_lowercase();
    if detail.contains("private") || detail.contains("私密") || detail.contains("not allowed") {
        (ErrorKind::Security, "该内容不能发送给 AI")
    } else if detail.contains("api key")
        || detail.contains("credential")
        || detail.contains("401")
        || detail.contains("未启用")
    {
        (ErrorKind::Authentication, "AI 配置或凭据不可用")
    } else if detail.contains("valid resource json")
        || detail.contains("valid article json")
        || detail.contains("provider output")
        || detail.contains("json object")
        || detail.contains("schema")
    {
        (ErrorKind::ProviderOutput, "AI 返回格式不正确")
    } else if detail.contains("timeout")
        || detail.contains("429")
        || detail.contains("http 5")
        || detail.contains("connection")
        || detail.contains("抓取失败")
    {
        (ErrorKind::Transient, "临时网络或 Provider 错误")
    } else if detail.contains("不存在")
        || detail.contains("not found")
        || detail.contains("缺少")
        || detail.contains("正文")
    {
        (ErrorKind::Input, "没有可处理的内容")
    } else {
        (ErrorKind::Storage, "知识处理结果保存失败")
    }
}

pub(super) fn sanitize_detail(detail: &str) -> String {
    let mut safe = detail
        .lines()
        .filter(|line| {
            let lower = line.to_ascii_lowercase();
            !lower.contains("authorization")
                && !lower.contains("api_key")
                && !lower.contains("api key=")
                && !lower.contains("credential=")
        })
        .collect::<Vec<_>>()
        .join("\n");
    safe = safe
        .split_whitespace()
        .map(|part| {
            if part.starts_with("sk-") {
                "[REDACTED]"
            } else {
                part
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    safe.chars().take(4096).collect()
}
