# 资源搜索 Top 5 验收基线

更新时间：2026-08-19

## 数据集

- 资源：20 个真实公开网站，覆盖图标、图片、设计、文档、Rust、代码仓库和灵感网站。
- 查询：10 条来自当前使用场景的中文/中英混合查询。
- 人工认可：每条查询在 `accepted` 位置记录至少一个用户认可 URL。
- 固定数据：`tests/fixtures/resource-regression.json`。

## 验收规则

每条查询取资源搜索前 5 条；只要人工认可 URL 位于 Top 5，即判定通过。测试同时锁定资源数为 20、查询数为 10，避免数据集被无意缩小后仍显示通过。

复现命令：

```powershell
cargo test --lib resource::tests::real_resource_regression_queries_have_an_accepted_top_five_result --locked -- --exact --nocapture
```

## v0.5.0 后结构化基线结果

| 查询 | 人工认可结果 | 排名 |
|---|---|---:|
| App icon | Koboyo Icons | 1 |
| 商用 SVG 图标 | SVG Repo | 1 |
| 开源 SVG 图标 | Heroicons | 2 |
| 在线图片编辑 | Photopea | 1 |
| 图片压缩 | Squoosh | 1 |
| 架构图 | Excalidraw | 1 |
| 设计配色 | Coolors | 1 |
| Rust crate 文档 | docs.rs | 1 |
| Rust GUI | egui | 1 |
| 设计灵感网站 | Awwwards | 1 |

结论：10/10 查询通过 Top 5 门槛；9 条 Top 1，1 条 Top 2。

## 解释边界

当前 fixture 使用真实 URL 和真实查询，但用途描述是人工整理的稳定文本。因此它验证搜索、排序和 JSON 输出回归，不证明任意网页经 AI 自动补全后都能达到同等召回率。

后续每次模型提示词或解析规则变化，应从本机真实资源库匿名抽样，人工检查 AI 补全字段，再把经确认且可公开的案例加入 fixture。私密 URL、正文和备注不得进入仓库。
