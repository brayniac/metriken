//! One series whose sample spacing changes: rows every 1 s, then every
//! 100 ms, then every 1 s again (and the reverse). Every query is compared
//! with values computed from the true counter, through a `MemoryStore` (one
//! sample stream per series) and through an `ArchiveReader` (the batched
//! rate path, and per series), with each interval hint the sources can
//! report for such a table.
//!
//! The bounds under test take the spacing of the samples near each point
//! (`promql::streaming::spacing`), and the reach, a gauge's hold and a raw
//! pair's gap are capped at what the source's interval hint gives. An
//! archive table's hint is at least the longest row gap its segments record
//! over the query's window, so through an archive everything is exact whatever its
//! probed interval; a `MemoryStore` declaring an interval finer than its rows
//! is still held to it
//! ([`a_store_declaring_a_finer_interval_than_its_rows_is_held_to_it`]). Design
//! and measurement: `docs/journal/2026-10-19-recording-identity-and-views.md`,
//! rules S10–S12.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use arrow::array::{ArrayRef, Int64Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use metriken_storage::{ArchiveReader, BufferPool, HistogramSnapshot, InMemorySource};
use parquet::arrow::ArrowWriter;
use parquet::file::metadata::KeyValue;
use parquet::file::properties::WriterProperties;

use crate::labels::Labels;
use crate::{MemoryStore, MetricsSource, QueryOptions, QueryResult, RateMode};

const S: u64 = 1_000_000_000;
const MS: u64 = 1_000_000;
const T0: u64 = 1000 * S;
/// The middle span: `[F0, F1)`.
const F0: u64 = 1060 * S;
const F1: u64 = 1120 * S;
const TEND: u64 = 1180 * S;
/// The query range, in seconds.
const START: f64 = 1010.0;
const END: f64 = 1170.0;

/// Rows every 1 s before `F0`, every 100 ms in `[F0, F1)`, every 1 s to
/// `TEND`.
fn coarse_fine_coarse() -> Vec<u64> {
    let mut ts: Vec<u64> = (T0..F0).step_by(S as usize).collect();
    ts.extend((F0..F1).step_by((100 * MS) as usize));
    ts.extend((F1..=TEND).step_by(S as usize));
    ts
}

/// Rows every 100 ms before `F0`, every 1 s in `[F0, F1)`, every 100 ms to
/// `TEND`: the first samples read are fine.
fn fine_coarse_fine() -> Vec<u64> {
    let mut ts: Vec<u64> = (T0..F0).step_by((100 * MS) as usize).collect();
    ts.extend((F0..F1).step_by(S as usize));
    ts.extend((F1..=TEND).step_by((100 * MS) as usize));
    ts
}

/// Counter `c`: 10/s throughout.
fn c_const(t: u64) -> f64 {
    (t - T0) as f64 / 1e9 * 10.0
}

/// Counter `c2`: 10/s, 30/s over `[F0, F1)`, then 10/s. The rate changes at
/// rows of both fixtures, so linear interpolation between rows is exact.
fn c_step(t: u64) -> f64 {
    let sec = |a: u64, b: u64| b.saturating_sub(a) as f64 / 1e9;
    10.0 * sec(T0, t.min(F0)) + 30.0 * sec(F0, t.clamp(F0, F1)) + 10.0 * sec(F1, t.max(F1))
}

/// Gauge `g`: deciseconds since `T0`, so the value says which row was read.
fn g_of(t: u64) -> i64 {
    ((t - T0) / (100 * MS)) as i64
}

/// Acquisition window: 1 ms either side of the stamp.
fn win(t: u64) -> (u64, u64) {
    (t - MS, t + MS)
}

// ───────────────────────── sources ─────────────────────────

fn store(ts: &[u64], interval_ms: u64) -> MemoryStore {
    let st = MemoryStore::builder()
        .sampling_interval_ms(interval_ms)
        .build();
    let w: Vec<(u64, u64)> = ts.iter().map(|&t| win(t)).collect();
    for (name, f) in [("c", c_const as fn(u64) -> f64), ("c2", c_step)] {
        st.insert_counter_series(
            name,
            Labels::default(),
            ts.to_vec(),
            ts.iter().map(|&t| f(t).round() as u64).collect(),
            Some(w.clone()),
        )
        .unwrap();
    }
    st.insert_gauge_series(
        "g",
        Labels::default(),
        ts.to_vec(),
        ts.iter().map(|&t| g_of(t)).collect(),
        Some(w),
    )
    .unwrap();
    st
}

fn field(name: &str, kind: &str, dt: DataType) -> Field {
    let mut meta = HashMap::new();
    meta.insert("metric".to_string(), name.to_string());
    meta.insert("metric_type".to_string(), kind.to_string());
    Field::new(name, dt, true).with_metadata(meta)
}

/// One group table holding `c`, `c2` and `g` with a table-level window
/// pair, declaring `sampling_interval_ms` when `declared_ms` is set.
fn segment(ts: &[u64], declared_ms: Option<u64>) -> Vec<u8> {
    segment_with(ts, declared_ms, None)
}

