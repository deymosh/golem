//! The tuner's decisions: where the split frontier is, which point wins, and
//! how far the context grows - with no I/O and no process spawning. The
//! binary's `tune.rs` measures; this module decides. The floors, the wall
//! and the reasoning behind them are in `docs/tuning.md`.
//!
//! Three rules travel together and must not be conflated:
//!
//! * the **spill** check is *relative* - a point's prefill against the best
//!   prefill its own engine reached at the same `ubatch-size` - and catches
//!   the Windows driver silently spilling VRAM to system memory;
//! * the **floors** are *absolute* - a fixed tokens/s bar - and catch a setup
//!   that is merely slow without spilling;
//! * the **depth floor** is applied only to a point that already passed the
//!   eval gate, because a point is ranked before it is gated.

/// The speed floors a tuned setup has to clear (docs/tuning.md).
///
/// A point under it is unusable however fast it looks next to worse points,
/// so it is excluded from the ranking rather than merely ranked low. These
/// are the defaults of `TUNE_MIN_*` in golem.toml.
#[derive(Debug, Clone, Copy)]
pub struct Floor {
    /// Median decode, tokens/s.
    pub decode: f64,
    /// Prefill, tokens/s.
    pub prefill: f64,
    /// Decode at full context (the gate's own measurement), tokens/s.
    pub depth_decode: f64,
}

impl Default for Floor {
    fn default() -> Self {
        // 25 felt fine in real sessions where 16.7 did not; 350 refuses the
        // ~80 s first prompt; 15 refuses a context that is unusable at depth.
        Floor { decode: 25.0, prefill: 350.0, depth_decode: 15.0 }
    }
}

/// The next split to probe, between one that fits and one that spills.
///
/// `n-cpu-moe N` keeps the experts of the first N layers in RAM, so a *lower*
/// N puts more of the model on the card: the splits that fit form a contiguous
/// run from the seed downward, and the frontier is a single bisection rather
/// than a walk. `fits` is the lowest split known to fit, `spills` the highest
/// known to spill; both are facts from measurement, and `fits > spills`.
///
/// `None` means the two bounds are adjacent - there is nothing left between
/// them to learn, and the caller stops instead of spending a model load on a
/// split it can already predict. That is the whole point of this function: a
/// walk over the grid spends nine loads to learn what three bisection steps
/// know.
pub fn between(fits: u32, spills: u32) -> Option<u32> {
    if fits <= spills + 1 {
        return None;
    }
    Some(fits - (fits - spills) / 2)
}

/// A tune's stages, in the order it spends model loads on them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Frontier,
    Refine,
    Context,
    Gate,
}

impl Phase {
    /// The stage as a progress bar names it.
    pub fn name(self) -> &'static str {
        match self {
            Phase::Frontier => "split frontier",
            Phase::Refine => "refine",
            Phase::Context => "context",
            Phase::Gate => "gate",
        }
    }

    /// The loads the stage typically spends (docs/tuning.md's budget: three
    /// to five, one or two, two or three, and one).
    fn typical_loads(self) -> u32 {
        match self {
            Phase::Frontier => 4,
            Phase::Refine => 2,
            Phase::Context => 3,
            Phase::Gate => 1,
        }
    }

    fn later(self) -> &'static [Phase] {
        match self {
            Phase::Frontier => &[Phase::Refine, Phase::Context, Phase::Gate],
            Phase::Refine => &[Phase::Context, Phase::Gate],
            Phase::Context => &[Phase::Gate],
            Phase::Gate => &[],
        }
    }
}

/// The loads a run still expects to spend, the one starting now included:
/// what is left of this stage's typical count - at least this load - and
/// every later stage's. `in_phase` is the loads this stage has already spent.
///
/// An estimate by construction - the search stops when it has its answer -
/// so a progress bar built on it grows its total when a stage runs long
/// rather than claiming a finish it cannot know.
pub fn loads_left(phase: Phase, in_phase: u32) -> u32 {
    let here = phase.typical_loads().saturating_sub(in_phase).max(1);
    here + phase.later().iter().map(|p| p.typical_loads()).sum::<u32>()
}

/// One measured sweep point.
#[derive(Debug, Clone)]
pub struct Point {
    pub engine: String,
    pub label: String,
    pub decode: f64,
    pub prefill: f64,
    pub ctx: u64,
    /// The `ubatch-size` it ran with, 0 for the engine's default. Prefill
    /// moves with it by design, so the spill check compares within it.
    pub ubatch: u32,
    /// Result of the eval gate for this point, when it was gated.
    pub passed_gate: Option<bool>,
    /// Decode at full depth, from the gate's own measurement.
    pub depth_decode: Option<f64>,
}

/// Why a point is out of the ranking, or `None` when it is in.
#[derive(Debug, Clone, PartialEq)]
pub enum Exclusion {
    /// Prefill collapsed against the best the same engine reached: the
    /// driver spilled to system memory.
    Spilling { prefill: f64, engine_best: f64 },
    /// Decode under the absolute floor.
    BelowDecodeFloor { decode: f64 },
    /// Prefill under the absolute floor.
    BelowPrefillFloor { prefill: f64 },
}

