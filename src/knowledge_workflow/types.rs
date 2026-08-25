use anyhow::{Result, bail};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum TaskKind {
    ResourceCompletion,
    ArticleSummary,
}

impl TaskKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::ResourceCompletion => "resource_completion",
            Self::ArticleSummary => "article_summary",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "resource_completion" => Ok(Self::ResourceCompletion),
            "article_summary" => Ok(Self::ArticleSummary),
            _ => bail!("unknown knowledge task kind: {value}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct TaskKey {
    pub kind: TaskKind,
    pub target_id: i64,
}

impl TaskKey {
    pub(crate) fn new(kind: TaskKind, target_id: i64) -> Self {
        Self { kind, target_id }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStatus {
    Queued,
    Running,
    Succeeded,
    Failed,
    Interrupted,
}

impl TaskStatus {
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "interrupted" => Ok(Self::Interrupted),
            _ => bail!("unknown knowledge task status: {value}"),
        }
    }

    pub(crate) fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Interrupted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskStage {
    Fetching,
    Organizing,
    Summarizing,
}

impl TaskStage {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Fetching => "fetching",
            Self::Organizing => "organizing",
            Self::Summarizing => "summarizing",
        }
    }

    pub(super) fn parse(value: Option<String>) -> Result<Option<Self>> {
        value
            .map(|value| match value.as_str() {
                "fetching" => Ok(Self::Fetching),
                "organizing" => Ok(Self::Organizing),
                "summarizing" => Ok(Self::Summarizing),
                _ => bail!("unknown knowledge task stage: {value}"),
            })
            .transpose()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorKind {
    Transient,
    Authentication,
    Security,
    Input,
    ProviderOutput,
    Storage,
    Interrupted,
}

impl ErrorKind {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Authentication => "authentication",
            Self::Security => "security",
            Self::Input => "input",
            Self::ProviderOutput => "provider_output",
            Self::Storage => "storage",
            Self::Interrupted => "interrupted",
        }
    }

    pub(super) fn parse(value: Option<String>) -> Result<Option<Self>> {
        value
            .map(|value| match value.as_str() {
                "transient" => Ok(Self::Transient),
                "authentication" => Ok(Self::Authentication),
                "security" => Ok(Self::Security),
                "input" => Ok(Self::Input),
                "provider_output" => Ok(Self::ProviderOutput),
                "storage" => Ok(Self::Storage),
                "interrupted" => Ok(Self::Interrupted),
                _ => bail!("unknown knowledge task error kind: {value}"),
            })
            .transpose()
    }

    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::Transient => "WORKFLOW_TRANSIENT",
            Self::Authentication => "WORKFLOW_AUTHENTICATION",
            Self::Security => "WORKFLOW_SECURITY",
            Self::Input => "WORKFLOW_TARGET_INVALID",
            Self::ProviderOutput => "WORKFLOW_PROVIDER_OUTPUT",
            Self::Storage => "WORKFLOW_STORAGE",
            Self::Interrupted => "WORKFLOW_INTERRUPTED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskSnapshot {
    pub task_id: i64,
    pub key: TaskKey,
    pub status: TaskStatus,
    pub current_stage: Option<TaskStage>,
    pub attempt_number: i64,
    pub automatic_retry: bool,
    pub error_kind: Option<ErrorKind>,
    pub user_message: Option<String>,
    pub technical_detail: Option<String>,
    pub change_seq: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestDisposition {
    Created,
    Existing,
    Retried,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestReceipt {
    pub key: TaskKey,
    pub disposition: RequestDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConnectionState {
    Running,
    Succeeded(String),
    Failed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KnowledgeNotice {
    Changed(TaskKey),
    ConnectionChanged(ConnectionState),
    ModuleFault {
        user_message: String,
        technical_detail: String,
    },
}
