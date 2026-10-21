//! The spacing of a series' rows near a pair of them, and the finest and
//! coarsest spacing of a table.
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
//! ([`spacing_at_least`]), and a table's cadence, for evaluating a query
//! across tables of different cadence, from [`spacing_range`] and
//! [`pair_spacings`] (`SamplerReader::spacing_range`).

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

/// The finest and coarsest spacing of `ts`'s rows outside holes, or `None`
/// with fewer than two rows (or only holes): the intervals a table whose
/// spacing changes is read at, at its fastest and slowest.
pub fn spacing_range(ts: &[u64]) -> Option<(u64, u64)> {
    pair_spacings(ts)
        .into_iter()
        .filter_map(|(_, s)| s)
        .fold(None, |acc, s| match acc {
            None => Some((s, s)),
            Some((lo, hi)) => Some((lo.min(s), hi.max(s))),
        })
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
        assert_eq!(spacing_range(&ts), Some((100, 1000)));
    }

    /// A long gap is measured against the spacing around it, not itself, so
    /// it is a hole and not the coarsest spacing.
    #[test]
    fn a_gap_is_not_its_own_spacing() {
        let mut ts: Vec<u64> = (0..10).map(|i| i * 100).collect();
        ts.extend((0..10).map(|i| 5000 + i * 100));
        let hi = ts.iter().position(|&t| t == 5000).unwrap();
        assert_eq!(at(&ts, hi), Some(100));
        assert_eq!(spacing_range(&ts), Some((100, 100)));
        assert_eq!(spacing_range(&[5, 9]), Some((4, 4)));
        assert_eq!(spacing_range(&[5]), None);
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

    /// Two rows at one time are not a spacing; an even count takes the
    /// upper middle.
    #[test]
    fn zero_gaps_are_skipped_and_an_even_count_takes_the_upper_middle() {
        assert_eq!(at(&[0, 0, 0, 10, 10, 30], 6), Some(20));
        assert_eq!(at(&[0, 10, 30, 60, 100], 5), Some(30));
    }
}
