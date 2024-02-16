//! Exposition of Metriken metrics
//!
//! Provides a standardized struct for a snapshot of the metric readings as well
//! as a way of producing the snapshots.

use std::time::SystemTime;

use chrono::{DateTime, Utc};
pub use histogram::Snapshot as HistogramSnapshot;
use metriken::{AtomicHistogram, RwLockHistogram, Value};

mod snapshot;
mod snapshotter;

pub use snapshot::Snapshot;
pub use snapshotter::{Snapshotter, SnapshotterBuilder};
