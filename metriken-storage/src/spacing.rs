//! The spacing of a series' rows near a pair of them, and the shortest and
//! longest gap of a segment's rows.
//!
//! The spacing of a pair of consecutive rows is the larger of the median of
//! up to [`SIDE_GAPS`] gaps before the pair and the median of up to
//! [`SIDE_GAPS`] gaps after it, one side's when the other has none, and none
//! when neither has. The pair's own gap is on neither side, so a long gap is
//! measured against the spacing around it, and a gap longer than twice the
//! spacing is a hole. Taking the larger keeps a series that slows down from
//! showing holes after the change: across a change from 100 ms to 1 s rows
//! the pair is 1 s apart and the median after it is 1 s. A gap of zero (two
//! rows at one time) is not a spacing and is skipped. The median of an even
//! count is the upper of the two middle gaps.
//!
//! metriken-query takes its per-point bounds from this
//! ([`spacing_at_least`]), a table's cadence for evaluating a query across
//! tables of different cadence from [`pair_spacings`], and an archive
//! table's interval hint from the [`SegmentSpacing`] each segment records
//! (`SamplerReader::spacing_over`): the shortest and longest gap of its
//! rows, kept by [`RowGaps`] as the rows are appended.

/// How many gaps on each side of a pair its spacing is the median of.
pub const SIDE_GAPS: usize = 8;

/// The upper median of the nonzero gaps among `gaps`, or `None` when there
/// is none.
fn median(gaps: impl Iterator<Item = u64>) -> Option<u64> {
    let mut buf = [0u64; SIDE_GAPS];
    let mut n = 0;
    for g in gaps.filter(|g| *g > 0).take(SIDE_GAPS) {
        // Insertion into the sorted prefix: eight values at most.
        let mut i = n;
        while i > 0 && buf[i - 1] > g {
            buf[i] = buf[i - 1];
            i -= 1;
        }
        buf[i] = g;
        n += 1;
    }
    (n > 0).then(|| buf[n / 2])
}

/// The spacing of the pair of rows `hi - 1` and `hi` among `len` rows in
/// time order, whose stamps `ts` gives by index (see the module docs). `hi`
/// may be `len`, past the last row: then the gaps ending at the last row are
/// the only side. `None` with no gap on either side.
///
/// The gaps after the pair are those the caller holds: a caller that has not
/// read [`SIDE_GAPS`] rows past `hi` while more are to come gets a spacing
/// that can change once it has.
pub fn local_spacing(ts: impl Fn(usize) -> u64, len: usize, hi: usize) -> Option<u64> {
    // The gaps ending at `k`, for `k` in the range: `ts(k) - ts(k - 1)`.
    let gaps = |from: usize, to: usize| (from.max(1)..to.min(len)).map(|k| ts(k) - ts(k - 1));
    let before = median(gaps(hi.saturating_sub(SIDE_GAPS), hi));
    let after = median(gaps(hi + 1, hi + 1 + SIDE_GAPS));
    before.max(after)
}

/// Whether the spacing of the pair `hi - 1`, `hi` ([`local_spacing`]) is at
/// least `x`, or `None` when the pair has no spacing. The same answer as
/// comparing `local_spacing` with `x`, without sorting: the median of `n`
/// gaps is at least `x` exactly when at least `n - n / 2` of them are. This
/// is what a per-point bound asks, and it is a few comparisons where two
/// medians are two sorts.
pub fn spacing_at_least(ts: impl Fn(usize) -> u64, len: usize, hi: usize, x: u64) -> Option<bool> {
    // Over the gaps ending at `from..to`: whether their median is at least
    // `x`, or `None` with no nonzero gap.
    let side = |from: usize, to: usize| -> Option<bool> {
        let (from, to) = (from.max(1), to.min(len));
        if from >= to {
            return None;
        }
        // As many gaps at least `x` as half the side's slots, rounded up,
        // are enough however many of the slots are zero: `n - n / 2` grows
        // with `n`.
        let enough = (to - from) - (to - from) / 2;
        let (mut n, mut at_least) = (0usize, 0usize);
        let mut prev = ts(from - 1);
        for k in from..to {
            let t = ts(k);
            let gap = t - prev;
            prev = t;
            if gap > 0 {
                n += 1;
                at_least += usize::from(gap >= x);
                if at_least >= enough {
                    return Some(true);
                }
            }
        }
        (n > 0).then(|| at_least >= n - n / 2)
    };
    match side(hi.saturating_sub(SIDE_GAPS), hi) {
        Some(true) => Some(true),
        before => match side(hi + 1, hi + 1 + SIDE_GAPS) {
            Some(true) => Some(true),
            after => before.or(after),
        },
    }
}

