//! Whether a model fits a card, from its GGUF header alone: what the KV
//! cache costs per token, what the weights leave on the card for a given
//! expert split, and so how much context the rest holds.
//!
//! The rule (docs/roadmap.md, "Discovery"): the engine sizes the load. Its
//! `llama-fit-params` reads the header, builds the context and reserves its
//! graphs without allocating anything, and prints what the card would hold
//! ([`breakdown`]). Golem adds what that leaves out - the MTP draft context
//! a drafting load keeps beside the model ([`Drafting`]) and the share of a
//! load outside the breakdown ([`ENGINE_OVERHEAD_MIB`]) - and the predicted
//! context is the largest multiple of [`CTX_STEP`] tokens that keeps the
//! whole card under the tuner's wall with the desktop's share where it is,
//! capped at the trained context ([`engine_context`]). A prediction is
//! marked as one until a load confirms it.

use crate::gguf::ModelFacts;

const MIB: f64 = 1_048_576.0;

/// What a load holds on the card beyond its memory breakdown, in MiB - the
/// CUDA context among it: the 35B's drafting load on the reference box read
/// 10745 MiB on the card for a breakdown of 10631.
pub const ENGINE_OVERHEAD_MIB: u64 = 114;

/// What drafting adds to the compute buffers, in MiB: the 35B's draft
/// context reserved 1132 MiB and its target's own buffer came out 126 MiB
/// smaller than without drafting (1866 against 1992), on b11379 at
/// `ubatch-size 2048`. The only drafting load measured; every model that
/// drafts takes it until another is.
pub const DRAFT_COMPUTE_MIB: u64 = 1006;

/// The recurrent state a drafting load keeps, against one that does not:
/// the 35B's held 4 copies (251 MiB against 63, `3 rs_seq`).
const DRAFT_RECURRENT_COPIES: f64 = 4.0;

/// One device's row of `llama-fit-params`' memory breakdown, in MiB: the
/// weights on the card, the context (KV cache and recurrent state) and the
/// compute buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breakdown {
    pub model: u64,
    pub context: u64,
    pub compute: u64,
}

/// The first device's row of the breakdown `llama-fit-params -v` prints:
/// `| - CUDA0 (RTX 3060) | 12287 = 11189 + ( 8429 = 3792 + 2644 + 1992) + -7331 |`
/// - total, free, then self = model + context + compute. The host's row is
/// not a device's. `None` when the text has no such row.
pub fn breakdown(log: &str) -> Option<Breakdown> {
    log.lines()
        .filter(|line| line.contains("common_memory_breakdown_print:"))
        .filter_map(|line| {
            let fields: Vec<&str> = line.split('|').collect();
            let device = fields.get(1)?.trim();
            if !device.starts_with("- ") || device.starts_with("- Host") {
                return None;
            }
            let numbers: Vec<u64> = fields
                .get(2)?
                .split(|c: char| !c.is_ascii_digit() && c != '-')
                .filter_map(|n| n.parse::<i64>().ok())
                .map(|n| n.max(0) as u64)
                .collect();
            // total, free, self, model, context, compute, unaccounted
            (numbers.len() >= 6).then(|| Breakdown { model: numbers[3], context: numbers[4], compute: numbers[5] })
        })
        .next()
}

/// The fitted context `llama-fit-params` prints on stdout (`-c 262144 -ngl
/// 99 ...`), or `None`.
pub fn fitted_context(stdout: &str) -> Option<u64> {
    stdout.split_whitespace().skip_while(|w| *w != "-c").nth(1)?.parse().ok()
}

/// What a drafting load keeps beside the model that b11379's
/// `llama-fit-params`, which has no MTP option, leaves out.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Drafting {
    /// The MTP block's weights, from the header (bytes).
    pub weights: u64,
    /// The draft context's KV cache per token: its layers in f16, whatever
    /// the target's cache type (bytes).
    pub per_token: f64,
}

