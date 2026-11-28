use serde::{Deserialize, Serialize};

use crate::download::{ImportChoice, MatchChoices};
use crate::failure::Failure;

/// How an existing library import processes audio files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryImportMode {
    /// Autotag via MusicBrainz/beets and organize into `Artist/Album/Track`.
    /// Copies when importing from an external source folder; moves when importing in place.
    TagAndOrganize,
    /// Autotag files in place without copying or moving them (`--nocopy --nomove`).
    TagInPlace,
    /// Register files directly in the beets database without moving, copying, or writing tags.
    RegisterAsIs,
}

impl LibraryImportMode {
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryImportMode::TagAndOrganize => "tag_and_organize",
            LibraryImportMode::TagInPlace => "tag_in_place",
            LibraryImportMode::RegisterAsIs => "register_as_is",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "tag_and_organize" => Some(LibraryImportMode::TagAndOrganize),
            "tag_in_place" => Some(LibraryImportMode::TagInPlace),
            "register_as_is" => Some(LibraryImportMode::RegisterAsIs),
            _ => None,
        }
    }
}

/// Lifecycle state of a library import batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryImportBatchStatus {
    Queued,
    Running,
    Paused,
    Completed,
    Cancelled,
    Failed,
}

impl LibraryImportBatchStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryImportBatchStatus::Queued => "queued",
            LibraryImportBatchStatus::Running => "running",
            LibraryImportBatchStatus::Paused => "paused",
            LibraryImportBatchStatus::Completed => "completed",
            LibraryImportBatchStatus::Cancelled => "cancelled",
            LibraryImportBatchStatus::Failed => "failed",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(LibraryImportBatchStatus::Queued),
            "running" => Some(LibraryImportBatchStatus::Running),
            "paused" => Some(LibraryImportBatchStatus::Paused),
            "completed" => Some(LibraryImportBatchStatus::Completed),
            "cancelled" => Some(LibraryImportBatchStatus::Cancelled),
            "failed" => Some(LibraryImportBatchStatus::Failed),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            LibraryImportBatchStatus::Completed
                | LibraryImportBatchStatus::Cancelled
                | LibraryImportBatchStatus::Failed
        )
    }
}

/// Processing state of a single unit (album directory or loose track) in a library import batch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryImportItemStatus {
    Pending,
    Importing,
    Imported,
    NeedsDecision,
    SkippedDuplicate,
    Failed,
    Dismissed,
}

impl LibraryImportItemStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryImportItemStatus::Pending => "pending",
            LibraryImportItemStatus::Importing => "importing",
            LibraryImportItemStatus::Imported => "imported",
            LibraryImportItemStatus::NeedsDecision => "needs_decision",
            LibraryImportItemStatus::SkippedDuplicate => "skipped_duplicate",
            LibraryImportItemStatus::Failed => "failed",
            LibraryImportItemStatus::Dismissed => "dismissed",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(LibraryImportItemStatus::Pending),
            "importing" => Some(LibraryImportItemStatus::Importing),
            "imported" => Some(LibraryImportItemStatus::Imported),
            "needs_decision" => Some(LibraryImportItemStatus::NeedsDecision),
            "skipped_duplicate" => Some(LibraryImportItemStatus::SkippedDuplicate),
            "failed" => Some(LibraryImportItemStatus::Failed),
            "dismissed" => Some(LibraryImportItemStatus::Dismissed),
            _ => None,
        }
    }
}

/// Where a library import batch was initiated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LibraryImportOrigin {
    Ui,
    Mcp,
}

impl LibraryImportOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            LibraryImportOrigin::Ui => "ui",
            LibraryImportOrigin::Mcp => "mcp",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "ui" => Some(LibraryImportOrigin::Ui),
            "mcp" => Some(LibraryImportOrigin::Mcp),
            _ => None,
        }
    }
}

/// Preview of a single album directory or loose singleton file discovered during a scan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryScanUnitPreview {
    pub source_path: String,
    pub display_name: String,
    pub is_album_dir: bool,
    pub file_count: u64,
}

