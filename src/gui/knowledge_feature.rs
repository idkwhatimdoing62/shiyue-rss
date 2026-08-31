//! Private desktop adapter for Knowledge Processing interactions.
//!
//! The workflow owns durable task state, execution, retries, and provider
//! details. This adapter owns the small amount of desktop interaction state
//! needed to submit those intents and translate workflow notices into
//! presentation outcomes. Route, Modal, projection adoption, and Notice
//! publication remain owned by the GUI root.

use std::collections::HashSet;

use eframe::egui;

use crate::desktop_library_projection::{DesktopProjectionFrame, ProjectionFreshness};
use crate::knowledge_workflow::{
    ConnectionState, KnowledgeEngine, KnowledgeNotice, TaskKey, TaskKind as KnowledgeTaskKind,
    TaskStatus as KnowledgeTaskStatus,
};
use crate::resource_enrichment;

#[derive(Debug, Default)]
pub(super) struct KnowledgeFeature {
    api_key_draft: String,
    settings_message: Option<String>,
    connection_state: Option<ConnectionState>,
    watched: HashSet<TaskKey>,
    pending_notices: Vec<TaskKey>,
}

impl KnowledgeFeature {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn api_key_draft_mut(&mut self) -> &mut String {
        &mut self.api_key_draft
    }

    pub(super) fn settings_message(&self) -> Option<&str> {
        self.settings_message.as_deref()
    }

    pub(super) fn connection_state(&self) -> Option<&ConnectionState> {
        self.connection_state.as_ref()
    }

    pub(super) fn connection_busy(&self) -> bool {
        matches!(self.connection_state, Some(ConnectionState::Running))
    }

    pub(super) fn save_api_key(&mut self) {
        match resource_enrichment::save_api_key(&self.api_key_draft) {
            Ok(_) => {
                self.api_key_draft.clear();
                self.settings_message = Some("API Key 已保存到 Windows 凭据管理器".into());
            }
            Err(error) => self.settings_message = Some(error.to_string()),
        }
    }

    pub(super) fn delete_api_key(&mut self) {
        match resource_enrichment::delete_api_key() {
            Ok(_) => self.settings_message = Some("API Key 已删除".into()),
            Err(error) => self.settings_message = Some(error.to_string()),
        }
    }

    pub(super) fn begin_connection_test(&mut self, engine: &KnowledgeEngine, ctx: &egui::Context) {
        match engine.test_connection() {
            Ok(()) => {
                self.connection_state = Some(ConnectionState::Running);
                ctx.request_repaint();
            }
            Err(error) => {
                self.connection_state = Some(ConnectionState::Failed {
                    detail: format!("{error:#}"),
                });
            }
        }
    }

    pub(super) fn request_resource_completion(
        &mut self,
        resource_id: i64,
        engine: &KnowledgeEngine,
        ctx: &egui::Context,
    ) -> anyhow::Result<()> {
        let key = TaskKey::new(KnowledgeTaskKind::ResourceCompletion, resource_id);
        engine.request(key).map(|_| {
            self.watched.insert(key);
            ctx.request_repaint();
        })?;
        Ok(())
    }

    pub(super) fn request_article_summary(
        &mut self,
        article_id: i64,
        engine: &KnowledgeEngine,
        ctx: &egui::Context,
    ) -> anyhow::Result<()> {
        let key = TaskKey::new(KnowledgeTaskKind::ArticleSummary, article_id);
        engine.request(key).map(|_| {
            self.watched.insert(key);
            ctx.request_repaint();
        })?;
        Ok(())
    }

    pub(super) fn watched_keys(&self) -> impl Iterator<Item = TaskKey> + '_ {
        self.watched.iter().copied()
    }

    pub(super) fn receive_updates(
        &mut self,
        engine: &KnowledgeEngine,
        ctx: &egui::Context,
    ) -> Vec<String> {
        let notices = engine.try_notices().collect::<Vec<_>>();
        let received_notice = !notices.is_empty();
        let mut user_notices = Vec::new();
        for notice in notices {
            match notice {
                KnowledgeNotice::Changed(key) => self.pending_notices.push(key),
                KnowledgeNotice::ConnectionChanged(state) => {
                    self.settings_message = Some(match &state {
                        ConnectionState::Running => "正在测试 DeepSeek 连接".to_owned(),
                        ConnectionState::Succeeded(message) => message.clone(),
                        ConnectionState::Failed { detail } => format!("连接失败：{detail}"),
                    });
                    self.connection_state = Some(state);
                }
                KnowledgeNotice::ModuleFault {
                    user_message,
                    technical_detail,
                } => {
                    tracing::warn!("knowledge module: {technical_detail}");
                    user_notices.push(user_message);
                }
            }
        }
        if received_notice {
            ctx.request_repaint();
        } else {
            // The workflow can change on its background executor. Poll at a
            // low idle cadence without forcing a permanent max-rate loop.
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
        user_notices
    }

    pub(super) fn publish_pending_notices(
        &mut self,
        frame: &DesktopProjectionFrame,
    ) -> Vec<String> {
        let keys = std::mem::take(&mut self.pending_notices);
        let mut notices = Vec::new();
        for key in keys.into_iter().collect::<HashSet<_>>() {
            let Some(view) = frame.knowledge(key) else {
                continue;
            };
            if matches!(view.freshness, ProjectionFreshness::Loading) {
                self.pending_notices.push(key);
                continue;
            }
            let terminal = view
                .data
                .as_ref()
                .is_some_and(|snapshot| snapshot.status.is_terminal());
            if let Some(notice) = view
                .data
                .as_ref()
                .and_then(|snapshot| match snapshot.status {
                    KnowledgeTaskStatus::Succeeded => match snapshot.key.kind {
                        KnowledgeTaskKind::ResourceCompletion => {
                            Some("资源抓取和 AI 整理已完成".to_owned())
                        }
                        KnowledgeTaskKind::ArticleSummary => {
                            Some("AI 总结和中文翻译已保存".to_owned())
                        }
                    },
                    KnowledgeTaskStatus::Failed | KnowledgeTaskStatus::Interrupted => {
                        snapshot.user_message.clone()
                    }
                    KnowledgeTaskStatus::Queued | KnowledgeTaskStatus::Running => None,
                })
            {
                notices.push(notice);
            }
            if terminal {
                self.watched.remove(&key);
            }
        }
        notices
    }
}

#[cfg(test)]
mod tests {
    use super::KnowledgeFeature;

    #[test]
    fn starts_without_pending_work_or_connection_state() {
        let feature = KnowledgeFeature::new();
        assert!(!feature.connection_busy());
        assert!(feature.settings_message().is_none());
        assert_eq!(feature.watched_keys().count(), 0);
    }

    #[test]
    fn gui_root_does_not_reimplement_knowledge_interaction() {
        let gui_root = include_str!("../gui.rs");
        for forbidden in [
            "KnowledgeNotice::",
            "KnowledgeEngine::request",
            "fn begin_ai_connection_test",
            "fn begin_article_ai",
            "fn retry_resource_task",
            "resource_enrichment::save_api_key",
            "resource_enrichment::delete_api_key",
        ] {
            assert!(
                !gui_root.contains(forbidden),
                "GUI root still owns Knowledge Processing interaction: {forbidden}"
            );
        }
    }
}
