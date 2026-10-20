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
}