/// [`segment`] whose footer records `spacing` under
/// `metriken_storage::spacing::FOOTER_KEY`, as a segment the archive writer
/// sealed does; [`segment`]'s records none, as one sealed before the key.
fn segment_with(ts: &[u64], declared_ms: Option<u64>, spacing: Option<String>) -> Vec<u8> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new(":window_begin", DataType::Int64, true),
        Field::new(":window_width", DataType::UInt64, true),
        field("c", "counter", DataType::UInt64),
        field("c2", "counter", DataType::UInt64),
        field("g", "gauge", DataType::Int64),
    ]));
    let counter = |f: fn(u64) -> f64| -> ArrayRef {
        Arc::new(UInt64Array::from(
            ts.iter().map(|&t| f(t).round() as u64).collect::<Vec<_>>(),
        ))
    };
    let cols: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(ts.to_vec())),
        Arc::new(Int64Array::from(vec![-(MS as i64); ts.len()])),
        Arc::new(UInt64Array::from(vec![2 * MS; ts.len()])),
        counter(c_const),
        counter(c_step),
        Arc::new(Int64Array::from(
            ts.iter().map(|&t| g_of(t)).collect::<Vec<_>>(),
        )),
    ];
    let mut kv: Vec<KeyValue> = declared_ms
        .map(|ms| KeyValue::new("sampling_interval_ms".to_string(), ms.to_string()))
        .into_iter()
        .collect();
    kv.extend(spacing.map(|v| KeyValue::new(metriken_storage::spacing::FOOTER_KEY.to_string(), v)));
    let props = WriterProperties::builder()
        .set_key_value_metadata((!kv.is_empty()).then_some(kv))
        .build();
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).unwrap();
    w.write(&RecordBatch::try_new(schema, cols).unwrap())
        .unwrap();
    w.close().unwrap();
    buf
}

/// A one-column counter table `other` growing 10/s, rows every `every_ms`.
fn steady(every_ms: u64) -> Vec<u8> {
    let rows: Vec<u64> = (T0..=TEND).step_by((every_ms * MS) as usize).collect();
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        field("other", "counter", DataType::UInt64),
    ]));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(rows.clone())),
        Arc::new(UInt64Array::from(
            rows.iter()
                .map(|&t| c_const(t).round() as u64)
                .collect::<Vec<_>>(),
        )),
    ];
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
    w.write(&RecordBatch::try_new(schema, cols).unwrap())
        .unwrap();
    w.close().unwrap();
    buf
}

fn archive(tables: Vec<(&str, Vec<Vec<u8>>)>) -> ArchiveReader {
    let labels: BTreeMap<String, String> = [("source".to_string(), "a".to_string())].into();
    ArchiveReader::from_in_memory(
        vec![InMemorySource {
            name: "a".into(),
            metadata: labels.clone(),
            labels,
            complete: true,
            tables: tables
                .into_iter()
                .map(|(name, segs)| (name.to_string(), segs))
                .collect(),
        }],
        None,
        BufferPool::new(64 << 20),
    )
    .unwrap()
}

/// The sources the fixture is read through whose interval hint is the
/// coarse 1 s: a `MemoryStore` declaring it, an archive table declaring it,
/// and, for rows that start coarse, an archive table of three segments
/// whose first segment's median gap it is.
fn coarse_hint_sources(ts: &[u64]) -> Vec<(String, Box<dyn MetricsSource>)> {
    let mut out: Vec<(String, Box<dyn MetricsSource>)> = vec![
        ("mem 1000ms".into(), Box::new(store(ts, 1000))),
        (
            "archive 1000ms".into(),
            Box::new(archive(vec![("s/grp", vec![segment(ts, Some(1000))])])),
        ),
    ];
    if ts[1] - ts[0] == S {
        out.push(("archive 3 segments".into(), Box::new(three_segments(ts))));
    }
    out
}

/// The archive sources whose probed interval is finer than the coarse rows:
/// one segment, whose median gap is 100 ms, with its footer recording its
/// spacing (as the writer seals it), recording none (as a segment sealed
/// before the key, measured when a query reads it), and recording a version
/// this reader does not read (measured too); one declaring 250 ms (what a
/// catalog's span over gaps gives for the coarse-fine-coarse rows); and, for
/// rows that start fine, three segments whose first is fine.
fn fine_hint_archives(ts: &[u64]) -> Vec<(String, Box<dyn MetricsSource>)> {
    let recorded =
        metriken_storage::spacing::encode_spacing(metriken_storage::spacing::segment_spacing(ts));
    let one = |spacing: Option<String>| -> Box<dyn MetricsSource> {
        Box::new(archive(vec![(
            "s/grp",
            vec![segment_with(ts, None, spacing)],
        )]))
    };
    let mut out: Vec<(String, Box<dyn MetricsSource>)> = vec![
        ("archive median recorded".into(), one(Some(recorded))),
        ("archive median".into(), one(None)),
        ("archive median v2".into(), one(Some("2:1,2,3,4".into()))),
        (
            "archive 250ms".into(),
            Box::new(archive(vec![("s/grp", vec![segment(ts, Some(250))])])),
        ),
    ];
    if ts[1] - ts[0] < S {
        out.push(("archive 3 segments".into(), Box::new(three_segments(ts))));
    }
    out
}

/// Every source above: those whose hint is the coarse interval, and the
/// archives whose probed interval is finer.
fn exact_sources(ts: &[u64]) -> Vec<(String, Box<dyn MetricsSource>)> {
    let mut out = coarse_hint_sources(ts);
    out.extend(fine_hint_archives(ts));
    out
}

/// `ts` as three segments, split where the spacing changes.
fn three_segments(ts: &[u64]) -> ArchiveReader {
    let split =
        |a: u64, b: u64| -> Vec<u64> { ts.iter().copied().filter(|&t| t >= a && t < b).collect() };
    archive(vec![(
        "s/grp",
        vec![
            segment(&split(T0, F0), None),
            segment(&split(F0, F1), None),
            segment(&split(F1, TEND + 1), None),
        ],
    )])
}

