# HTML5 DOM 与 Readability 解析器评估

## 结论

拾阅不把现有正文管线整体替换为 Readability。第一阶段迁移已经进入生产路径：

1. RSS 片段继续直接进入语义块解析，不运行 Readability。
2. 完整网页快照使用 `scraper/html5ever` 修复 DOM，并依次选择 `main`、`body` 和单篇 `article`。
3. 只有完整页作用域中恰好存在一篇 `article` 时，才运行 `dom_smoothie` 候选路径；零篇或多篇文章按专题/索引页保留整个作用域。
4. Readability 候选必须通过正文纯度和语义完整度门禁，否则自动回退 HTML5 DOM 结果。
5. 拾阅继续使用自己的语义块模型和渲染器，避免 Readability 的纯文本输出丢失表格、图片链接、代码语言、公式和列表层级。

## 为什么不直接替换

`scraper` 建立在 Servo 的 `html5ever` 和选择器实现之上，适合承担浏览器级 HTML5 容错解析与 DOM 查询。它不负责判断哪一块是正文，因此不会擅自丢弃专题页卡片或 RSS 已提供的正文片段。

`dom_smoothie` 是 Mozilla Readability 的 Rust 近似实现，适合从包含导航、推荐和页脚的完整网页中选择主文章。它的目标是“找出一篇主要文章”，而拾阅还要保留 Martin Fowler Architecture 这类索引页的多张卡片。它也明确说明格式化文本可能把相邻词连接，并且表格文本输出不等价于表格结构。

因此，两者不是互斥替代关系：HTML5 DOM 适合成为底层语法层；Readability 适合成为完整网页抓取时的可选内容选择器。

## 已完成的 DOM 迁移

第二阶段继续保持 `content_blocks`、语义 Block 和 GUI 渲染接口不变，只替换它们之前的 HTML 准备层：

- `strip_ignored_elements` 不再计算字符串标签范围，而是在 `scraper/html5ever` 建立的树上删除 `head`、脚本、样式、导航和页头页尾等节点；`script[type^="math/tex"]` 作为公式源单独保留；
- `og:title`、`title`、`h1` 和 `base[href]` 均通过 DOM 选择器及属性读取，不再依赖手写标签、空格和引号容错；
- `picture` 在 DOM 中选择一张响应式图片，删除其余 `source/img` 候选；存在回退图片时保留其 `alt`，并把选中的高分辨率地址写回；
- 新增 Rust Blog 无标准 `article` 的完整页 fixture，以及 Simon Willison 风格的畸形段落、响应式图片、公式和定义列表组合 fixture；
- 零篇 `article` 的完整页继续保守地使用 `main/body` DOM 作用域，不自动扩大 Readability 范围；RSS 片段入口保持不变。

## 结构验证

`responsive-semantics.html` 同时覆盖以下结构：

- `picture/source/srcset` 与 `img` 回退；
- `figure/figcaption`；
- 一个术语对应多个说明的 `dl/dt/dd`；
- `rowspan` 与 `colspan` 同时出现的表格。

测试先用 `scraper` 验证 HTML5 DOM 能识别这些节点，再让组合 fixture 通过生产 Readability 门禁，最后用拾阅语义块断言图片只出现一次、图注位置不丢失、定义关系完整、逻辑列和跨度保持。

回归还覆盖四类隔离场景：畸形 HTML 的段落和块级标签由 HTML5 DOM 修复；无 `article` 的单篇正文保留整个 `main`；包含多篇 `article` 的 RSS 片段不会竞争“主文章”；Readability 若删除 `math/tex` 公式，即使其余文字仍可读也必须回退。

## 生产门禁

Readability 候选只有同时满足以下条件才会被接受：

- 输入来自完整网页快照，并且 HTML5 DOM 作用域中恰好存在一篇 `article`；
- 候选至少保留 HTML5 DOM 作用域的全部规范化正文文字；
- 候选新增文字不得超过正文长度的 10%，短正文最多容许 8 个非空白字符的差异；
- 图片、`figure/figcaption`、`dl/dt/dd`、表格单元格及跨度、列表、链接、代码和公式节点数量均不得减少；
- 解析失败或任一门禁失败时无条件回退，不保存半成品候选。

索引页和 RSS 片段通过入口分流保证不进入 Readability，而不是在 Readability 运行后尝试恢复内容。

## 下一步评估

1. 继续收集无标准 `article` 的单篇正文 fixture，先形成可解释的正文纯度门禁，再决定是否允许这类页面进入 Readability 候选路径；
2. 给新增的无 `article` 和畸形页面增加视觉快照，补充当前结构断言；
3. 逐步收拢剩余流式标签解析的内部边界，但不改变 `content_blocks` 和语义 Block 接口。

## 参考实现

- [`scraper` 官方文档](https://docs.rs/scraper/0.27.0/scraper/)
- [`dom_smoothie` 官方文档](https://docs.rs/dom_smoothie/0.18.0/dom_smoothie/)
- [Mozilla Readability 源码仓库](https://github.com/mozilla/readability)
- [MathJax SVG 输出文档](https://docs.mathjax.org/en/stable/output/svg.html)
