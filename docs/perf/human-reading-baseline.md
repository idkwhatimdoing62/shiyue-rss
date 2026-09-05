# Human-first 阅读性能回归基准

这组测试不把主观“感觉不卡”当作验收标准，而是固定正文样本和可重复指标。运行：

```text
cargo test --lib human_reading_fixture_keeps_scroll_frame_metrics_bounded
```

当前基准位于 `src/article_document_presentation.rs` 的
`human_reading_fixture_keeps_scroll_frame_metrics_bounded`，覆盖：

- 900 段长文在顶部、中部、底部和回滚位置的渲染；
- 每帧布局段落数不超过 80；
- 单个文本布局不超过 `BODY_GALLEY_MAX_CHARS`；
- 预热后的滚动帧正文呈现耗时低于 3 秒（首帧包含 HTML 准备，不计入滚动预算；这是跨机器捕获阻塞的宽松诊断上限，不等同于目标帧率）；
- 不同滚动位置的正文总高度漂移不超过 1 px。

图片稳定性由 `image_height_change_above_viewport_reports_scroll_anchor_adjustment` 和
`real_scroll_area_keeps_document_height_stable_across_scroll_offsets` 覆盖，验证慢图片加载不会把视口内容向下推移。

密集内联样式排版可单独运行：

```text
cargo test --release --lib dense_inline_ranges_layout_benchmark -- --ignored --nocapture
```

该样本包含 800 个加粗、行内代码和链接区间，并重复生成 40 次布局。2026-09-03
在当前开发机上的同一次优化对比中，区间逐段全量扫描耗时约 `16.36 ms`，改为合并区间和
单调游标后约 `5.77 ms`。测试同时固定链接高于行内代码、行内代码高于加粗的格式优先级。

这组基准是结构性回归门槛。若要优化到“足够流畅”，仍需在目标 Windows 硬件上用性能采样记录实际帧时间，再把实测预算补充到这里；不要仅凭一次手工滚动决定保留优化。
