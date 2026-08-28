//! Desktop-only projection adoption and invalidation module (ADR-0014).
//!
//! A frame is assembled from memory. SQLite loading, revision watching, and
//! maintenance quiescence are owned by the worker behind this interface.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use crate::article_library_lifecycle::{
    ArticleLibraryLifecycle, ArticleLibraryProjection, ProjectionScope as ArticleProjectionScope,
};
use crate::db::Db;
use crate::excerpt_thought_lifecycle::{
    ExcerptThoughtLifecycle, ExcerptThoughtProjection, ProjectionScope as ExcerptProjectionScope,
    SYSTEM_CLOCK,
};
use crate::knowledge_workflow::{KnowledgeProjectionObserver, TaskKey, TaskSnapshot};
use crate::library_projection_revision::{
    self, LibraryGeneration, LibraryProjectionRevision, ProjectionFamily, ProjectionStamp,
};
use crate::local_data_maintenance::MaintenanceParticipant;
use crate::resource_library_lifecycle::{
    NoProcessingHandoff, ProjectionScope, ResourceCollection, ResourceLibraryLifecycle,
    ResourceLibraryProjection, SystemClock,
};

const MAX_INBOX_PER_FRAME: usize = 32;
const MAX_RESOURCE_SCOPES: usize = 16;
const MAX_ARTICLE_SCOPES: usize = 16;
const MAX_EXCERPT_SCOPES: usize = 16;
const MAX_PENDING_WORKER_EVENTS: usize =
    MAX_RESOURCE_SCOPES + MAX_ARTICLE_SCOPES + MAX_EXCERPT_SCOPES + 4;
const REVISION_WATCH_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Default)]
pub(crate) struct DesktopProjectionDemand {
    pub(crate) articles: Vec<ArticleProjectionScope>,
    pub(crate) resources: Vec<ResourceProjectionDemand>,
    pub(crate) excerpts: Vec<ExcerptProjectionScope>,
    pub(crate) knowledge: Vec<TaskKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ResourceProjectionDemand {
    Collection(ResourceCollection),
    Detail(i64),
}

impl ResourceProjectionDemand {
    fn initial_scope(self) -> ProjectionScope {
        match self {
            Self::Collection(collection) => ProjectionScope::collection(collection),
            Self::Detail(resource_id) => ProjectionScope::Resource(resource_id),
        }
    }