/// Why a point is out of the ranking, or `None` when it is in.
///
/// `engine_best_prefill` is the best prefill the point's *own* engine
/// reached at the point's own `ubatch-size`, not the best across engines or
/// batch sizes: both move prefill by design (ik_llama.cpp runs ~480 t/s where
/// llama.cpp runs ~830 on the reference box; ub1024 prefills ~560 where
/// ub2048 prefills ~870), so comparing across them would call an honestly
/// slower setup "spilling". `rank` computes that best and passes it in.
///
/// The order: spilling first, then the decode floor, then the prefill
/// floor. Every comparison is strict, so a point exactly on a
/// floor is kept.
pub fn exclusion(point: &Point, engine_best_prefill: f64, floor: &Floor) -> Option<Exclusion> {
    // With no measurement to compare against, nothing can be "far under" it.
    if engine_best_prefill > 0.0 && point.prefill < 0.75 * engine_best_prefill {
        return Some(Exclusion::Spilling {
            prefill: point.prefill,
            engine_best: engine_best_prefill,
        });
    }
    if point.decode < floor.decode {
        return Some(Exclusion::BelowDecodeFloor { decode: point.decode });
    }
    if point.prefill < floor.prefill {
        return Some(Exclusion::BelowPrefillFloor { prefill: point.prefill });
    }
    None
}

/// How close to the best decode a point must be to count as tied with it,
/// as a fraction of the best.
///
/// Best-of-3 decode on one unchanged setup varies by about 1% (63.1 to 63.9
/// t/s in the 2026-10-25 pair), so a gap under 3% is noise, not a ranking.
/// Within the tie, prefill decides: ub2048 and ub1024 decoded alike there,
/// and ub2048 prefilled 55% faster - which a session pays on every first
/// prompt.
pub const DECODE_TIE: f64 = 0.03;

/// The best prefill each engine reached at each `ubatch-size`, in the order
/// they first appear.
fn best_prefill(points: &[Point]) -> Vec<(&str, u32, f64)> {
    let mut best: Vec<(&str, u32, f64)> = Vec::new();
    for p in points {
        match best.iter_mut().find(|(e, u, _)| *e == p.engine && *u == p.ubatch) {
            Some((_, _, b)) => {
                if p.prefill > *b {
                    *b = p.prefill;
                }
            }
            None => best.push((p.engine.as_str(), p.ubatch, p.prefill)),
        }
    }
    best
}

fn best_of(best: &[(&str, u32, f64)], point: &Point) -> f64 {
    best.iter()
        .find(|(e, u, _)| *e == point.engine && *u == point.ubatch)
        .map(|(_, _, b)| *b)
        .unwrap_or(0.0)
}

/// Point indices, best first, with excluded points dropped.
///
/// Best decode first, except that the points within [`DECODE_TIE`] of the
/// best decode form one band at the top, ordered by prefill. Below the band
/// decode alone decides. The depth floor is deliberately *not* applied: a
/// point is ranked before it is gated (see [`winner`]). Exact ties keep input
/// order, so the sweep's own order decides.
pub fn rank(points: &[Point], floor: &Floor) -> Vec<usize> {
    let best = best_prefill(points);
    let mut kept: Vec<usize> = (0..points.len())
        .filter(|&i| exclusion(&points[i], best_of(&best, &points[i]), floor).is_none())
        .collect();
    let desc = |x: f64, y: f64| y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal);
    // Stable sorts: equal values stay in the order they were measured.
    kept.sort_by(|&a, &b| desc(points[a].decode, points[b].decode));
    let Some(&top) = kept.first() else { return kept };
    let cut = points[top].decode * (1.0 - DECODE_TIE);
    let band = kept.iter().take_while(|&&i| points[i].decode >= cut).count();
    kept[..band].sort_by(|&a, &b| desc(points[a].prefill, points[b].prefill));
    kept
}

/// The point to adopt: the first of [`rank`] that passed the gate and whose
/// depth decode meets the floor. `None` when nothing qualifies.
///
/// A context you cannot use at depth is a fake win, so the depth floor
/// applies here rather than in `rank`. Only a *measured* depth under the
/// floor disqualifies a point: an unmeasured depth is not a violation.
pub fn winner(points: &[Point], floor: &Floor) -> Option<usize> {
    rank(points, floor).into_iter().find(|&i| {
        let p = &points[i];
        p.passed_gate == Some(true)
            && !matches!(p.depth_decode, Some(d) if d > 0.0 && d < floor.depth_decode)
    })
}

/// One rung of the context ladder.
#[derive(Debug, Clone, PartialEq)]
pub enum Rung {
    /// Measure this context next.
    Grow(u64),
    /// The ladder is done.
    Stop,
}

/// What the last rung did, fed back to [`next_context`].
///
/// The ladder needs two bits a plain `{ Starting, Fits, Failed }` signature
/// cannot carry: *which* context a failed
/// grow was aiming at, and whether the one midpoint probe owed after a
/// failure has already been spent. So `Failed` carries the attempt and
/// `Probed` marks the probe as done - the state the ladder needs to stay a
/// pure function.
#[derive(Debug, Clone, PartialEq)]
pub enum RungOutcome {
    /// Nothing measured yet (the seed context): grow normally.
    Starting,
    /// The last rung fit: grow normally.
    Fits,
    /// The last rung, grown to `attempt`, did not fit. The ladder owes one
    /// midpoint probe between the last good context and `attempt`.
    Failed { attempt: u64 },
    /// The one midpoint probe owed after a failure has been measured, fitting
    /// or not: the ladder is finished.
    Probed,
}