/// The spacing between each two rows of `ts` (in time order) that are not a
/// hole: [`local_spacing`] of the pair, or the pair's own gap for a pair with
/// no other gap either side, by the index of the later row. `None` for a gap
/// that is a hole (longer than twice that) or two rows at one time.
///
/// Linear in the rows: the medians are of a window of the last
/// [`SIDE_GAPS`] gaps kept sorted as it slides, so a table's rows are
/// measured once rather than sorted per pair.
pub fn pair_spacings(ts: &[u64]) -> Vec<(usize, Option<u64>)> {
    let len = ts.len();
    if len < 2 {
        return Vec::new();
    }
    // `ending[j]`: the median of the nonzero gaps ending at `j - 7 ..= j`
    // (from 1), the side before the pair `(j, j + 1)` and the side after the
    // pair `(j - 8, j - 7)`.
    let mut ending: Vec<Option<u64>> = vec![None; len];
    let mut slots = [0u64; SIDE_GAPS];
    let mut sorted: Vec<u64> = Vec::with_capacity(SIDE_GAPS);
    for j in 1..len {
        let gap = ts[j] - ts[j - 1];
        let slot = (j - 1) % SIDE_GAPS;
        let out = std::mem::replace(&mut slots[slot], gap);
        if j > SIDE_GAPS && out > 0 {
            let at = sorted.partition_point(|g| *g < out);
            sorted.remove(at);
        }
        if gap > 0 {
            let at = sorted.partition_point(|g| *g < gap);
            sorted.insert(at, gap);
        }
        ending[j] = sorted.get(sorted.len() / 2).copied();
    }
    (1..len)
        .map(|k| {
            let gap = ts[k] - ts[k - 1];
            if gap == 0 {
                return (k, None);
            }
            let after = if k + SIDE_GAPS < len {
                ending[k + SIDE_GAPS]
            } else {
                // Fewer than a full side after the pair: the window is cut
                // at the last row, which `ending` does not hold. From `k`,
                // the gaps after the pair are the only side.
                let tail = &ts[k..];
                local_spacing(|i| tail[i], tail.len(), 0)
            };
            let spacing = ending[k - 1].max(after).unwrap_or(gap);
            (k, (gap <= spacing.saturating_mul(2)).then_some(spacing))
        })
        .collect()
}

/// A segment's row spacing, recorded in its footer under [`FOOTER_KEY`]
/// when the segment is written, so a reader knows how far apart a table's
/// rows are over a range from the footers it already reads at open, without
/// decoding a column: the shortest and the longest gap between two
/// consecutive rows, in nanoseconds, two rows at one time excepted. A gap
/// that is a hole counts, so `max_gap` bounds every spacing in the segment
/// from above, and `min_gap` from below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SegmentSpacing {
    pub min_gap: u64,
    pub max_gap: u64,
}

/// The gaps of rows as they are appended to a segment, kept as they arrive:
/// one subtraction and two comparisons per row, nothing held but the last
/// stamp. Every segment builder keeps one.
#[derive(Clone, Copy, Debug, Default)]
pub struct RowGaps {
    prev: Option<u64>,
    gaps: Option<(u64, u64)>,
}

impl RowGaps {
    /// A row stamped `ts`, after the rows already pushed. A row at the stamp
    /// of the one before (a long segment's next occupant) adds no gap.
    #[inline]
    pub fn push(&mut self, ts: u64) {
        if let Some(prev) = self.prev.replace(ts) {
            let gap = ts.abs_diff(prev);
            if gap > 0 {
                self.gaps = Some(match self.gaps {
                    Some((lo, hi)) => (lo.min(gap), hi.max(gap)),
                    None => (gap, gap),
                });
            }
        }
    }

    /// The spacing of the rows pushed, or `None` without a nonzero gap.
    pub fn spacing(&self) -> Option<SegmentSpacing> {
        self.gaps
            .map(|(min_gap, max_gap)| SegmentSpacing { min_gap, max_gap })
    }
}

/// The footer key a segment's [`SegmentSpacing`] is recorded under.
///
/// The value is `1:<min_gap>,<max_gap>`, or `1:-` for a segment with no gap.
/// The leading number is the encoding's version: a reader takes the first
/// two fields of version 1 and ignores any after them, and treats another
/// version as absent, as it treats a segment written before the key existed:
/// it measures the segment's rows itself. A reader that predates the key
/// ignores it, as it does any footer key it does not know.
pub const FOOTER_KEY: &str = "metriken.spacing";

