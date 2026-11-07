//! The tuner's decisions: where the split frontier is, which point wins, and
//! how far the context grows - with no I/O and no process spawning. The
//! binary's `tune.rs` measures; this module decides. The floors, the wall
//! and the reasoning behind them are in `docs/tuning.md`.
//!
//! Four rules travel together and must not be conflated:
//!
//! * the **wall** is the card's size less the desktop's headroom, and a point
//!   whose whole-card reading is over it does not fit, however fast it ran;
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
    /// The share of the agent suite's tasks the gate must see solved, on
    /// each API (`agent::passes`).
    pub agent: f64,
}

impl Default for Floor {
    fn default() -> Self {
        // 25 felt fine in real sessions where 16.7 did not; 350 refuses the
        // ~80 s first prompt; 15 refuses a context that is unusable at depth;
        // 0.75 is nine of the suite's eleven tasks on each API.
        Floor { decode: 25.0, prefill: 350.0, depth_decode: 15.0, agent: 0.75 }
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
    /// Median decode of the bench's runs, tokens/s.
    pub decode: f64,
    /// Best decode of the same runs, 0 when unknown: with the median it is
    /// the bench's own spread, which sets how wide a decode tie is
    /// ([`tie_band`]).
    pub decode_best: f64,
    pub prefill: f64,
    pub ctx: u64,
    /// The `ubatch-size` it ran with, 0 for the engine's default. Prefill
    /// moves with it by design, so the spill check compares within it.
    pub ubatch: u32,
    /// The bench's whole-card VRAM reading, MiB.
    pub vram_mib: f64,
    /// Result of the eval gate for this point, when it was gated.
    pub passed_gate: Option<bool>,
    /// Decode at full depth, from the gate's own measurement.
    pub depth_decode: Option<f64>,
}

/// Why a point is out of the ranking, or `None` when it is in.
#[derive(Debug, Clone, PartialEq)]
pub enum Exclusion {
    /// The whole-card reading is over the wall: the setup does not fit.
    OverWall { vram_mib: f64, wall_mib: u64 },
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
/// `wall_mib` is the whole-card ceiling (`Machine::wall_mib` in the binary).
/// The split search already refuses a split over it, but the refine probes
/// and the splits the bisection measured on its way are ranked too, and a
/// split one layer below the frontier can decode faster precisely because
/// it holds more than the card can spare.
///
/// The order: the wall first, then spilling, then the decode floor, then the
/// prefill floor. Every comparison is strict, so a point exactly on the wall
/// or a floor is kept.
pub fn exclusion(point: &Point, engine_best_prefill: f64, floor: &Floor, wall_mib: u64) -> Option<Exclusion> {
    if point.vram_mib > wall_mib as f64 {
        return Some(Exclusion::OverWall { vram_mib: point.vram_mib, wall_mib });
    }
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

/// The narrowest a decode tie is, as a fraction of the best decode.
///
/// Best-of-3 decode on one unchanged setup varies by about 1% on a quiet
/// desktop (63.1 to 63.9 t/s in the 2026-10-25 pair), so a gap under 3% is
/// noise, not a ranking. Within the tie, prefill decides: ub2048 and ub1024
/// decoded alike there, and ub2048 prefilled 55% faster - which a session
/// pays on every first prompt.
pub const DECODE_TIE: f64 = 0.03;

/// The widest a decode tie is, however noisy the benches were: a gap past
/// it is a ranking whatever the spread says.
pub const DECODE_TIE_MAX: f64 = 0.08;

/// How far under `top`'s decode `other` may fall and still tie with it, as a
/// fraction: the wider of the two benches' own spreads (best over median),
/// held between [`DECODE_TIE`] and [`DECODE_TIE_MAX`].
///
/// A drafting model's decode moves with what each run happens to accept,
/// and a busy desktop moves it further: in the reference box's third tune
/// of 2026-10-29 the n35 bench read 52.2 median / 54.3 best and the ub1024
/// bench 54.3 / 57.0, so a 4% gap between the medians sat inside both
/// benches' own spread - and a fixed 3% band ranked ub1024 first on it,
/// at a third less prefill than the quiet pair had measured for ub2048.
pub fn tie_band(top: &Point, other: &Point) -> f64 {
    spread(top).max(spread(other)).clamp(DECODE_TIE, DECODE_TIE_MAX)
}

/// Whether `other` decodes close enough to `top` to tie with it.
pub fn tied(top: &Point, other: &Point) -> bool {
    other.decode >= top.decode * (1.0 - tie_band(top, other))
}

/// A bench's spread, best over median, as a fraction of the median; 0 when
/// the best is unknown.
pub fn spread(p: &Point) -> f64 {
    if p.decode > 0.0 && p.decode_best > p.decode {
        (p.decode_best - p.decode) / p.decode
    } else {
        0.0
    }
}

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
/// Best decode first, except that the points tied with the best decode
/// ([`tie_band`]) form one band at the top, ordered by prefill. Below the
/// band decode alone decides. The depth floor is deliberately *not* applied:
/// a point is ranked before it is gated (see [`winner`]). Exact ties keep
/// input order, so the sweep's own order decides.
pub fn rank(points: &[Point], floor: &Floor, wall_mib: u64) -> Vec<usize> {
    let best = best_prefill(points);
    let mut kept: Vec<usize> = (0..points.len())
        .filter(|&i| exclusion(&points[i], best_of(&best, &points[i]), floor, wall_mib).is_none())
        .collect();
    let desc = |x: f64, y: f64| y.partial_cmp(&x).unwrap_or(std::cmp::Ordering::Equal);
    // Stable sorts: equal values stay in the order they were measured.
    kept.sort_by(|&a, &b| desc(points[a].decode, points[b].decode));
    let Some(&top) = kept.first() else { return kept };
    // Each point's band is its own, so a noisy bench can tie with the top
    // while a quieter one above it does not: partition, not a prefix.
    let (mut band, rest): (Vec<usize>, Vec<usize>) =
        kept.into_iter().partition(|&i| tied(&points[top], &points[i]));
    band.sort_by(|&a, &b| desc(points[a].prefill, points[b].prefill));
    band.extend(rest);
    band
}

/// Why `points[i]` is out of [`rank`], judged against the same per-engine
/// bests `rank` uses, or `None` when it is ranked.
pub fn excluded(points: &[Point], i: usize, floor: &Floor, wall_mib: u64) -> Option<Exclusion> {
    let best = best_prefill(points);
    exclusion(&points[i], best_of(&best, &points[i]), floor, wall_mib)
}

/// The point to adopt: the first of [`rank`] that passed the gate and whose
/// depth decode meets the floor. `None` when nothing qualifies.
///
/// A context you cannot use at depth is a fake win, so the depth floor
/// applies here rather than in `rank`. Only a *measured* depth under the
/// floor disqualifies a point: an unmeasured depth is not a violation.
pub fn winner(points: &[Point], floor: &Floor, wall_mib: u64) -> Option<usize> {
    rank(points, floor, wall_mib).into_iter().find(|&i| {
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
    /// Probes that loaded and went over the wall: real points of the line
    /// too, for [`ContextSearch::measured_slope`].
    over: Vec<(u64, u64)>,
    probe: Probe,
    wall_mib: u64,
    ceiling: u64,
    slope: Option<f64>,
}

impl ContextSearch {
    /// Start from the point the split search already measured.
    ///
    /// A seed already over the wall is no line to extend - there is no room
    /// to grow into - so the search starts with nothing and stops at once.
    pub fn new(seed_ctx: u64, seed_vram_mib: u64, wall_mib: u64, ceiling: u64) -> ContextSearch {
        let fits = if seed_vram_mib <= wall_mib { vec![(seed_ctx, seed_vram_mib)] } else { Vec::new() };
        ContextSearch { fits, over: Vec::new(), probe: Probe::None, wall_mib, ceiling: ceiling.max(seed_ctx), slope: None }
    }

    /// What one more context token costs, already measured on another
    /// candidate of the same model, `ubatch-size` and drafting: the KV
    /// cache is the same per token whatever the split, so the seed alone
    /// fixes the line, and the doubling that would measure its slope again
    /// - a whole model load - is not spent.
    pub fn with_slope(mut self, per_token_mib: Option<f64>) -> ContextSearch {
        self.slope = per_token_mib.filter(|s| *s > 0.0);
        self
    }

    /// The context to measure next.
    pub fn next(&self) -> Rung {
        if let (Some(per_token_mib), [(ctx, vram)], Probe::None | Probe::Fits) = (self.slope, self.fits.as_slice(), &self.probe) {
            let fit = Fit { base_mib: *vram as f64 - per_token_mib * *ctx as f64, per_token_mib };
            return predict(Some(fit), *ctx, self.wall_mib, self.ceiling);
        }
        next_predicted(&self.fits, self.wall_mib, self.ceiling, &self.probe)
    }

    /// The slope this search measured: through its lowest and highest real
    /// points, fits and over-the-wall probes alike, the widest span and so
    /// the least noise. `None` when it has fewer than two.
    pub fn measured_slope(&self) -> Option<f64> {
        let points: Vec<(u64, u64)> = self.fits.iter().chain(&self.over).copied().collect();
        let low = points.iter().min_by_key(|(ctx, _)| *ctx)?;
        let high = points.iter().max_by_key(|(ctx, _)| *ctx)?;
        Fit::from_points(low.0, low.1, high.0, high.1).map(|f| f.per_token_mib).filter(|s| *s > 0.0)
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
            self.over.push((ctx, vram_mib));
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

/// How far apart two whole-card readings of one setup can land, MiB.
///
/// Two loads on a quiet desktop read 9 MiB apart; on a busy one the desktop
/// itself moves between loads - on the reference box's evening tune of
/// 2026-10-29 the seed read 11115 MiB and a context 1024 tokens larger 11279,
/// where the KV cache accounts for 25. A prediction that promises less
/// context than this much VRAM buys is a probe of the noise.
pub const READING_NOISE_MIB: f64 = 128.0;

/// The largest context the fit allows, on a [`CONTEXT_STEP`], capped by
/// `limit` and only worth growing to when it beats what already fit by more
/// than a reading's noise is worth in tokens ([`READING_NOISE_MIB`]): that
/// evening's search spent its last two loads on predictions 2048 and 1024
/// tokens above the seed, both of which read over the wall.
fn predict(fit: Option<Fit>, current: u64, wall_mib: u64, limit: u64) -> Rung {
    let Some(fit) = fit else { return Rung::Stop };
    let Some(predicted) = fit.context_under(wall_mib) else {
        return Rung::Stop;
    };
    let target = (predicted / CONTEXT_STEP * CONTEXT_STEP).min(limit);
    let noise = (READING_NOISE_MIB / fit.per_token_mib) as u64;
    if target > current.saturating_add(noise) {
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

/// The context a winner must serve: enough for the gate's needle at 90% of
/// the context (`gate_tokens` / 0.9, rounded up to a 1024-token step), so a
/// setup that passes the gate is one an agent session can use at that size.
/// A model trained on less is held only to its trained context (`0` is
/// unknown, and holds it to the gate).
pub fn required_ctx(gate_tokens: u64, ctx_train: u64) -> u64 {
    let need = (gate_tokens * 10).div_ceil(9).div_ceil(1024) * 1024;
    if ctx_train > 0 {
        need.min(ctx_train)
    } else {
        need
    }
}

/// The context the split frontier is searched at when the tuner grows the
/// context itself: the preset's, raised to what the gate needs. A split is
/// only as good as the context it leaves room for - Agents-A1, pulled at
/// 32768, found its frontier at `n-cpu-moe 23` (2026-11-07) and that split
/// filled the card at 54272 tokens, a quarter of the 223232 the gate needs.
pub fn frontier_ctx(preset_ctx: u64, gate_tokens: u64, ctx_train: u64) -> u64 {
    preset_ctx.max(required_ctx(gate_tokens, ctx_train))
}

/// A measured point kept for later runs (`data/tuned/<host>/points.jsonl`):
/// what one candidate measured, under the key that says when it is the same
/// candidate ([`point_key`]), with the desktop's share of the card at the
/// start of the run that measured it.
#[derive(Debug, Clone, PartialEq)]
pub struct Kept {
    pub key: String,
    pub ts: String,
    pub desktop_mib: f64,
    pub decode: f64,
    pub decode_best: f64,
    pub prefill: f64,
    pub vram_mib: f64,
    /// The load went over the wall and was not benched.
    pub wall_only: bool,
    pub after_load_mib: Option<f64>,
}

/// A candidate's identity for reuse: the model file, the engine and its
/// build on its backend, the candidate's effective flags in order, and the
/// bench's runs and prefill size. A change to any of them is another point.
pub fn point_key(model: &str, engine: &str, build: &str, flags: &[(String, String)], runs: u32, prefill: u32) -> String {
    let mut flags: Vec<String> = flags.iter().map(|(k, v)| format!("{k}={v}")).collect();
    flags.sort();
    format!("{model}|{engine}|{build}|{}|runs={runs}|prefill={prefill}", flags.join(","))
}

impl Kept {
    pub fn to_json(&self) -> String {
        use crate::json::Value;
        let num = |n: f64| crate::json::parse(&format!("{n}")).unwrap_or(Value::Null);
        Value::object(vec![
            ("key", Value::str(&self.key)),
            ("ts", Value::str(&self.ts)),
            ("desktop_mib", num(self.desktop_mib)),
            ("decode", num(self.decode)),
            ("decode_best", num(self.decode_best)),
            ("prefill", num(self.prefill)),
            ("vram_mib", num(self.vram_mib)),
            ("wall_only", Value::Bool(self.wall_only)),
            ("after_load_mib", self.after_load_mib.map_or(Value::Null, num)),
        ])
        .to_json()
    }

    /// One line of the file read back; `None` for a line that is not one.
    pub fn parse(line: &str) -> Option<Kept> {
        let v = crate::json::parse(line).ok()?;
        let f = |k: &str| v.get(k).and_then(crate::json::Value::as_f64);
        Some(Kept {
            key: v.get("key")?.as_str()?.to_string(),
            ts: v.get("ts").and_then(|t| t.as_str()).unwrap_or_default().to_string(),
            desktop_mib: f("desktop_mib")?,
            decode: f("decode")?,
            decode_best: f("decode_best").unwrap_or(0.0),
            prefill: f("prefill").unwrap_or(0.0),
            vram_mib: f("vram_mib")?,
            wall_only: v.get("wall_only").and_then(|b| b.as_bool()).unwrap_or(false),
            after_load_mib: f("after_load_mib"),
        })
    }
}

/// Whether a kept point stands for a measurement now, and as what: the
/// newest one under `key` in `kept`, its readings moved by how far the
/// desktop's share moved since (`desktop_now_mib`). It stands when the
/// desktop moved no more than a reading's noise ([`READING_NOISE_MIB`]) -
/// the speeds are the same setup's - or when it read over the wall and the
/// moved reading still is, whichever way the desktop went. Anything else is
/// measured again.
pub fn reuse(kept: &[Kept], key: &str, desktop_now_mib: f64, wall_mib: u64) -> Option<Kept> {
    let newest = kept.iter().filter(|k| k.key == key).max_by(|a, b| a.ts.cmp(&b.ts))?;
    let drift = desktop_now_mib - newest.desktop_mib;
    let moved = Kept {
        vram_mib: newest.vram_mib + drift,
        after_load_mib: newest.after_load_mib.map(|m| m + drift),
        desktop_mib: desktop_now_mib,
        ..newest.clone()
    };
    if drift.abs() <= READING_NOISE_MIB {
        return Some(moved);
    }
    (newest.wall_only && moved.vram_mib > wall_mib as f64).then_some(moved)
}

/// The flags that move a setup's speed and memory but not what it answers:
/// how much context it holds, how many threads it runs and how its weights
/// are loaded. The split, the layers on the card and the batch sizes are not
/// among them - each changes which kernels sum a token's numbers and in what
/// order, and a greedy answer turns on that (docs/tuning.md); `fit` picks the
/// split, so it is not either.
pub const SPEED_ONLY_FLAGS: &[&str] = &["ctx-size", "threads", "threads-batch", "load-mode"];

/// A gate's setup, as [`kept_agent`] matches it against the rows.
#[derive(Debug, Clone, PartialEq)]
pub struct GateSetup {
    pub host: String,
    pub model_file: String,
    pub engine: String,
    /// The adapter's pin (`b11379`); a row records the engine's own
    /// `<pin>-<commit>` (`b11379-1537a0a8b`).
    pub build: String,
    pub backend: String,
    /// The setup's effective flags, as a row records them.
    pub flags: Vec<(String, String)>,
    pub ctx: u64,
}

/// The agent suite an earlier gate ran that stands for this one's: the
/// newest eval row of the same host that ran today's suite (every task of
/// [`crate::agent::TASKS`]) on both APIs, with
/// the same model file, engine, build and backend and the same flags but
/// the ones that move only the speed ([`SPEED_ONLY_FLAGS`]), at a context no
/// larger than this setup's - the suite's turns fit in what it had. The suite
/// asks at temperature 0 with a fixed token cap, so one setup answers it the
/// same way every run. `None` when there is no such row, or when the newest
/// one is under `floor`: a suite that failed is run again, not carried.
pub fn kept_agent<'a>(rows: &'a [crate::results::Row], setup: &GateSetup, floor: f64) -> Option<&'a crate::results::Row> {
    let answers = |flags: &[(String, String)]| {
        let mut kept: Vec<(String, String)> =
            flags.iter().filter(|(k, _)| !SPEED_ONLY_FLAGS.contains(&k.as_str())).cloned().collect();
        kept.sort();
        kept
    };
    let wanted = answers(&setup.flags);
    let build = format!("{}-", setup.build);
    let newest = rows
        .iter()
        .filter(|r| r.kind == crate::results::Kind::Eval)
        .filter(|r| ["anthropic", "openai"].iter().all(|api| r.agent.iter().any(|s| s.api == *api)))
        .filter(|r| r.agent.iter().all(|s| s.total as usize == crate::agent::TASKS.len()))
        .filter(|r| r.host == setup.host && r.model_file == setup.model_file && r.engine == setup.engine)
        .filter(|r| r.engine_build == setup.build || r.engine_build.starts_with(&build))
        .filter(|r| r.served_backend() == setup.backend)
        .filter(|r| answers(&r.flags) == wanted)
        .filter(|r| r.ctx.is_some_and(|ctx| ctx <= setup.ctx))
        .max_by(|a, b| a.ts.cmp(&b.ts))?;
    crate::agent::passes(&newest.agent, floor).then_some(newest)
}

/// How close to the decode floor a split has to be before the thread probe is
/// worth a model load.
///
/// Threads move decode by a few percent (16 of 20 threads beat all 20 by ~8%
/// on the reference box), so a setup already well clear of the floor is not
/// one a thread count rescues - and the probe is a whole load, which is what
/// this tuner is spending its budget on. Within a quarter of the floor, the
/// knob can still be the difference between passing and failing.
pub const THREAD_PROBE_MARGIN: f64 = 1.25;

/// Whether the thread probe is worth a load at this decode.
///
/// Pure, and separate from the probe itself, because the answer is the whole
/// reason the knob is not swept: see [`THREAD_PROBE_MARGIN`].
pub fn thread_probe_worth_it(decode: f64, floor: f64) -> bool {
    decode <= floor * THREAD_PROBE_MARGIN
}

/// The thread counts worth probing on this CPU.
///
/// A hybrid P/E part has fewer logical threads than twice its physical
/// cores (the i7-12700KF: 8 P with hyperthreads + 4 E = 20 logical, 12
/// physical), so the P-cores-plus-HT count is `logical - physical`; a
/// plain SMT box wants its physical count and all-but-two.
pub fn thread_candidates(cores: u32, phys: u32) -> Vec<u32> {
    // A CPU without SMT (logical = physical) has no P-core count to read
    // off, and falls through to the plain list.
    let p = cores.saturating_sub(phys);
    if phys > 0 && phys * 2 > cores && p > 0 {
        vec![p, p * 2, cores]
    } else {
        vec![if phys > 0 { phys } else { cores / 2 }, cores.saturating_sub(2), cores]
    }
}

/// What a sweep candidate's file is written with: the caller's overrides,
/// plus `load-mode = auto` when the preset locks its weights.
///
/// The caller's list is what the winner's overlay is built from, so this is
/// deliberately a *copy* plus one entry: the sweep measures in mmap, and the
/// adopted setup keeps the mode the preset serves with. See
/// the binary's `Tuner::measure` for why that is the difference between a sweep and a
/// crashed desktop.
pub fn candidate_overrides(locks: bool, overrides: &[crate::ini::Entry]) -> Vec<crate::ini::Entry> {
    let mut out = overrides.to_vec();
    if locks {
        out.push(crate::ini::Entry::new("load-mode", "auto"));
    }
    out
}

/// The distinct layer names in a `-ot` placement pattern.
///
/// The tool prints its pattern with the dots escaped for its own regex -
/// `blk\.3\.ffn_.*_exps.=CPU` - so a layer is `blk`, a backslash, a dot and
/// digits, and the answer is the count of unique ones. Splitting the pattern on non-alphanumeric characters instead finds no
/// `blk.` token at all, because the backslash is one of them; the seed then
/// silently falls back to the preset's own split, and the sweep starts from
/// the wrong place. That is the bug this function exists to not have.
pub fn layer_names(pattern: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut rest = pattern;
    while let Some(at) = rest.find("blk") {
        let candidate = &rest[at..];
        // `blk` + an optional escaping backslash + `.` + the layer number.
        let after_blk = candidate.strip_prefix("blk").unwrap_or(candidate);
        let after_slash = after_blk.strip_prefix('\\').unwrap_or(after_blk);
        if let Some(digits) = after_slash.strip_prefix('.') {
            let number: String = digits.chars().take_while(char::is_ascii_digit).collect();
            if !number.is_empty() {
                out.insert(format!("blk.{number}"));
            }
        }
        rest = &candidate[3..];
    }
    out
}

/// The VRAM reading of a bench that stopped over the wall (`OVER_WALL
/// vram=11500MiB`).
pub fn over_wall_reading(text: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("OVER_WALL vram="))
        .and_then(|v| v.trim_end_matches("MiB").parse().ok())
}

/// The bench's reading right after the load (`vram after the load: 10956
/// MiB (wall ...)`).
pub fn after_load_reading(text: &str) -> Option<f64> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("vram after the load: "))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|v| v.parse().ok())
}

/// What makes two context searches share a slope: the engine, the
/// `ubatch-size` (the compute buffers) and the drafting (an MTP head keeps a
/// KV cache of its own). The split does not enter: the KV cache is on the
/// card whatever the split.
pub fn slope_key(engine: &str, ubatch: u32, overrides: &[crate::ini::Entry]) -> String {
    let spec = overrides.iter().rev().find(|e| e.key == "spec-type").map_or("preset", |e| e.value.as_str());
    format!("{engine}|ub{ubatch}|{spec}")
}

/// The bench's own RESULT line, out of everything the run printed.
pub fn bench_result(text: &str) -> Option<crate::evalparse::Bench> {
    text.lines()
        .filter(|line| line.trim_start().starts_with("RESULT"))
        .map(crate::evalparse::Bench::parse)
        .find(|b| b.decode_median.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bench_says_its_two_readings() {
        let over = "vram after the load: 11442 MiB (wall 11264 MiB)\nOVER_WALL vram=11442MiB\n";
        assert_eq!(over_wall_reading(over), Some(11442.0));
        assert_eq!(after_load_reading(over), Some(11442.0));
        let full = "vram after the load: 10956 MiB (wall 11264 MiB)\n\nRESULT decode_best=57.3 vram=11028MiB\n";
        assert_eq!(over_wall_reading(full), None);
        assert_eq!(after_load_reading(full), Some(10956.0));
        assert_eq!(after_load_reading("RESULT decode_best=1"), None);
    }

    #[test]
    fn a_sweep_candidate_uses_mmap_and_the_overlay_does_not() {
        let measured = vec![crate::ini::Entry::new("n-cpu-moe", "35"), crate::ini::Entry::new("ctx-size", "249344")];
        // The preset locks its weights: the candidate file gets mmap...
        let for_file = candidate_overrides(true, &measured);
        assert_eq!(for_file.len(), measured.len() + 1);
        assert!(for_file.iter().any(|e| e.key == "load-mode" && e.value == "auto"));
        // ...and the list the overlay is built from is untouched, so the
        // adopted setup keeps the preset's `mlock`.
        assert_eq!(measured.len(), 2, "the caller's list is not modified");
        assert!(!measured.iter().any(|e| e.key == "load-mode"));

        // A preset that does not lock is measured exactly as it serves.
        assert_eq!(candidate_overrides(false, &measured), measured);
    }

    #[test]
    fn layer_names_reads_the_escaped_placement_pattern() {
        // What llama-fit-params actually prints, escaping the dots for its own
        // regex: the unique `blk\.N` are the layers.
        let pattern = "blk\\.3\\.ffn_.*_exps.=CPU";
        assert_eq!(layer_names(pattern).len(), 1, "{:?}", layer_names(pattern));
        assert!(layer_names(pattern).contains("blk.3"));

        let many = "blk\\.0\\.ffn_.*_exps.=CPU,blk\\.1\\.ffn_.*_exps.=CPU,blk\\.2\\.ffn_.*_exps.=CPU";
        assert_eq!(layer_names(many).len(), 3);
        // A repeated layer counts once, as `sort -u | wc -l` does.
        let repeated = "blk\\.4\\.ffn_.*_exps.=CPU,blk\\.4\\.ffn_.*_exps.=CPU";
        assert_eq!(layer_names(repeated).len(), 1);
        // The unescaped spelling is read too, and a pattern with no layer at
        // all yields nothing - which is what makes the caller fall back.
        assert_eq!(layer_names("blk.7.ffn_.*_exps.=CPU").len(), 1);
        assert!(layer_names("nothing here").is_empty());
    }

    #[test]
    fn a_hybrid_cpu_probes_the_p_core_counts() {
        assert_eq!(thread_candidates(20, 12), vec![8, 16, 20]);
        // A plain SMT box: physical, all-but-two, all.
        assert_eq!(thread_candidates(16, 8), vec![8, 14, 16]);
        // No physical count to be had: half, all-but-two, all.
        assert_eq!(thread_candidates(8, 0), vec![4, 6, 8]);
    }

    #[test]
    fn the_thread_probe_is_spent_only_near_the_floor() {
        // The knob is worth a model load when decode is close enough to the
        // floor that a few percent decides it, and not otherwise: the
        // reference box's 62 t/s against a 25 t/s floor is 2.5x clear.
        assert!(!thread_probe_worth_it(62.0, 25.0), "62 t/s is not near a 25 t/s floor");
        assert!(thread_probe_worth_it(30.0, 25.0), "30 t/s is");
        assert!(thread_probe_worth_it(25.0, 25.0), "and so is exactly the floor");
        assert!(thread_probe_worth_it(31.0, 25.0), "31 is inside the margin");
        assert!(!thread_probe_worth_it(31.5, 25.0), "31.5 is outside it");
    }

    #[test]
    fn the_bench_line_is_found_among_the_noise() {
        let text = "warmup...\nrun 1: prompt=115 tok\nRESULT decode_best=312.5 decode_median=310.2 prefill=18681 vram=1515\n";
        let bench = bench_result(text).expect("a bench line");
        assert_eq!(bench.decode_median, Some(310.2));
        assert_eq!(bench.prefill, Some(18681.0));
        assert_eq!(bench.vram_mib, Some(1515.0));
        // An eval run has RESULT lines but no decode: not a bench.
        assert!(bench_result("RESULT gen_tok_s_best=330.2\nRESULT long_ctx_100=PASS\n").is_none());
    }

    #[test]
    fn a_winner_holds_the_gate_at_ninety_percent() {
        // 200000 / 0.9 = 222223, the next 1024 step is 223232, and the gate
        // there asks for 200908 - capped at 200000.
        assert_eq!(required_ctx(200_000, 262_144), 223_232);
        assert!(223_232 * 9 / 10 >= 200_000);
        // A model trained on less is held to what it was trained on.
        assert_eq!(required_ctx(200_000, 32_768), 32_768);
        assert_eq!(required_ctx(200_000, 0), 223_232);
    }

    #[test]
    fn the_frontier_is_searched_at_the_context_the_gate_needs() {
        // A fresh pull's 32768 is raised to the gate's need.
        assert_eq!(frontier_ctx(32_768, 200_000, 262_144), 223_232);
        // A preset that already asks for more keeps it.
        assert_eq!(frontier_ctx(248_832, 200_000, 262_144), 248_832);
        // A model trained on less is searched at the preset's own.
        assert_eq!(frontier_ctx(32_768, 200_000, 32_768), 32_768);
    }

    /// The reference box's wall: 12288 MiB less the default 1024 of headroom.
    const WALL: u64 = 11264;

    fn point(engine: &str, label: &str, decode: f64, prefill: f64, ctx: u64) -> Point {
        Point {
            engine: engine.into(),
            label: label.into(),
            decode,
            decode_best: 0.0,
            prefill,
            ctx,
            ubatch: 0,
            vram_mib: 10000.0,
            passed_gate: None,
            depth_decode: None,
        }
    }

    fn at_ubatch(mut p: Point, ubatch: u32) -> Point {
        p.ubatch = ubatch;
        p
    }

    fn reading(mut p: Point, vram_mib: f64) -> Point {
        p.vram_mib = vram_mib;
        p
    }

    #[test]
    fn a_split_over_the_wall_is_not_ranked_however_fast() {
        // The b11879 frontier on the reference box, 2026-10-25: the splits
        // below the frontier decode faster because they hold more than the
        // card can spare, and the search had already called both too big.
        let floor = Floor::default();
        let points = vec![
            reading(point("llama.cpp", "n33", 61.8, 689.0, 236544), 11813.0),
            reading(point("llama.cpp", "n35", 56.6, 879.0, 236544), 10975.0),
            reading(point("llama.cpp", "n34", 60.3, 898.0, 236544), 11473.0),
        ];
        assert_eq!(rank(&points, &floor, WALL), vec![1]);
        assert_eq!(
            excluded(&points, 2, &floor, WALL),
            Some(Exclusion::OverWall { vram_mib: 11473.0, wall_mib: WALL })
        );
        assert_eq!(excluded(&points, 1, &floor, WALL), None);

        // Exactly on the wall is a fit: the comparison is strict.
        let on = reading(point("llama.cpp", "edge", 60.0, 800.0, 236544), WALL as f64);
        assert_eq!(exclusion(&on, 800.0, &floor, WALL), None);
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
        let ranked = rank(&points, &floor, WALL);
        assert!(ranked.contains(&1), "ub1024 is ranked, not excluded: {ranked:?}");

        // A collapse within one ubatch is still a spill.
        let mut spilled = points.clone();
        spilled.push(at_ubatch(point("llama.cpp", "n33-ub1024", 66.0, 380.0, 4096), 1024));
        assert!(!rank(&spilled, &floor, WALL).contains(&2));
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
        assert_eq!(rank(&points, &floor, WALL), vec![1, 0, 2]);
    }

    fn best(mut p: Point, decode_best: f64) -> Point {
        p.decode_best = decode_best;
        p
    }

    #[test]
    fn a_noisy_bench_widens_the_tie_to_its_own_spread() {
        // The reference box's third tune, 2026-10-29: medians 4% apart,
        // each bench 4-5% wide. A fixed 3% band ranked ub1024 first.
        let floor = Floor::default();
        let points = vec![
            best(at_ubatch(point("llama.cpp", "n35", 52.2, 706.0, 236544), 2048), 54.3),
            best(at_ubatch(point("llama.cpp", "ub1024", 54.3, 509.0, 236544), 1024), 57.0),
        ];
        assert_eq!(rank(&points, &floor, WALL), vec![0, 1]);
        assert!((tie_band(&points[1], &points[0]) - 0.0497).abs() < 0.001);

        // The same medians from quiet benches are a ranking.
        let quiet = vec![
            best(at_ubatch(point("llama.cpp", "n35", 52.2, 706.0, 236544), 2048), 52.5),
            best(at_ubatch(point("llama.cpp", "ub1024", 54.3, 509.0, 236544), 1024), 54.6),
        ];
        assert_eq!(rank(&quiet, &floor, WALL), vec![1, 0]);
    }

    #[test]
    fn the_spread_never_opens_the_tie_past_its_ceiling() {
        // The first tune's ub1024 bench, 47.2 median / 57.4 best: a 22%
        // spread, held to 8%, so the n35 16% above it stays ahead.
        let floor = Floor::default();
        let points = vec![
            best(at_ubatch(point("llama.cpp", "n35", 56.3, 722.0, 236544), 2048), 56.3),
            best(at_ubatch(point("llama.cpp", "ub1024", 47.2, 900.0, 236544), 1024), 57.4),
        ];
        assert_eq!(tie_band(&points[0], &points[1]), DECODE_TIE_MAX);
        assert_eq!(rank(&points, &floor, WALL), vec![0, 1]);
        // An unknown best is no spread: the floor of the band holds.
        assert_eq!(spread(&point("llama.cpp", "x", 50.0, 800.0, 4096)), 0.0);
    }

    #[test]
    fn a_point_tied_by_its_own_spread_joins_the_band_past_a_quieter_one() {
        // b is 4% under the top with a quiet bench (out of the band); c is
        // 6% under with a 7% spread (in it): the band is not a prefix.
        let floor = Floor::default();
        let points = vec![
            best(point("llama.cpp", "a", 60.0, 700.0, 4096), 60.3),
            best(point("llama.cpp", "b", 57.6, 900.0, 4096), 57.8),
            best(point("llama.cpp", "c", 56.4, 800.0, 4096), 60.3),
        ];
        assert_eq!(rank(&points, &floor, WALL), vec![2, 0, 1]);
    }

    #[test]
    fn the_search_stops_when_a_probe_would_only_measure_the_noise() {
        // The reference box's evening tune, 2026-10-29: the seed at 11115
        // MiB under an 11264 wall, then 262144 and 242688 over it. The next
        // prediction promised 2048 tokens - under what 128 MiB of noise is
        // worth at that slope - and the search spent two more loads on it.
        let mut search = ContextSearch::new(236544, 11115, 11264, 262144);
        assert_eq!(search.next(), Rung::Grow(262144));
        search.record(262144, 11730, false);
        assert_eq!(search.next(), Rung::Grow(242688), "6144 tokens is worth a load");
        search.record(242688, 11483, false);
        assert_eq!(search.next(), Rung::Stop);
    }

    #[test]
    fn the_default_floor_is_the_one_the_plan_agreed() {
        let f = Floor::default();
        assert_eq!((f.decode, f.prefill, f.depth_decode, f.agent), (25.0, 350.0, 15.0, 0.75));
    }

    #[test]
    fn a_floor_excludes_below_it_but_keeps_a_point_exactly_on_it() {
        let floor = Floor::default();
        // Exactly on both floors, and its engine's own best prefill: kept,
        // because every comparison is strict.
        let on = point("llama.cpp", "n0", 25.0, 350.0, 4096);
        assert_eq!(exclusion(&on, 350.0, &floor, WALL), None);

        // A hair under either floor: excluded, and named for which one.
        let low_decode = point("llama.cpp", "n1", 24.9, 800.0, 4096);
        assert_eq!(
            exclusion(&low_decode, 800.0, &floor, WALL),
            Some(Exclusion::BelowDecodeFloor { decode: 24.9 })
        );
        let low_prefill = point("llama.cpp", "n2", 40.0, 349.9, 4096);
        assert_eq!(
            exclusion(&low_prefill, 349.9, &floor, WALL),
            Some(Exclusion::BelowPrefillFloor { prefill: 349.9 })
        );

        // Spilling is reported first when a point is both spilling and slow.
        let both = point("llama.cpp", "n3", 10.0, 480.0, 4096);
        assert_eq!(
            exclusion(&both, 830.0, &floor, WALL),
            Some(Exclusion::Spilling { prefill: 480.0, engine_best: 830.0 })
        );

        // The spill line is strict too: three quarters of the engine's best
        // is kept, a hair under it is a spill.
        let at_line = point("llama.cpp", "n4", 40.0, 600.0, 4096);
        assert_eq!(exclusion(&at_line, 800.0, &floor, WALL), None);
        let under_line = point("llama.cpp", "n5", 40.0, 599.9, 4096);
        assert_eq!(
            exclusion(&under_line, 800.0, &floor, WALL),
            Some(Exclusion::Spilling { prefill: 599.9, engine_best: 800.0 })
        );
    }

    #[test]
    fn a_slow_engine_is_not_spilling_but_a_collapse_within_one_engine_is() {
        let floor = Floor::default();
        // Engines differ in prefill by design: ik_llama.cpp prefills ~480
        // where llama.cpp prefills ~830. Each on its own best is fine.
        let ik = point("ik_llama.cpp", "n0", 30.0, 480.0, 4096);
        assert_eq!(exclusion(&ik, 480.0, &floor, WALL), None);
        let llama_fast = point("llama.cpp", "n0", 60.0, 830.0, 4096);
        assert_eq!(exclusion(&llama_fast, 830.0, &floor, WALL), None);

        // Within one engine, 480 against 830 is the sysmem cliff.
        let llama_slow = point("llama.cpp", "n9", 70.0, 480.0, 4096);
        assert_eq!(
            exclusion(&llama_slow, 830.0, &floor, WALL),
            Some(Exclusion::Spilling { prefill: 480.0, engine_best: 830.0 })
        );

        // Through rank, the per-engine best decides: the collapsed llama.cpp
        // point is dropped, the honestly slow ik_llama.cpp point survives.
        let points = vec![
            point("llama.cpp", "fast", 60.0, 830.0, 4096),
            point("llama.cpp", "spill", 70.0, 480.0, 4096),
            point("ik_llama.cpp", "slow", 30.0, 480.0, 4096),
        ];
        assert_eq!(rank(&points, &floor, WALL), vec![0, 2]);
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
        assert_eq!(rank(&points, &floor, WALL), vec![1, 0, 2, 3]);
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
        assert_eq!(winner(&points, &floor, WALL), Some(2));
    }

    #[test]
    fn winner_keeps_a_gated_point_with_no_depth_measurement() {
        let floor = Floor::default();
        // Only a point whose depth was measured *and* shallow is skipped.
        let mut points = vec![point("llama.cpp", "x", 50.0, 800.0, 4096)];
        points[0].passed_gate = Some(true);
        assert_eq!(winner(&points, &floor, WALL), Some(0));

        // Nothing qualifies when every point fails the gate.
        let mut failed = vec![point("llama.cpp", "y", 50.0, 800.0, 4096)];
        failed[0].passed_gate = Some(false);
        assert_eq!(winner(&failed, &floor, WALL), None);
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
    fn a_frontier_search_from_the_seed_alone_is_six_probes_at_most() {
        // The worst case the tuner can hit: the whole range is open. Wherever
        // the true frontier sits, six probes (log2 of 41, rounded up) narrow
        // 0..=41 to one layer, which is what the sweep's nine candidates were
        // spending loads to do.
        for frontier in 1..=41u32 {
            let mut fits = 41u32;
            let mut spills = 0u32;
            let mut count = 0;
            while let Some(next) = between(fits, spills) {
                count += 1;
                if next >= frontier {
                    fits = next
                } else {
                    spills = next
                }
            }
            assert!(count <= 6, "{count} probes to find {frontier} in 0..=41");
            assert_eq!((fits, spills), (frontier, frontier - 1));
        }
    }

    // ---- the context prediction -----------------------------------------

    #[test]
    fn a_slope_measured_on_one_candidate_spares_the_next_its_doubling() {
        // Flint on the reference box, 2026-10-28, ubatch 2048, wall 11264:
        // n-cpu-moe 25 measured 32768 at 10883 and 65536 over the wall at
        // 11483, then fit 53248 at 11247.
        let wall = 11264;
        let mut first = ContextSearch::new(32768, 10883, wall, 131072);
        assert_eq!(first.next(), Rung::Grow(65536), "no slope yet: the doubling");
        first.record(65536, 11483, false);
        assert_eq!(first.next(), Rung::Grow(53248));
        first.record(53248, 11247, false);
        let slope = first.measured_slope().expect("three real points");
        assert!((slope - 600.0 / 32768.0).abs() < 1e-9, "the widest span: {slope}");

        // n-cpu-moe 27 started from 32768 at 10193 and spent 65536 (10796),
        // 90112 (11300, over) and 88064 (11260) on its line. With the slope
        // it predicts straight past the doubling (the reading at 91136 is
        // the measured line's through 90112).
        let mut second = ContextSearch::new(32768, 10193, wall, 131072).with_slope(Some(slope));
        assert_eq!(second.next(), Rung::Grow(91136));
        second.record(91136, 11320, false);
        assert_eq!(second.next(), Rung::Grow(88064), "two real points now: the measured line");
        second.record(88064, 11260, false);
        assert_eq!(second.next(), Rung::Stop);

        // The 35B, 2026-10-29: the seed 236544 at 11028 MiB (10956 once
        // loaded, so the bench adds 72), and 262144 over the wall read once
        // loaded at 11442. Taken as read, the line predicts 250880, over the
        // wall again; with the bench's 72 added it predicts what fits.
        let mut raw = ContextSearch::new(236544, 11028, 11264, 262144);
        raw.record(262144, 11442, false);
        assert_eq!(raw.next(), Rung::Grow(250880));
        let mut corrected = ContextSearch::new(236544, 11028, 11264, 262144);
        corrected.record(262144, 11442 + 72, false);
        assert_eq!(corrected.next(), Rung::Grow(248832));

        // A slope that is no slope is ignored.
        let none = ContextSearch::new(32768, 10193, wall, 131072).with_slope(Some(0.0));
        assert_eq!(none.next(), Rung::Grow(65536));
    }

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

    fn kept(key: &str, ts: &str, desktop: f64, vram: f64, wall_only: bool) -> Kept {
        Kept {
            key: key.into(),
            ts: ts.into(),
            desktop_mib: desktop,
            decode: if wall_only { 0.0 } else { 62.1 },
            decode_best: 0.0,
            prefill: if wall_only { 0.0 } else { 850.0 },
            vram_mib: vram,
            wall_only,
            after_load_mib: Some(vram - 70.0),
        }
    }

    #[test]
    fn a_point_key_is_the_candidate_whatever_the_flag_order() {
        let a = [("n-cpu-moe".to_string(), "35".to_string()), ("ctx-size".to_string(), "248832".to_string())];
        let b = [a[1].clone(), a[0].clone()];
        let key = point_key("m.gguf:21713463040", "llama.cpp", "b11379-cuda", &a, 3, 4096);
        assert_eq!(key, point_key("m.gguf:21713463040", "llama.cpp", "b11379-cuda", &b, 3, 4096));
        assert_ne!(key, point_key("m.gguf:21713463040", "llama.cpp", "b11379-vulkan", &a, 3, 4096));
        assert_ne!(key, point_key("m.gguf:21713463040", "llama.cpp", "b11379-cuda", &a, 5, 4096));
    }

    #[test]
    fn a_kept_point_stands_while_the_desktop_holds_still() {
        let points = vec![kept("k", "2026-10-29T10:00Z", 800.0, 11115.0, false), kept("k", "2026-10-30T10:00Z", 895.0, 11343.0, false)];
        // The newest, its reading moved by the desktop's 60 MiB.
        let same = reuse(&points, "k", 955.0, 11264).expect("within the noise");
        assert_eq!((same.vram_mib, same.ts.as_str(), same.after_load_mib), (11403.0, "2026-10-30T10:00Z", Some(11333.0)));
        assert!(reuse(&points, "k", 1100.0, 11264).is_none(), "the desktop moved 205 MiB: measured again");
        assert!(reuse(&points, "other", 895.0, 11264).is_none());
        // Over the wall stays over the wall while the moved reading is; the
        // reference box's n31 read 11743 MiB at an 800 MiB desktop and stood
        // at 466 (11409 MiB).
        let over = vec![kept("w", "2026-10-30T10:00Z", 600.0, 11400.0, true)];
        assert!(reuse(&over, "w", 900.0, 11264).is_some_and(|k| k.wall_only && k.vram_mib == 11700.0));
        assert!(reuse(&over, "w", 300.0, 11264).is_none(), "300 MiB less desktop may fit");
        let n31 = vec![kept("n31", "2026-11-03T00:50Z", 800.0, 11743.0, true)];
        assert!(reuse(&n31, "n31", 466.0, 11264).is_some_and(|k| k.vram_mib == 11409.0));
        let line = same.to_json();
        assert_eq!(Kept::parse(&line), Some(same));
        assert_eq!(Kept::parse("{}"), None);
    }

    /// The reference box's chat gate of 2026-11-03 (`n-cpu-moe 25` at 32768
    /// tokens), as `results.jsonl` holds it, with `ts`, the split, the
    /// context and the suite's scores to vary.
    fn gate_row(ts: &str, n_cpu_moe: u32, ctx: u64, cache: &str, solved: [u32; 2]) -> crate::results::Row {
        gate_row_of(ts, n_cpu_moe, ctx, cache, solved, crate::agent::TASKS.len())
    }

    fn gate_row_of(ts: &str, n_cpu_moe: u32, ctx: u64, cache: &str, solved: [u32; 2], total: usize) -> crate::results::Row {
        let line = format!(
            r#"{{"ts": "{ts}", "kind": "eval", "host": "rtx-3060_12g_32g_i7-12700kf", "gpu": "NVIDIA GeForce RTX 3060", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "backend": "cuda", "model": "ornith-1.5-35b", "model_file": "Ornith-1.5-35B-Q4_K_M.gguf", "ctx": {ctx}, "flags": {{"jinja": true, "flash-attn": "on", "n-gpu-layers": 99, "cache-type-k": "{cache}", "cache-type-v": "{cache}", "spec-type": "draft-mtp", "ctx-size": {ctx}, "load-mode": "mlock", "n-cpu-moe": {n_cpu_moe}, "batch-size": 2048, "ubatch-size": 2048, "temp": 0.6, "fit": "off"}}, "eval": {{"tools": "PASS", "needle": "PASS"}}, "agent": {{"anthropic": {{"solved": {}, "total": {total}}}, "openai": {{"solved": {}, "total": {total}}}}}}}"#,
            solved[0], solved[1]
        );
        crate::results::Row::parse(&line).expect("a row")
    }

    fn coder_setup(ctx: u64) -> GateSetup {
        let flags = [
            ("jinja", "true"),
            ("flash-attn", "on"),
            ("n-gpu-layers", "99"),
            ("cache-type-k", "q8_0"),
            ("cache-type-v", "q8_0"),
            ("spec-type", "draft-mtp"),
            ("ctx-size", "242688"),
            ("load-mode", "mmap+mlock"),
            ("threads", "16"),
            ("n-cpu-moe", "35"),
            ("batch-size", "2048"),
            ("ubatch-size", "2048"),
            ("temp", "0.6"),
            ("fit", "off"),
        ];
        GateSetup {
            host: "rtx-3060_12g_32g_i7-12700kf".into(),
            model_file: "Ornith-1.5-35B-Q4_K_M.gguf".into(),
            engine: "llama.cpp".into(),
            build: "b11379".into(),
            backend: "cuda".into(),
            flags: flags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ctx,
        }
    }

    #[test]
    fn a_gate_keeps_the_suite_of_the_same_setup_at_another_context() {
        let rows = vec![gate_row("2026-11-03T02:19Z", 35, 32768, "q8_0", [12, 12])];
        // Another context, thread count and load mode: the same answers.
        let kept = kept_agent(&rows, &coder_setup(242688), 0.75).expect("kept");
        assert_eq!(kept.ts, "2026-11-03T02:19Z");
        // Another split or prompt batch sums the numbers in another order,
        // and a greedy answer turns on it: MiMo-9B solved 12 of 12 at
        // ubatch-size 1024 and 11 at 2048, each the same on every run.
        let other_split = vec![gate_row("2026-11-03T02:19Z", 25, 32768, "q8_0", [12, 12])];
        assert!(kept_agent(&other_split, &coder_setup(242688), 0.75).is_none());
        let mut other_batch = coder_setup(242688);
        other_batch.flags.iter_mut().filter(|(k, _)| k == "ubatch-size").for_each(|(_, v)| *v = "1024".into());
        assert!(kept_agent(&rows, &other_batch, 0.75).is_none());
        // Less context than the suite had is not held to its result.
        assert!(kept_agent(&rows, &coder_setup(16384), 0.75).is_none());
        // Another build, backend, host or model file is another setup.
        let changes: [fn(&mut GateSetup); 4] = [
            |s| s.build = "b11936".into(),
            |s| s.backend = "vulkan".into(),
            |s| s.host = "other".into(),
            |s| s.model_file = "Ornith-1.5-35B-Q5_K_M.gguf".into(),
        ];
        for change in changes {
            let mut setup = coder_setup(242688);
            change(&mut setup);
            assert!(kept_agent(&rows, &setup, 0.75).is_none(), "{setup:?}");
        }
        // A cache type is another setup too.
        let other_cache = vec![gate_row("2026-11-03T02:19Z", 35, 32768, "q4_0", [12, 12])];
        assert!(kept_agent(&other_cache, &coder_setup(242688), 0.75).is_none());
        // A suite of fewer tasks than today's is another suite.
        let older = vec![gate_row_of("2026-11-03T02:19Z", 35, 32768, "q8_0", [11, 11], 11)];
        assert!(kept_agent(&older, &coder_setup(242688), 0.75).is_none());
    }

    #[test]
    fn a_kept_suite_is_the_newest_and_must_have_passed() {
        let rows = vec![
            gate_row("2026-11-03T01:44Z", 35, 32768, "q8_0", [12, 12]),
            gate_row("2026-11-03T02:19Z", 35, 32768, "q8_0", [12, 8]),
        ];
        // The newest scored 8 of 12 on one API, under 75%: run it again.
        assert!(kept_agent(&rows, &coder_setup(242688), 0.75).is_none());
        assert_eq!(kept_agent(&rows, &coder_setup(242688), 0.6).map(|r| r.ts.as_str()), Some("2026-11-03T02:19Z"));
        // A row whose suite ran on one API only does not stand for a gate's.
        let one = crate::results::Row::parse(
            r#"{"ts": "t", "kind": "eval", "model": "m", "host": "rtx-3060_12g_32g_i7-12700kf", "model_file": "Ornith-1.5-35B-Q4_K_M.gguf", "engine": "llama.cpp", "engine_build": "b11379", "backend": "cuda", "ctx": 1, "agent": {"anthropic": {"solved": 11, "total": 11}}}"#,
        )
        .expect("a row");
        let mut setup = coder_setup(242688);
        setup.flags.clear();
        assert!(kept_agent(&[one], &setup, 0.75).is_none());
    }
}
