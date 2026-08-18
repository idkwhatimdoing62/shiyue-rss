# 拾阅

拾阅是一款面向 Windows 的桌面 RSS 阅读器，强调干净的排版、稳定的图片加载，以及阅读过程中自然的摘录与归档体验。

## 功能

- 独立的网站资源库：收藏工具站、设计素材、具体页面，并在 GUI 中完成搜索、编辑、确认、归档和整理
- 统一搜索网站资源与精选文章；`shiyue-cli resource` 为本机 AI 提供稳定的 JSON 查询接口
- 可选 DeepSeek/OpenAI-compatible 自动整理用途、分类和标签；私密资源不会发送给云端模型
- 三栏式订阅、文章列表与正文阅读界面
- HTML/RSS 正文使用 HTML5 DOM 容错清洗并完成中文段落排版；无标准 `article` 的正文与畸形 HTML 也有 fixture 回归
- 支持图注、定义列表、复杂表格、代码语言、脚注以及 MathJax 公式排版
- WebP、PNG、JPEG、GIF 图片显示，包含懒加载、并发限制和失败自动重试
- 已成功加载的远程图片写入本地 SHA-256 内容寻址缓存；相同内容只保存一份，之后可离线阅读
- 跨段落自由选取文字，并复制、收藏或记录想法
- 集中的“摘录与想法”资料库
- 摘录保存前后文稳定锚点；网页正文更新后仍会重新定位原文位置
- 统一“文章收藏”库：订阅文章可在正文中一键收藏，也可粘贴网址或 HTML 保存网页正文快照
- FTS5 全文搜索、BM25 相关性排序、命中高亮和搜索历史；中文短词自动兼容精确匹配
- 文章标签与“稍后读”队列
- 文章列表批量勾选、批量收藏、批量加入稍后读和批量归档
- 网页抓取自动识别正文、标题及中文编码，支持包含多张内容卡片的指南/索引页；保存后即使原网页失效也能继续阅读文字内容
- 重复保存同一网址会生成独立快照，不会覆盖旧正文或已有摘录
- 文章归档；归档后刷新订阅源也不会重新出现
- 已读、未读、收藏、失败退避和后台定时更新
- 关闭窗口后驻留系统托盘
- 左栏“资料库管理”显示数据库、图片缓存、备份和日志占用，并提供完整性检查、数据库压缩、备份、恢复与清理
- 数据库恢复前自动创建安全副本；备份可选使用当前 Windows 用户凭据加密

## 下载

前往 [Releases](https://github.com/idkwhatimdoing62/shiyue-rss/releases) 下载最新的 Windows 便携包。解压后双击 `shiyue.exe` 即可运行；`shiyue-cli.exe` 仅在需要终端管理订阅时使用。

> Windows SmartScreen 可能提示“未知发布者”，这是因为当前发行文件尚未购买代码签名证书。可核对 Release 中提供的 SHA-256 后再运行。

## 数据位置

为了兼容早期测试版，拾阅继续使用原有的 `rrss` 数据目录：

- 数据库与日志：`%LOCALAPPDATA%\rrss\data\`
- 图片内容寻址缓存：`%LOCALAPPDATA%\rrss\data\image-cache\`
- 数据库备份：`%LOCALAPPDATA%\rrss\data\backups\`
- 配置：`%APPDATA%\rrss\config\`

升级或更换新版程序不会清除已有订阅、网页收藏、归档、标签、稍后读和摘录。可从左栏“资料库管理”创建普通备份或“Windows 用户加密备份”，并在恢复前自动保留当前数据库的安全副本。

图片缓存按内容去重，默认超过 1 GB 时按最近使用时间自动淘汰；可手动收缩到 512 MB 或清空。备份默认保留最近 10 份。清理图片不会删除文章、摘录或想法，只会使相关图片在下次阅读时重新下载。

## 从源码构建

需要稳定版 Rust 工具链和 Windows 10/11：

```powershell
git clone https://github.com/idkwhatimdoing62/shiyue-rss.git
cd shiyue-rss
cargo test
cargo build --release
```

构建产物：

- `target\release\shiyue.exe`：无命令行黑框的桌面阅读器
- `target\release\shiyue-cli.exe`：保留终端输出的订阅管理工具

## 命令行

双击 `shiyue.exe` 打开桌面阅读器。需要脚本或终端管理订阅时，使用同一发行包内的 `shiyue-cli.exe`：

```text
shiyue-cli add <url>
shiyue-cli rm <id|url>
shiyue-cli list
shiyue-cli update
shiyue-cli set-interval <id> <30s|5m|6h|2d>
shiyue-cli enable <id>
shiyue-cli disable <id>
shiyue-cli resource search <query> --json
shiyue-cli resource get <id> --json
shiyue-cli resource recent --json
shiyue-cli resource pending --json
shiyue-cli resource add <url> --json
shiyue-cli resource retry <id> --json
```

## 隐私

订阅内容、网页正文快照、阅读状态、归档、摘录与想法均保存在本机 SQLite 数据库中。程序只会访问你添加的订阅源、主动收藏的网页、文章图片和打开原文时所选择的网页；网页及图片下载会拒绝本机和内网地址。远程图片首次成功加载后会进入本地缓存，之后可离线显示。

实时使用的 `rrss.db` 仍是普通 SQLite 文件，不宣称数据库透明加密。“Windows 用户加密备份”使用系统 DPAPI 保护备份内容，通常只能由创建备份的同一 Windows 用户账户解密，适合复制到移动盘或同步盘；若账户凭据不可恢复，该备份也可能无法解密。

## 字体与许可证

项目内嵌字体仅用于改善中英文阅读显示，其许可证文本位于 `assets/`。拾阅源代码采用 [MIT License](LICENSE) 发布。
