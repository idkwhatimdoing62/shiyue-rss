//! Pure desktop interaction state.
//!
//! The desktop adapter owns rendering and I/O. This module owns the user
//! interaction model: exactly one route, at most one blocking modal, one
//! route-owned panel, one popover, one notice, and one guarded transition.

use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ArticleCollection {
    Feed(Option<i64>),
    Saved,
    ReadLater,
    SearchResult(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Route {
    Articles(ArticleCollection),
    Resources,
    Excerpts,
    Archive,
    Storage,
}

impl Default for Route {
    fn default() -> Self {
        Self::Articles(ArticleCollection::Feed(None))
    }
}

impl Route {
    pub(crate) fn is_articles(self) -> bool {
        matches!(self, Self::Articles(_))
    }

    pub(crate) fn article_collection(self) -> Option<ArticleCollection> {
        match self {
            Self::Articles(collection) => Some(collection),
            _ => None,
        }
    }

    pub(crate) fn stable_key(self) -> Option<String> {
        match self {
            Self::Articles(ArticleCollection::Feed(Some(feed_id))) => {
                Some(format!("articles:feed:{feed_id}"))
            }
            Self::Articles(ArticleCollection::Feed(None)) => Some("articles".to_owned()),
            Self::Articles(ArticleCollection::Saved) => Some("articles:saved".to_owned()),
            Self::Articles(ArticleCollection::ReadLater) => Some("articles:read-later".to_owned()),
            Self::Articles(ArticleCollection::SearchResult(_)) => None,
            Self::Resources => Some("resources".to_owned()),
            Self::Excerpts => Some("excerpts".to_owned()),
            Self::Archive => Some("archive".to_owned()),
            Self::Storage => Some("storage".to_owned()),
        }
    }

    pub(crate) fn from_stable_key(value: &str) -> Option<Self> {
        match value {
            "articles" => Some(Self::default()),
            "articles:saved" => Some(Self::Articles(ArticleCollection::Saved)),
            "articles:read-later" => Some(Self::Articles(ArticleCollection::ReadLater)),
            "resources" => Some(Self::Resources),
            "excerpts" => Some(Self::Excerpts),
            "archive" => Some(Self::Archive),
            "storage" => Some(Self::Storage),
            _ => value
                .strip_prefix("articles:feed:")
                .and_then(|id| id.parse().ok())
                .map(|id| Self::Articles(ArticleCollection::Feed(Some(id)))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModalKind {
    AddFeed,
    DeleteFeed,
    Search,
    EditTags,
    WriteThought,
    SaveWebPage,
    DeleteWebPage,
    AddResource,
    DeleteResource,
    ImportResources,
    RestoreBackup,
    ClearImages,
}

impl ModalKind {
    pub(crate) fn is_compatible(self, route: Route) -> bool {
        match self {
            Self::AddFeed | Self::DeleteFeed | Self::Search => true,
            Self::AddResource | Self::DeleteResource | Self::ImportResources => {
                route == Route::Resources
            }
            Self::RestoreBackup | Self::ClearImages => route == Route::Storage,
            Self::EditTags | Self::WriteThought | Self::SaveWebPage | Self::DeleteWebPage => {
                route.is_articles()
            }
        }
    }
}

pub(crate) trait ModalPayload {
    fn kind(&self) -> ModalKind;
    fn is_dirty(&self) -> bool;
    fn active_request_id(&self) -> Option<u64> {
        None
    }

    fn is_compatible(&self, route: Route) -> bool {
        self.kind().is_compatible(route)
    }
}

pub(crate) trait PanelPayload {
    fn is_dirty(&self) -> bool;
    fn is_compatible(&self, route: Route) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DiscardOwner {
    Modal,
    Panel,
}

#[derive(Debug, Clone)]
pub(crate) struct Notice {
    pub(crate) message: String,
    pub(crate) created: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UiEffect {
    RouteChanged { from: Route, to: Route },
    PersistRoute(String),
    InvalidTransition(String),
}

pub(crate) enum UiAction<M, P, O> {
    Navigate(Route),
    ReplaceUnavailableRoute(Route),
    OpenModal(M),
    CloseModal,
    CompleteModal,
    SetPanel(Option<P>),
    SetPopover(Option<O>),
    KeepEditing,
    ConfirmDiscard,
    ClearNotice,
}

enum PendingTransition<M, P> {
    Navigate(Route),
    OpenModal(M),
    CloseModal,
    SetPanel(Option<P>),
}

pub(crate) struct UiState<M, P, O> {
    route: Route,
    modal: Option<M>,
    panel: Option<P>,
    popover: Option<O>,
    notice: Option<Notice>,
    pending: Option<PendingTransition<M, P>>,
    discard_owner: Option<DiscardOwner>,
}

impl<M, P, O> Default for UiState<M, P, O> {
    fn default() -> Self {
        Self {
            route: Route::default(),
            modal: None,
            panel: None,
            popover: None,
            notice: None,
            pending: None,
            discard_owner: None,
        }
    }
}

impl<M, P, O> UiState<M, P, O>
where
    M: ModalPayload,
    P: PanelPayload,
{
    pub(crate) fn reduce(&mut self, action: UiAction<M, P, O>) -> Vec<UiEffect> {
        match action {
            UiAction::Navigate(route) => self.request_navigate(route),
            UiAction::ReplaceUnavailableRoute(route) => self.apply_navigate(route),
            UiAction::OpenModal(modal) => self.request_open_modal(modal),
            UiAction::CloseModal => self.request_close_modal(),
            UiAction::CompleteModal => {
                self.complete_modal();
                Vec::new()
            }
            UiAction::SetPanel(panel) => self.request_set_panel(panel),
            UiAction::SetPopover(popover) => {
                self.set_popover(popover);
                Vec::new()
            }
            UiAction::KeepEditing => {
                self.keep_editing();
                Vec::new()
            }
            UiAction::ConfirmDiscard => self.confirm_discard(),
            UiAction::ClearNotice => {
                self.clear_notice();
                Vec::new()
            }
        }
    }

    pub(crate) fn route(&self) -> Route {
        self.route
    }

    pub(crate) fn initialize_route(&mut self, route: Route) {
        self.route = route;
        self.modal = None;
        self.panel = None;
        self.popover = None;
        self.pending = None;
        self.discard_owner = None;
    }

    fn request_navigate(&mut self, route: Route) -> Vec<UiEffect> {
        if route == self.route {
            return Vec::new();
        }
        if self.guard(PendingTransition::Navigate(route)) {
            return Vec::new();
        }
        self.apply_navigate(route)
    }

    pub(crate) fn modal(&self) -> Option<&M> {
        self.modal.as_ref()
    }

    pub(crate) fn modal_mut(&mut self) -> Option<&mut M> {
        self.modal.as_mut()
    }

    pub(crate) fn modal_kind(&self) -> Option<ModalKind> {
        self.modal.as_ref().map(ModalPayload::kind)
    }

    pub(crate) fn accepts_modal_event(&self, kind: ModalKind, request_id: u64) -> bool {
        self.modal.as_ref().is_some_and(|modal| {
            modal.kind() == kind && modal.active_request_id() == Some(request_id)
        })
    }

    pub(crate) fn has_modal(&self) -> bool {
        self.modal.is_some()
    }

    fn request_open_modal(&mut self, modal: M) -> Vec<UiEffect> {
        if !modal.is_compatible(self.route) {
            return vec![UiEffect::InvalidTransition(
                "当前页面不能打开这个操作".to_owned(),
            )];
        }
        if self.modal.as_ref().is_some_and(ModalPayload::is_dirty) {
            self.guard(PendingTransition::OpenModal(modal));
            return Vec::new();
        }
        self.modal = Some(modal);
        self.popover = None;
        self.pending = None;
        self.discard_owner = None;
        Vec::new()
    }

    fn request_close_modal(&mut self) -> Vec<UiEffect> {
        if self.modal.is_none() {
            return Vec::new();
        }
        if self.guard(PendingTransition::CloseModal) {
            return Vec::new();
        }
        self.modal = None;
        self.pending = None;
        self.discard_owner = None;
        Vec::new()
    }

    fn complete_modal(&mut self) {
        self.modal = None;
        self.pending = None;
        self.discard_owner = None;
    }

    pub(crate) fn panel(&self) -> Option<&P> {
        self.panel.as_ref()
    }

    pub(crate) fn panel_mut(&mut self) -> Option<&mut P> {
        self.panel.as_mut()
    }

    fn request_set_panel(&mut self, panel: Option<P>) -> Vec<UiEffect> {
        if panel
            .as_ref()
            .is_some_and(|candidate| !candidate.is_compatible(self.route))
        {
            return vec![UiEffect::InvalidTransition(
                "当前页面不能打开这个编辑区".to_owned(),
            )];
        }
        if self.panel.as_ref().is_some_and(PanelPayload::is_dirty) {
            self.guard(PendingTransition::SetPanel(panel));
            return Vec::new();
        }
        self.panel = panel;
        self.pending = None;
        self.discard_owner = None;
        Vec::new()
    }

    pub(crate) fn finish_panel(&mut self) {
        self.panel = None;
        if self.discard_owner == Some(DiscardOwner::Panel) {
            self.pending = None;
            self.discard_owner = None;
        }
    }

    pub(crate) fn popover(&self) -> Option<&O> {
        self.popover.as_ref()
    }

    pub(crate) fn set_popover(&mut self, popover: Option<O>) {
        if self.modal.is_none() {
            self.popover = popover;
        } else {
            self.popover = None;
        }
    }

    pub(crate) fn show_notice(&mut self, message: impl Into<String>, created: Instant) {
        self.notice = Some(Notice {
            message: message.into(),
            created,
        });
    }

    pub(crate) fn notice(&self) -> Option<&Notice> {
        self.notice.as_ref()
    }

    pub(crate) fn clear_notice(&mut self) {
        self.notice = None;
    }

    pub(crate) fn discard_owner(&self) -> Option<DiscardOwner> {
        self.discard_owner
    }

    pub(crate) fn keep_editing(&mut self) {
        self.pending = None;
        self.discard_owner = None;
    }

    pub(crate) fn confirm_discard(&mut self) -> Vec<UiEffect> {
        let Some(pending) = self.pending.take() else {
            self.discard_owner = None;
            return Vec::new();
        };
        self.discard_owner = None;
        match pending {
            PendingTransition::Navigate(route) => {
                self.modal = None;
                self.panel = None;
                self.apply_navigate(route)
            }
            PendingTransition::OpenModal(modal) => {
                self.modal = Some(modal);
                self.popover = None;
                Vec::new()
            }
            PendingTransition::CloseModal => {
                self.modal = None;
                Vec::new()
            }
            PendingTransition::SetPanel(panel) => {
                self.panel = panel;
                Vec::new()
            }
        }
    }

    fn guard(&mut self, pending: PendingTransition<M, P>) -> bool {
        if self.modal.as_ref().is_some_and(ModalPayload::is_dirty) {
            self.pending = Some(pending);
            self.discard_owner = Some(DiscardOwner::Modal);
            return true;
        }
        if self.panel.as_ref().is_some_and(PanelPayload::is_dirty) {
            self.pending = Some(pending);
            self.discard_owner = Some(DiscardOwner::Panel);
            return true;
        }
        false
    }

    fn apply_navigate(&mut self, route: Route) -> Vec<UiEffect> {
        let previous = self.route;
        self.route = route;
        self.modal = None;
        self.popover = None;
        if self
            .panel
            .as_ref()
            .is_some_and(|panel| !panel.is_compatible(route))
        {
            self.panel = None;
        }
        self.pending = None;
        self.discard_owner = None;
        vec![
            UiEffect::RouteChanged {
                from: previous,
                to: route,
            },
            UiEffect::PersistRoute(route.stable_key().unwrap_or_else(|| "articles".to_owned())),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestModal {
        kind: ModalKind,
        dirty: bool,
        request_id: Option<u64>,
    }

    impl ModalPayload for TestModal {
        fn kind(&self) -> ModalKind {
            self.kind
        }

        fn is_dirty(&self) -> bool {
            self.dirty
        }

        fn active_request_id(&self) -> Option<u64> {
            self.request_id
        }
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestPanel(bool);

    impl PanelPayload for TestPanel {
        fn is_dirty(&self) -> bool {
            self.0
        }

        fn is_compatible(&self, route: Route) -> bool {
            route == Route::Resources
        }
    }

    type State = UiState<TestModal, TestPanel, u64>;

    fn modal(kind: ModalKind, dirty: bool) -> TestModal {
        TestModal {
            kind,
            dirty,
            request_id: None,
        }
    }

    #[test]
    fn clean_navigation_is_mutually_exclusive_and_persisted() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(modal(ModalKind::Search, false)));
        let effects = state.reduce(UiAction::Navigate(Route::Resources));
        assert_eq!(state.route(), Route::Resources);
        assert!(!state.has_modal());
        assert_eq!(
            effects,
            vec![
                UiEffect::RouteChanged {
                    from: Route::default(),
                    to: Route::Resources
                },
                UiEffect::PersistRoute("resources".into())
            ]
        );
    }

    #[test]
    fn dirty_modal_guards_and_then_continues_the_requested_navigation() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(modal(ModalKind::AddFeed, true)));
        assert!(state.reduce(UiAction::Navigate(Route::Storage)).is_empty());
        assert_eq!(state.route(), Route::default());
        assert_eq!(state.discard_owner(), Some(DiscardOwner::Modal));
        let effects = state.reduce(UiAction::ConfirmDiscard);
        assert_eq!(state.route(), Route::Storage);
        assert!(!state.has_modal());
        assert_eq!(
            effects,
            vec![
                UiEffect::RouteChanged {
                    from: Route::default(),
                    to: Route::Storage
                },
                UiEffect::PersistRoute("storage".into())
            ]
        );
    }

    #[test]
    fn keeping_editing_clears_the_pending_transition() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(modal(ModalKind::AddFeed, true)));
        state.reduce(UiAction::CloseModal);
        state.reduce(UiAction::KeepEditing);
        assert_eq!(state.discard_owner(), None);
        assert_eq!(state.modal_kind(), Some(ModalKind::AddFeed));
    }

    #[test]
    fn opening_a_clean_modal_replaces_it_without_an_implicit_queue() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(modal(ModalKind::Search, false)));
        state.reduce(UiAction::OpenModal(modal(ModalKind::DeleteFeed, false)));
        assert_eq!(state.modal_kind(), Some(ModalKind::DeleteFeed));
        state.reduce(UiAction::CompleteModal);
        assert_eq!(state.modal_kind(), None);
    }

    #[test]
    fn opening_over_a_dirty_modal_requires_one_explicit_discard() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(modal(ModalKind::AddFeed, true)));
        state.reduce(UiAction::OpenModal(modal(ModalKind::Search, false)));
        assert_eq!(state.modal_kind(), Some(ModalKind::AddFeed));
        assert_eq!(state.discard_owner(), Some(DiscardOwner::Modal));
        state.reduce(UiAction::ConfirmDiscard);
        assert_eq!(state.modal_kind(), Some(ModalKind::Search));
        state.reduce(UiAction::CloseModal);
        assert_eq!(state.modal_kind(), None);
    }

    #[test]
    fn incompatible_modal_is_rejected_without_a_blocker() {
        let mut state = State::default();
        let effects = state.reduce(UiAction::OpenModal(TestModal {
            kind: ModalKind::AddResource,
            dirty: false,
            request_id: None,
        }));
        assert!(!state.has_modal());
        assert!(matches!(
            effects.as_slice(),
            [UiEffect::InvalidTransition(_)]
        ));
    }

    #[test]
    fn dirty_panel_guards_route_change_and_is_removed_after_confirmation() {
        let mut state = State::default();
        state.reduce(UiAction::Navigate(Route::Resources));
        state.reduce(UiAction::SetPanel(Some(TestPanel(true))));
        state.reduce(UiAction::Navigate(Route::Archive));
        assert_eq!(state.discard_owner(), Some(DiscardOwner::Panel));
        state.reduce(UiAction::ConfirmDiscard);
        assert_eq!(state.route(), Route::Archive);
        assert!(state.panel().is_none());
    }

    #[test]
    fn modal_opening_clears_popover_and_prevents_new_popovers() {
        let mut state = State::default();
        state.reduce(UiAction::SetPopover(Some(1)));
        state.reduce(UiAction::OpenModal(modal(ModalKind::Search, false)));
        assert!(state.popover().is_none());
        state.reduce(UiAction::SetPopover(Some(2)));
        assert!(state.popover().is_none());
    }

    #[test]
    fn notice_is_owned_and_cleared_by_the_reducer() {
        let mut state = State::default();
        state.show_notice("saved", Instant::now());
        assert_eq!(
            state.notice().map(|notice| notice.message.as_str()),
            Some("saved")
        );
        state.reduce(UiAction::ClearNotice);
        assert!(state.notice().is_none());
    }

    #[test]
    fn only_stable_routes_have_restore_keys() {
        let transient = Route::Articles(ArticleCollection::SearchResult(9));
        assert_eq!(transient.stable_key(), None);
        let route = Route::from_stable_key("articles:feed:42").unwrap();
        assert_eq!(route, Route::Articles(ArticleCollection::Feed(Some(42))));
    }

    #[test]
    fn every_modal_kind_has_an_explicit_route_compatibility_policy() {
        let routes = [
            Route::default(),
            Route::Resources,
            Route::Excerpts,
            Route::Archive,
            Route::Storage,
        ];
        let cases = [
            (ModalKind::AddFeed, [true, true, true, true, true]),
            (ModalKind::DeleteFeed, [true, true, true, true, true]),
            (ModalKind::Search, [true, true, true, true, true]),
            (ModalKind::EditTags, [true, false, false, false, false]),
            (ModalKind::WriteThought, [true, false, false, false, false]),
            (ModalKind::SaveWebPage, [true, false, false, false, false]),
            (ModalKind::DeleteWebPage, [true, false, false, false, false]),
            (ModalKind::AddResource, [false, true, false, false, false]),
            (
                ModalKind::DeleteResource,
                [false, true, false, false, false],
            ),
            (
                ModalKind::ImportResources,
                [false, true, false, false, false],
            ),
            (ModalKind::RestoreBackup, [false, false, false, false, true]),
            (ModalKind::ClearImages, [false, false, false, false, true]),
        ];
        for (kind, expected) in cases {
            for (route, expected) in routes.into_iter().zip(expected) {
                assert_eq!(kind.is_compatible(route), expected, "{kind:?} on {route:?}");
            }
        }
    }

    #[test]
    fn only_the_current_request_can_change_a_modal() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(TestModal {
            kind: ModalKind::SaveWebPage,
            dirty: true,
            request_id: Some(7),
        }));
        assert!(state.accepts_modal_event(ModalKind::SaveWebPage, 7));
        assert!(!state.accepts_modal_event(ModalKind::SaveWebPage, 6));
        state.reduce(UiAction::CompleteModal);
        assert!(!state.accepts_modal_event(ModalKind::SaveWebPage, 7));
    }

    #[test]
    fn discarding_an_async_modal_invalidates_late_results() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(TestModal {
            kind: ModalKind::SaveWebPage,
            dirty: true,
            request_id: Some(41),
        }));
        state.reduce(UiAction::CloseModal);
        assert_eq!(state.discard_owner(), Some(DiscardOwner::Modal));
        assert!(state.accepts_modal_event(ModalKind::SaveWebPage, 41));

        state.reduce(UiAction::ConfirmDiscard);
        assert!(!state.has_modal());
        assert!(!state.accepts_modal_event(ModalKind::SaveWebPage, 41));
    }

    #[test]
    fn unavailable_route_replacement_is_an_explicit_reducer_transition() {
        let mut state = State::default();
        state.reduce(UiAction::OpenModal(modal(ModalKind::AddFeed, true)));
        let effects = state.reduce(UiAction::ReplaceUnavailableRoute(Route::Resources));
        assert_eq!(state.route(), Route::Resources);
        assert!(!state.has_modal());
        assert!(matches!(
            effects.as_slice(),
            [UiEffect::RouteChanged { .. }, UiEffect::PersistRoute(_)]
        ));
    }
}
