//! The spacing of a series' samples near a point, which the engine's
//! per-point bounds are taken from.
//!
//! Four bounds depend on how far apart a series' samples are: how far
//! before a `rate`'s window a sample is interpolated from (the reach, two
//! spacings), how long a gauge is held past a sample (the step, or one
//! spacing when that is longer), the longest pair a raw `irate` gives a
//! value for (four spacings), and which gaps a rate's band treats as holes
//! (longer than two spacings). They used to come from one interval hint per
//! table, or from the first nine samples, which is wrong for any series
//! whose spacing changes: at a 100 ms hint a 1 s span read as holes, and at
//! a 1 s hint a gap inside a 100 ms span did not.
//!
//! Each now takes the spacing of the pair of samples around the point
//! (`metriken_storage::spacing::local_spacing`, which defines it:
//! `docs/journal/2026-10-19-recording-identity-and-views.md`, S3 and S10).
//! The reach, the gauge's hold and the raw pair's gap are no longer than
//! the hint gives, because a read starts that far before the range: a bound
//! past it would reach samples a later start does not read, and a result
//! would depend on where its range starts. An archive table's hint is at
//! least the longest gap between rows its segments record over the query's
//! window (`archive::timed`), so there the cap does not bind; a source whose
//! hint is finer than its rows (a `MemoryStore` declaring 100 ms over 1 s
//! rows) is still held to it. The hole test has no cap: it only decides
//! whether a point has a band.

use std::cell::Cell;
use std::collections::VecDeque;

use metriken_storage::spacing::spacing_at_least;
pub(crate) use metriken_storage::spacing::SIDE_GAPS;

/// A sample a producer keeps: its stamp, and whether the gap ending at it is
/// a hole once worked out.
pub(crate) trait Stamped {
    fn ts(&self) -> u64;
    fn hole(&self) -> &HoleMemo;
}

/// Whether the gap ending at a kept sample is a hole: unknown until asked.
/// A pair is asked at many points (every grid point between two coarse
/// rows, a rate's reach and both of its edges), and the answer does not
/// change once the samples after it are held.
#[derive(Clone, Debug, Default)]
pub(crate) struct HoleMemo(Cell<u8>);

impl HoleMemo {
    // Zero, the default, is unknown.
    const HOLE: u8 = 1;
    const NOT: u8 = 2;
}

/// Whether the spacing of the pair `kept[hi - 1]`, `kept[hi]` is at least
/// `x`, or `None` when the pair has no spacing (`spacing_at_least`). `hi`
/// may be `kept.len()`, past the last sample.
///
/// The gaps after the pair are those `kept` holds: the producers hold
/// [`SIDE_GAPS`] samples past a point before they evaluate it ([`sees_past`]),
/// or have read to the end of the stream.
pub(crate) fn at_least<K: Stamped>(kept: &VecDeque<K>, hi: usize, x: u64) -> Option<bool> {
    spacing_at_least(|i| kept[i].ts(), kept.len(), hi, x)
}

/// Whether the gap between `kept[hi - 1]` and `kept[hi]` is a hole: longer
/// than twice the pair's spacing. A pair with no spacing is not a hole.
///
/// `floor` is the shortest nonzero gap the producer has read, or `u64::MAX`
/// before it has read one. The pair's spacing is a median of gaps the
/// producer has read (it holds [`SIDE_GAPS`] samples past a pair before it
/// asks), so it is at least `floor`, and a gap no longer than twice `floor`
/// is no hole: one comparison, which answers for a regular series.
pub(crate) fn is_hole<K: Stamped>(kept: &VecDeque<K>, hi: usize, floor: u64) -> bool {
    is_hole_gap(kept, hi, kept[hi].ts() - kept[hi - 1].ts(), floor)
}

/// [`is_hole`] for a caller that has the pair's `gap` in hand: the common
/// answer is then one comparison, with no lookup.
pub(crate) fn is_hole_gap<K: Stamped>(kept: &VecDeque<K>, hi: usize, gap: u64, floor: u64) -> bool {
    if gap <= floor.saturating_mul(2) {
        return false;
    }
    let memo = &kept[hi].hole().0;
    match memo.get() {
        HoleMemo::HOLE => true,
        HoleMemo::NOT => false,
        _ => {
            let hole = at_least(kept, hi, gap.div_ceil(2)) == Some(false);
            memo.set(if hole { HoleMemo::HOLE } else { HoleMemo::NOT });
            hole
        }
    }
}

/// Whether `d`, no longer than the gap between `kept[hi - 1]` and
/// `kept[hi]`, is within `times` (two or more) of the pair's spacing and
/// within `cap`: `d <= min(times * spacing, cap)`, or `d <= cap` for a pair
/// with no spacing. What a rate's reach (two spacings) and a raw pair's gap
/// (four) are. A pair that is not a hole is at most two spacings apart, so
/// only a hole needs the spacing itself. `floor` is as [`is_hole`] takes it.
pub(crate) fn within<K: Stamped>(
    kept: &VecDeque<K>,
    hi: usize,
    d: u64,
    times: u64,
    cap: u64,
    floor: u64,
) -> bool {
    // `d` is no longer than the pair's gap, and the pair's spacing is at
    // least `floor`: within twice it, the answer needs no lookup.
    d <= cap
        && (d <= floor.saturating_mul(2)
            || !is_hole(kept, hi, floor)
            || at_least(kept, hi, d.div_ceil(times)) != Some(false))
}

/// Whether `kept`, in time order, holds [`SIDE_GAPS`] samples after the
/// first at or after `t`: what a point at `t` needs to know the spacing of
/// the pairs it reads.
pub(crate) fn sees_past<K: Stamped>(kept: &VecDeque<K>, t: u64) -> bool {
    kept.len() > SIDE_GAPS && kept[kept.len() - 1 - SIDE_GAPS].ts() >= t
}

/// Drop the samples of `kept` before `left` that no later point reads:
/// all but the last at or before `left` and the [`SIDE_GAPS`] before it,
/// whose gaps the spacing of the pair at `left` is taken over.
pub(crate) fn trim<K: Stamped>(kept: &mut VecDeque<K>, left: u64) {
    while kept.len() > SIDE_GAPS + 1 && kept[SIDE_GAPS + 1].ts() <= left {
        kept.pop_front();
    }
}