impl Drafting {
    /// The MTP head's terms, from the header; `None` for a file without one.
    pub fn of(facts: &ModelFacts) -> Option<Drafting> {
        if facts.nextn_layers == 0 {
            return None;
        }
        let first = facts.n_layer.saturating_sub(facts.nextn_layers);
        let weights = facts
            .tensor_bytes
            .iter()
            .filter(|(name, _)| block_of(name).is_some_and(|b| b >= first))
            .map(|(_, bytes)| bytes)
            .sum();
        let per_token =
            facts.nextn_layers as f64 * facts.n_head_kv as f64 * (facts.key_length + facts.value_length) as f64 * 2.0;
        Some(Drafting { weights, per_token })
    }
}

/// The context the card holds with the engine's own sizing: `breakdown` is
/// fit-params' at `probe_ctx` tokens on the split to serve, `room_mib` the
/// card less the desktop's share less the tuner's headroom. The context's
/// part that does not grow (the recurrent state) is the breakdown's context
/// less the cache it reports for `probe_ctx`; drafting adds its weights, its
/// cache, the recurrent state's extra copies and [`DRAFT_COMPUTE_MIB`].
/// `None` for a cache type with no known size.
pub fn engine_context(
    breakdown: &Breakdown,
    probe_ctx: u64,
    facts: &ModelFacts,
    cache_k: &str,
    cache_v: &str,
    drafting: Option<&Drafting>,
    room_mib: u64,
) -> Option<u64> {
    let kv = cache_bytes_per_token(facts, cache_k, cache_v, false)?;
    let recurrent = (breakdown.context as f64 * MIB - kv * probe_ctx as f64).max(0.0);
    let mut fixed = (breakdown.model + breakdown.compute + ENGINE_OVERHEAD_MIB) as f64 * MIB + recurrent;
    let mut per_token = kv;
    if let Some(draft) = drafting {
        fixed += draft.weights as f64 + recurrent * (DRAFT_RECURRENT_COPIES - 1.0) + DRAFT_COMPUTE_MIB as f64 * MIB;
        per_token += draft.per_token;
    }
    Some(context(room_mib * (1 << 20), fixed as u64, 0, per_token, facts.n_ctx_train))
}

/// Contexts are predicted in steps of this many tokens, the tuner's own.
pub const CTX_STEP: u64 = 256;

/// Bytes per element of a KV cache type, as llama.cpp stores it: the
/// quantized ones in blocks of 32 values with their scale.
pub fn cache_bytes_per_element(cache_type: &str) -> Option<f64> {
    Some(match cache_type {
        "f32" => 4.0,
        "f16" | "bf16" => 2.0,
        "q8_0" => 34.0 / 32.0,
        "q5_1" => 24.0 / 32.0,
        "q5_0" => 22.0 / 32.0,
        "q4_1" => 20.0 / 32.0,
        "q4_0" | "iq4_nl" => 18.0 / 32.0,
        _ => return None,
    })
}

/// What one token of context costs in the KV cache, in bytes: the layers
/// that keep a cache (and the MTP head's, when the engine drafts with it) x
/// the KV heads x each head's key and value. `None` for a cache type with no
/// known size.
pub fn cache_bytes_per_token(facts: &ModelFacts, cache_k: &str, cache_v: &str, drafting: bool) -> Option<f64> {
    let k = cache_bytes_per_element(cache_k)?;
    let v = cache_bytes_per_element(cache_v)?;
    let layers = facts.n_layer_kv + if drafting { facts.nextn_layers } else { 0 };
    Some(layers as f64 * facts.n_head_kv as f64 * (facts.key_length as f64 * k + facts.value_length as f64 * v))
}

/// The bytes of weights a load keeps on the card with every layer offloaded
/// and the routed experts of the first `n_cpu_moe` layers in RAM - llama.cpp's
/// `--n-cpu-moe`. The token embedding is never on the card: llama.cpp keeps
/// the input layer on the CPU.
pub fn card_weight_bytes(facts: &ModelFacts, n_cpu_moe: u64) -> u64 {
    facts
        .tensor_bytes
        .iter()
        .filter(|(name, _)| !name.starts_with("token_embd."))
        .filter(|(name, _)| !(name.contains("_exps.") && block_of(name).is_some_and(|b| b < n_cpu_moe)))
        .map(|(_, bytes)| bytes)
        .sum()
}

