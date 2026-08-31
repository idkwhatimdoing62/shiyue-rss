//! Private desktop adapter for Library Search interactions.
//!
//! The Library Search module owns query execution and ranking. This adapter
//! owns desktop drafts, request generations, background delivery, history
//! presentation, and result rendering. The GUI root only adopts navigation
//! intents returned by this module.

use std::path::Path;
use std::sync::mpsc as std_mpsc;

use eframe::egui;

use crate::db::Db;
use crate::gui_modal::{self, ModalHostAction};
use crate::gui_state::ModalKind;
use crate::gui_theme::ReaderTheme;
use crate::library_search::{
    LibrarySearch, LibrarySearchResult, PrimaryIdentity, ResultType, SearchOrigin, SearchOutcome,
    SearchRequest, SearchScope,
};
use crate::model::SearchHistoryEntry;

#[derive(Debug, Default)]
pub(super) struct SearchDialog {
    query: String,
    searched_query: String,
    results: Vec<LibrarySearchResult>,
    error: Option<String>,
    focus_input: bool,
    history: Vec<SearchHistoryEntry>,
    searching: bool,
    active_request: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchTarget {
    Modal,
    ResourceRoute,
}

struct SearchEvent {
    request_id: u64,
    target: SearchTarget,
    result: Result<SearchOutcome, String>,
}

pub(super) struct SearchFeature {
    search_event_tx: std_mpsc::Sender<SearchEvent>,
    search_event_rx: std_mpsc::Receiver<SearchEvent>,
    request_generation: u64,
    resource_query: String,
    resource_results: Vec<LibrarySearchResult>,
    resource_searching: bool,
    resource_request: Option<u64>,
    resource_error: Option<String>,
}

pub(super) struct ModalOutcome {
    pub(super) modal_action: ModalHostAction,
    pub(super) selected_hit: Option<LibrarySearchResult>,
    pub(super) notices: Vec<String>,
}

impl SearchFeature {
    pub(super) fn new() -> Self {
        let (search_event_tx, search_event_rx) = std_mpsc::channel();
        Self {
            search_event_tx,
            search_event_rx,
            request_generation: 0,
            resource_query: String::new(),
            resource_results: Vec::new(),
            resource_searching: false,
            resource_request: None,
            resource_error: None,
        }
    }

    pub(super) fn new_dialog(&self, db: &Db) -> SearchDialog {
        SearchDialog {
            history: LibrarySearch::new(db).history(12).unwrap_or_default(),
            ..SearchDialog::default()
        }
    }

    pub(super) fn resource_query(&self) -> &str {
        &self.resource_query
    }

    pub(super) fn resource_query_mut(&mut self) -> &mut String {
        &mut self.resource_query
    }

    pub(super) fn resource_results(&self) -> &[LibrarySearchResult] {
        &self.resource_results
    }

    pub(super) fn resource_searching(&self) -> bool {
        self.resource_searching
    }

    pub(super) fn resource_error(&self) -> Option<&str> {
        self.resource_error.as_deref()
    }

    pub(super) fn start_resource_search(
        &mut self,
        query: String,
        context: &egui::Context,
        db_path: &Path,
    ) {
        if query.is_empty() {
            return;
        }
        let request_id = self.start_search_job(
            SearchRequest {
                query,
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Human,
                limit: 50,
            },
            SearchTarget::ResourceRoute,
            context,
            db_path,
        );
        self.resource_results.clear();
        self.resource_error = None;
        self.resource_searching = true;
        self.resource_request = Some(request_id);
    }

