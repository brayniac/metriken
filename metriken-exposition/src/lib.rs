//! Exposition of Metriken metrics
//!
//! Provides a standardized struct for a snapshot of the metric readings as well
//! as a way of producing the snapshots.

// use core::error::Error;

pub use histogram::Snapshot as HistogramSnapshot;

mod error;
mod snapshot;
mod snapshotter;

pub use error::Error;
pub use snapshot::Snapshot;
pub use snapshotter::{Snapshotter, SnapshotterBuilder};