/// The context the card holds: the largest multiple of [`CTX_STEP`] whose
/// cache fits in `wall` less `weights` and `compute` (bytes all), capped at
/// `ctx_train`. 0 when the weights alone do not fit.
pub fn context(wall: u64, weights: u64, compute: u64, per_token: f64, ctx_train: u64) -> u64 {
    let left = wall.saturating_sub(weights).saturating_sub(compute);
    if left == 0 || per_token <= 0.0 {
        return 0;
    }
    let tokens = (left as f64 / per_token) as u64 / CTX_STEP * CTX_STEP;
    if ctx_train > 0 {
        tokens.min(ctx_train)
    } else {
        tokens
    }
}

/// How a predicted context reads for a person: the class it falls in for an
/// agent's 200K window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 200K tokens or more.
    Fits200K,
    /// At least 64K, under 200K.
    Fits64K,
    /// Some context, under 64K.
    Small,
    /// The weights alone do not fit.
    DoesNotFit,
}

/// The verdict for a predicted context.
pub fn verdict(ctx: u64) -> Verdict {
    match ctx {
        c if c >= 200_000 => Verdict::Fits200K,
        c if c >= 65_536 => Verdict::Fits64K,
        0 => Verdict::DoesNotFit,
        _ => Verdict::Small,
    }
}

