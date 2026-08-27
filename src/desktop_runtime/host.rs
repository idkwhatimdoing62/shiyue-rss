//! Native desktop host Adapter and its deterministic state machine.

use anyhow::Result;
use eframe::egui::{self, ViewportCommand};
use std::sync::mpsc::{self, Receiver};
use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostEvent {
    ToggleVisibility,
    RefreshAllFeeds,
    Quit,
    CloseRequested,
    FocusChanged(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum HostEffect {
    ShowAndFocus,
    Hide,
    CancelClose,
    Close,
    RefreshAllFeeds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HostPhase {
    Visible,
    Hidden,
    Exiting,
}

pub(super) struct HostState {
    phase: HostPhase,
    focused: bool,
    tray_available: bool,
}

impl HostState {
    pub(super) fn new(tray_available: bool) -> Self {
        Self {
            phase: HostPhase::Visible,
            focused: true,
            tray_available,
        }
    }

    pub(super) fn focused(&self) -> bool {
        self.focused && self.phase == HostPhase::Visible
    }

    pub(super) fn hidden(&self) -> bool {
        self.phase == HostPhase::Hidden
    }

    pub(super) fn reduce(&mut self, event: HostEvent) -> Vec<HostEffect> {
        if self.phase == HostPhase::Exiting {
            return Vec::new();
        }
        match event {
            HostEvent::FocusChanged(focused) => {
                self.focused = focused;
                Vec::new()
            }
            HostEvent::RefreshAllFeeds => vec![HostEffect::RefreshAllFeeds],
            HostEvent::ToggleVisibility if self.tray_available => {
                if self.phase == HostPhase::Visible {
                    self.phase = HostPhase::Hidden;
                    self.focused = false;
                    vec![HostEffect::Hide]
                } else {
                    self.phase = HostPhase::Visible;
                    self.focused = true;
                    vec![HostEffect::ShowAndFocus]
                }
            }
            HostEvent::ToggleVisibility => Vec::new(),
            HostEvent::CloseRequested if self.tray_available => {
                self.phase = HostPhase::Hidden;
                self.focused = false;
                vec![HostEffect::CancelClose, HostEffect::Hide]
            }
            HostEvent::CloseRequested => {
                self.phase = HostPhase::Exiting;
                Vec::new()
            }
            HostEvent::Quit => {
                self.phase = HostPhase::Exiting;
                vec![HostEffect::Close]
            }
        }
    }
}

pub(super) struct NativeTray {
    _tray: TrayIcon,
    toggle: MenuId,
    refresh: MenuId,
    quit: MenuId,
    events: Receiver<MenuEvent>,
}

impl NativeTray {
    pub(super) fn build(ctx: &egui::Context) -> Result<Self> {
        let menu = Menu::new();
        let toggle = MenuItem::new("显示 / 隐藏", true, None);
        let refresh = MenuItem::new("抓取一次", true, None);
        let quit = MenuItem::new("退出", true, None);
        menu.append(&toggle)?;
        menu.append(&refresh)?;
        menu.append(&quit)?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("拾阅")
            .with_icon(make_icon())
            .build()?;
        let (sender, events) = mpsc::channel();
        let repaint = ctx.clone();
        MenuEvent::set_event_handler(Some(move |event| {
            let _ = sender.send(event);
            repaint.request_repaint();
        }));
        Ok(Self {
            _tray: tray,
            toggle: toggle.id().clone(),
            refresh: refresh.id().clone(),
            quit: quit.id().clone(),
            events,
        })
    }

    pub(super) fn drain(&self) -> Vec<HostEvent> {
        self.events
            .try_iter()
            .filter_map(|event| {
                if event.id == self.toggle {
                    Some(HostEvent::ToggleVisibility)
                } else if event.id == self.refresh {
                    Some(HostEvent::RefreshAllFeeds)
                } else if event.id == self.quit {
                    Some(HostEvent::Quit)
                } else {
                    None
                }
            })
            .collect()
    }
}

fn make_icon() -> Icon {
    let (width, height) = (32_u32, 32_u32);
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);
    for _ in 0..(width * height) {
        rgba.extend_from_slice(&[0xE9, 0x5A, 0x2B, 0xFF]);
    }
    Icon::from_rgba(rgba, width, height).expect("生成托盘图标失败")
}

pub(super) fn apply_effect(ctx: &egui::Context, effect: HostEffect) {
    match effect {
        HostEffect::ShowAndFocus => {
            ctx.send_viewport_cmd(ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(ViewportCommand::Focus);
        }
        HostEffect::Hide => ctx.send_viewport_cmd(ViewportCommand::Visible(false)),
        HostEffect::CancelClose => ctx.send_viewport_cmd(ViewportCommand::CancelClose),
        HostEffect::Close => ctx.send_viewport_cmd(ViewportCommand::Close),
        HostEffect::RefreshAllFeeds => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_hides_with_tray_but_exits_without_tray() {
        let mut with_tray = HostState::new(true);
        assert_eq!(
            with_tray.reduce(HostEvent::CloseRequested),
            vec![HostEffect::CancelClose, HostEffect::Hide]
        );
        assert!(with_tray.hidden());

        let mut without_tray = HostState::new(false);
        assert!(without_tray.reduce(HostEvent::CloseRequested).is_empty());
        assert!(without_tray.reduce(HostEvent::RefreshAllFeeds).is_empty());
    }

    #[test]
    fn exiting_is_irreversible() {
        let mut state = HostState::new(true);
        assert_eq!(state.reduce(HostEvent::Quit), vec![HostEffect::Close]);
        assert!(state.reduce(HostEvent::ToggleVisibility).is_empty());
        assert!(state.reduce(HostEvent::RefreshAllFeeds).is_empty());
    }

    #[test]
    fn tray_refresh_emits_exactly_one_intent() {
        let mut state = HostState::new(true);
        assert_eq!(
            state.reduce(HostEvent::RefreshAllFeeds),
            vec![HostEffect::RefreshAllFeeds]
        );
    }
}
