//! The tuner's decisions: which sweep point wins, and how far the context
//! ladder climbs.
//!
//! A port of the decision half of `scripts/tune.sh` - its "5. Winner" and
//! "6. Gate the winner" sections, plus `maximize_ctx` - with no I/O and no
//! process spawning: the shell measures, this module decides. The floors and
//! the reasoning behind them are in `docs/plan-tune-model-context.md`.
//!
//! Three rules travel together and must not be conflated:
//!
//! * the **spill** check is *relative* - a point's prefill against the best
//!   prefill its own engine reached - and catches the Windows driver
//!   silently spilling VRAM to system memory;
//! * the **floors** are *absolute* - a fixed tokens/s bar - and catch a setup
//!   that is merely slow without spilling;
//! * the **depth floor** is applied only to a point that already passed the
//!   eval gate, because a point is ranked before it is gated.

/// The speed floor agreed with the user (docs/plan-tune-model-context.md).
///
/// A point under it is unusable however fast it looks next to worse points,
/// so it is excluded from the ranking rather than merely ranked low. These
/// are the defaults of `TUNE_MIN_*` in `scripts/tune.sh`.
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
/// split it can already predict. That is the whole point of this function: the
/// grid the shell swept spent nine loads to learn what three bisection steps
/// know.
pub fn between(fits: u32, spills: u32) -> Option<u32> {
    if fits <= spills + 1 {
        return None;
    }
    Some(fits - (fits - spills) / 2)
}

/// One measured sweep point.
#[derive(Debug, Clone)]
pub struct Point {
    pub engine: String,
    pub label: String,
    pub decode: f64,
    pub prefill: f64,
    pub ctx: u64,
    /// Result of the eval gate for this point, when it was gated.
    pub passed_gate: Option<bool>,
    /// Decode at full depth, from the gate's own measurement.
    pub depth_decode: Option<f64>,
}

/// Why a point is out of the ranking, or `None` when it is in.
#[derive(Debug, Clone, PartialEq)]
pub enum Exclusion {
    /// Prefill collapsed against the best the same engine reached: the
    /// driver spilled to system memory (`scripts/tune.sh` "5. Winner").
    Spilling { prefill: f64, engine_best: f64 },
    /// Decode under the absolute floor.
    BelowDecodeFloor { decode: f64 },
    /// Prefill under the absolute floor.
    BelowPrefillFloor { prefill: f64 },
}

/// Why a point is out of the ranking, or `None` when it is in.
///
/// `engine_best_prefill` is the best prefill the point's *own* engine
/// reached, not the best across engines: engines differ in prefill by design
/// (ik_llama.cpp runs ~480 t/s where llama.cpp runs ~830 on the reference
/// box), so comparing across engines would call an honest slow engine
/// "spilling". `rank` computes the per-engine best and passes it in.
///
/// The order matches the shell: spilling first, then the decode floor, then
/// the prefill floor. Every comparison is strict, so a point exactly on a
/// floor is kept.
pub fn exclusion(point: &Point, engine_best_prefill: f64, floor: &Floor) -> Option<Exclusion> {
    // The shell guards the spill test with `m > 0`: with no measurement to
    // compare against, nothing can be "far under" it.
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

/// The best prefill each engine reached, in the order engines first appear.
fn best_prefill_per_engine(points: &[Point]) -> Vec<(String, f64)> {
    let mut best: Vec<(String, f64)> = Vec::new();
    for p in points {
        match best.iter_mut().find(|(e, _)| e == &p.engine) {
            Some((_, b)) => {
                if p.prefill > *b {
                    *b = p.prefill;
                }
            }
            None => best.push((p.engine.clone(), p.prefill)),
        }
    }
    best
}

fn engine_best(best: &[(String, f64)], engine: &str) -> f64 {
    best.iter().find(|(e, _)| e == engine).map(|(_, b)| *b).unwrap_or(0.0)
}

/// Point indices, best decode first, with excluded points dropped.
///
/// The spill check needs each engine's own best prefill, computed here from
/// `points` (the shell does the same in its `ENG_MAXPRE` pass). The depth
/// floor is deliberately *not* applied: a point is ranked before it is gated
/// (see [`winner`]). Ties in decode keep input order, so the sweep's own
/// order decides.
pub fn rank(points: &[Point], floor: &Floor) -> Vec<usize> {
    let best = best_prefill_per_engine(points);
    let mut kept: Vec<usize> = (0..points.len())
        .filter(|&i| exclusion(&points[i], engine_best(&best, &points[i].engine), floor).is_none())
        .collect();
    // A stable sort: equal decodes stay in the order they were measured.
    kept.sort_by(|&a, &b| {
        points[b].decode.partial_cmp(&points[a].decode).unwrap_or(std::cmp::Ordering::Equal)
    });
    kept
}

/// The point to adopt: the first of [`rank`] that passed the gate and whose
/// depth decode meets the floor. `None` when nothing qualifies.
///
/// A context you cannot use at depth is a fake win, so the depth floor
/// applies here rather than in `rank`. Mirroring the shell, only a *measured*
/// depth under the floor disqualifies a point: an unmeasured depth is not a
/// violation (the shell skips a point only when `gen_tok_s_at_depth` is
/// present and under the floor).
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
/// The shell's `maximize_ctx` loop carries two bits the plain
/// `{ Starting, Fits, Failed }` signature cannot: *which* context a failed
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
/// nothing. Mirrors `maximize_ctx` in `scripts/tune.sh`.
///
/// `current` is the last context that fit (the caller only advances it on a
/// `Fits`), and `ceiling` is the model's trained context (or a pinned
/// `--ctx`). Normal growth is `min(current * 2, ceiling)`, and stops when
/// that no longer exceeds `current`.
///
/// After a `Failed { attempt }` the next rung is the midpoint
/// `(current + attempt) / 2`, tried exactly once: if it adds nothing
/// (`<= current`) the ladder stops at once, and once it has been measured
/// the caller reports `Probed`, which also stops. That is the shell's single
/// midpoint probe, encoded so the loop always terminates.
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

/// The largest context the fit allows, capped by `limit` and only worth
/// growing to when it beats what already fit.
fn predict(fit: Option<Fit>, current: u64, wall_mib: u64, limit: u64) -> Rung {
    let Some(predicted) = fit.and_then(|f| f.context_under(wall_mib)) else {
        return Rung::Stop;
    };
    let target = predicted.min(limit);
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
            passed_gate: None,
            depth_decode: None,
        }
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
        // because the shell compares strictly.
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
        // The shell only skips a point whose depth was measured *and* shallow.
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

    #[test]
    fn a_prediction_under_what_already_fit_stops() {
        // The highest context that fit already touches the wall, so the
        // arithmetic has nothing to add and the loop ends on a measured
        // answer rather than on a guess.
        let fits = vec![(32768, 6758), (240000, 11780)];
        assert_eq!(next_predicted(&fits, 11776, 262144, &Probe::Fits), Rung::Stop);
    }
}