/// `blk.<N>.…` -> N.
fn block_of(name: &str) -> Option<u64> {
    name.strip_prefix("blk.")?.split('.').next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 35B's cache geometry, as its header gives it.
    fn ornith_35b() -> ModelFacts {
        ModelFacts { n_head: 16, n_head_kv: 2, key_length: 256, value_length: 256, n_layer_kv: 10, nextn_layers: 1, ..ModelFacts::default() }
    }

    #[test]
    fn the_cache_costs_what_its_layers_and_heads_say() {
        // 10 layers x 2 heads x 512 values x 34/32 bytes = 10880 bytes.
        assert_eq!(cache_bytes_per_token(&ornith_35b(), "q8_0", "q8_0", false), Some(10_880.0));
        // Drafting with the MTP head adds its layer.
        assert_eq!(cache_bytes_per_token(&ornith_35b(), "q8_0", "q8_0", true), Some(11_968.0));
        assert_eq!(cache_bytes_per_token(&ornith_35b(), "f16", "f16", false), Some(20_480.0));
        assert_eq!(cache_bytes_per_token(&ornith_35b(), "q8_0", "turbo", false), None);
    }

    #[test]
    fn the_split_keeps_the_first_layers_experts_and_the_embedding_off_the_card() {
        let facts = ModelFacts {
            tensor_bytes: vec![
                ("token_embd.weight".into(), 1000),
                ("blk.0.ffn_up_exps.weight".into(), 100),
                ("blk.0.attn_q.weight".into(), 10),
                ("blk.1.ffn_up_exps.weight".into(), 100),
                ("blk.1.ffn_up_shexp.weight".into(), 5),
                ("output.weight".into(), 50),
            ],
            ..ModelFacts::default()
        };
        assert_eq!(card_weight_bytes(&facts, 0), 265);
        assert_eq!(card_weight_bytes(&facts, 1), 165, "block 0's experts in RAM");
        assert_eq!(card_weight_bytes(&facts, 99), 65, "a shared expert stays on the card");
    }

    #[test]
    fn the_context_is_what_is_left_over_the_cost_per_token_in_steps() {
        let mib = 1 << 20;
        // 11264 MiB wall, 8000 MiB weights, 500 MiB compute: 2764 MiB left,
        // at 10880 bytes a token 266384 tokens, down to 266240.
        assert_eq!(context(11_264 * mib, 8_000 * mib, 500 * mib, 10_880.0, 0), 266_240);
        assert_eq!(context(11_264 * mib, 8_000 * mib, 500 * mib, 10_880.0, 262_144), 262_144, "capped at the trained context");
        assert_eq!(context(8_000 * mib, 9_000 * mib, 500 * mib, 10_880.0, 0), 0, "the weights do not fit");
        assert_eq!(verdict(262_144), Verdict::Fits200K);
        assert_eq!(verdict(131_072), Verdict::Fits64K);
        assert_eq!(verdict(32_768), Verdict::Small);
        assert_eq!(verdict(0), Verdict::DoesNotFit);
    }

    /// b11379's `llama-fit-params -v` on the 35B at the served split
    /// (`n-cpu-moe 35`, 248832 tokens, q8_0, `ubatch-size 2048`), 2026-11-03.
    const FIT_35B: &str = "\
0.00.696.209 I common_memory_breakdown_print: | memory breakdown [MiB] | total    free     self   model   context   compute    unaccounted |
0.00.696.209 I common_memory_breakdown_print: |   - CUDA0 (RTX 3060)   | 12287 = 11189 + ( 8429 =  3792 +    2644 +    1992) +       -7331 |
0.00.696.210 I common_memory_breakdown_print: |   - Host               |                  17387 = 16382 +       0 +    1004                |
";

    #[test]
    fn the_breakdown_is_the_first_devices_row() {
        assert_eq!(breakdown(FIT_35B), Some(Breakdown { model: 3792, context: 2644, compute: 1992 }));
        let small = "I common_memory_breakdown_print: |   - CUDA0 (RTX 3060)   | 12287 = 11253 + (1771 =   373 +     204 +    1194) +        -737 |";
        assert_eq!(breakdown(small), Some(Breakdown { model: 373, context: 204, compute: 1194 }));
        assert_eq!(breakdown("no breakdown here"), None);
        assert_eq!(fitted_context("-c 262144 -ngl 99 -ot \"blk\\.0\\.ffn=CPU\"\n"), Some(262_144));
        assert_eq!(fitted_context("nothing fitted"), None);
    }

    #[test]
    fn the_35b_predicts_its_tuned_context_with_the_desktop_it_had() {
        let mut facts = ornith_35b();
        facts.n_layer = 41;
        facts.n_ctx_train = 262_144;
        facts.tensor_bytes = vec![("blk.39.attn_q.weight".into(), 1 << 20), ("blk.40.nextn.eh_proj.weight".into(), 522 << 20)];
        let draft = Drafting::of(&facts).expect("an MTP head");
        assert_eq!(draft.weights, 522 << 20, "only the MTP block");
        // One layer x 2 heads x 512 values in f16: the draft's 486 MiB at 248832.
        assert_eq!(draft.per_token, 2048.0);
        let b = breakdown(FIT_35B).unwrap();
        // The tune read 11249 MiB on the card at 248832 tokens, where this
        // load holds 10745: a 504 MiB desktop, so the room is 12288 - 1024 - 504.
        let at_tune = engine_context(&b, 248_832, &facts, "q8_0", "q8_0", Some(&draft), 12_288 - 1_024 - 504).unwrap();
        assert_eq!(at_tune, 250_112);
        assert!((at_tune as f64 / 248_832.0 - 1.0).abs() < 0.05, "within 5% of the tuned context");
        // The desktop's share moves it: 772 MiB, as on 2026-11-03.
        assert_eq!(engine_context(&b, 248_832, &facts, "q8_0", "q8_0", Some(&draft), 12_288 - 1_024 - 772), Some(228_352));
        // Without drafting the trained context fits.
        assert_eq!(engine_context(&b, 248_832, &facts, "q8_0", "q8_0", None, 12_288 - 1_024 - 504), Some(262_144));
        assert_eq!(engine_context(&b, 248_832, &facts, "q8_0", "turbo", None, 10_000), None);
        assert!(Drafting::of(&ModelFacts::default()).is_none());
    }
}