// ───────────────────────── evaluation ─────────────────────────

#[derive(Clone, Copy, Debug)]
struct P {
    t: u64,
    v: f64,
    band: Option<(f64, f64)>,
}

fn run(src: &dyn MetricsSource, q: &str, step: f64, opts: &QueryOptions) -> Vec<P> {
    let r = src
        .query_range_opts(q, START, END, step, opts)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    let QueryResult::Matrix { result } = r else {
        panic!("{q}: not a matrix");
    };
    let Some(m) = result.into_iter().next() else {
        return Vec::new();
    };
    m.values
        .iter()
        .enumerate()
        .map(|(i, &(t, v))| P {
            t: (t * 1e9).round() as u64,
            v,
            band: m.bands.as_ref().and_then(|b| b[i]),
        })
        .collect()
}

/// What one query got wrong, by count.
#[derive(Default, Debug, PartialEq)]
struct Misses {
    /// Points expected and absent.
    missing: usize,
    /// Points further than the tolerance from the truth.
    wrong: usize,
    /// Rate points without a band, or with one that excludes the truth.
    band: usize,
}

impl Misses {
    fn clean(&self) -> bool {
        *self == Misses::default()
    }
}

/// Score `points` against `expect` at `times`.
fn score(
    points: &[P],
    times: &[u64],
    expect: &dyn Fn(u64) -> f64,
    tol: f64,
    banded: bool,
) -> Misses {
    let by_t: BTreeMap<u64, P> = points.iter().map(|p| (p.t, *p)).collect();
    let mut m = Misses::default();
    for &t in times {
        let Some(p) = by_t.get(&t) else {
            m.missing += 1;
            continue;
        };
        let e = expect(t);
        if (p.v - e).abs() > tol {
            m.wrong += 1;
        }
        if banded
            && !p
                .band
                .is_some_and(|(lo, hi)| lo - 1e-9 <= e && e <= hi + 1e-9)
        {
            m.band += 1;
        }
    }
    m
}

/// The grid points of the query range at `step`.
fn grid(step: f64) -> Vec<u64> {
    let step_ns = (step * 1e9) as u64;
    let (a, b) = ((START * 1e9) as u64, (END * 1e9) as u64);
    ((a / step_ns) * step_ns..=b)
        .step_by(step_ns as usize)
        .collect()
}

fn irate_truth(f: fn(u64) -> f64, step: f64) -> impl Fn(u64) -> f64 {
    let s = (step * 1e9) as u64;
    move |t| (f(t) - f(t - s)) / (s as f64 / 1e9)
}

/// A window reaching before the first row starts at the first row
/// (`range_start`), so the truth does too.
fn rate_truth(f: fn(u64) -> f64, r_s: u64) -> impl Fn(u64) -> f64 {
    move |t| {
        let left = (t - r_s * S).max(T0);
        (f(t) - f(left)) / ((t - left) as f64 / 1e9)
    }
}

/// Each case's misses, printed one line per case; the test asserts none.
struct Report {
    name: &'static str,
    failed: Vec<String>,
}

impl Report {
    fn new(name: &'static str) -> Self {
        Self {
            name,
            failed: Vec::new(),
        }
    }

    fn check(&mut self, case: String, m: Misses) {
        println!("{} | {case} | {m:?}", self.name);
        if !m.clean() {
            self.failed.push(format!("{case}: {m:?}"));
        }
    }

    fn assert(self) {
        assert!(
            self.failed.is_empty(),
            "{} cases wrong:\n{}",
            self.failed.len(),
            self.failed.join("\n")
        );
    }
}

