# 拾阅

拾阅是一款面向 Windows 的桌面 RSS 阅读器，强调干净的排版、稳定的图片加载，以及阅读过程中自然的摘录与归档体验。

## 功能

- 三栏式订阅、文章列表与正文阅读界面
- HTML/RSS 正文清洗与中文段落排版
- WebP、PNG、JPEG、GIF 图片显示，包含懒加载、并发限制和失败自动重试
- 跨段落自由选取文字，并复制、收藏或记录想法
- 集中的“摘录与想法”资料库
- 文章归档；归档后刷新订阅源也不会重新出现
- 已读、未读、收藏、失败退避和后台定时更新
- 关闭窗口后驻留系统托盘

## 下载

前往 [Releases](https://github.com/idkwhatimdoing62/shiyue-rss/releases) 下载最新的 Windows 便携包。解压后双击 `shiyue.exe` 即可运行；`shiyue-cli.exe` 仅在需要终端管理订阅时使用。

> Windows SmartScreen 可能提示“未知发布者”，这是因为当前发行文件尚未购买代码签名证书。可核对 Release 中提供的 SHA-256 后再运行。

## 数据位置

为了兼容早期测试版，拾阅继续使用原有的 `rrss` 数据目录：

- 数据库与日志：`%LOCALAPPDATA%\rrss\data\`
- 配置：`%APPDATA%\rrss\config\`

升级或更换新版程序不会清除已有订阅、归档和摘录。建议定期备份 `rrss.db`。

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
```

## 隐私

订阅内容、阅读状态、归档、摘录与想法均保存在本机 SQLite 数据库中。程序只会访问你添加的订阅源、文章图片和打开原文时所选择的网页。

## 字体与许可证

项目内嵌字体仅用于改善中英文阅读显示，其许可证文本位于 `assets/`。拾阅源代码采用 [MIT License](LICENSE) 发布。