/// The context ladder: double toward the ceiling; after a failed rung, try
/// one midpoint; stop when the ceiling is reached or the midpoint adds
/// nothing. `--ctx-ladder` selects it over the prediction
/// ([`ContextSearch`]).
///
/// `current` is the last context that fit (the caller only advances it on a
/// `Fits`), and `ceiling` is the model's trained context (or a pinned
/// `--ctx`). Normal growth is `min(current * 2, ceiling)`, and stops when
/// that no longer exceeds `current`.
///
/// After a `Failed { attempt }` the next rung is the midpoint
/// `(current + attempt) / 2`, tried exactly once: if it adds nothing
/// (`<= current`) the ladder stops at once, and once it has been measured
/// the caller reports `Probed`, which also stops. One midpoint probe,
/// encoded so the loop always terminates.
pub fn next_context(current: u64, ceiling: u64, last: RungOutcome) -> Rung {
    match last {
        // The probe was the last rung: nothing more to try.
        RungOutcome::Probed => Rung::Stop,
        RungOutcome::Failed { attempt } => {
            let mid = current.saturating_add(attempt) / 2;
            if mid > current {
                Rung::Grow(mid)
            } else {
                Rung::Stop
            }
        }
        RungOutcome::Starting | RungOutcome::Fits => {
            let next = current.saturating_mul(2).min(ceiling);
            if next > current {
                Rung::Grow(next)
            } else {
                Rung::Stop
            }
        }
    }
}

/// VRAM against context, as a straight line.
///
/// Two measurements at one split fix it exactly, and that is the whole
/// reason it exists: the model and its KV cache are what grow with context,
/// so the card's ceiling is arithmetic once the slope is known - where the
/// ladder spent four doublings to discover a number two measurements
/// already describe.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    /// MiB the model costs before any context at all.
    pub base_mib: f64,
    /// MiB one more context token costs.
    pub per_token_mib: f64,
}

impl Fit {
    /// The line through two measured points, or `None` when they are not two
    /// different contexts (one point is not a slope).
    pub fn from_points(ctx_a: u64, vram_a: u64, ctx_b: u64, vram_b: u64) -> Option<Fit> {
        if ctx_b <= ctx_a {
            return None;
        }
        let per_token_mib = (vram_b as f64 - vram_a as f64) / (ctx_b - ctx_a) as f64;
        Some(Fit { base_mib: vram_a as f64 - per_token_mib * ctx_a as f64, per_token_mib })
    }

    /// The largest context whose VRAM stays under `wall`, or `None` when the
    /// fit cannot say.
    ///
    /// A slope that is not positive is two measurements disagreeing with the
    /// model rather than a ceiling - noise, or a point taken while the
    /// desktop was holding VRAM - and a prediction from it would be worse
    /// than the ladder it replaces. `None` is that answer.
    pub fn context_under(&self, wall_mib: u64) -> Option<u64> {
        if !(self.per_token_mib > 0.0) {
            return None;
        }
        let room = wall_mib as f64 - self.base_mib;
        if room <= 0.0 {
            return Some(0);
        }
        Some((room / self.per_token_mib).floor() as u64)
    }
}

/// What the last context probe did, fed back to [`next_predicted`].
///
/// The distinction that matters is between the two ways a probe can fail.
/// The plan states it as a rule because the obvious wiring gets it wrong: a
/// failure *over the VRAM wall* is a real measurement - the model loaded and
/// the card went past the wall - so it is a point, and a third point gives a
/// steeper slope. A failure *under the decode floor* is not: the model
/// spilled to system memory, and its VRAM reading describes where the spill
/// ended up rather than what the context costs.
#[derive(Debug, Clone, PartialEq)]
pub enum Probe {
    /// Nothing probed beyond the points themselves.
    None,
    /// The probe fit, and its point is in the caller's list.
    Fits,
    /// The probe loaded and the card went over the wall.
    OverWall { attempt: u64, vram_mib: u64 },
    /// The probe fell under the decode floor.
    UnderFloor { attempt: u64 },
}

/// The context to measure next, predicted from what has been measured.
///
/// `fits` are the (context, VRAM MiB) pairs that fit, in any order; `wall_mib`
/// is what the card can hold; `ceiling` is the model's trained context (or a
/// pinned `--ctx`). One point is not a slope, so the first grow is the
/// ladder's doubling and the prediction starts with the second measurement.
///
/// **A prediction must not fall back to the ladder's midpoint rule.** That
/// rule is right for a ladder, where `current` is a rung one doubling below
/// the attempt, and wrong for a prediction, where `current` is the
/// calibration point near the bottom: seed 4096, prediction 240000, failure,
/// midpoint 122048 - where the ladder would have reached 249344. So an
/// over-wall failure re-predicts from the failed point, and only a floor
/// failure - where the failed point says nothing about cost - bisects.
pub fn next_predicted(fits: &[(u64, u64)], wall_mib: u64, ceiling: u64, last: &Probe) -> Rung {
    let current = fits.iter().map(|(ctx, _)| *ctx).max().unwrap_or(0);
    match last {
        // The spilled point is not a point, so there is nothing to re-fit:
        // bisect between the best context that fit and the attempt, which
        // converges on the floor's own edge.
        Probe::UnderFloor { attempt } => {
            let mid = current.saturating_add(*attempt) / 2;
            if mid > current {
                Rung::Grow(mid)
            } else {
                Rung::Stop
            }
        }
        // The failed point is real, and it is the highest one there is. The
        // two highest give a steeper slope and a second prediction, which is
        // what lands near the true ceiling instead of halfway to it.
        Probe::OverWall { attempt, vram_mib } => {
            let below = fits
                .iter()
                .filter(|(ctx, _)| ctx < attempt)
                .max_by_key(|(ctx, _)| *ctx);
            let Some((ctx, vram)) = below else { return Rung::Stop };
            let fit = Fit::from_points(*ctx, *vram, *attempt, *vram_mib);
            // Strictly below the attempt: a prediction that landed on a
            // context already known to be too big would be the same probe
            // twice, and the loop would not end.
            predict(fit, current, wall_mib, ceiling.min(attempt.saturating_sub(1)))
        }
        Probe::None | Probe::Fits => {
            if fits.len() < 2 {
                return grow(current, ceiling);
            }
            let mut sorted: Vec<(u64, u64)> = fits.to_vec();
            sorted.sort_by_key(|(ctx, _)| *ctx);
            let (ctx_b, vram_b) = sorted[sorted.len() - 1];
            let (ctx_a, vram_a) = sorted[sorted.len() - 2];
            let fit = Fit::from_points(ctx_a, vram_a, ctx_b, vram_b);
            predict(fit, current, wall_mib, ceiling)
        }
    }
}