/// The two fixtures.
fn fixtures() -> [(&'static str, Vec<u64>); 2] {
    [
        ("coarse-fine-coarse", coarse_fine_coarse()),
        ("fine-coarse-fine", fine_coarse_fine()),
    ]
}

type Sources = fn(&[u64]) -> Vec<(String, Box<dyn MetricsSource>)>;

/// Every rate query at both steps and on both paths, with its truth.
fn rate_suite(report: &mut Report, sources: Sources, queries: &[&str]) {
    for (fixture, ts) in fixtures() {
        for (name, src) in sources(&ts) {
            for step in [1.0, 0.1] {
                for per_series in [false, true] {
                    let opts = QueryOptions::default().with_per_series_rates(per_series);
                    for &q in queries {
                        let truth: Box<dyn Fn(u64) -> f64> = match q {
                            "irate(c[5s])" => Box::new(irate_truth(c_const, step)),
                            "irate(c2[5s])" => Box::new(irate_truth(c_step, step)),
                            "rate(c[10s])" => Box::new(rate_truth(c_const, 10)),
                            "rate(c2[10s])" => Box::new(rate_truth(c_step, 10)),
                            "rate(c2[60s])" => Box::new(rate_truth(c_step, 60)),
                            _ => unreachable!("{q}"),
                        };
                        let batched =
                            || crate::promql::streaming::dispatch::BATCH_RATES.with(|n| n.get());
                        let before = batched();
                        let points = run(src.as_ref(), q, step, &opts);
                        // The archive answers on its batched path unless
                        // asked not to; the store has none.
                        let want_batch = name.starts_with("archive") && !per_series;
                        assert_eq!(batched() > before, want_batch, "{name} {q}");
                        report.check(
                            format!("{fixture} {name} step {step} per_series {per_series} {q}"),
                            score(&points, &grid(step), truth.as_ref(), 1e-6, true),
                        );
                    }
                }
            }
        }
    }
}

const RATES: [&str; 5] = [
    "irate(c[5s])",
    "irate(c2[5s])",
    "rate(c[10s])",
    "rate(c2[10s])",
    "rate(c2[60s])",
];

/// S10: rate values exact, every grid point present, and a band containing
/// the truth on every point, at the coarse hint and through archives whose
/// probed interval is the fine 100 ms or 250 ms: there a table's interval
/// is at least the longest row gap its segments record (or, without the record,
/// measure). Before 0.42.0, rows that start fine lost the band on every point of
/// the coarse span, and at a fine probed interval a `rate` window starting
/// in a coarse span started late (70 of 140 points wrong near the change,
/// 4.9%).
#[test]
fn rates_are_exact_and_banded_at_any_archive_hint() {
    let mut report = Report::new("rate");
    rate_suite(&mut report, exact_sources, &RATES);
    report.assert();
}

/// S10: a `MemoryStore` declaring 100 ms over these rows: `irate` and a
/// constant-rate `rate` are exact and banded everywhere.
#[test]
fn irate_is_exact_and_banded_at_a_declared_fine_interval() {
    let mut report = Report::new("rate");
    rate_suite(
        &mut report,
        |ts| vec![("mem 100ms".into(), Box::new(store(ts, 100)) as Box<_>)],
        &["irate(c[5s])", "irate(c2[5s])", "rate(c[10s])"],
    );
    report.assert();
}

/// S10: a gauge is held to the next row at every grid point. Before 0.42.0, at a
/// fine probed interval, it dropped every point more than 100 ms past a
/// coarse row (800 of 1601 at a 100 ms step).
#[test]
fn a_gauge_keeps_every_point_at_any_archive_hint() {
    let mut report = Report::new("gauge");
    for (fixture, ts) in fixtures() {
        for (name, src) in exact_sources(&ts) {
            for step in [1.0, 0.1] {
                let points = run(src.as_ref(), "g", step, &QueryOptions::default());
                let latest = |t: u64| g_of(ts[ts.partition_point(|&x| x <= t) - 1]) as f64;
                report.check(
                    format!("{fixture} {name} step {step} g"),
                    score(&points, &grid(step), &latest, 1e-9, false),
                );
            }
        }
    }
    report.assert();
}

/// The rows of the query range after its start.
fn rows_in_range(ts: &[u64]) -> Vec<u64> {
    ts.iter()
        .copied()
        .filter(|&t| t > (START * 1e9) as u64 && t <= (END * 1e9) as u64)
        .collect()
}

/// S10: raw `irate` gives the rate at every row. Before 0.42.0, at a 100 ms
/// probed interval, it gave no value for any 1 s pair.
#[test]
fn raw_irate_gives_every_pair_at_any_archive_hint() {
    let mut report = Report::new("raw");
    for (fixture, ts) in fixtures() {
        for (name, src) in exact_sources(&ts) {
            let points = run(
                src.as_ref(),
                "irate(c[5s])",
                1.0,
                &QueryOptions::with_rate_mode(RateMode::Raw),
            );
            report.check(
                format!("{fixture} {name} raw irate(c[5s])"),
                score(&points, &rows_in_range(&ts), &|_| 10.0, 1e-6, false),
            );
        }
    }
    report.assert();
}

/// A one-second hole inside the fine span is a hole: no band on the points
/// whose edges fall in it, while the coarse spans keep theirs; the gauge is
/// not held across it; raw `irate` gives no value for the pair spanning it.
/// The hint (1 s) is coarser than the hole, so only the local spacing can
/// find it. Before 0.42.0 the band was kept, the gauge held for the second, and
/// the pair given a value.
#[test]
fn a_gap_inside_the_fine_span_is_a_hole() {
    let hole = (1090 * S, 1091 * S);
    let ts: Vec<u64> = coarse_fine_coarse()
        .into_iter()
        .filter(|&t| t <= hole.0 || t >= hole.1)
        .collect();
    for (name, src) in coarse_hint_sources(&ts) {
        for per_series in [false, true] {
            let opts = QueryOptions::default().with_per_series_rates(per_series);
            let points = run(src.as_ref(), "irate(c[5s])", 0.1, &opts);
            let inside: Vec<&P> = points
                .iter()
                .filter(|p| p.t > hole.0 + 100 * MS && p.t < hole.1)
                .collect();
            assert!(!inside.is_empty(), "{name}");
            assert!(
                inside.iter().all(|p| p.band.is_none()),
                "{name} per_series {per_series}: a point inside the hole has a band"
            );
            let coarse = points.iter().filter(|p| p.t < F0 - S);
            assert!(
                coarse.clone().count() > 400 && coarse.clone().all(|p| p.band.is_some()),
                "{name} per_series {per_series}: a coarse point lost its band"
            );
        }
        // Held one 100 ms step past 1090.0, not the 1 s of the hint.
        let g = run(src.as_ref(), "g", 0.1, &QueryOptions::default());
        let held: Vec<u64> = g
            .iter()
            .map(|p| p.t)
            .filter(|&t| t > hole.0 && t < hole.1)
            .collect();
        assert_eq!(held, vec![hole.0 + 100 * MS], "{name}");
        let raw = run(
            src.as_ref(),
            "irate(c[5s])",
            1.0,
            &QueryOptions::with_rate_mode(RateMode::Raw),
        );
        assert!(raw.iter().any(|p| p.t == hole.0), "{name}");
        assert!(
            !raw.iter().any(|p| p.t == hole.1),
            "{name}: a value across the hole"
        );
    }

    // A `rate` whose window starts inside the hole, more than two 100 ms
    // spacings after the row before it, starts it at the row after the hole,
    // as across any hole: a counter that jumps by 1000 at that row reads
    // 10/s, not a share of the jump. Before 0.42.0 the reach was two of the 1 s
    // hint and the window was interpolated across the hole.
    let st = MemoryStore::builder().sampling_interval_ms(1000).build();
    st.insert_counter_series(
        "j",
        Labels::default(),
        ts.clone(),
        ts.iter()
            .map(|&t| c_const(t).round() as u64 + if t >= hole.1 { 1000 } else { 0 })
            .collect(),
        Some(ts.iter().map(|&t| win(t)).collect()),
    )
    .unwrap();
    let rate = run(&st, "rate(j[10s])", 0.1, &QueryOptions::default());
    let starting_in_hole: Vec<&P> = rate
        .iter()
        .filter(|p| p.t > hole.0 + 10 * S + 200 * MS && p.t < hole.1 + 10 * S)
        .collect();
    assert_eq!(starting_in_hole.len(), 7);
    assert!(
        starting_in_hole.iter().all(|p| (p.v - 10.0).abs() < 1e-6),
        "{starting_in_hole:?}"
    );
}

/// A point's stamp, and its value's and band's bits.
type Bits = (u64, u64, Option<(u64, u64)>);

/// A rate at one time has one value and one band from any start, through
/// the changes of spacing and at every hint: the local bounds reach no
/// further back than the read does. Before 0.42.0 the bands differed (a range
/// starting in the fine span took its hole test from fine samples).
#[test]
fn rates_through_a_change_of_spacing_are_the_same_from_any_start() {
    for (fixture, ts) in fixtures() {
        let mut sources = exact_sources(&ts);
        sources.push(("mem 100ms".into(), Box::new(store(&ts, 100))));
        for (name, src) in sources {
            for per_series in [false, true] {
                let opts = QueryOptions::default().with_per_series_rates(per_series);
                for q in ["irate(c2[5s])", "rate(c2[10s])", "rate(c2[60s])"] {
                    // The points from `from` on of a range starting at `start`.
                    let range = |start: f64, from: f64| -> Vec<Bits> {
                        let r = src.query_range_opts(q, start, END, 0.1, &opts).unwrap();
                        let QueryResult::Matrix { result } = r else {
                            panic!()
                        };
                        let m = &result[0];
                        m.values
                            .iter()
                            .enumerate()
                            .filter(|(_, (t, _))| *t >= from - 1e-6)
                            .map(|(i, &(t, v))| {
                                let b = m.bands.as_ref().and_then(|b| b[i]);
                                (
                                    (t * 1e9).round() as u64,
                                    v.to_bits(),
                                    b.map(|(lo, hi)| (lo.to_bits(), hi.to_bits())),
                                )
                            })
                            .collect()
                    };
                    // 1135.5: the first windows' left edges fall between two
                    // 1 s rows, and the row before each is read only when the
                    // read reaches back as far as the rows are apart.
                    for start in [1061.0, 1119.5, 1121.0, 1135.0, 1135.5] {
                        let from = f64::max(start, 1135.0);
                        let early = range(START, from);
                        assert!(!early.is_empty());
                        assert_eq!(
                            early,
                            range(start, from),
                            "{fixture} {name} per_series {per_series} {q} from {start}"
                        );
                    }
                }
            }
        }
    }
}

/// What this does not fix: a `MemoryStore` that declares an interval finer
/// than its rows (built at 100 ms, holding 1 s rows) has no segments to
/// record their spacing, and its declared interval still bounds a `rate`'s
/// reach, a gauge's hold and a raw pair's gap (two, one and four of it),
/// because its reads start that far before a range. A `rate(c2[r])` whose
/// window starts in a coarse span starts it at the first row after its left
/// edge, a gauge drops the points more than 100 ms past a coarse row, and
/// raw `irate` gives no value for a 1 s pair.
#[test]
fn a_store_declaring_a_finer_interval_than_its_rows_is_held_to_it() {
    for (fixture, ts) in fixtures() {
        let src = store(&ts, 100);
        let rate = run(&src, "rate(c2[10s])", 0.1, &QueryOptions::default());
        let m = score(&rate, &grid(0.1), &rate_truth(c_step, 10), 1e-6, false);
        assert!(m.missing == 0 && m.wrong > 0, "{fixture}: {m:?}");
        let g = run(&src, "g", 0.1, &QueryOptions::default());
        assert!(g.len() < grid(0.1).len(), "{fixture}");
        let raw = run(
            &src,
            "irate(c[5s])",
            1.0,
            &QueryOptions::with_rate_mode(RateMode::Raw),
        );
        assert!(raw.len() < rows_in_range(&ts).len(), "{fixture}");
    }
}

// ───────────────────────── cross-cadence ─────────────────────────

/// The evaluation timestamps of `q` in `[a, b)`.
fn eval_in(reader: &ArchiveReader, q: &str, step: f64, a: u64, b: u64) -> Vec<u64> {
    crate::eval_timestamps_for(reader, q, step, RateMode::Grid)
        .map(|p| p.iter().copied().filter(|&t| t >= a && t < b).collect())
        .unwrap_or_default()
}

/// Whether `ts` are spaced `gap_ns` apart throughout.
fn spaced(ts: &[u64], gap_ns: u64) -> bool {
    ts.len() >= 2 && ts.windows(2).all(|w| w[1] - w[0] == gap_ns)
}

/// S11: a table read every 1 s beside one that is fine in the middle. Where
/// both read every second the query is on the step's grid; in the middle,
/// where the fine table is ten times faster, it is at the slow table's rows.
/// The values are right throughout.
#[test]
fn cross_cadence_is_chosen_per_span() {
    let reader = archive(vec![
        ("s/grp", vec![segment(&coarse_fine_coarse(), None)]),
        ("t", vec![steady(1000)]),
    ]);
    let q = "irate(c[5s]) + irate(other[5s])";
    let coarse = eval_in(&reader, q, 0.1, 1020 * S, 1050 * S);
    assert!(spaced(&coarse, 100 * MS), "coarse span: {coarse:?}");
    let fine = eval_in(&reader, q, 0.1, 1070 * S, 1110 * S);
    assert!(spaced(&fine, S), "fine span: {fine:?}");
    let after = eval_in(&reader, q, 0.1, 1130 * S, 1160 * S);
    assert!(spaced(&after, 100 * MS), "after: {after:?}");

    let points = run(&reader, q, 0.1, &QueryOptions::default());
    assert!(points.len() > 1000, "{} points", points.len());
    assert!(points.iter().all(|p| (p.v - 20.0).abs() < 1e-6));
}

/// S11: a table that goes from 100 ms to 1 s rows beside a table read every
/// 100 ms is evaluated at its own rows where it is slow, and on the grid
/// where the two agree.
#[test]
fn a_table_that_slows_down_is_evaluated_at_its_own_rows_there() {
    let mut ts: Vec<u64> = (T0..F0).step_by((100 * MS) as usize).collect();
    ts.extend((F0..=TEND).step_by(S as usize));
    let reader = archive(vec![
        ("s/grp", vec![segment(&ts, None)]),
        ("t", vec![steady(100)]),
    ]);
    let q = "irate(c[5s]) + irate(other[5s])";
    let fast = eval_in(&reader, q, 0.1, 1020 * S, 1050 * S);
    assert!(spaced(&fast, 100 * MS), "fast span: {fast:?}");
    let slow = eval_in(&reader, q, 0.1, 1070 * S, 1170 * S);
    assert!(spaced(&slow, S), "slow span: {slow:?}");

    let points = run(&reader, q, 0.1, &QueryOptions::default());
    assert!(points.iter().all(|p| (p.v - 20.0).abs() < 1e-6));
}

/// One cadence in every span, and a query of one table: no relocation.
#[test]
fn one_cadence_is_not_relocated() {
    let reader = archive(vec![
        ("s/grp", vec![segment(&coarse_fine_coarse(), None)]),
        ("t", vec![steady(1000)]),
    ]);
    // One sampler.
    assert!(crate::eval_timestamps_for(&reader, "irate(c[5s])", 0.1, RateMode::Grid).is_none());
    // A step coarser than both tables everywhere.
    assert!(crate::eval_timestamps_for(
        &reader,
        "irate(c[5s]) + irate(other[5s])",
        5.0,
        RateMode::Grid
    )
    .is_none());
}

// ───────────────────────── per-row functions ─────────────────────────

/// S12: unstrided histogram functions give a point per row, so their count
/// per point drops tenfold across the fine span; the strided forms give one
/// point per stride, all equal. `avg_over_time` is a mean over samples.
#[test]
fn per_row_and_per_sample_functions_are_as_documented() {
    let ts = coarse_fine_coarse();
    let config = histogram::Config::new(3, 64).unwrap();
    let st = MemoryStore::builder().sampling_interval_ms(1000).build();
    // 100 observations a second in bucket 100 and 10 in bucket 200.
    let snaps: Vec<HistogramSnapshot> = ts
        .iter()
        .map(|&t| {
            let s = (t - T0) as f64 / 1e9;
            let a = (100.0 * s).round() as u64;
            HistogramSnapshot {
                index: vec![100, 200],
                count: vec![a, a + (10.0 * s).round() as u64],
            }
        })
        .collect();
    st.insert_histogram_series("h", Labels::default(), config, ts.clone(), snaps)
        .unwrap();
    let rows = ts
        .iter()
        .filter(|&&t| ((START * 1e9) as u64..=(END * 1e9) as u64).contains(&t))
        .count();
    for step in [1.0, 0.1] {
        let opts = QueryOptions::default();
        let unstrided = run(&st, "histogram_count(h)", step, &opts);
        assert_eq!(unstrided.len(), rows, "step {step}: one point per row");
        let fine = unstrided.iter().find(|p| p.t == 1080 * S).unwrap();
        let coarse = unstrided.iter().find(|p| p.t == 1040 * S).unwrap();
        assert_eq!((coarse.v, fine.v), (110.0, 11.0));
        for q in [
            "histogram_quantile(0.95, h)",
            "histogram_quantiles([0.5, 0.95], h)",
            "histogram_sum(h)",
            "histogram_mean(h)",
            "histogram_irate(h)",
        ] {
            let points = run(&st, q, step, &opts);
            assert_eq!(points.len(), rows, "{q} step {step}: one point per row");
        }
        let sum = run(&st, "histogram_sum(h)", step, &opts);
        let at = |t: u64| sum.iter().find(|p| p.t == t).unwrap().v;
        assert!(
            at(1040 * S) > 9.0 * at(1080 * S),
            "a sum per row scales with the row spacing"
        );

        let strided = run(&st, "histogram_count(h, 1)", step, &opts);
        assert_eq!(strided.len(), 161, "step {step}: one point per second");
        assert!(strided.iter().all(|p| p.v == 110.0));
        let strided = run(&st, "histogram_quantiles([0.95], h, 1)", step, &opts);
        assert_eq!(strided.len(), 161, "step {step}: one point per second");
    }

    // A mean over samples: at 1061 s, a window of [1056, 1061] holds 5
    // coarse rows (1056..1060) and 10 fine ones (1060.1..1061.0).
    let store = store(&ts, 1000);
    let avg = run(
        &store,
        "avg_over_time(g[5s])",
        1.0,
        &QueryOptions::default(),
    );
    let at = avg.iter().find(|p| p.t == 1061 * S).unwrap();
    let in_window: Vec<f64> = ts
        .iter()
        .filter(|&&t| (1056 * S..=1061 * S).contains(&t))
        .map(|&t| g_of(t) as f64)
        .collect();
    let mean = in_window.iter().sum::<f64>() / in_window.len() as f64;
    assert!((at.v - mean).abs() < 1e-9, "{} vs {mean}", at.v);
}

// ───────────────────────── a pause in a table ─────────────────────────

/// Counter `pc`: 10/s to 760 s past `T0`, then 100/s. Gauge `pg`: seconds
/// past `T0`, absent after `g_ends` when it is set.
fn pause_segment(ts: &[u64], g_ends: Option<u64>) -> Vec<u8> {
    let pc = |t: u64| {
        let s = (t - T0) / S;
        if s <= 760 {
            s * 10
        } else {
            7600 + (s - 760) * 100
        }
    };
    let schema = Arc::new(Schema::new(vec![
        Field::new("timestamp", DataType::UInt64, false),
        Field::new(":window_begin", DataType::Int64, true),
        Field::new(":window_width", DataType::UInt64, true),
        field("pc", "counter", DataType::UInt64),
        field("pg", "gauge", DataType::Int64),
    ]));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(UInt64Array::from(ts.to_vec())),
        Arc::new(Int64Array::from(vec![-(MS as i64); ts.len()])),
        Arc::new(UInt64Array::from(vec![2 * MS; ts.len()])),
        Arc::new(UInt64Array::from(
            ts.iter().map(|&t| pc(t)).collect::<Vec<_>>(),
        )),
        Arc::new(Int64Array::from(
            ts.iter()
                .map(|&t| (!g_ends.is_some_and(|e| t > e)).then(|| ((t - T0) / S) as i64))
                .collect::<Vec<_>>(),
        )),
    ];
    let mut buf = Vec::new();
    let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
    w.write(&RecordBatch::try_new(schema, cols).unwrap())
        .unwrap();
    w.close().unwrap();
    buf
}

