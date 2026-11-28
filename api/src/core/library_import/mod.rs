//! Core backend engine for 1-Click Existing Library Import.
//!
//! Scans unimported audio files against the folder's beets database, imports them
//! sequentially (or in chunks for `register_as_is`) with full pause/resume/cancel
//! persistence, and lets users resolve `needs_decision` or `failed` units via the
//! Match Picker without ever deleting source files.

pub mod resolve;
pub mod runner;
pub mod scan;

pub use resolve::resolve_item;
pub use runner::{
    cancel_batch, get_batch_detail, get_batch_detail_in, list_batches, pause_batch,
    recover_on_startup, resume_batch, scan_preview, scan_preview_in, start_batch,
    LibraryImportCoordinator, COORDINATOR,
};
pub use scan::{resolve_and_validate_source_path_in, scan_unimported_units, ScanResult, ScannedUnit};