/// The context search as one value the caller drives: ask [`next`], measure
/// that context, hand the reading back to [`record`], until `next` says stop.
///
/// It exists so the calibration point cannot be forgotten. The winner of the
/// split search was measured at the preset's context, and that reading is the
/// first point of the line; a caller that starts [`next_predicted`] from no
/// points asks it to grow from a context of zero, which stops at once - the
/// search never ran and the tuner kept the preset's context. Holding the
/// points here, seeded at construction, is what makes that impossible.
///
/// [`next`]: ContextSearch::next
/// [`record`]: ContextSearch::record
#[derive(Debug, Clone, PartialEq)]
pub struct ContextSearch {
    fits: Vec<(u64, u64)>,
    probe: Probe,
    wall_mib: u64,
    ceiling: u64,
}

impl ContextSearch {
    /// Start from the point the split search already measured.
    ///
    /// A seed already over the wall is no line to extend - there is no room
    /// to grow into - so the search starts with nothing and stops at once.
    pub fn new(seed_ctx: u64, seed_vram_mib: u64, wall_mib: u64, ceiling: u64) -> ContextSearch {
        let fits = if seed_vram_mib <= wall_mib { vec![(seed_ctx, seed_vram_mib)] } else { Vec::new() };
        ContextSearch { fits, probe: Probe::None, wall_mib, ceiling: ceiling.max(seed_ctx) }
    }

    /// The context to measure next.
    pub fn next(&self) -> Rung {
        next_predicted(&self.fits, self.wall_mib, self.ceiling, &self.probe)
    }

    /// What measuring `ctx` showed. `under_floor` is the decode floor's
    /// verdict, which the caller owns; the wall is this search's. Returns
    /// what the probe was, so the caller can say it.
    pub fn record(&mut self, ctx: u64, vram_mib: u64, under_floor: bool) -> Probe {
        // Under the floor wins when both are true: the spill is what made the
        // decode slow, and its VRAM reading is where the spill ended up rather
        // than what the context costs.
        self.probe = if under_floor {
            Probe::UnderFloor { attempt: ctx }
        } else if vram_mib > self.wall_mib {
            Probe::OverWall { attempt: ctx, vram_mib }
        } else {
            self.fits.push((ctx, vram_mib));
            Probe::Fits
        };
        self.probe.clone()
    }
}

/// What a predicted context is rounded down to.
///
/// Two loads of one context read a few MiB apart (9 MiB measured on the
/// 35B), and near the wall that is several hundred tokens' worth: a
/// prediction finer than this re-fits the line through the noise and spends
/// a load per wobble, each a full model read, for a gain no session notices.
pub const CONTEXT_STEP: u64 = 1024;

/// The largest context the fit allows, on a [`CONTEXT_STEP`], capped by
/// `limit` and only worth growing to when it beats what already fit.
fn predict(fit: Option<Fit>, current: u64, wall_mib: u64, limit: u64) -> Rung {
    let Some(predicted) = fit.and_then(|f| f.context_under(wall_mib)) else {
        return Rung::Stop;
    };
    let target = (predicted / CONTEXT_STEP * CONTEXT_STEP).min(limit);
    if target > current {
        Rung::Grow(target)
    } else {
        Rung::Stop
    }
}

