# Library Search Top 5 验收基线

更新时间：2026-08-25

## 数据集

- 固定数据：`tests/fixtures/library-search-regression.json`
- 公开主身份：40 个，其中 Resource 25 个、Article 15 个
- 查询：25 条，覆盖工具、素材、文档、文章、架构、数据库、隐私和后台工作流
- 隐私、归档、失效资源不进入公开 fixture，使用独立确定性测试验证

## 验收规则

每条查询使用 ADR-0008 的 Library Search，取前 5 条结果。人工认可 URL 位于 Top 5 即召回成功。Recall@5 必须为 100%；MRR 只记录趋势，暂不作为发布门槛。

复现命令：

```powershell
cargo test mixed_library_regression -- --nocapture
```

当前结果：25/25 查询通过 Recall@5，MRR = 1.000。

## 性能基线

本地性能夹具包含 1,000 个 Resources、10,000 个 Articles 和 2,000 条 Excerpts。它记录 30 次混合查询的 P50/P95，并强制每次查询低于两秒。

复现命令：

```powershell
cargo test benchmark_records_p50_p95 -- --ignored --nocapture
```

性能数字依赖机器，只用于同一环境的趋势比较；CI 的硬门槛仍是每次查询不得超过两秒。

2026-08-25 本机 debug 测试结果：P50 = 50.7883 ms，P95 = 63.9456 ms，30/30 查询低于两秒。

## 维护约束

新增查询时必须先确认真实使用意图和认可结果，再加入 fixture。不能通过删除困难查询或扩大认可列表掩盖回归。私密 URL、正文、备注和凭据不得进入仓库。