/// Rows every second from 0 to 100 s and from 700 to 800 s past `T0`: a
/// ten-minute pause, which is the table's longest gap.
fn paused_rows() -> Vec<u64> {
    let mut ts: Vec<u64> = (0..=100).map(|i| T0 + i * S).collect();
    ts.extend((700..=800).map(|i| T0 + i * S));
    ts
}

fn instant(src: &dyn MetricsSource, q: &str, t: u64) -> Vec<f64> {
    match src.query(q, Some(t as f64 / 1e9)).unwrap() {
        QueryResult::Vector { result } => result.iter().map(|s| s.value.1).collect(),
        r => panic!("{q}: {r:?}"),
    }
}

/// A pause lengthens how far back a table is read, and nothing else: an
/// instant query still steps at the table's interval (1 s), so a gauge is
/// not held into the pause or past its own last row, and `irate` at the end
/// is the last pair's rate. A bound taken as the interval would
/// make the pause's 600 s the instant step, and each of these read
/// otherwise. One segment and two (the
/// pause between them).
#[test]
fn a_pause_lengthens_the_read_not_the_step() {
    let ts = paused_rows();
    let (before, after): (Vec<u64>, Vec<u64>) = ts.iter().partition(|&&t| t <= T0 + 100 * S);
    for (name, reader) in [
        (
            "one segment",
            archive(vec![("s/grp", vec![pause_segment(&ts, None)])]),
        ),
        (
            "two segments",
            archive(vec![(
                "s/grp",
                vec![pause_segment(&before, None), pause_segment(&after, None)],
            )]),
        ),
    ] {
        assert_eq!(
            instant(&reader, "pg", T0 + 400 * S),
            Vec::<f64>::new(),
            "{name}"
        );
        assert_eq!(instant(&reader, "pg", T0 + 800 * S), vec![800.0], "{name}");
        for q in ["irate(pc[5m])", "irate(pc[1m])", "rate(pc[30s])"] {
            let v = instant(&reader, q, T0 + 800 * S);
            assert_eq!(v.len(), 1, "{name} {q}");
            assert!((v[0] - 100.0).abs() < 1e-6, "{name} {q}: {v:?}");
        }
    }
    // A gauge that stops at 760 s in a table that goes on to 800 s.
    let stopped = archive(vec![(
        "s/grp",
        vec![pause_segment(&ts, Some(T0 + 760 * S))],
    )]);
    assert_eq!(instant(&stopped, "pg", T0 + 800 * S), Vec::<f64>::new());
    assert_eq!(instant(&stopped, "pg", T0 + 760 * S), vec![760.0]);
}