/// The ladder's doubling, for the one grow the prediction cannot make: the
/// second point it needs a slope from.
fn grow(current: u64, ceiling: u64) -> Rung {
    let next = current.saturating_mul(2).min(ceiling);
    if next > current {
        Rung::Grow(next)
    } else {
        Rung::Stop
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(engine: &str, label: &str, decode: f64, prefill: f64, ctx: u64) -> Point {
        Point {
            engine: engine.into(),
            label: label.into(),
            decode,
            prefill,
            ctx,
            ubatch: 0,
            passed_gate: None,
            depth_decode: None,
        }
    }

    fn at_ubatch(mut p: Point, ubatch: u32) -> Point {
        p.ubatch = ubatch;
        p
    }

    #[test]
    fn a_smaller_ubatch_is_not_spilling_against_a_larger_one() {
        // The 2026-10-25 pair: ub1024 prefills ~560 where ub2048 prefills
        // ~870, on the same engine, with nothing spilled.
        let floor = Floor::default();
        let points = vec![
            at_ubatch(point("llama.cpp", "ub2048", 63.6, 889.0, 4096), 2048),
            at_ubatch(point("llama.cpp", "ub1024", 63.9, 564.0, 4096), 1024),
        ];
        let ranked = rank(&points, &floor);
        assert!(ranked.contains(&1), "ub1024 is ranked, not excluded: {ranked:?}");

        // A collapse within one ubatch is still a spill.
        let mut spilled = points.clone();
        spilled.push(at_ubatch(point("llama.cpp", "n33-ub1024", 66.0, 380.0, 4096), 1024));
        assert!(!rank(&spilled, &floor).contains(&2));
    }

    #[test]
    fn a_decode_tie_goes_to_the_faster_prefill() {
        let floor = Floor::default();
        let points = vec![
            at_ubatch(point("llama.cpp", "ub1024", 63.9, 564.0, 4096), 1024),
            // Within 3%: a tie, and the faster prefill wins it.
            at_ubatch(point("llama.cpp", "ub2048", 63.6, 889.0, 4096), 2048),
            // 9% under the best: not a tie, whatever its prefill.
            at_ubatch(point("llama.cpp", "slow", 58.0, 900.0, 4096), 2048),
        ];
        assert_eq!(rank(&points, &floor), vec![1, 0, 2]);
    }

    #[test]
    fn the_default_floor_is_the_one_the_plan_agreed() {
        let f = Floor::default();
        assert_eq!((f.decode, f.prefill, f.depth_decode), (25.0, 350.0, 15.0));
    }

    #[test]
    fn a_floor_excludes_below_it_but_keeps_a_point_exactly_on_it() {
        let floor = Floor::default();
        // Exactly on both floors, and its engine's own best prefill: kept,
        // because every comparison is strict.
        let on = point("llama.cpp", "n0", 25.0, 350.0, 4096);
        assert_eq!(exclusion(&on, 350.0, &floor), None);

        // A hair under either floor: excluded, and named for which one.
        let low_decode = point("llama.cpp", "n1", 24.9, 800.0, 4096);
        assert_eq!(
            exclusion(&low_decode, 800.0, &floor),
            Some(Exclusion::BelowDecodeFloor { decode: 24.9 })
        );
        let low_prefill = point("llama.cpp", "n2", 40.0, 349.9, 4096);
        assert_eq!(
            exclusion(&low_prefill, 349.9, &floor),
            Some(Exclusion::BelowPrefillFloor { prefill: 349.9 })
        );

        // Spilling is reported first when a point is both spilling and slow.
        let both = point("llama.cpp", "n3", 10.0, 480.0, 4096);
        assert_eq!(
            exclusion(&both, 830.0, &floor),
            Some(Exclusion::Spilling { prefill: 480.0, engine_best: 830.0 })
        );
    }

    #[test]
    fn a_slow_engine_is_not_spilling_but_a_collapse_within_one_engine_is() {
        let floor = Floor::default();
        // Engines differ in prefill by design: ik_llama.cpp prefills ~480
        // where llama.cpp prefills ~830. Each on its own best is fine.
        let ik = point("ik_llama.cpp", "n0", 30.0, 480.0, 4096);
        assert_eq!(exclusion(&ik, 480.0, &floor), None);
        let llama_fast = point("llama.cpp", "n0", 60.0, 830.0, 4096);
        assert_eq!(exclusion(&llama_fast, 830.0, &floor), None);

        // Within one engine, 480 against 830 is the sysmem cliff.
        let llama_slow = point("llama.cpp", "n9", 70.0, 480.0, 4096);
        assert_eq!(
            exclusion(&llama_slow, 830.0, &floor),
            Some(Exclusion::Spilling { prefill: 480.0, engine_best: 830.0 })
        );

        // Through rank, the per-engine best decides: the collapsed llama.cpp
        // point is dropped, the honestly slow ik_llama.cpp point survives.
        let points = vec![
            point("llama.cpp", "fast", 60.0, 830.0, 4096),
            point("llama.cpp", "spill", 70.0, 480.0, 4096),
            point("ik_llama.cpp", "slow", 30.0, 480.0, 4096),
        ];
        assert_eq!(rank(&points, &floor), vec![0, 2]);
    }

    #[test]
    fn rank_is_best_decode_first_and_ties_keep_input_order() {
        let floor = Floor::default();
        let points = vec![
            point("llama.cpp", "a", 40.0, 800.0, 4096),
            point("llama.cpp", "b", 55.0, 800.0, 4096),
            point("llama.cpp", "c", 40.0, 800.0, 4096), // ties with a: a first
            point("llama.cpp", "d", 30.0, 800.0, 4096),
        ];
        assert_eq!(rank(&points, &floor), vec![1, 0, 2, 3]);
    }

    #[test]
    fn winner_skips_a_failed_gate_and_a_shallow_depth() {
        let floor = Floor::default();
        let mut points = vec![
            point("llama.cpp", "top", 60.0, 800.0, 4096), // fastest, gate failed
            point("llama.cpp", "shallow", 55.0, 800.0, 4096), // passed, too shallow
            point("llama.cpp", "deep", 40.0, 800.0, 4096), // passed, deep enough
            point("llama.cpp", "ungated", 30.0, 800.0, 4096), // never gated
        ];
        points[0].passed_gate = Some(false);
        points[1].passed_gate = Some(true);
        points[1].depth_decode = Some(10.0); // under the 15 t/s depth floor
        points[2].passed_gate = Some(true);
        points[2].depth_decode = Some(30.0);
        points[3].depth_decode = Some(50.0); // no gate verdict: not a winner
        assert_eq!(winner(&points, &floor), Some(2));
    }

    #[test]
    fn winner_keeps_a_gated_point_with_no_depth_measurement() {
        let floor = Floor::default();
        // Only a point whose depth was measured *and* shallow is skipped.
        let mut points = vec![point("llama.cpp", "x", 50.0, 800.0, 4096)];
        points[0].passed_gate = Some(true);
        assert_eq!(winner(&points, &floor), Some(0));

        // Nothing qualifies when every point fails the gate.
        let mut failed = vec![point("llama.cpp", "y", 50.0, 800.0, 4096)];
        failed[0].passed_gate = Some(false);
        assert_eq!(winner(&failed, &floor), None);
    }

    #[test]
    fn the_ladder_doubles_toward_the_ceiling_then_stops() {
        // Each fit doubles from the seed.
        assert_eq!(next_context(4096, 262144, RungOutcome::Starting), Rung::Grow(8192));
        assert_eq!(next_context(8192, 262144, RungOutcome::Fits), Rung::Grow(16384));
        // The doubling is capped by the ceiling...
        assert_eq!(next_context(200000, 262144, RungOutcome::Fits), Rung::Grow(262144));
        // ...and at (or past) the ceiling there is nothing left to grow.
        assert_eq!(next_context(262144, 262144, RungOutcome::Fits), Rung::Stop);
        assert_eq!(next_context(300000, 262144, RungOutcome::Starting), Rung::Stop);
    }

    #[test]
    fn a_failed_rung_buys_one_midpoint_probe() {
        // cur 4096, the grow to 8192 did not fit: probe (4096 + 8192) / 2.
        assert_eq!(
            next_context(4096, 262144, RungOutcome::Failed { attempt: 8192 }),
            Rung::Grow(6144)
        );
        // Once the probe has been made, the ladder is done whatever it did.
        assert_eq!(next_context(6144, 262144, RungOutcome::Probed), Rung::Stop);
        // A midpoint that adds nothing stops instead of looping.
        assert_eq!(next_context(4096, 262144, RungOutcome::Failed { attempt: 4096 }), Rung::Stop);
        assert_eq!(next_context(4096, 262144, RungOutcome::Failed { attempt: 4097 }), Rung::Stop);
    }

    #[test]
    fn the_frontier_is_found_by_bisection_not_by_walking() {
        // A seed of 35 that fits, and a split of 0 known to spill: the search
        // halves the gap each time, so nine candidates become three probes.
        let mut fits = 35u32;
        let mut spills = 0u32;
        let mut probes = Vec::new();
        while let Some(next) = between(fits, spills) {
            probes.push(next);
            // Pretend the frontier is at 30: below that, it does not fit.
            if next >= 30 {
                fits = next;
            } else {
                spills = next;
            }
        }
        assert_eq!(probes, vec![18, 27, 31, 29, 30], "five probes, not nine");
        assert_eq!((fits, spills), (30, 29), "the frontier is pinned to one layer");

        // Adjacent bounds are done: no load is spent learning nothing.
        assert_eq!(between(30, 29), None);
        assert_eq!(between(30, 30), None);
        assert_eq!(between(1, 0), None);
    }

    #[test]
    fn a_frontier_search_from_the_seed_alone_is_four_probes_at_most() {
        // The worst case the tuner can hit: the whole range is open. Four
        // probes narrow 0..=41 to one layer, which is what the sweep's nine
        // candidates were spending loads to do.
        let mut fits = 41u32;
        let mut spills = 0u32;
        let mut count = 0;
        while let Some(next) = between(fits, spills) {
            count += 1;
            if next >= 20 {
                fits = next
            } else {
                spills = next
            }
        }
        assert!(count <= 6, "{count} probes to narrow 0..=41");
        assert_eq!(fits - spills, 1);
    }

    // ---- the context prediction -----------------------------------------

    /// A fit whose slope is the reference box's own: ~12.7 KB per context
    /// token at q8_0 KV, which is ~0.0121 MiB.
    fn reference_fit() -> Fit {
        // 11.1 GB at 236544 against 6.6 GB at 32768 - the two rows the
        // engine gate measured on the 35B.
        Fit::from_points(32768, 6758, 236544, 11366).expect("two contexts")
    }

    #[test]
    fn two_points_give_the_slope_and_the_base() {
        let fit = reference_fit();
        // (11366 - 6758) / (236544 - 32768) = 4608 / 203776
        assert!((fit.per_token_mib - 0.02261).abs() < 0.0001, "{}", fit.per_token_mib);
        // The base is what the model costs before any context: the smaller
        // point minus its own context's worth.
        assert!((fit.base_mib - 6017.0).abs() < 5.0, "{}", fit.base_mib);
        // One point is not a slope, and neither is a repeated context.
        assert_eq!(Fit::from_points(32768, 6758, 32768, 6758), None);
        assert_eq!(Fit::from_points(32768, 6758, 16384, 5000), None);
    }

    #[test]
    fn the_ceiling_is_the_context_the_wall_leaves_room_for() {
        let fit = reference_fit();
        // A 12 GB card with ~0.5 GB of desktop: ~11.5 GB = 11776 MiB.
        let ceiling = fit.context_under(11776).expect("a slope");
        assert!((236000..=256000).contains(&ceiling), "{ceiling}");
        // A wall the model alone does not fit under is no context at all,
        // not a negative number.
        assert_eq!(fit.context_under(5000), Some(0));
        // A flat or falling slope is two measurements disagreeing with the
        // model, and the honest answer is that the fit cannot say.
        let flat = Fit { base_mib: 6000.0, per_token_mib: 0.0 };
        assert_eq!(flat.context_under(11776), None);
        let falling = Fit { base_mib: 6000.0, per_token_mib: -0.01 };
        assert_eq!(falling.context_under(11776), None);
    }

    #[test]
    fn one_point_doubles_before_it_predicts() {
        // The calibration load is one point, and a line needs two: the first
        // grow is the ladder's doubling, and the prediction starts after it.
        let one = vec![(4096, 6300)];
        assert_eq!(next_predicted(&one, 11776, 262144, &Probe::None), Rung::Grow(8192));
        // At the trained ceiling there is nothing left to double.
        let at_ceiling = vec![(262144, 11000)];
        assert_eq!(next_predicted(&at_ceiling, 11776, 262144, &Probe::None), Rung::Stop);
    }

    #[test]
    fn two_points_predict_the_ceiling_instead_of_climbing_to_it() {
        // The ladder reached 249344 in four doublings from 8192. Two
        // measurements and the wall give the same neighbourhood in one.
        let fits = vec![(32768, 6758), (65536, 7463)];
        match next_predicted(&fits, 11776, 262144, &Probe::Fits) {
            Rung::Grow(ctx) => assert!((180000..=262144).contains(&ctx), "{ctx}"),
            other => panic!("expected a predicted context, got {other:?}"),
        }
    }

    #[test]
    fn an_over_wall_failure_re_predicts_rather_than_bisecting() {
        // The trap the plan names: seed 4096, prediction 240000, failure. The
        // ladder's midpoint rule would answer 122048 - worse than the 249344
        // the ladder itself reached - because `current` here is the
        // calibration point near the bottom, not a rung one doubling below.
        let fits = vec![(4096, 6300), (200000, 10800)];
        let probe = Probe::OverWall { attempt: 240000, vram_mib: 11900 };
        match next_predicted(&fits, 11776, 262144, &probe) {
            Rung::Grow(ctx) => {
                // The steeper slope from the failed point puts the answer
                // near the true ceiling, well above the 122048 midpoint.
                assert!(ctx > 150000, "{ctx} is a bisection, not a prediction");
                assert!(ctx < 240000, "{ctx} is the attempt again");
            }
            other => panic!("expected a re-prediction, got {other:?}"),
        }
    }

    #[test]
    fn an_over_wall_failure_converges_on_the_true_ceiling() {
        // A real over-wall point lies on the real line: the model loaded and
        // the card went past the wall, so its VRAM is what that context
        // actually cost. Two such points are the line, which is why one
        // re-prediction lands at the ceiling instead of walking to it.
        const BASE: f64 = 6000.0;
        const PER_TOKEN: f64 = 0.0245;
        const WALL: u64 = 11776;
        let true_ceiling = ((WALL as f64 - BASE) / PER_TOKEN) as u64;
        let vram_at = |ctx: u64| (BASE + PER_TOKEN * ctx as f64).round() as u64;

        let fits = vec![(8192, vram_at(8192))];
        let attempt = 262144;
        let next = match next_predicted(
            &fits,
            WALL,
            262144,
            &Probe::OverWall { attempt, vram_mib: vram_at(attempt) },
        ) {
            Rung::Grow(ctx) => ctx,
            other => panic!("expected a re-prediction, got {other:?}"),
        };
        // The trap the plan names, stated as a number: the ladder's midpoint
        // between 8192 and 262144 is 135168, which is nowhere near the
        // ceiling the arithmetic finds.
        let midpoint = (8192 + attempt) / 2;
        assert!(
            next.abs_diff(true_ceiling) < true_ceiling / 100,
            "{next} should land within 1% of {true_ceiling}, not at the midpoint {midpoint}"
        );
        assert!(next > midpoint, "{next} is a bisection, not a prediction");

        // And it terminates: that probe fits, and the next prediction from
        // two points on the same line is the same ceiling - nothing left to
        // grow to.
        let fits = vec![(8192, vram_at(8192)), (next, vram_at(next))];
        assert_eq!(next_predicted(&fits, WALL, 262144, &Probe::Fits), Rung::Stop);
    }

    #[test]
    fn an_over_wall_failure_always_makes_progress() {
        // Even when the relationship is not the straight line the fit
        // assumes, each over-wall failure is a point above the wall and each
        // prediction lands strictly lower - so the loop has to end.
        let mut attempt = 262144u64;
        let mut probes = 0;
        while let Rung::Grow(next) = next_predicted(
            &[(8192, 6400)],
            11776,
            262144,
            &Probe::OverWall { attempt, vram_mib: 13000 },
        ) {
            assert!(next < attempt, "{next} does not improve on {attempt}");
            attempt = next;
            probes += 1;
            assert!(probes < 200, "the loop does not terminate");
        }
    }

    #[test]
    fn a_floor_failure_bisects_because_the_point_says_nothing() {
        // Spilled to system memory: its VRAM reading describes where the
        // spill ended up, so it is not a point and there is nothing to
        // re-fit. The midpoint between what fit and the attempt is the
        // honest next question.
        let fits = vec![(8192, 6400), (16384, 6900)];
        let probe = Probe::UnderFloor { attempt: 131072 };
        assert_eq!(next_predicted(&fits, 11776, 262144, &probe), Rung::Grow(73728));
        // Converged: a floor failure just above what fits has nothing left
        // to bisect, so the loop stops instead of asking twice.
        let probe = Probe::UnderFloor { attempt: 8193 };
        assert_eq!(next_predicted(&fits, 11776, 262144, &probe), Rung::Stop);
    }

    /// Drive a whole search against a card whose VRAM is a straight line in
    /// the context, and return (the context it settled on, loads spent).
    fn search_on_card(seed: u64, wall: u64, ceiling: u64, vram_at: impl Fn(u64) -> u64) -> (u64, u32) {
        let mut search = ContextSearch::new(seed, vram_at(seed), wall, ceiling);
        let (mut best, mut loads) = (seed, 0);
        while let Rung::Grow(ctx) = search.next() {
            loads += 1;
            assert!(loads < 20, "the search does not terminate");
            if search.record(ctx, vram_at(ctx), false) == Probe::Fits {
                best = ctx;
            }
        }
        (best, loads)
    }

    #[test]
    fn the_search_grows_from_the_winners_own_point() {
        // The regression this type exists for: the winner of the split search
        // is a measured point, and a search that does not count it never
        // starts. From the 35B's preset context the first grow is the doubling
        // capped at the trained context, not a stop.
        let search = ContextSearch::new(236544, 11060, 11264, 262144);
        assert_eq!(search.next(), Rung::Grow(262144));
        // A seed already past the wall has no room to grow into.
        let full = ContextSearch::new(236544, 11400, 11264, 262144);
        assert_eq!(full.next(), Rung::Stop);
    }

    #[test]
    fn the_search_lands_under_the_wall_in_two_or_three_loads() {
        // The 35B on this card, as measured while serving: ~0.02 MiB per
        // context token on top of ~6.3 GB. The true ceiling under an 11264
        // MiB wall is ~250K; the doubling from 236544 hits the trained 262144,
        // goes over, and the re-prediction from that real point lands under.
        let vram_at = |ctx: u64| 6330 + ctx / 50;
        let true_ceiling = (11264 - 6330) * 50 + 49;
        let (best, loads) = search_on_card(236544, 11264, 262144, vram_at);
        assert!(vram_at(best) <= 11264, "{best} is over the wall");
        assert!(best * 100 >= true_ceiling * 98, "{best} is not within 2% of {true_ceiling}");
        assert!(loads <= 3, "{loads} loads");
        // From a small seed the ladder would double five times; the line
        // needs one doubling and a prediction.
        let (best, loads) = search_on_card(8192, 11264, 262144, vram_at);
        assert!(best * 100 >= true_ceiling * 98, "{best}");
        assert!(loads <= 4, "{loads} loads");
    }

    #[test]
    fn the_load_estimate_is_the_documented_budget_and_never_runs_out() {
        // A fresh run expects about ten loads.
        assert_eq!(loads_left(Phase::Frontier, 0), 10);
        // A frontier that took six loads still owes the one starting now.
        assert_eq!(loads_left(Phase::Frontier, 6), 1 + 2 + 3 + 1);
        assert_eq!(loads_left(Phase::Context, 1), 2 + 1);
        assert_eq!(loads_left(Phase::Gate, 0), 1);
        assert_eq!(loads_left(Phase::Gate, 3), 1, "a gate after a failed one");
    }

    #[test]
    fn the_search_does_not_chase_the_noise_in_a_reading() {
        // The 35B's own readings at n-cpu-moe 35: the seed fits, the trained
        // context goes over, and the re-prediction from that point lands on
        // a step under the wall. A second reading a few MiB off the line then
        // predicts a few hundred tokens more, which is not worth a load.
        let mut search = ContextSearch::new(236544, 11037, 11264, 262144);
        assert_eq!(search.next(), Rung::Grow(262144));
        search.record(262144, 11606, false);
        assert_eq!(search.next(), Rung::Grow(245760));
        assert_eq!(search.record(245760, 11242, false), Probe::Fits);
        assert_eq!(search.next(), Rung::Stop);
    }

    #[test]
    fn the_trained_context_caps_a_model_the_card_could_hold_more_of() {
        // The 0.5B: the card would hold far more than its 32K trained context.
        let (best, loads) = search_on_card(8192, 11264, 32768, |ctx| 900 + ctx / 100);
        assert_eq!((best, loads), (32768, 2));
    }

    #[test]
    fn a_floor_failure_bisects_and_is_not_a_point() {
        let mut search = ContextSearch::new(65536, 7000, 11264, 262144);
        assert_eq!(search.next(), Rung::Grow(131072));
        assert_eq!(search.record(131072, 12000, true), Probe::UnderFloor { attempt: 131072 });
        assert_eq!(search.next(), Rung::Grow(98304));
    }

    #[test]
    fn a_prediction_under_what_already_fit_stops() {
        // The highest context that fit already touches the wall, so the
        // arithmetic has nothing to add and the loop ends on a measured
        // answer rather than on a guess.
        let fits = vec![(32768, 6758), (240000, 11780)];
        assert_eq!(next_predicted(&fits, 11776, 262144, &Probe::Fits), Rung::Stop);
    }
}
