// 正式 Windows 版本使用 GUI 子系统：双击启动时不创建控制台窗口。
// debug 构建仍保留控制台，便于开发期间查看 panic 与日志。
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

fn main() -> anyhow::Result<()> {
    rrss::run_gui()
}