/// The range control: a `rate` over the pause is the same on both paths,
/// and right away from the pause's edges.
#[test]
fn a_rate_across_a_pause_is_right_on_both_paths() {
    let ts = paused_rows();
    let reader = archive(vec![("s/grp", vec![pause_segment(&ts, None)])]);
    let pc = |t: u64| -> f64 {
        let s = (t - T0) as f64 / 1e9;
        if s <= 760.0 {
            s * 10.0
        } else {
            7600.0 + (s - 760.0) * 100.0
        }
    };
    let mut answers = Vec::new();
    for per_series in [false, true] {
        let opts = QueryOptions::default().with_per_series_rates(per_series);
        let r = reader
            .query_range_opts("rate(pc[1m])", 1050.0, 1790.0, 10.0, &opts)
            .unwrap();
        let QueryResult::Matrix { result } = r else {
            panic!()
        };
        let points: Vec<(u64, f64)> = result[0]
            .values
            .iter()
            .map(|&(t, v)| ((t * 1e9).round() as u64, v))
            .collect();
        for &(t, v) in &points {
            let edge = (T0 + 60 * S..=T0 + 100 * S).contains(&t)
                || (T0 + 760 * S..=T0 + 790 * S).contains(&t);
            if edge {
                let want = (pc(t) - pc(t - 60 * S)) / 60.0;
                assert!(
                    (v - want).abs() < 1e-6,
                    "per_series {per_series} at {t}: {v} vs {want}"
                );
            }
        }
        assert!(points.iter().filter(|(t, _)| *t >= T0 + 760 * S).count() >= 3);
        answers.push(format!("{points:?}"));
    }
    assert_eq!(answers[0], answers[1]);
}

