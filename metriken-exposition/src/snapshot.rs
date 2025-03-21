use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime};

#[cfg(feature = "msgpack")]
use rmp_serde::encode::Error as SerializeMsgpackError;
#[cfg(feature = "json")]
use serde_json::Error as JsonError;

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Counter {
    pub name: String,
    pub value: u64,
    pub metadata: HashMap<String, String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Gauge {
    pub name: String,
    pub value: i64,
    pub metadata: HashMap<String, String>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Histogram {
    pub name: String,
    pub value: histogram::Histogram,
    pub metadata: HashMap<String, String>,
}

/// Contains a snapshot of metric readings.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapshotV1 {
    pub systemtime: SystemTime,

    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: HashMap<String, String>,

    pub counters: Vec<Counter>,
    pub gauges: Vec<Gauge>,
    pub histograms: Vec<Histogram>,
}

/// Contains a snapshot of metric readings.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SnapshotV2 {
    pub systemtime: SystemTime,
    pub duration: Duration,

    #[cfg_attr(feature = "serde", serde(default))]
    pub metadata: HashMap<String, String>,

    pub counters: Vec<Counter>,
    pub gauges: Vec<Gauge>,
    pub histograms: Vec<Histogram>,
}

#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(untagged))]
pub enum Snapshot {
    V1(SnapshotV1),
    V2(SnapshotV2),
}

#[cfg(feature = "parquet")]
pub(crate) struct HashedSnapshot {
    pub(crate) ts: u64,
    pub(crate) duration: Option<u64>,
    pub(crate) counters: HashMap<String, Counter>,
    pub(crate) gauges: HashMap<String, Gauge>,
    pub(crate) histograms: HashMap<String, Histogram>,
}

/// Return the metric name: for Rezolus v4 data, this is the metric name
/// from the snapshot. Rezolus v5 snapshots have metrics with opaque names
/// with the real name being in the metadata.
pub(crate) fn canonicalize_metric_name(
    snapshot_name: &str,
    metadata: &HashMap<String, String>,
) -> String {
    // If the metric key doesn't exist, it is old-style data and return as-is.
    let Some(name) = metadata.get("metric") else {
        return snapshot_name.to_string();
    };

    let metadata: BTreeMap<&str, &str> = metadata
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();

    // Separate keys into key's with a specific desired ordering and keys to be
    // ignored. We are indifferent to the ordering of keys in neither of these buckets.
    let ordered = ["name", "op", "state", "direction"];
    let mut ignore: HashSet<&str> =
        ["metric", "unit", "grouping_power", "max_value_power", "id"].into();
    ignore.extend(ordered);

    let mut unique_name = name.to_string();

    // Append name, op, state, and direction in specified order
    for k in ordered {
        if let Some(v) = metadata.get(&k) {
            unique_name = unique_name + "/" + *v;
        }
    }

    // Append remaining keys in any order to ensure uniqueness
    for (k, v) in &metadata {
        if ignore.contains(*k) {
            continue;
        }
        unique_name = unique_name + "/" + v;
    }

    // Append "id", if it exists, to the very end
    if let Some(v) = metadata.get("id") {
        unique_name = unique_name + "/" + v;
    }

    unique_name
}

impl Snapshot {
    pub fn systemtime(&self) -> SystemTime {
        match self {
            Snapshot::V1(s) => s.systemtime,
            Snapshot::V2(s) => s.systemtime,
        }
    }

    pub fn duration(&self) -> Option<Duration> {
        match self {
            Snapshot::V1(_) => None,
            Snapshot::V2(s) => Some(s.duration),
        }
    }

    pub fn metadata(&mut self) -> HashMap<String, String> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.metadata),
            Snapshot::V2(s) => std::mem::take(&mut s.metadata),
        }
    }

    pub fn counters(&mut self) -> Vec<Counter> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.counters),
            Snapshot::V2(s) => std::mem::take(&mut s.counters),
        }
    }

    pub fn gauges(&mut self) -> Vec<Gauge> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.gauges),
            Snapshot::V2(s) => std::mem::take(&mut s.gauges),
        }
    }

    pub fn histograms(&mut self) -> Vec<Histogram> {
        match self {
            Snapshot::V1(s) => std::mem::take(&mut s.histograms),
            Snapshot::V2(s) => std::mem::take(&mut s.histograms),
        }
    }

    #[cfg(feature = "json")]
    pub fn to_json<T>(val: &T) -> Result<Vec<u8>, JsonError>
    where
        T: serde::Serialize + ?Sized,
    {
        let mut res = serde_json::to_vec(val)?;
        res.push(b'\n');
        Ok(res)
    }

    #[cfg(feature = "msgpack")]
    pub fn to_msgpack<T>(val: &T) -> Result<Vec<u8>, SerializeMsgpackError>
    where
        T: serde::Serialize + ?Sized,
    {
        rmp_serde::encode::to_vec(val)
    }
}

#[cfg(feature = "parquet")]
impl From<Snapshot> for HashedSnapshot {
    fn from(mut snapshot: Snapshot) -> Self {
        let ts: u64 = snapshot
            .systemtime()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("System Clock is earlier than 1970; needs reset")
            .as_nanos() as u64;

        let duration: Option<u64> = snapshot.duration().map(|x| x.as_nanos() as u64);

        let counters: HashMap<String, Counter> = HashMap::from_iter(
            snapshot
                .counters()
                .into_iter()
                .map(|v| (canonicalize_metric_name(&v.name, &v.metadata), v)),
        );
        let gauges: HashMap<String, Gauge> = HashMap::from_iter(
            snapshot
                .gauges()
                .into_iter()
                .map(|v| (canonicalize_metric_name(&v.name, &v.metadata), v)),
        );
        let histograms: HashMap<String, Histogram> = HashMap::from_iter(
            snapshot
                .histograms()
                .into_iter()
                .map(|v| (canonicalize_metric_name(&v.name, &v.metadata), v)),
        );

        Self {
            ts,
            duration,
            counters,
            gauges,
            histograms,
        }
    }
}