/// Read-only preview of unimported audio files discovered in a folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryScanPreview {
    pub source_path: String,
    pub target_path: String,
    pub in_place: bool,
    pub total_units: u64,
    pub album_units: u64,
    pub singleton_units: u64,
    pub total_files: u64,
    pub already_indexed_files: u64,
    pub sample_units: Vec<LibraryScanUnitPreview>,
}

/// Persistent summary of a library import batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryImportBatch {
    pub id: String,
    pub user_id: String,
    pub target_folder_id: String,
    pub target_path: String,
    pub source_path: String,
    pub mode: LibraryImportMode,
    pub status: LibraryImportBatchStatus,
    pub total_units: u64,
    pub total_files: u64,
    pub completed_units: u64,
    pub imported_units: u64,
    pub needs_decision_units: u64,
    pub skipped_units: u64,
    pub failed_units: u64,
    pub already_indexed_files: u64,
    pub origin: LibraryImportOrigin,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub finished_at: Option<String>,
}

/// Persistent item within a library import batch (one album folder or one loose audio file).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryImportItem {
    pub id: String,
    pub batch_id: String,
    pub seq: u64,
    pub source_path: String,
    pub display_name: String,
    pub is_album_dir: bool,
    pub file_count: u64,
    pub status: LibraryImportItemStatus,
    pub skip_kind: Option<String>,
    pub failure: Option<Failure>,
    pub choices: Option<MatchChoices>,
    pub updated_at: String,
}

/// Batch details with a paginated/filtered slice of items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryImportBatchDetail {
    pub batch: LibraryImportBatch,
    pub items: Vec<LibraryImportItem>,
    pub total_matching_items: u64,
}

/// Resolution action for an item waiting in `needs_decision` or `failed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LibraryImportResolveAction {
    Choose(ImportChoice),
    Dismiss,
}

/// Live WebSocket event emitted as a library import batch progresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryImportEvent {
    pub batch: LibraryImportBatch,
    pub latest_item: Option<LibraryImportItem>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_wire_names_round_trip() {
        for mode in [
            LibraryImportMode::TagAndOrganize,
            LibraryImportMode::TagInPlace,
            LibraryImportMode::RegisterAsIs,
        ] {
            let s = mode.as_str();
            assert_eq!(LibraryImportMode::from_str(s), Some(mode));
            assert_eq!(serde_json::to_value(mode).unwrap(), serde_json::json!(s));
        }

        for status in [
            LibraryImportBatchStatus::Queued,
            LibraryImportBatchStatus::Running,
            LibraryImportBatchStatus::Paused,
            LibraryImportBatchStatus::Completed,
            LibraryImportBatchStatus::Cancelled,
            LibraryImportBatchStatus::Failed,
        ] {
            let s = status.as_str();
            assert_eq!(LibraryImportBatchStatus::from_str(s), Some(status));
            assert_eq!(serde_json::to_value(status).unwrap(), serde_json::json!(s));
        }

        for status in [
            LibraryImportItemStatus::Pending,
            LibraryImportItemStatus::Importing,
            LibraryImportItemStatus::Imported,
            LibraryImportItemStatus::NeedsDecision,
            LibraryImportItemStatus::SkippedDuplicate,
            LibraryImportItemStatus::Failed,
            LibraryImportItemStatus::Dismissed,
        ] {
            let s = status.as_str();
            assert_eq!(LibraryImportItemStatus::from_str(s), Some(status));
            assert_eq!(serde_json::to_value(status).unwrap(), serde_json::json!(s));
        }

        for origin in [LibraryImportOrigin::Ui, LibraryImportOrigin::Mcp] {
            let s = origin.as_str();
            assert_eq!(LibraryImportOrigin::from_str(s), Some(origin));
            assert_eq!(serde_json::to_value(origin).unwrap(), serde_json::json!(s));
        }
    }
}