    fn from_scope(scope: ProjectionScope) -> Self {
        match scope {
            ProjectionScope::Collection { collection, .. } => Self::Collection(collection),
            ProjectionScope::Resource(resource_id) => Self::Detail(resource_id),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectionFreshness {
    Loading,
    Current,
    Refreshing,
    Failed { technical_detail: String },
    Maintenance,
}

#[derive(Debug, Clone)]
pub(crate) struct ResourceProjectionView {
    pub(crate) freshness: ProjectionFreshness,
    pub(crate) data: Option<Arc<ResourceLibraryProjection>>,
    pub(crate) has_more: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ArticleProjectionView {
    pub(crate) freshness: ProjectionFreshness,
    pub(crate) data: Option<Arc<ArticleLibraryProjection>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ExcerptProjectionView {
    pub(crate) freshness: ProjectionFreshness,
    pub(crate) data: Option<Arc<ExcerptThoughtProjection>>,
}

#[derive(Debug, Clone)]
pub(crate) struct KnowledgeProjectionView {
    pub(crate) freshness: ProjectionFreshness,
    pub(crate) data: Option<TaskSnapshot>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DesktopProjectionFrame {
    articles: HashMap<ArticleProjectionScope, ArticleProjectionView>,
    resources: HashMap<ResourceProjectionDemand, ResourceProjectionView>,
    excerpts: HashMap<ExcerptProjectionScope, ExcerptProjectionView>,
    knowledge: HashMap<TaskKey, KnowledgeProjectionView>,
}

impl DesktopProjectionFrame {
    pub(crate) fn article(&self, scope: ArticleProjectionScope) -> Option<&ArticleProjectionView> {
        self.articles.get(&scope)
    }

    pub(crate) fn resource(
        &self,
        demand: ResourceProjectionDemand,
    ) -> Option<&ResourceProjectionView> {
        self.resources.get(&demand)
    }

    pub(crate) fn knowledge(&self, key: TaskKey) -> Option<&KnowledgeProjectionView> {
        self.knowledge.get(&key)
    }

    pub(crate) fn excerpt(&self, scope: ExcerptProjectionScope) -> Option<&ExcerptProjectionView> {
        self.excerpts.get(&scope)
    }
}

pub(crate) enum DesktopProjectionFact {
    AdoptArticle(Box<ArticleLibraryProjection>),
    AdoptResource(Box<ResourceLibraryProjection>),
    AdoptExcerpt(Box<ExcerptThoughtProjection>),
    RetryResource(ResourceProjectionDemand),
    LoadMoreResources(ResourceCollection),
    MaintenanceStarted,
}

impl DesktopProjectionFact {
    pub(crate) fn adopt_article(projection: ArticleLibraryProjection) -> Self {
        Self::AdoptArticle(Box::new(projection))
    }

    pub(crate) fn adopt_resource(projection: ResourceLibraryProjection) -> Self {
        Self::AdoptResource(Box::new(projection))
    }

    pub(crate) fn adopt_excerpt(projection: ExcerptThoughtProjection) -> Self {
        Self::AdoptExcerpt(Box::new(projection))
    }
}

struct ArticleSlot {
    data: Option<Arc<ArticleLibraryProjection>>,
    freshness: ProjectionFreshness,
    in_flight: bool,
    last_used: u64,
}

struct ResourceSlot {
    data: Option<Arc<ResourceLibraryProjection>>,
    freshness: ProjectionFreshness,
    in_flight: bool,
    last_used: u64,
}

struct ExcerptSlot {
    data: Option<Arc<ExcerptThoughtProjection>>,
    freshness: ProjectionFreshness,
    in_flight: bool,
    last_used: u64,
}

enum WorkerCommand {
    LoadArticle {
        scope: ArticleProjectionScope,
    },
    LoadResource {
        demand: ResourceProjectionDemand,
        scope: ProjectionScope,
        append: bool,
    },
    LoadExcerpt {
        scope: ExcerptProjectionScope,
    },
    Quiesce {
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Resume {
        reply: std_mpsc::Sender<Result<(), String>>,
    },
    Shutdown,
}

enum WorkerEvent {
    ArticleLoaded {
        scope: ArticleProjectionScope,
        projection: Box<ArticleLibraryProjection>,
    },
    ArticleFailed {
        scope: ArticleProjectionScope,
        technical_detail: String,
    },
    ResourceLoaded {
        demand: ResourceProjectionDemand,
        projection: Box<ResourceLibraryProjection>,
        append: bool,
    },
    ResourceFailed {
        demand: ResourceProjectionDemand,
        technical_detail: String,
    },
    ExcerptLoaded {
        scope: ExcerptProjectionScope,
        projection: Box<ExcerptThoughtProjection>,
    },
    ExcerptFailed {
        scope: ExcerptProjectionScope,
        technical_detail: String,
    },
    RevisionObserved {
        family: ProjectionFamily,
        stamp: ProjectionStamp,
    },
    Maintenance(bool),
}

struct ProjectionMaintenanceParticipant {
    command_tx: std_mpsc::SyncSender<WorkerCommand>,
}

impl MaintenanceParticipant for ProjectionMaintenanceParticipant {
    fn name(&self) -> &'static str {
        "desktop_library_projection"
    }

    fn quiesce(&self, deadline: Instant, _epoch: &str) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(WorkerCommand::Quiesce { reply: reply_tx })
            .context("DESKTOP_PROJECTION_STOPPED: cannot quiesce")?;
        reply_rx
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .context("DESKTOP_PROJECTION_QUIESCE_TIMEOUT")?
            .map_err(anyhow::Error::msg)
    }

    fn resume(&self, _epoch: &str) -> Result<()> {
        let (reply_tx, reply_rx) = std_mpsc::channel();
        self.command_tx
            .send(WorkerCommand::Resume { reply: reply_tx })
            .context("DESKTOP_PROJECTION_STOPPED: cannot resume")?;
        reply_rx
            .recv_timeout(Duration::from_secs(1))
            .context("DESKTOP_PROJECTION_RESUME_TIMEOUT")?
            .map_err(anyhow::Error::msg)
    }
}

pub(crate) struct DesktopLibraryProjection {
    command_tx: std_mpsc::SyncSender<WorkerCommand>,
    event_rx: std_mpsc::Receiver<WorkerEvent>,
    knowledge: KnowledgeProjectionObserver,
    articles: HashMap<ArticleProjectionScope, ArticleSlot>,
    resources: HashMap<ResourceProjectionDemand, ResourceSlot>,
    excerpts: HashMap<ExcerptProjectionScope, ExcerptSlot>,
    observed_knowledge: HashSet<TaskKey>,
    active_generation: Option<LibraryGeneration>,
    known_article_revision: i64,
    known_resource_revision: i64,
    known_excerpt_revision: i64,
    maintenance: bool,
    tick: u64,
    join: Option<std::thread::JoinHandle<()>>,
}

impl DesktopLibraryProjection {
    pub(crate) fn start(
        db_path: PathBuf,
        knowledge: KnowledgeProjectionObserver,
        repaint: eframe::egui::Context,
    ) -> Result<Self> {
        // Fail startup at the interface rather than producing an eternally
        // loading desktop route.
        Db::open(&db_path).context("open Desktop Library Projection")?;
        let (command_tx, command_rx) = std_mpsc::sync_channel(32);
        let (event_tx, event_rx) = std_mpsc::sync_channel(32);
        let join = std::thread::Builder::new()
            .name("shiyue-desktop-library-projection".into())
            .spawn(move || projection_worker(db_path, command_rx, event_tx, repaint))?;
        Ok(Self {
            command_tx,
            event_rx,
            knowledge,
            articles: HashMap::new(),
            resources: HashMap::new(),
            excerpts: HashMap::new(),
            observed_knowledge: HashSet::new(),
            active_generation: None,
            known_article_revision: 0,
            known_resource_revision: 0,
            known_excerpt_revision: 0,
            maintenance: false,
            tick: 0,
            join: Some(join),
        })
    }

    pub(crate) fn maintenance_participant(&self) -> Arc<dyn MaintenanceParticipant> {
        Arc::new(ProjectionMaintenanceParticipant {
            command_tx: self.command_tx.clone(),
        })
    }

    pub(crate) fn accept(&mut self, fact: DesktopProjectionFact) {
        match fact {
            DesktopProjectionFact::AdoptArticle(projection) => {
                self.adopt_article(*projection);
            }
            DesktopProjectionFact::AdoptResource(projection) => {
                self.adopt_resource(*projection);
            }
            DesktopProjectionFact::AdoptExcerpt(projection) => {
                self.adopt_excerpt(*projection);
            }
            DesktopProjectionFact::RetryResource(scope) => {
                if let Some(slot) = self.resources.get_mut(&scope) {
                    slot.in_flight = false;
                    slot.freshness = if slot.data.is_some() {
                        ProjectionFreshness::Refreshing
                    } else {
                        ProjectionFreshness::Loading
                    };
                    self.schedule(scope);
                }
            }
            DesktopProjectionFact::LoadMoreResources(collection) => {
                let demand = ResourceProjectionDemand::Collection(collection);
                let next_cursor = self
                    .resources
                    .get(&demand)
                    .and_then(|slot| slot.data.as_ref())
                    .and_then(|projection| projection.next_cursor);
                if let Some(cursor) = next_cursor {
                    self.schedule_page(
                        demand,
                        ProjectionScope::collection_after(collection, cursor),
                        true,
                    );
                }
            }
            DesktopProjectionFact::MaintenanceStarted => {
                self.enter_maintenance();
            }
        }
    }

    /// The only per-frame interface. It drains at most 32 asynchronous facts,
    /// schedules missing work, and returns a stable in-memory snapshot.
    pub(crate) fn frame(&mut self, demand: DesktopProjectionDemand) -> DesktopProjectionFrame {
        self.tick = self.tick.wrapping_add(1);
        self.drain_events();
        // Knowledge notices are hints only; the latest snapshot already lives
        // in workflow-owned memory. Bound the drain for predictable frame work.
        let _changed = self
            .knowledge
            .try_changed()
            .take(MAX_INBOX_PER_FRAME)
            .collect::<Vec<_>>();

        let mut articles = HashMap::new();
        for scope in demand.articles.iter().copied().collect::<HashSet<_>>() {
            let slot = self.articles.entry(scope).or_insert_with(|| ArticleSlot {
                data: None,
                freshness: if self.maintenance {
                    ProjectionFreshness::Maintenance
                } else {
                    ProjectionFreshness::Loading
                },
                in_flight: false,
                last_used: self.tick,
            });
            slot.last_used = self.tick;
            if !self.maintenance
                && !slot.in_flight
                && matches!(
                    slot.freshness,
                    ProjectionFreshness::Loading | ProjectionFreshness::Refreshing
                )
            {
                self.schedule_article(scope);
            }
            let slot = self.articles.get(&scope).expect("article slot inserted");
            articles.insert(
                scope,
                ArticleProjectionView {
                    freshness: slot.freshness.clone(),
                    data: slot.data.clone(),
                },
            );
        }

        let mut resources = HashMap::new();
        for resource_demand in demand.resources.iter().copied().collect::<HashSet<_>>() {
            let slot = self
                .resources
                .entry(resource_demand)
                .or_insert_with(|| ResourceSlot {
                    data: None,
                    freshness: if self.maintenance {
                        ProjectionFreshness::Maintenance
                    } else {
                        ProjectionFreshness::Loading
                    },
                    in_flight: false,
                    last_used: self.tick,
                });
            slot.last_used = self.tick;
            if !self.maintenance
                && !slot.in_flight
                && matches!(
                    slot.freshness,
                    ProjectionFreshness::Loading | ProjectionFreshness::Refreshing
                )
            {
                self.schedule(resource_demand);
            }
            let slot = self
                .resources
                .get(&resource_demand)
                .expect("resource slot inserted");
            resources.insert(
                resource_demand,
                ResourceProjectionView {
                    freshness: slot.freshness.clone(),
                    data: slot.data.clone(),
                    has_more: slot
                        .data
                        .as_ref()
                        .is_some_and(|projection| projection.next_cursor.is_some()),
                },
            );
        }

        let mut excerpts = HashMap::new();
        for scope in demand.excerpts.iter().copied().collect::<HashSet<_>>() {
            let slot = self.excerpts.entry(scope).or_insert_with(|| ExcerptSlot {
                data: None,
                freshness: if self.maintenance {
                    ProjectionFreshness::Maintenance
                } else {
                    ProjectionFreshness::Loading
                },
                in_flight: false,
                last_used: self.tick,
            });
            slot.last_used = self.tick;
            if !self.maintenance
                && !slot.in_flight
                && matches!(
                    slot.freshness,
                    ProjectionFreshness::Loading | ProjectionFreshness::Refreshing
                )
            {
                self.schedule_excerpt(scope);
            }
            let slot = self.excerpts.get(&scope).expect("excerpt slot inserted");
            excerpts.insert(
                scope,
                ExcerptProjectionView {
                    freshness: slot.freshness.clone(),
                    data: slot.data.clone(),
                },
            );
        }

        let demanded_knowledge = demand.knowledge.iter().copied().collect::<HashSet<_>>();
        for key in self
            .observed_knowledge
            .difference(&demanded_knowledge)
            .copied()
            .collect::<Vec<_>>()
        {
            self.knowledge.forget(key);
        }
        for key in demanded_knowledge
            .difference(&self.observed_knowledge)
            .copied()
        {
            self.knowledge.observe(key);
        }
        self.observed_knowledge = demanded_knowledge;

        let mut knowledge = HashMap::new();
        for key in self.observed_knowledge.iter().copied() {
            let materialized = self.knowledge.snapshot(key);
            knowledge.insert(
                key,
                KnowledgeProjectionView {
                    freshness: if self.maintenance {
                        ProjectionFreshness::Maintenance
                    } else if materialized.is_some() {
                        ProjectionFreshness::Current
                    } else {
                        ProjectionFreshness::Loading
                    },
                    data: materialized.flatten(),
                },
            );
        }
        self.evict_inactive();
        DesktopProjectionFrame {
            articles,
            resources,
            excerpts,
            knowledge,
        }
    }

    fn schedule_article(&mut self, scope: ArticleProjectionScope) {
        if self.articles.values().filter(|slot| slot.in_flight).count() >= MAX_ARTICLE_SCOPES {
            return;
        }
        let Some(slot) = self.articles.get_mut(&scope) else {
            return;
        };
        if slot.in_flight || self.maintenance {
            return;
        }
        match self
            .command_tx
            .try_send(WorkerCommand::LoadArticle { scope })
        {
            Ok(()) => slot.in_flight = true,
            Err(std_mpsc::TrySendError::Full(_)) => {}
            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                slot.freshness = ProjectionFreshness::Failed {
                    technical_detail: "DESKTOP_PROJECTION_WORKER_STOPPED".into(),
                };
            }
        }
    }

    fn schedule(&mut self, demand: ResourceProjectionDemand) {
        self.schedule_page(demand, demand.initial_scope(), false);
    }

    fn schedule_page(
        &mut self,
        demand: ResourceProjectionDemand,
        scope: ProjectionScope,
        append: bool,
    ) {
        if self
            .resources
            .values()
            .filter(|slot| slot.in_flight)
            .count()
            >= MAX_RESOURCE_SCOPES
        {
            return;
        }
        let Some(slot) = self.resources.get_mut(&demand) else {
            return;
        };
        if slot.in_flight || self.maintenance {
            return;
        }
        match self.command_tx.try_send(WorkerCommand::LoadResource {
            demand,
            scope,
            append,
        }) {
            Ok(()) => {
                slot.in_flight = true;
                if append {
                    slot.freshness = ProjectionFreshness::Refreshing;
                }
            }
            Err(std_mpsc::TrySendError::Full(_)) => {}
            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                slot.freshness = ProjectionFreshness::Failed {
                    technical_detail: "DESKTOP_PROJECTION_WORKER_STOPPED".into(),
                };
            }
        }
    }

    fn schedule_excerpt(&mut self, scope: ExcerptProjectionScope) {
        if self.excerpts.values().filter(|slot| slot.in_flight).count() >= MAX_EXCERPT_SCOPES {
            return;
        }
        let Some(slot) = self.excerpts.get_mut(&scope) else {
            return;
        };
        if slot.in_flight || self.maintenance {
            return;
        }
        match self
            .command_tx
            .try_send(WorkerCommand::LoadExcerpt { scope })
        {
            Ok(()) => slot.in_flight = true,
            Err(std_mpsc::TrySendError::Full(_)) => {}
            Err(std_mpsc::TrySendError::Disconnected(_)) => {
                slot.freshness = ProjectionFreshness::Failed {
                    technical_detail: "DESKTOP_PROJECTION_WORKER_STOPPED".into(),
                };
            }
        }
    }

    fn drain_events(&mut self) {
        for _ in 0..MAX_INBOX_PER_FRAME {
            let Ok(event) = self.event_rx.try_recv() else {
                break;
            };
            match event {
                WorkerEvent::ArticleLoaded { scope, projection } => {
                    self.adopt_article_for(scope, *projection)
                }
                WorkerEvent::ArticleFailed {
                    scope,
                    technical_detail,
                } => {
                    let slot = self.articles.entry(scope).or_insert_with(|| ArticleSlot {
                        data: None,
                        freshness: ProjectionFreshness::Loading,
                        in_flight: false,
                        last_used: self.tick,
                    });
                    slot.in_flight = false;
                    slot.freshness = ProjectionFreshness::Failed { technical_detail };
                }
                WorkerEvent::ResourceLoaded {
                    demand,
                    projection,
                    append,
                } => self.adopt_resource_for(demand, *projection, append),
                WorkerEvent::ResourceFailed {
                    demand,
                    technical_detail,
                } => {
                    let slot = self
                        .resources
                        .entry(demand)
                        .or_insert_with(|| ResourceSlot {
                            data: None,
                            freshness: ProjectionFreshness::Loading,
                            in_flight: false,
                            last_used: self.tick,
                        });
                    slot.in_flight = false;
                    slot.freshness = ProjectionFreshness::Failed { technical_detail };
                }
                WorkerEvent::ExcerptLoaded { scope, projection } => {
                    self.adopt_excerpt_for(scope, *projection)
                }
                WorkerEvent::ExcerptFailed {
                    scope,
                    technical_detail,
                } => {
                    let slot = self.excerpts.entry(scope).or_insert_with(|| ExcerptSlot {
                        data: None,
                        freshness: ProjectionFreshness::Loading,
                        in_flight: false,
                        last_used: self.tick,
                    });
                    slot.in_flight = false;
                    slot.freshness = ProjectionFreshness::Failed { technical_detail };
                }
                WorkerEvent::RevisionObserved { family, stamp } => {
                    self.observe_revision(family, stamp)
                }
                WorkerEvent::Maintenance(active) => {
                    if active {
                        self.enter_maintenance();
                    } else {
                        self.maintenance = false;
                        for slot in self.articles.values_mut() {
                            slot.freshness = ProjectionFreshness::Loading;
                        }
                        for slot in self.resources.values_mut() {
                            slot.freshness = ProjectionFreshness::Loading;
                        }
                        for slot in self.excerpts.values_mut() {
                            slot.freshness = ProjectionFreshness::Loading;
                        }
                    }
                }
            }
        }
    }

    fn enter_maintenance(&mut self) {
        self.maintenance = true;
        self.active_generation = None;
        self.known_article_revision = 0;
        self.known_resource_revision = 0;
        self.known_excerpt_revision = 0;
        for slot in self.articles.values_mut() {
            slot.data = None;
            slot.in_flight = false;
            slot.freshness = ProjectionFreshness::Maintenance;
        }
        for slot in self.resources.values_mut() {
            slot.data = None;
            slot.in_flight = false;
            slot.freshness = ProjectionFreshness::Maintenance;
        }
        for slot in self.excerpts.values_mut() {
            slot.data = None;
            slot.in_flight = false;
            slot.freshness = ProjectionFreshness::Maintenance;
        }
    }

    fn observe_revision(&mut self, family: ProjectionFamily, stamp: ProjectionStamp) {
        if self.active_generation.as_ref() != Some(&stamp.generation) {
            self.active_generation = Some(stamp.generation);
            self.known_article_revision = 0;
            self.known_resource_revision = 0;
            self.known_excerpt_revision = 0;
            *self.known_revision_mut(family) = stamp.revision;
            for slot in self.articles.values_mut() {
                slot.data = None;
                slot.in_flight = false;
                slot.freshness = ProjectionFreshness::Loading;
            }
            for slot in self.resources.values_mut() {
                slot.data = None;
                slot.in_flight = false;
                slot.freshness = ProjectionFreshness::Loading;
            }
            for slot in self.excerpts.values_mut() {
                slot.data = None;
                slot.in_flight = false;
                slot.freshness = ProjectionFreshness::Loading;
            }
            return;
        }
        if stamp.revision <= self.known_revision(family) {
            return;
        }
        *self.known_revision_mut(family) = stamp.revision;
        match family {
            ProjectionFamily::Article => {
                for slot in self.articles.values_mut() {
                    slot.in_flight = false;
                    slot.freshness = if slot.data.is_some() {
                        ProjectionFreshness::Refreshing
                    } else {
                        ProjectionFreshness::Loading
                    };
                }
            }
            ProjectionFamily::Resource => {
                for slot in self.resources.values_mut() {
                    slot.in_flight = false;
                    slot.freshness = if slot.data.is_some() {
                        ProjectionFreshness::Refreshing
                    } else {
                        ProjectionFreshness::Loading
                    };
                }
            }
            ProjectionFamily::Excerpt => {
                for slot in self.excerpts.values_mut() {
                    slot.in_flight = false;
                    slot.freshness = if slot.data.is_some() {
                        ProjectionFreshness::Refreshing
                    } else {
                        ProjectionFreshness::Loading
                    };
                }
            }
        }
    }

    fn known_revision(&self, family: ProjectionFamily) -> i64 {
        match family {
            ProjectionFamily::Article => self.known_article_revision,
            ProjectionFamily::Resource => self.known_resource_revision,
            ProjectionFamily::Excerpt => self.known_excerpt_revision,
        }
    }

    fn known_revision_mut(&mut self, family: ProjectionFamily) -> &mut i64 {
        match family {
            ProjectionFamily::Article => &mut self.known_article_revision,
            ProjectionFamily::Resource => &mut self.known_resource_revision,
            ProjectionFamily::Excerpt => &mut self.known_excerpt_revision,
        }
    }

    fn adopt_article(&mut self, projection: ArticleLibraryProjection) {
        self.adopt_article_for(projection.scope, projection);
    }

    fn adopt_article_for(
        &mut self,
        scope: ArticleProjectionScope,
        mut projection: ArticleLibraryProjection,
    ) {
        let stamp = &projection.stamp;
        if let Some(generation) = &self.active_generation
            && generation != &stamp.generation
        {
            return;
        }
        if stamp.revision < self.known_article_revision {
            return;
        }
        self.active_generation = Some(stamp.generation.clone());
        if stamp.revision > self.known_article_revision {
            self.known_article_revision = stamp.revision;
            for (cached_scope, slot) in &mut self.articles {
                if *cached_scope != scope
                    && slot
                        .data
                        .as_ref()
                        .is_some_and(|data| data.stamp.revision < stamp.revision)
                {
                    slot.in_flight = false;
                    slot.freshness = ProjectionFreshness::Refreshing;
                }
            }
        }
        projection.scope = scope;
        self.articles.insert(
            scope,
            ArticleSlot {
                data: Some(Arc::new(projection)),
                freshness: ProjectionFreshness::Current,
                in_flight: false,
                last_used: self.tick,
            },
        );
    }

    fn adopt_resource(&mut self, projection: ResourceLibraryProjection) {
        let demand = ResourceProjectionDemand::from_scope(projection.scope);
        self.adopt_resource_for(demand, projection, false);
    }

    fn adopt_resource_for(
        &mut self,
        demand: ResourceProjectionDemand,
        mut projection: ResourceLibraryProjection,
        append: bool,
    ) {
        let stamp = &projection.stamp;
        if let Some(generation) = &self.active_generation
            && generation != &stamp.generation
        {
            return;
        }
        if stamp.revision < self.known_resource_revision {
            return;
        }
        self.active_generation = Some(stamp.generation.clone());
        if stamp.revision > self.known_resource_revision {
            self.known_resource_revision = stamp.revision;
            for (cached_demand, slot) in &mut self.resources {
                if *cached_demand != demand
                    && slot
                        .data
                        .as_ref()
                        .is_some_and(|data| data.stamp.revision < stamp.revision)
                {
                    slot.in_flight = false;
                    slot.freshness = ProjectionFreshness::Refreshing;
                }
            }
        }
        if append
            && let Some(existing) = self
                .resources
                .get(&demand)
                .and_then(|slot| slot.data.as_ref())
        {
            let mut seen = existing
                .resources
                .iter()
                .map(|resource| resource.id)
                .collect::<HashSet<_>>();
            let mut merged = existing.resources.clone();
            merged.extend(
                projection
                    .resources
                    .drain(..)
                    .filter(|resource| seen.insert(resource.id)),
            );
            projection.resources = merged;
        }
        projection.scope = demand.initial_scope();
        self.resources.insert(
            demand,
            ResourceSlot {
                data: Some(Arc::new(projection)),
                freshness: ProjectionFreshness::Current,
                in_flight: false,
                last_used: self.tick,
            },
        );
    }

    fn adopt_excerpt(&mut self, projection: ExcerptThoughtProjection) {
        self.adopt_excerpt_for(projection.scope, projection);
    }

    fn adopt_excerpt_for(
        &mut self,
        scope: ExcerptProjectionScope,
        mut projection: ExcerptThoughtProjection,
    ) {
        let stamp = &projection.stamp;
        if let Some(generation) = &self.active_generation
            && generation != &stamp.generation
        {
            return;
        }
        if stamp.revision < self.known_excerpt_revision {
            return;
        }
        self.active_generation = Some(stamp.generation.clone());
        if stamp.revision > self.known_excerpt_revision {
            self.known_excerpt_revision = stamp.revision;
            for (cached_scope, slot) in &mut self.excerpts {
                if *cached_scope != scope
                    && slot
                        .data
                        .as_ref()
                        .is_some_and(|data| data.stamp.revision < stamp.revision)
                {
                    slot.in_flight = false;
                    slot.freshness = ProjectionFreshness::Refreshing;
                }
            }
        }
        projection.scope = scope;
        self.excerpts.insert(
            scope,
            ExcerptSlot {
                data: Some(Arc::new(projection)),
                freshness: ProjectionFreshness::Current,
                in_flight: false,
                last_used: self.tick,
            },
        );
    }

    fn evict_inactive(&mut self) {
        if self.articles.len() > MAX_ARTICLE_SCOPES {
            let mut inactive = self
                .articles
                .iter()
                .filter(|(_, slot)| !slot.in_flight)
                .map(|(scope, slot)| (*scope, slot.last_used))
                .collect::<Vec<_>>();
            inactive.sort_by_key(|(_, last_used)| *last_used);
            let overflow = self.articles.len() - MAX_ARTICLE_SCOPES;
            for (scope, _) in inactive.into_iter().take(overflow) {
                self.articles.remove(&scope);
            }
        }
        if self.resources.len() <= MAX_RESOURCE_SCOPES {
            // Excerpt eviction still needs to run.
        } else {
            let mut inactive = self
                .resources
                .iter()
                .filter(|(_, slot)| !slot.in_flight)
                .map(|(scope, slot)| (*scope, slot.last_used))
                .collect::<Vec<_>>();
            inactive.sort_by_key(|(_, last_used)| *last_used);
            let overflow = self.resources.len() - MAX_RESOURCE_SCOPES;
            for (scope, _) in inactive.into_iter().take(overflow) {
                self.resources.remove(&scope);
            }
        }
        if self.excerpts.len() > MAX_EXCERPT_SCOPES {
            let mut inactive = self
                .excerpts
                .iter()
                .filter(|(_, slot)| !slot.in_flight)
                .map(|(scope, slot)| (*scope, slot.last_used))
                .collect::<Vec<_>>();
            inactive.sort_by_key(|(_, last_used)| *last_used);
            let overflow = self.excerpts.len() - MAX_EXCERPT_SCOPES;
            for (scope, _) in inactive.into_iter().take(overflow) {
                self.excerpts.remove(&scope);
            }
        }
    }
}

impl Drop for DesktopLibraryProjection {
    fn drop(&mut self) {
        for key in self.observed_knowledge.drain() {
            self.knowledge.forget(key);
        }
        let _ = self.command_tx.send(WorkerCommand::Shutdown);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn projection_worker(
    db_path: PathBuf,
    command_rx: std_mpsc::Receiver<WorkerCommand>,
    event_tx: std_mpsc::SyncSender<WorkerEvent>,
    repaint: eframe::egui::Context,
) {
    let mut db = Db::open(&db_path).ok();
    let mut explicitly_quiesced = false;
    let mut last_watch = Instant::now() - REVISION_WATCH_INTERVAL;
    let mut last_revisions: Option<(LibraryGeneration, LibraryProjectionRevision)> = None;
    let mut pending_events = VecDeque::with_capacity(MAX_PENDING_WORKER_EVENTS);
    loop {
        if !flush_worker_events(&mut pending_events, &event_tx, &repaint) {
            break;
        }
        let maintenance_active = crate::local_data_maintenance::MaintenanceFence::observe(&db_path)
            .map(|availability| availability.is_active())
            .unwrap_or(true);
        if explicitly_quiesced || maintenance_active {
            if db.take().is_some() {
                enqueue_worker_event(&mut pending_events, WorkerEvent::Maintenance(true));
            }
        } else if db.is_none() {
            match Db::open(&db_path) {
                Ok(opened) => {
                    db = Some(opened);
                    last_revisions = None;
                    enqueue_worker_event(&mut pending_events, WorkerEvent::Maintenance(false));
                }
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }

        if let Some(opened) = db.as_ref()
            && last_watch.elapsed() >= REVISION_WATCH_INTERVAL
        {
            last_watch = Instant::now();
            if let Ok(revisions) = library_projection_revision::read(&opened.conn) {
                let generation = opened.library_generation();
                let generation_changed = last_revisions
                    .as_ref()
                    .is_none_or(|(known, _)| known != &generation);
                for family in [
                    ProjectionFamily::Article,
                    ProjectionFamily::Resource,
                    ProjectionFamily::Excerpt,
                ] {
                    let revision = revisions.family(family);
                    let changed = generation_changed
                        || last_revisions
                            .as_ref()
                            .is_none_or(|(_, known)| known.family(family) != revision);
                    if changed {
                        enqueue_worker_event(
                            &mut pending_events,
                            WorkerEvent::RevisionObserved {
                                family,
                                stamp: ProjectionStamp {
                                    generation: generation.clone(),
                                    revision,
                                },
                            },
                        );
                    }
                }
                last_revisions = Some((generation, revisions));
            }
        }

        match command_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(WorkerCommand::LoadArticle { scope }) => {
                let event = if let Some(opened) = db.as_ref() {
                    match ArticleLibraryLifecycle::new(opened).project(scope) {
                        Ok(projection) => WorkerEvent::ArticleLoaded {
                            scope,
                            projection: Box::new(projection),
                        },
                        Err(error) => WorkerEvent::ArticleFailed {
                            scope,
                            technical_detail: safe_detail(&error.technical_detail),
                        },
                    }
                } else {
                    WorkerEvent::ArticleFailed {
                        scope,
                        technical_detail: "MAINTENANCE_IN_PROGRESS".into(),
                    }
                };
                enqueue_worker_event(&mut pending_events, event);
            }
            Ok(WorkerCommand::LoadResource {
                demand,
                scope,
                append,
            }) => {
                let event = if let Some(opened) = db.as_ref() {
                    let lifecycle =
                        ResourceLibraryLifecycle::new(opened, &NoProcessingHandoff, &SystemClock);
                    match lifecycle.project(scope) {
                        Ok(projection) => WorkerEvent::ResourceLoaded {
                            demand,
                            projection: Box::new(projection),
                            append,
                        },
                        Err(error) => WorkerEvent::ResourceFailed {
                            demand,
                            technical_detail: safe_detail(&error.technical_detail),
                        },
                    }
                } else {
                    WorkerEvent::ResourceFailed {
                        demand,
                        technical_detail: "MAINTENANCE_IN_PROGRESS".into(),
                    }
                };
                enqueue_worker_event(&mut pending_events, event);
            }
            Ok(WorkerCommand::LoadExcerpt { scope }) => {
                let event = if let Some(opened) = db.as_ref() {
                    match ExcerptThoughtLifecycle::new(opened, &SYSTEM_CLOCK).project(scope) {
                        Ok(projection) => WorkerEvent::ExcerptLoaded {
                            scope,
                            projection: Box::new(projection),
                        },
                        Err(error) => WorkerEvent::ExcerptFailed {
                            scope,
                            technical_detail: safe_detail(&error.technical_detail),
                        },
                    }
                } else {
                    WorkerEvent::ExcerptFailed {
                        scope,
                        technical_detail: "MAINTENANCE_IN_PROGRESS".into(),
                    }
                };
                enqueue_worker_event(&mut pending_events, event);
            }
            Ok(WorkerCommand::Quiesce { reply }) => {
                explicitly_quiesced = true;
                db = None;
                enqueue_worker_event(&mut pending_events, WorkerEvent::Maintenance(true));
                let _ = reply.send(Ok(()));
            }
            Ok(WorkerCommand::Resume { reply }) => {
                explicitly_quiesced = false;
                match Db::open(&db_path) {
                    Ok(opened) => {
                        db = Some(opened);
                        last_revisions = None;
                        enqueue_worker_event(&mut pending_events, WorkerEvent::Maintenance(false));
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        let _ = reply.send(Err(safe_detail(&format!("{error:#}"))));
                    }
                }
            }
            Ok(WorkerCommand::Shutdown) | Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
}

fn enqueue_worker_event(pending: &mut VecDeque<WorkerEvent>, event: WorkerEvent) -> bool {
    let replace_at = match &event {
        WorkerEvent::ArticleLoaded { scope, .. } | WorkerEvent::ArticleFailed { scope, .. } => {
            pending.iter().position(|queued| {
                matches!(
                    queued,
                    WorkerEvent::ArticleLoaded {
                        scope: queued_scope,
                        ..
                    } | WorkerEvent::ArticleFailed {
                        scope: queued_scope,
                        ..
                    } if queued_scope == scope
                )
            })
        }
        WorkerEvent::ResourceLoaded { demand, .. } | WorkerEvent::ResourceFailed { demand, .. } => {
            pending.iter().position(|queued| {
                matches!(
                    queued,
                    WorkerEvent::ResourceLoaded {
                        demand: queued_demand,
                        ..
                    } | WorkerEvent::ResourceFailed {
                        demand: queued_demand,
                        ..
                    } if queued_demand == demand
                )
            })
        }
        WorkerEvent::ExcerptLoaded { scope, .. } | WorkerEvent::ExcerptFailed { scope, .. } => {
            pending.iter().position(|queued| {
                matches!(
                    queued,
                    WorkerEvent::ExcerptLoaded {
                        scope: queued_scope,
                        ..
                    } | WorkerEvent::ExcerptFailed {
                        scope: queued_scope,
                        ..
                    } if queued_scope == scope
                )
            })
        }
        WorkerEvent::RevisionObserved { family, .. } => pending.iter().position(|queued| {
            matches!(
                queued,
                WorkerEvent::RevisionObserved {
                    family: queued_family,
                    ..
                } if queued_family == family
            )
        }),
        WorkerEvent::Maintenance(_) => pending
            .iter()
            .position(|queued| matches!(queued, WorkerEvent::Maintenance(_))),
    };
    if let Some(index) = replace_at {
        pending.remove(index);
    }
    if pending.len() >= MAX_PENDING_WORKER_EVENTS {
        return false;
    }
    pending.push_back(event);
    true
}

fn flush_worker_events(
    pending: &mut VecDeque<WorkerEvent>,
    event_tx: &std_mpsc::SyncSender<WorkerEvent>,
    repaint: &eframe::egui::Context,
) -> bool {
    while let Some(event) = pending.pop_front() {
        match event_tx.try_send(event) {
            Ok(()) => repaint.request_repaint(),
            Err(std_mpsc::TrySendError::Full(event)) => {
                pending.push_front(event);
                break;
            }
            Err(std_mpsc::TrySendError::Disconnected(_)) => return false,
        }
    }
    true
}

fn safe_detail(detail: &str) -> String {
    detail.chars().take(2_000).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::article_library_lifecycle::ArticleLibraryCounts;
    use crate::excerpt_thought_lifecycle::ExcerptThoughtCounts;
    use crate::resource_library_lifecycle::{
        ResourceCollection, ResourceCursor, ResourceLibraryCounts,
    };

    fn projection(scope: ProjectionScope, epoch: &str, revision: i64) -> ResourceLibraryProjection {
        ResourceLibraryProjection {
            stamp: ProjectionStamp {
                generation: LibraryGeneration::from_epoch(epoch),
                revision,
            },
            scope,
            resources: Vec::new(),
            detail: None,
            counts: ResourceLibraryCounts::default(),
            next_cursor: None,
        }
    }

    fn article_projection(
        scope: ArticleProjectionScope,
        epoch: &str,
        revision: i64,
    ) -> ArticleLibraryProjection {
        ArticleLibraryProjection {
            stamp: ProjectionStamp {
                generation: LibraryGeneration::from_epoch(epoch),
                revision,
            },
            scope,
            articles: Vec::new(),
            article_ai: HashMap::new(),
            tags: HashMap::new(),
            fixed_bookmark_ids: HashSet::new(),
            counts: ArticleLibraryCounts {
                bookmarks: 0,
                read_later: 0,
                archived: 0,
            },
            feed_unread: Vec::new(),
        }
    }

    fn excerpt_projection(
        scope: ExcerptProjectionScope,
        epoch: &str,
        revision: i64,
    ) -> ExcerptThoughtProjection {
        ExcerptThoughtProjection {
            stamp: ProjectionStamp {
                generation: LibraryGeneration::from_epoch(epoch),
                revision,
            },
            scope,
            excerpts: Vec::new(),
            counts: ExcerptThoughtCounts::default(),
        }
    }

    fn harness() -> (
        DesktopLibraryProjection,
        std_mpsc::Receiver<WorkerCommand>,
        std_mpsc::SyncSender<WorkerEvent>,
    ) {
        let (command_tx, command_rx) = std_mpsc::sync_channel(32);
        let (event_tx, event_rx) = std_mpsc::sync_channel(64);
        (
            DesktopLibraryProjection {
                command_tx,
                event_rx,
                knowledge: KnowledgeProjectionObserver::disconnected_for_test(),
                articles: HashMap::new(),
                resources: HashMap::new(),
                excerpts: HashMap::new(),
                observed_knowledge: HashSet::new(),
                active_generation: None,
                known_article_revision: 0,
                known_resource_revision: 0,
                known_excerpt_revision: 0,
                maintenance: false,
                tick: 0,
                join: None,
            },
            command_rx,
            event_tx,
        )
    }

    #[test]
    fn identical_demand_does_not_schedule_duplicate_loads() {
        let (mut module, commands, _events) = harness();
        let scope = ProjectionScope::collection(ResourceCollection::Active);
        let resource_demand = ResourceProjectionDemand::Collection(ResourceCollection::Active);
        let demand = DesktopProjectionDemand {
            resources: vec![resource_demand],
            ..DesktopProjectionDemand::default()
        };
        module.frame(demand.clone());
        assert!(matches!(
            commands.try_recv(),
            Ok(WorkerCommand::LoadResource {
                demand,
                scope: loaded_scope,
                append: false,
            }) if demand == resource_demand && loaded_scope == scope
        ));
        module.frame(demand);
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn knowledge_residency_exactly_tracks_current_demand() {
        let (mut module, _commands, _events) = harness();
        let observer = module.knowledge.clone();
        let first = TaskKey::new(crate::knowledge_workflow::TaskKind::ArticleSummary, 7);
        let second = TaskKey::new(crate::knowledge_workflow::TaskKind::ResourceCompletion, 9);

        module.frame(DesktopProjectionDemand {
            knowledge: vec![first, first, second],
            ..DesktopProjectionDemand::default()
        });
        assert!(observer.is_resident(first));
        assert!(observer.is_resident(second));

        module.frame(DesktopProjectionDemand {
            knowledge: vec![second],
            ..DesktopProjectionDemand::default()
        });
        assert!(!observer.is_resident(first));
        assert!(observer.is_resident(second));

        module.frame(DesktopProjectionDemand::default());
        assert!(!observer.is_resident(second));

        module.frame(DesktopProjectionDemand {
            knowledge: vec![first],
            ..DesktopProjectionDemand::default()
        });
        assert!(observer.is_resident(first));
        drop(module);
        assert!(!observer.is_resident(first));
    }

    #[test]
    fn a_frame_drains_at_most_thirty_two_worker_facts() {
        let (mut module, _commands, events) = harness();
        for revision in 1..=40 {
            events
                .send(WorkerEvent::RevisionObserved {
                    family: ProjectionFamily::Resource,
                    stamp: ProjectionStamp {
                        generation: LibraryGeneration::from_epoch("one"),
                        revision,
                    },
                })
                .unwrap();
        }
        module.frame(DesktopProjectionDemand::default());
        assert_eq!(module.event_rx.try_iter().count(), 8);
    }

    #[test]
    fn old_generation_results_are_never_adopted() {
        let (mut module, _commands, _events) = harness();
        let scope = ProjectionScope::collection(ResourceCollection::Active);
        module.accept(DesktopProjectionFact::adopt_resource(projection(
            scope, "current", 4,
        )));
        module.accept(DesktopProjectionFact::adopt_resource(projection(
            scope, "old", 99,
        )));
        let frame = module.frame(DesktopProjectionDemand {
            resources: vec![ResourceProjectionDemand::Collection(
                ResourceCollection::Active,
            )],
            ..DesktopProjectionDemand::default()
        });
        let adopted = frame
            .resource(ResourceProjectionDemand::Collection(
                ResourceCollection::Active,
            ))
            .unwrap()
            .data
            .as_ref()
            .unwrap();
        assert_eq!(
            adopted.stamp.generation,
            LibraryGeneration::from_epoch("current")
        );
        assert_eq!(adopted.stamp.revision, 4);
    }

    #[test]
    fn article_and_excerpt_reject_regression_and_invalidate_independently() {
        let (mut module, _commands, _events) = harness();
        let article_scope = ArticleProjectionScope::ArticleBookmarks;
        let excerpt_scope = ExcerptProjectionScope::Library;
        module.accept(DesktopProjectionFact::adopt_article(article_projection(
            article_scope,
            "one",
            3,
        )));
        module.accept(DesktopProjectionFact::adopt_excerpt(excerpt_projection(
            excerpt_scope,
            "one",
            5,
        )));
        module.accept(DesktopProjectionFact::adopt_article(article_projection(
            article_scope,
            "one",
            2,
        )));
        module.accept(DesktopProjectionFact::adopt_excerpt(excerpt_projection(
            excerpt_scope,
            "old",
            99,
        )));

        module.observe_revision(
            ProjectionFamily::Article,
            ProjectionStamp {
                generation: LibraryGeneration::from_epoch("one"),
                revision: 4,
            },
        );
        let frame = module.frame(DesktopProjectionDemand {
            articles: vec![article_scope],
            excerpts: vec![excerpt_scope],
            ..DesktopProjectionDemand::default()
        });
        let article = frame.article(article_scope).unwrap();
        assert_eq!(article.freshness, ProjectionFreshness::Refreshing);
        assert_eq!(article.data.as_ref().unwrap().stamp.revision, 3);
        let excerpt = frame.excerpt(excerpt_scope).unwrap();
        assert_eq!(excerpt.freshness, ProjectionFreshness::Current);
        assert_eq!(excerpt.data.as_ref().unwrap().stamp.revision, 5);
    }

    #[test]
    fn accepted_article_and_excerpt_are_the_frame_authority_and_maintenance_clears_them() {
        let (mut module, _commands, _events) = harness();
        let article_scope = ArticleProjectionScope::ArticleBookmarks;
        let excerpt_scope = ExcerptProjectionScope::Library;
        let mut articles = article_projection(article_scope, "one", 3);
        articles.counts.bookmarks = 7;
        let mut excerpts = excerpt_projection(excerpt_scope, "one", 5);
        excerpts.counts.library_excerpts = 4;

        module.accept(DesktopProjectionFact::adopt_article(articles));
        module.accept(DesktopProjectionFact::adopt_excerpt(excerpts));
        let demand = DesktopProjectionDemand {
            articles: vec![article_scope],
            excerpts: vec![excerpt_scope],
            ..DesktopProjectionDemand::default()
        };
        let frame = module.frame(demand.clone());
        assert_eq!(
            frame
                .article(article_scope)
                .and_then(|view| view.data.as_deref())
                .map(|projection| projection.counts.bookmarks),
            Some(7)
        );
        assert_eq!(
            frame
                .excerpt(excerpt_scope)
                .and_then(|view| view.data.as_deref())
                .map(|projection| projection.counts.library_excerpts),
            Some(4)
        );

        module.accept(DesktopProjectionFact::MaintenanceStarted);
        let frame = module.frame(demand);
        let article = frame.article(article_scope).unwrap();
        assert_eq!(article.freshness, ProjectionFreshness::Maintenance);
        assert!(article.data.is_none());
        let excerpt = frame.excerpt(excerpt_scope).unwrap();
        assert_eq!(excerpt.freshness, ProjectionFreshness::Maintenance);
        assert!(excerpt.data.is_none());
    }

    #[test]
    fn warm_frames_are_memory_only_and_do_not_enqueue_work() {
        let (mut module, commands, _events) = harness();
        let scope = ProjectionScope::collection(ResourceCollection::Active);
        module.accept(DesktopProjectionFact::adopt_resource(projection(
            scope, "one", 1,
        )));
        let resource_demand = ResourceProjectionDemand::Collection(ResourceCollection::Active);
        let demand = DesktopProjectionDemand {
            resources: vec![resource_demand],
            ..DesktopProjectionDemand::default()
        };
        let started = Instant::now();
        for _ in 0..1_000 {
            let frame = module.frame(demand.clone());
            assert!(frame.resource(resource_demand).unwrap().data.is_some());
        }
        assert!(commands.try_recv().is_err());
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn load_more_keeps_the_cursor_inside_the_module() {
        let (mut module, commands, events) = harness();
        let demand = ResourceProjectionDemand::Collection(ResourceCollection::Active);
        let cursor = ResourceCursor {
            updated_at: 42,
            id: 7,
        };
        let mut first = projection(
            ProjectionScope::collection(ResourceCollection::Active),
            "one",
            1,
        );
        first.next_cursor = Some(cursor);
        module.accept(DesktopProjectionFact::adopt_resource(first));

        module.accept(DesktopProjectionFact::LoadMoreResources(
            ResourceCollection::Active,
        ));
        assert!(matches!(
            commands.try_recv(),
            Ok(WorkerCommand::LoadResource {
                demand: loaded_demand,
                scope: ProjectionScope::Collection {
                    collection: ResourceCollection::Active,
                    after: Some(loaded_cursor),
                    ..
                },
                append: true,
            }) if loaded_demand == demand && loaded_cursor == cursor
        ));

        events
            .send(WorkerEvent::ResourceLoaded {
                demand,
                projection: Box::new(projection(
                    ProjectionScope::collection_after(ResourceCollection::Active, cursor),
                    "one",
                    1,
                )),
                append: true,
            })
            .unwrap();
        let frame = module.frame(DesktopProjectionDemand {
            resources: vec![demand],
            ..DesktopProjectionDemand::default()
        });
        let view = frame.resource(demand).unwrap();
        assert!(!view.has_more);
        assert_eq!(
            view.data.as_ref().unwrap().scope,
            ProjectionScope::collection(ResourceCollection::Active)
        );
    }

    #[test]
    fn a_full_delivery_channel_does_not_drop_a_worker_result() {
        let (event_tx, event_rx) = std_mpsc::sync_channel(1);
        event_tx.send(WorkerEvent::Maintenance(false)).unwrap();
        let mut pending = VecDeque::new();
        enqueue_worker_event(
            &mut pending,
            WorkerEvent::ResourceFailed {
                demand: ResourceProjectionDemand::Detail(9),
                technical_detail: "expected".into(),
            },
        );
        let repaint = eframe::egui::Context::default();

        assert!(flush_worker_events(&mut pending, &event_tx, &repaint));
        assert_eq!(pending.len(), 1);
        assert!(matches!(
            event_rx.recv().unwrap(),
            WorkerEvent::Maintenance(false)
        ));
        assert!(flush_worker_events(&mut pending, &event_tx, &repaint));
        assert!(pending.is_empty());
        assert!(matches!(
            event_rx.recv().unwrap(),
            WorkerEvent::ResourceFailed {
                demand: ResourceProjectionDemand::Detail(9),
                ..
            }
        ));
    }

    #[test]
    fn another_connection_revisions_are_adopted_for_every_library_family() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "shiyue-desktop-projection-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db_path = root.join("library.db");
        drop(Db::open(&db_path).unwrap());
        let mut module = DesktopLibraryProjection::start(
            db_path.clone(),
            KnowledgeProjectionObserver::disconnected_for_test(),
            eframe::egui::Context::default(),
        )
        .unwrap();
        let article_scope = ArticleProjectionScope::ArticleBookmarks;
        let resource_demand = ResourceProjectionDemand::Collection(ResourceCollection::Active);
        let excerpt_scope = ExcerptProjectionScope::Library;
        let frame_demand = DesktopProjectionDemand {
            articles: vec![article_scope],
            resources: vec![resource_demand],
            excerpts: vec![excerpt_scope],
            ..DesktopProjectionDemand::default()
        };
        let initial_deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let frame = module.frame(frame_demand.clone());
            if frame
                .article(article_scope)
                .and_then(|view| view.data.as_ref())
                .is_some()
                && frame
                    .resource(resource_demand)
                    .and_then(|view| view.data.as_ref())
                    .is_some()
                && frame
                    .excerpt(excerpt_scope)
                    .and_then(|view| view.data.as_ref())
                    .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < initial_deadline,
                "initial projection timed out"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        let mut external = Db::open(&db_path).unwrap();
        let tx = external.conn.transaction().unwrap();
        let revisions = library_projection_revision::record(
            &tx,
            crate::library_projection_revision::ProjectionImpact::article()
                .with(ProjectionFamily::Resource)
                .with(ProjectionFamily::Excerpt),
        )
        .unwrap();
        tx.commit().unwrap();
        let started = Instant::now();
        let deadline = started + Duration::from_millis(750);
        loop {
            let frame = module.frame(frame_demand.clone());
            let all_current = frame
                .article(article_scope)
                .and_then(|view| view.data.as_ref())
                .is_some_and(|projection| projection.stamp.revision == revisions.article)
                && frame
                    .resource(resource_demand)
                    .and_then(|view| view.data.as_ref())
                    .is_some_and(|projection| projection.stamp.revision == revisions.resource)
                && frame
                    .excerpt(excerpt_scope)
                    .and_then(|view| view.data.as_ref())
                    .is_some_and(|projection| projection.stamp.revision == revisions.excerpt);
            if all_current {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "cross-process revision was not adopted within 750ms"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(started.elapsed() < Duration::from_millis(750));

        drop(external);
        drop(module);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn gui_features_consume_library_frames_without_parallel_projection_state() {
        let gui = include_str!("gui.rs");
        let resource_feature = include_str!("gui/resource_feature.rs");
        assert!(!gui.contains("ArticleLibraryLifecycle::new(&self.db).project"));
        assert!(!gui.contains("ExcerptThoughtLifecycle::new(&self.db, &SYSTEM_CLOCK).project"));
        assert!(!gui.contains("self.db.article_ai"));
        assert!(gui.contains("fn article_projection("));
        assert!(gui.contains(".article(scope)"));
        assert!(gui.contains("fn excerpt_projection("));
        assert!(gui.contains(".excerpt(scope)"));
        assert!(!gui.contains("article_projection_stamp:"));
        assert!(!gui.contains("articles: Vec<Article>"));
        assert!(!gui.contains("article_ai: HashMap"));
        assert!(!gui.contains("excerpt_projection: Option"));
        for source in [gui, resource_feature] {
            assert!(!source.contains(".project(ProjectionScope::Resource"));
            assert!(!source.contains("resource_projection: Option"));
        }
    }
}