/// The spacing of rows stamped `ts`, in any order and with repeats (a long
/// segment has a row per occupant per tick): of the distinct stamps, in
/// order. `None` with no nonzero gap. What a reader measures for a segment
/// that records none.
pub fn segment_spacing(ts: &[u64]) -> Option<SegmentSpacing> {
    let mut gaps = RowGaps::default();
    if ts.windows(2).all(|w| w[0] <= w[1]) {
        ts.iter().for_each(|&t| gaps.push(t));
    } else {
        let mut sorted = ts.to_vec();
        sorted.sort_unstable();
        sorted.iter().for_each(|&t| gaps.push(t));
    }
    gaps.spacing()
}

/// `props` with `spacing` in the footer it writes, under [`FOOTER_KEY`],
/// replacing one it carries.
pub(crate) fn with_footer(
    props: parquet::file::properties::WriterProperties,
    spacing: Option<SegmentSpacing>,
) -> parquet::file::properties::WriterProperties {
    let mut kv = props.key_value_metadata().cloned().unwrap_or_default();
    kv.retain(|e| e.key != FOOTER_KEY);
    kv.push(parquet::file::metadata::KeyValue::new(
        FOOTER_KEY.to_string(),
        encode_spacing(spacing),
    ));
    props
        .into_builder()
        .set_key_value_metadata(Some(kv))
        .build()
}

/// Whether `props` carries a footer spacing already.
pub(crate) fn has_footer(props: &parquet::file::properties::WriterProperties) -> bool {
    props
        .key_value_metadata()
        .is_some_and(|kv| kv.iter().any(|e| e.key == FOOTER_KEY))
}

/// The footer value recording `spacing` (see [`FOOTER_KEY`]).
pub fn encode_spacing(spacing: Option<SegmentSpacing>) -> String {
    match spacing {
        Some(s) => format!("1:{},{}", s.min_gap, s.max_gap),
        None => "1:-".to_string(),
    }
}

/// The spacing a footer value records: `Some(None)` for a segment with no
/// gap, `None` for a value this reader does not read (another version, or
/// malformed), which a reader treats as a segment that recorded none.
pub fn decode_spacing(value: &str) -> Option<Option<SegmentSpacing>> {
    let rest = value.strip_prefix("1:")?;
    if rest == "-" {
        return Some(None);
    }
    let mut fields = rest.split(',').map(|f| f.parse::<u64>());
    let mut next = || fields.next()?.ok();
    let s = SegmentSpacing {
        min_gap: next()?,
        max_gap: next()?,
    };
    (s.min_gap > 0 && s.min_gap <= s.max_gap).then_some(Some(s))
}

