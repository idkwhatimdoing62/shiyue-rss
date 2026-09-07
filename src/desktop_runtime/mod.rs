//! Desktop Runtime & Settings.
//!
//! This deep Module is the sole owner of native window/tray behavior,
//! notification policy, versioned settings persistence, paths, logging and
//! desktop styling. GUI code sees only `DesktopSession` and semantic intents.

mod host;
mod settings;
mod style;

use anyhow::{Context, Result};
use eframe::egui;
use std::fs::OpenOptions;
use std::sync::Mutex;
use std::time::Duration;

use crate::config::{Config, NetworkMode, UI_SCALE_OPTIONS};
use host::{HostEffect, HostEvent, HostState, NativeTray};
use settings::{AtomicTomlSettingsStore, SettingsStore};

pub(crate) use settings::Paths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DesktopIntent {
    RefreshAllFeeds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SettingsChange {
    UiScale(u16),
    NetworkMode(NetworkMode),
}

trait NotificationHost {
    fn notify_new_articles(&self, feeds: usize, articles: usize);
}

struct NativeNotificationHost;

impl NotificationHost for NativeNotificationHost {
    fn notify_new_articles(&self, feeds: usize, articles: usize) {
        let body = format!("{feeds} 个源共 {articles} 篇新文章");
        if let Err(error) = notify_rust::Notification::new()
            .summary("拾阅")
            .body(&body)
            .show()
        {
            tracing::warn!("弹通知失败: {error}");
        }
    }
}

pub(crate) struct DesktopSession {
    settings: Config,
    store: Box<dyn SettingsStore>,
    tray: Option<NativeTray>,
    host: HostState,
    notifications: Box<dyn NotificationHost>,
}

impl DesktopSession {
    fn start(ctx: &egui::Context, paths: &Paths, settings: Config) -> Self {
        let tray = match NativeTray::build(ctx) {
            Ok(tray) => Some(tray),
            Err(error) => {
                tracing::warn!("系统托盘不可用，关闭窗口将直接退出: {error:#}");
                None
            }
        };
        let host = HostState::new(tray.is_some());
        Self {
            settings,
            store: Box::new(AtomicTomlSettingsStore::new(paths.config_file.clone())),
            tray,
            host,
            notifications: Box::new(NativeNotificationHost),
        }
    }

    pub(crate) fn settings(&self) -> &Config {
        &self.settings
    }

    pub(crate) fn ui_scale_options(&self) -> &'static [u16] {
        &UI_SCALE_OPTIONS
    }

    pub(crate) fn apply(&mut self, change: SettingsChange, ctx: &egui::Context) -> Result<()> {
        let mut candidate = self.settings.clone();
        match change {
            SettingsChange::UiScale(percent) => candidate.ui_scale_percent = percent,
            SettingsChange::NetworkMode(mode) => candidate.network_mode = mode,
        }
        candidate.validate()?;
        // Persistence comes first: a failed write leaves the active snapshot
        // and all immediate desktop effects unchanged.
        self.store.save(&candidate)?;
        self.settings = candidate;
        ctx.set_zoom_factor(self.settings.ui_scale_factor());
        Ok(())
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> Vec<DesktopIntent> {
        let focused = ctx.input(|input| input.viewport().focused.unwrap_or(true));
        let mut events = vec![HostEvent::FocusChanged(focused)];
        if let Some(tray) = &self.tray {
            events.extend(tray.drain());
        }
        if ctx.input(|input| input.viewport().close_requested()) {
            events.push(HostEvent::CloseRequested);
        }

        let mut intents = Vec::new();
        for event in events {
            for effect in self.host.reduce(event) {
                if effect == HostEffect::RefreshAllFeeds {
                    intents.push(DesktopIntent::RefreshAllFeeds);
                } else {
                    host::apply_effect(ctx, effect);
                }
            }
        }
        // Hidden windows have no normal frame traffic; this is a bounded
        // fallback for platforms that fail to wake on a native tray event.
        if self.host.hidden() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        intents
    }

    pub(crate) fn notify_new_articles(&self, feeds: usize, articles: usize) {
        if should_notify(&self.settings, &self.host, articles) {
            self.notifications.notify_new_articles(feeds, articles);
        }
    }
}

fn should_notify(settings: &Config, host: &HostState, articles: usize) -> bool {
    articles > 0 && settings.notifications && !host.focused()
}

pub(crate) struct CommandEnvironment {
    pub paths: Paths,
    pub settings: Config,
}

pub(crate) fn command_environment() -> Result<CommandEnvironment> {
    let paths = Paths::resolve()?;
    init_logging(&paths)?;
    let settings = AtomicTomlSettingsStore::new(paths.config_file.clone()).load()?;
    Ok(CommandEnvironment { paths, settings })
}

pub(crate) fn launch() -> Result<()> {
    let environment = command_environment()?;
    let paths = environment.paths;
    let settings = environment.settings;
    let native = style::native_options();
    eframe::run_native(
        style::WINDOW_TITLE,
        native,
        Box::new(move |creation| {
            style::install(&creation.egui_ctx);
            egui_extras::install_image_loaders(&creation.egui_ctx);
            creation
                .egui_ctx
                .set_zoom_factor(settings.ui_scale_factor());
            let session = DesktopSession::start(&creation.egui_ctx, &paths, settings.clone());
            crate::gui::GuiApp::new(creation, &paths, settings.clone(), session)
                .map(|app| Box::new(app) as Box<dyn eframe::App>)
                .map_err(Into::into)
        }),
    )
    .map_err(|error| anyhow::anyhow!("egui 启动失败: {error}"))
}

fn init_logging(paths: &Paths) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)
        .with_context(|| format!("无法打开日志文件：{}", paths.log_file.display()))?;
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(file))
        .with_ansi(false)
        .try_init();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct RecordingNotificationHost {
        calls: RefCell<Vec<(usize, usize)>>,
    }

    impl NotificationHost for RecordingNotificationHost {
        fn notify_new_articles(&self, feeds: usize, articles: usize) {
            self.calls.borrow_mut().push((feeds, articles));
        }
    }

    #[test]
    fn focused_or_disabled_notifications_do_not_reach_the_adapter() {
        let mut host = HostState::new(false);
        host.reduce(HostEvent::FocusChanged(true));
        assert!(!should_notify(&Config::default(), &host, 2));

        host.reduce(HostEvent::FocusChanged(false));
        assert!(should_notify(&Config::default(), &host, 2));

        let settings = Config {
            notifications: false,
            ..Config::default()
        };
        assert!(!should_notify(&settings, &host, 2));

        let recorder = RecordingNotificationHost::default();
        if should_notify(&settings, &host, 2) {
            recorder.notify_new_articles(1, 2);
        }
        assert!(recorder.calls.borrow().is_empty());
    }

    #[test]
    fn gui_does_not_reach_behind_the_desktop_runtime_interface() {
        let gui = include_str!("../gui.rs");
        let resource_feature = include_str!("../gui/resource_feature.rs");
        for forbidden in [
            "config_file:",
            "TrayIcon",
            "MenuEvent",
            "notify_rust",
            "ViewportCommand",
            "hidden:",
            "quitting:",
        ] {
            for source in [gui, resource_feature] {
                assert!(
                    !source.contains(forbidden),
                    "GUI contains forbidden desktop implementation detail: {forbidden}"
                );
            }
        }
    }
}