/// S11 at the ends of a slow table: past its last row, and inside a hole of
/// it, a query across it and a fast table has no points rather than the
/// fast table's grid with the slow operand held forward.
#[test]
fn past_a_slow_tables_last_row_there_are_no_grid_points() {
    let slow = |rows: &[u64]| -> Vec<u8> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("timestamp", DataType::UInt64, false),
            field("other", "counter", DataType::UInt64),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(UInt64Array::from(rows.to_vec())),
            Arc::new(UInt64Array::from(
                rows.iter()
                    .map(|&t| c_const(t).round() as u64)
                    .collect::<Vec<_>>(),
            )),
        ];
        let mut buf = Vec::new();
        let mut w = ArrowWriter::try_new(&mut buf, schema.clone(), None).unwrap();
        w.write(&RecordBatch::try_new(schema, cols).unwrap())
            .unwrap();
        w.close().unwrap();
        buf
    };
    let fast: Vec<u64> = (T0..=TEND).step_by(S as usize).collect();
    let q = "irate(c[5s]) + irate(other[5s])";
    // 10 s rows to 1100 s.
    let ends: Vec<u64> = (T0..=1100 * S).step_by(10 * S as usize).collect();
    // 10 s rows with nothing from 1050 to 1120 s.
    let holed: Vec<u64> = ends
        .iter()
        .copied()
        .chain((1120 * S..=TEND).step_by(10 * S as usize))
        .filter(|&t| !(1050 * S < t && t < 1120 * S))
        .collect();
    for (name, rows) in [
        ("ends at 1100 s", ends.clone()),
        ("hole 1050-1120 s", holed.clone()),
    ] {
        let reader = archive(vec![
            ("s/grp", vec![segment(&fast, None)]),
            ("t", vec![slow(&rows)]),
        ]);
        let points = crate::eval_timestamps_for(&reader, q, 1.0, RateMode::Grid).unwrap();
        assert_eq!(&points[..], &rows[..], "{name}");
    }
}