/// The spacing over several segments: the shortest gap of any, the longest
/// of any.
pub fn merge_spacing(
    a: Option<SegmentSpacing>,
    b: Option<SegmentSpacing>,
) -> Option<SegmentSpacing> {
    match (a, b) {
        (Some(a), Some(b)) => Some(SegmentSpacing {
            min_gap: a.min_gap.min(b.min_gap),
            max_gap: a.max_gap.max(b.max_gap),
        }),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ts: &[u64], hi: usize) -> Option<u64> {
        local_spacing(|i| ts[i], ts.len(), hi)
    }

    /// The larger side's median, the pair's own gap on neither side.
    #[test]
    fn the_larger_median_either_side_of_the_pair() {
        // 100 ms rows to 1.0 s, then 1 s rows.
        let mut ts: Vec<u64> = (0..=10).map(|i| i * 100).collect();
        ts.extend((2..=12).map(|i| i * 1000));
        // The pair (1000, 2000): before 100, after 1000.
        let hi = ts.iter().position(|&t| t == 2000).unwrap();
        assert_eq!(at(&ts, hi), Some(1000));
        // Inside the fine rows, far from the change.
        assert_eq!(at(&ts, 4), Some(100));
        // One side only at either end, and past the last row.
        assert_eq!(at(&ts, 1), Some(100));
        assert_eq!(at(&ts, ts.len()), Some(1000));
        // A pair alone has no spacing.
        assert_eq!(at(&[5, 9], 1), None);
        assert_eq!(
            segment_spacing(&ts).map(|s| (s.min_gap, s.max_gap)),
            Some((100, 1000))
        );
    }

    /// A long gap is measured against the spacing around it, not itself, so
    /// it is a hole; it is the segment's longest gap.
    #[test]
    fn a_gap_is_not_its_own_spacing() {
        let mut ts: Vec<u64> = (0..10).map(|i| i * 100).collect();
        ts.extend((0..10).map(|i| 5000 + i * 100));
        let hi = ts.iter().position(|&t| t == 5000).unwrap();
        assert_eq!(at(&ts, hi), Some(100));
        assert_eq!(
            segment_spacing(&ts).map(|s| (s.min_gap, s.max_gap)),
            Some((100, 4100))
        );
        assert_eq!(segment_spacing(&[5, 9]).map(|s| s.max_gap), Some(4));
        assert_eq!(segment_spacing(&[5]), None);
    }

    /// `spacing_at_least` and `pair_spacings` answer as `local_spacing`
    /// does, on rows with jitter, changes of spacing, holes and repeated
    /// stamps.
    #[test]
    fn the_fast_forms_agree_with_local_spacing() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % n
        };
        for case in 0..400 {
            let len = 1 + next(60) as usize;
            let mut ts = vec![next(50)];
            for _ in 1..len {
                let gap = match next(10) {
                    0 => 0,
                    1 => 20 + next(80),
                    2..=4 => 1 + next(3),
                    _ => 8 + next(4),
                };
                ts.push(ts.last().unwrap() + gap);
            }
            let spacings = pair_spacings(&ts);
            for hi in 0..=len {
                let l = at(&ts, hi);
                for x in [0, 1, 2, 3, 5, 8, 10, 12, 30, 100] {
                    assert_eq!(
                        spacing_at_least(|i| ts[i], len, hi, x),
                        l.map(|l| l >= x),
                        "case {case} hi {hi} x {x}: {ts:?}"
                    );
                }
                if (1..len).contains(&hi) {
                    let gap = ts[hi] - ts[hi - 1];
                    let want = (gap > 0).then(|| l.unwrap_or(gap)).filter(|s| gap <= 2 * s);
                    assert_eq!(spacings[hi - 1], (hi, want), "case {case} hi {hi}: {ts:?}");
                }
            }
        }
    }

    /// The larger median's cost: within eight rows of a slowdown a hole is
    /// measured against the slower side. 100 ms rows, a 1.5 s gap, three
    /// more 100 ms rows, then 1 s rows: the eight gaps after the 1.5 s one
    /// are three of 100 ms and five of 1 s, so its spacing is 1 s and 1.5 s
    /// is not a hole.
    #[test]
    fn a_hole_just_before_a_slowdown_is_measured_against_the_slow_side() {
        let mut ts: Vec<u64> = (0..=20).map(|i| i * 100).collect();
        ts.extend([3500, 3600, 3700, 3800]);
        ts.extend((1..=10).map(|i| 3800 + i * 1000));
        let hi = ts.iter().position(|&t| t == 3500).unwrap();
        assert_eq!(local_spacing(|i| ts[i], ts.len(), hi), Some(1000));
        let hole = pair_spacings(&ts)[hi - 1];
        assert_eq!(hole, (hi, Some(1000)), "a 1.5 s gap is not a hole here");
        // Without the slowdown after it, the same gap is one.
        let mut fine: Vec<u64> = (0..=20).map(|i| i * 100).collect();
        fine.extend((0..20).map(|i| 3500 + i * 100));
        let hi = fine.iter().position(|&t| t == 3500).unwrap();
        assert_eq!(pair_spacings(&fine)[hi - 1], (hi, None));
    }

    /// Two rows at one time are not a spacing; an even count takes the
    /// upper middle.
    #[test]
    fn zero_gaps_are_skipped_and_an_even_count_takes_the_upper_middle() {
        assert_eq!(at(&[0, 0, 0, 10, 10, 30], 6), Some(20));
        assert_eq!(at(&[0, 10, 30, 60, 100], 5), Some(30));
    }

    /// A segment's spacing round-trips through its footer value; a value of
    /// another version or a malformed one reads as absent; fields after the
    /// second are ignored. Rows pushed one at a time give what the stamps
    /// measure.
    #[test]
    fn segment_spacing_round_trips_and_tolerates_other_versions() {
        let mut ts: Vec<u64> = (0..20).map(|i| i * 1000).collect();
        ts.extend((1..=50).map(|i| 19_000 + i * 100));
        let s = segment_spacing(&ts).unwrap();
        assert_eq!((s.min_gap, s.max_gap), (100, 1000));
        let mut gaps = RowGaps::default();
        for &t in &ts {
            gaps.push(t);
            gaps.push(t);
        }
        assert_eq!(gaps.spacing(), Some(s));
        assert_eq!(decode_spacing(&encode_spacing(Some(s))), Some(Some(s)));
        assert_eq!(decode_spacing(&encode_spacing(None)), Some(None));
        assert_eq!(segment_spacing(&[7]), None);
        assert_eq!(segment_spacing(&[7, 7]), None);
        // Unsorted rows read as sorted.
        let mut shuffled = ts.clone();
        shuffled.reverse();
        assert_eq!(segment_spacing(&shuffled), Some(s));
        assert_eq!(decode_spacing("1:100,1000,42,extra"), Some(Some(s)));
        for bad in ["2:100,1000", "1:100", "1:x,1", "1:0,1", "1:5,4", "", "MNS1"] {
            assert_eq!(decode_spacing(bad), None, "{bad}");
        }
    }
}
