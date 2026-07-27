//! Recorder: appends bytes as `rx` records into append-only JSONL segments,
//! independent of line boundaries.
//!
//! Not yet implemented — see `TASKS.md` T1.2 (segment files, ring eviction,
//! fsync, crash-safe recovery).

use wrap_proto::Record;

/// Placeholder for the append-only recorder.
///
/// Will eventually own segment files and per-device ring eviction. Kept as
/// a skeleton type here so `serialwrapd` demonstrates depending on
/// `wrap-proto` for its record shape.
#[derive(Debug, Default)]
pub struct Recorder;

impl Recorder {
    /// Skeleton constructor.
    pub fn new() -> Self {
        Self
    }

    /// Placeholder append; not implemented yet.
    pub fn append(&mut self, _record: &Record) {
        unimplemented!("Recorder::append is not implemented yet (see TASKS.md T1.2)")
    }
}