/// The read-back reaches as far as the rows are apart through every way an
/// archive joins tables: a query over two group tables of one recording (a
/// union) and an archive of two recordings. A range starting at 1135.5 s, whose first
/// windows' left edges fall between 1 s rows, answers as one starting
/// earlier.
#[test]
fn the_read_back_covers_the_rows_through_unions_and_recordings() {
    let ts = coarse_fine_coarse();
    let recording = |name: &str| {
        let labels: BTreeMap<String, String> = [("source".to_string(), name.to_string())].into();
        InMemorySource {
            name: name.into(),
            metadata: labels.clone(),
            labels,
            complete: true,
            tables: vec![
                ("s/grp".to_string(), vec![segment(&ts, None)]),
                ("s/other".to_string(), vec![steady(1000)]),
            ],
        }
    };
    let union =
        ArchiveReader::from_in_memory(vec![recording("a")], None, BufferPool::new(64 << 20))
            .unwrap();
    let two = ArchiveReader::from_in_memory(
        vec![recording("a"), recording("b")],
        None,
        BufferPool::new(64 << 20),
    )
    .unwrap();
    for (name, reader) in [("union", union), ("two recordings", two)] {
        for per_series in [false, true] {
            let opts = QueryOptions::default().with_per_series_rates(per_series);
            let points = |start: f64| -> Vec<String> {
                let r = reader
                    .query_range_opts("rate(c2[10s]) + rate(other[10s])", start, END, 0.1, &opts)
                    .unwrap();
                let QueryResult::Matrix { result } = r else {
                    panic!()
                };
                result
                    .iter()
                    .map(|m| {
                        let kept: Vec<_> = m
                            .values
                            .iter()
                            .enumerate()
                            .filter(|(_, (t, _))| *t >= 1135.5 - 1e-6)
                            .map(|(i, &(t, v))| (t, v, m.bands.as_ref().and_then(|b| b[i])))
                            .collect();
                        format!("{kept:?}")
                    })
                    .collect()
            };
            assert_eq!(
                points(START),
                points(1135.5),
                "{name} per_series {per_series}"
            );
        }
    }
}