    pub(super) fn show_modal(
        &mut self,
        context: &egui::Context,
        dialog: &mut SearchDialog,
        db: &Db,
        db_path: &Path,
    ) -> ModalOutcome {
        let theme = ReaderTheme::sspai();
        let mut submit = false;
        let mut selected_hit = None;
        let mut clear_history = false;

        let response = gui_modal::show(context, ModalKind::Search, false, |ui, focus| {
            ui.horizontal(|ui| {
                let input = ui.add_sized(
                    egui::vec2((ui.available_width() - 76.0).max(180.0), 34.0),
                    egui::TextEdit::singleline(&mut dialog.query)
                        .hint_text("搜索文章、网页快照、摘录和想法…")
                        .font(egui::TextStyle::Body),
                );
                if dialog.focus_input && focus == crate::gui_modal::InitialFocus::PrimaryField {
                    input.request_focus();
                    dialog.focus_input = false;
                }
                if input.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                if ui
                    .add_sized(
                        egui::vec2(68.0, 34.0),
                        egui::Button::new(egui::RichText::new("搜索").size(15.0).color(theme.text))
                            .fill(theme.selected_bg)
                            .stroke(egui::Stroke::new(1.0, theme.border)),
                    )
                    .clicked()
                {
                    submit = true;
                }
            });
            ui.add_space(7.0);
            ui.label(
                egui::RichText::new(
                    "支持标题、作者、正文、网址、摘录原文和想法内容；最多显示 200 条。",
                )
                .size(13.0)
                .color(theme.muted),
            );
            ui.add_space(8.0);
            ui.separator();
            ui.add_space(6.0);

            if let Some(error) = &dialog.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
                return;
            }
            if dialog.searching {
                ui.horizontal(|ui| {
                    ui.add(egui::Spinner::new());
                    ui.label("正在搜索资料库…");
                });
                return;
            }
            if dialog.searched_query.is_empty() {
                if !dialog.history.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("最近搜索")
                                .size(15.0)
                                .color(theme.text)
                                .family(egui::FontFamily::Name("cjk-bold".into())),
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("清空").size(13.0).color(theme.muted),
                                    )
                                    .stroke(egui::Stroke::NONE),
                                )
                                .clicked()
                            {
                                clear_history = true;
                            }
                        });
                    });
                    ui.add_space(5.0);
                    let history = dialog.history.clone();
                    ui.horizontal_wrapped(|ui| {
                        for entry in history {
                            if ui
                                .add(
                                    egui::Button::new(format!(
                                        "{}  · {}",
                                        entry.query, entry.result_count
                                    ))
                                    .fill(theme.code_bg)
                                    .stroke(egui::Stroke::new(1.0, theme.border)),
                                )
                                .clicked()
                            {
                                dialog.query = entry.query;
                                submit = true;
                            }
                        }
                    });
                    ui.add_space(18.0);
                }
                ui.vertical_centered(|ui| {
                    ui.add_space(55.0);
                    ui.label(
                        egui::RichText::new("在一个入口里找回所有阅读资料")
                            .size(20.0)
                            .color(theme.text)
                            .family(egui::FontFamily::Name("cjk-bold".into())),
                    );
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("快捷键 Ctrl + F")
                            .size(15.0)
                            .color(theme.muted),
                    );
                });
                return;
            }
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(format!("找到 {} 条结果", dialog.results.len()))
                        .size(15.0)
                        .color(theme.text)
                        .family(egui::FontFamily::Name("cjk-bold".into())),
                );
                ui.label(
                    egui::RichText::new(format!("“{}”", dialog.searched_query))
                        .size(13.0)
                        .color(theme.muted),
                );
            });
            ui.add_space(6.0);
            if dialog.results.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(75.0);
                    ui.label(egui::RichText::new("没有匹配内容").size(16.0));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("换一个更短或更常见的关键词试试。")
                            .size(13.0)
                            .color(theme.muted),
                    );
                });
                return;
            }

            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for hit in &dialog.results {
                        let (kind, kind_color) = match hit.primary {
                            PrimaryIdentity::Resource(_) => ("资源", theme.accent),
                            PrimaryIdentity::Article(_) => ("文章", theme.link),
                        };
                        let response = egui::Frame::new()
                            .fill(theme.code_bg)
                            .stroke(egui::Stroke::new(1.0, theme.border))
                            .corner_radius(egui::CornerRadius::same(7))
                            .inner_margin(egui::Margin::symmetric(14, 11))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(
                                        egui::RichText::new(kind)
                                            .size(13.0)
                                            .color(kind_color)
                                            .background_color(theme.selected_bg),
                                    );
                                    if hit.archived {
                                        ui.label(
                                            egui::RichText::new("已归档")
                                                .size(13.0)
                                                .color(theme.muted),
                                        );
                                    }
                                    ui.label(
                                        egui::RichText::new(super::format_timestamp(
                                            hit.updated_at,
                                        ))
                                        .size(13.0)
                                        .color(theme.muted),
                                    );
                                });
                                ui.add_space(5.0);
                                let title = hit
                                    .title
                                    .as_deref()
                                    .filter(|title| !title.trim().is_empty())
                                    .unwrap_or("未命名资料");
                                ui.add(
                                    egui::Label::new(super::search_highlight_layout_job(
                                        title,
                                        &dialog.searched_query,
                                        15.0,
                                        theme.text,
                                        egui::FontFamily::Name("cjk-bold".into()),
                                        theme,
                                    ))
                                    .wrap(),
                                );
                                ui.add_space(5.0);
                                let preview = hit
                                    .evidence
                                    .first()
                                    .map(|evidence| {
                                        super::search_preview(
                                            &evidence.text,
                                            &dialog.searched_query,
                                            180,
                                        )
                                    })
                                    .unwrap_or_default();
                                ui.add(
                                    egui::Label::new(super::search_highlight_layout_job(
                                        &preview,
                                        &dialog.searched_query,
                                        13.0,
                                        theme.muted,
                                        egui::FontFamily::Proportional,
                                        theme,
                                    ))
                                    .wrap(),
                                );
                            })
                            .response
                            .interact(egui::Sense::click());
                        if response.hovered() {
                            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                            ui.painter().rect_stroke(
                                response.rect,
                                egui::CornerRadius::same(7),
                                egui::Stroke::new(1.0, theme.accent),
                                egui::StrokeKind::Inside,
                            );
                        }
                        if response.clicked() {
                            selected_hit = Some(hit.clone());
                        }
                        ui.add_space(9.0);
                    }
                });
        });

        let mut notices = Vec::new();
        if clear_history {
            if let Err(error) = LibrarySearch::new(db).clear_history() {
                notices.push(format!("清空搜索历史失败：{error}"));
            }
            dialog.history.clear();
        }
        if submit {
            self.run_modal_search(dialog, context, db_path);
        }
        ModalOutcome {
            modal_action: response.action,
            selected_hit,
            notices,
        }
    }

    pub(super) fn receive_events(
        &mut self,
        db: &Db,
        dialog: Option<&mut SearchDialog>,
    ) -> Vec<String> {
        let mut notices = Vec::new();
        let mut dialog = dialog;
        while let Ok(event) = self.search_event_rx.try_recv() {
            match event.target {
                SearchTarget::Modal => {
                    let refreshed_history = event
                        .result
                        .as_ref()
                        .ok()
                        .map(|_| LibrarySearch::new(db).history(12).unwrap_or_default());
                    if let Some(dialog) = dialog.as_deref_mut()
                        && accepts_search_response(dialog.active_request, event.request_id)
                    {
                        dialog.searching = false;
                        dialog.active_request = None;
                        match event.result {
                            Ok(outcome) => {
                                if !outcome.warnings.is_empty() {
                                    notices.push("搜索完成，但搜索历史没有保存".to_owned());
                                }
                                dialog.results = outcome.results;
                                dialog.history = refreshed_history.unwrap_or_default();
                            }
                            Err(error) => {
                                dialog.results.clear();
                                dialog.error = Some(error);
                            }
                        }
                    }
                }
                SearchTarget::ResourceRoute => {
                    if accepts_search_response(self.resource_request, event.request_id) {
                        self.resource_searching = false;
                        self.resource_request = None;
                        match event.result {
                            Ok(outcome) => {
                                self.resource_results = outcome.results;
                                self.resource_error = None;
                            }
                            Err(error) => {
                                self.resource_results.clear();
                                self.resource_error = Some(error);
                            }
                        }
                    }
                }
            }
        }
        notices
    }

    fn run_modal_search(
        &mut self,
        dialog: &mut SearchDialog,
        context: &egui::Context,
        db_path: &Path,
    ) {
        let query = dialog.query.trim().to_owned();
        if query.is_empty() {
            dialog.searched_query.clear();
            dialog.error = None;
            dialog.results.clear();
            dialog.searching = false;
            dialog.active_request = None;
            return;
        }
        let request_id = self.start_search_job(
            SearchRequest {
                query: query.clone(),
                scope: SearchScope::Curated,
                result_type: ResultType::All,
                origin: SearchOrigin::Human,
                limit: 50,
            },
            SearchTarget::Modal,
            context,
            db_path,
        );
        dialog.searched_query = query;
        dialog.error = None;
        dialog.results.clear();
        dialog.searching = true;
        dialog.active_request = Some(request_id);
    }

    fn start_search_job(
        &mut self,
        request: SearchRequest,
        target: SearchTarget,
        context: &egui::Context,
        db_path: &Path,
    ) -> u64 {
        self.request_generation = self.request_generation.wrapping_add(1);
        let request_id = self.request_generation;
        let db_path = db_path.to_owned();
        let event_tx = self.search_event_tx.clone();
        let repaint = context.clone();
        std::thread::spawn(move || {
            let result = Db::open(&db_path)
                .map_err(|error| format!("无法打开资料库：{error}"))
                .and_then(|db| {
                    LibrarySearch::new(&db).search(request).map_err(|failure| {
                        format!("{}（{}）", failure.user_message, failure.technical_detail)
                    })
                });
            let _ = event_tx.send(SearchEvent {
                request_id,
                target,
                result,
            });
            repaint.request_repaint();
        });
        request_id
    }
}

pub(super) fn prepare_dialog(dialog: &mut SearchDialog, db: &Db) {
    dialog.focus_input = true;
    dialog.history = LibrarySearch::new(db).history(12).unwrap_or_default();
}

fn accepts_search_response(active_request: Option<u64>, incoming_request: u64) -> bool {
    active_request == Some(incoming_request)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_adapters_reject_late_background_results() {
        assert!(accepts_search_response(Some(9), 9));
        assert!(!accepts_search_response(Some(10), 9));
        assert!(!accepts_search_response(None, 9));
    }

    #[test]
    fn gui_root_no_longer_owns_search_execution_or_state() {
        let gui_root = include_str!("../gui.rs");
        for delegated in [
            "LibrarySearch::new(&self.db).search",
            "LibrarySearch::new(&self.db).clear_history",
            "fn start_search_job",
            "struct SearchEvent",
            "fn accepts_search_response",
        ] {
            assert!(
                !gui_root.contains(delegated),
                "GUI root still owns {delegated}"
            );
        }
    }
}
