//! Hugging Face, as `golem pull owner/repo[:quant]` reads it: the reference a
//! person types, the Hub's answer about a repository, the quant to fetch
//! for this machine, and the preset section that serves it.
//!
//! Pure: the binary fetches `https://huggingface.co/api/models/<repo>?blobs=true`
//! and the base model's `generation_config.json`, and hands the text here.
//! The Hub's answer carries every file with its size and, for a GGUF repo, a
//! `gguf` block with the architecture, the trained context and the
//! parameter count; it publishes no benchmark scores.

use crate::json::{self, Value};

/// What a person typed: a repository, and the quant when they named one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub repo: String,
    pub quant: Option<String>,
}

/// `owner/repo`, `owner/repo:Q4_K_M`, and the same with a `hf.co/`,
/// `huggingface.co/` or `https://` prefix - the forms a model page and other
/// tools print. Anything else is refused with what was expected.
pub fn parse_ref(text: &str) -> Result<Ref, String> {
    let mut rest = text.trim();
    for prefix in ["https://", "http://"] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    for prefix in ["huggingface.co/", "hf.co/"] {
        rest = rest.strip_prefix(prefix).unwrap_or(rest);
    }
    let (repo, quant) = match rest.split_once(':') {
        Some((repo, quant)) => (repo, Some(quant.trim())),
        None => (rest, None),
    };
    let repo = repo.trim_end_matches('/');
    let word = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    match repo.split_once('/') {
        Some((owner, name)) if word(owner) && word(name) => Ok(Ref {
            repo: repo.to_string(),
            quant: quant.filter(|q| !q.is_empty()).map(str::to_string),
        }),
        _ => Err(format!("expected owner/repo[:quant], got {text:?}")),
    }
}

/// One file in a repository, by its path there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub path: String,
    pub size: u64,
}

impl File {
    /// The name it is stored under in `data/models`: shards in a
    /// subdirectory of the repo land side by side, which is where the
    /// engine looks for the rest of a split model.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }
}

/// The Hub's answer about a repository.
#[derive(Debug, Clone, PartialEq)]
pub struct Repo {
    pub id: String,
    /// The commit the answer describes.
    pub sha: String,
    pub files: Vec<File>,
    pub arch: Option<String>,
    pub ctx_train: Option<u64>,
    pub params: Option<u64>,
    pub base_model: Option<String>,
    pub license: Option<String>,
}

/// Read `api/models/<repo>?blobs=true`. A gated or missing repository
/// answers with an `error` member, which is returned as the message.
pub fn parse_repo(text: &str) -> Result<Repo, String> {
    let doc = json::parse(text).map_err(|e| format!("the Hub's answer is not JSON: {e:?}"))?;
    if let Some(error) = doc.get("error").and_then(Value::as_str) {
        return Err(error.to_string());
    }
    let id = doc.get("id").and_then(Value::as_str).ok_or("the Hub's answer has no id")?;
    let number = |v: Option<&Value>| v.and_then(Value::as_f64).map(|n| n as u64);
    let files = match doc.get("siblings") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|s| {
                Some(File {
                    path: s.get("rfilename")?.as_str()?.to_string(),
                    size: number(s.get("size")).unwrap_or(0),
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    let gguf = doc.get("gguf");
    let card = doc.get("cardData");
    // `base_model` is a string or a list of them; the first is the one.
    let base_model = card.and_then(|c| c.get("base_model")).and_then(|b| match b {
        Value::String(s) => Some(s.clone()),
        Value::Array(items) => items.first().and_then(Value::as_str).map(str::to_string),
        _ => None,
    });
    Ok(Repo {
        id: id.to_string(),
        sha: doc.get("sha").and_then(Value::as_str).unwrap_or("").to_string(),
        files,
        arch: gguf.and_then(|g| g.get("architecture")).and_then(Value::as_str).map(str::to_string),
        ctx_train: number(gguf.and_then(|g| g.get("context_length"))),
        params: number(gguf.and_then(|g| g.get("total"))),
        base_model,
        license: card.and_then(|c| c.get("license")).and_then(Value::as_str).map(str::to_string),
    })
}

/// One quant of the model: its label and its files, shards in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Quant {
    pub label: String,
    pub files: Vec<File>,
}

impl Quant {
    pub fn size(&self) -> u64 {
        self.files.iter().map(|f| f.size).sum()
    }

    /// Full precision: never picked unless asked for by name.
    pub fn is_full(&self) -> bool {
        let label = self.label.to_ascii_uppercase();
        ["BF16", "F16", "F32", "FP16"].contains(&label.as_str())
    }
}

/// `name-00002-of-00003` -> (`name`, 2); anything else is not a shard.
fn shard(stem: &str) -> Option<(&str, u32)> {
    let (head, total) = stem.rsplit_once("-of-")?;
    let (base, index) = head.rsplit_once('-')?;
    let digits = |s: &str| s.len() == 5 && s.bytes().all(|b| b.is_ascii_digit());
    (digits(index) && digits(total)).then(|| (base, index.parse().unwrap_or(0)))
}

fn is_quant_token(token: &str) -> bool {
    let t = token.to_ascii_uppercase();
    let digit_after = |p: &str| t.strip_prefix(p).is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
    digit_after("Q") || digit_after("IQ") || digit_after("TQ")
        || ["BF16", "F16", "F32", "FP16"].contains(&t.as_str())
        || t.starts_with("MXFP4")
}

/// The quant a file name carries: `Qwen3.5-9B-UD-Q4_K_XL` -> `UD-Q4_K_XL`,
/// `Model.Q8_0` -> `Q8_0`. The last token that looks like a quant, with an
/// `UD` before it kept (Unsloth's dynamic quants are a different file).
pub fn quant_label(stem: &str) -> Option<String> {
    let tokens: Vec<&str> = stem.split(['-', '.']).collect();
    let at = tokens.iter().rposition(|t| is_quant_token(t))?;
    let from = if at > 0 && tokens[at - 1].eq_ignore_ascii_case("UD") { at - 1 } else { at };
    Some(tokens[from..].join("-"))
}

/// Projectors (vision) and speculative drafts ship beside the quants and
/// are not one.
fn is_companion(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("mmproj") || lower.starts_with("mtp-") || lower.contains("-draft")
}

/// The quants of the repository, smallest first, split files grouped.
pub fn quants(repo: &Repo) -> Vec<Quant> {
    let mut out: Vec<(String, Quant)> = Vec::new();
    for file in &repo.files {
        let name = file.name();
        let Some(stem) = name.strip_suffix(".gguf") else { continue };
        if is_companion(name) {
            continue;
        }
        let (base, index) = shard(stem).unwrap_or((stem, 0));
        let Some(label) = quant_label(base) else { continue };
        // Grouped by directory and base name, so two shards of one quant
        // meet and two quants never do.
        let dir = file.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        let key = format!("{dir}/{base}");
        let entry = match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, quant)) => quant,
            None => {
                out.push((key, Quant { label, files: Vec::new() }));
                &mut out.last_mut().expect("just pushed").1
            }
        };
        entry.files.push(file.clone());
        let _ = index;
    }
    let mut quants: Vec<Quant> = out.into_iter().map(|(_, q)| q).collect();
    for quant in &mut quants {
        quant.files.sort_by(|a, b| a.path.cmp(&b.path));
    }
    quants.sort_by_key(Quant::size);
    quants
}

/// The vision projector to fetch with the model: F16, else BF16, else the
/// smallest there is. `None` for a text-only repository.
pub fn projector(repo: &Repo) -> Option<&File> {
    let projectors: Vec<&File> =
        repo.files.iter().filter(|f| f.name().to_ascii_lowercase().starts_with("mmproj")).collect();
    for want in ["F16", "BF16"] {
        if let Some(f) = projectors.iter().find(|f| {
            f.name().strip_suffix(".gguf").and_then(quant_label).is_some_and(|l| l.eq_ignore_ascii_case(want))
        }) {
            return Some(f);
        }
    }
    projectors.into_iter().min_by_key(|f| f.size)
}

/// A mixture of experts, by the architecture the Hub reports (`qwen35moe`).
pub fn is_moe(repo: &Repo) -> bool {
    repo.arch.as_deref().is_some_and(|a| a.to_ascii_lowercase().contains("moe"))
}

/// How many bytes of weights this machine is offered, from its VRAM and RAM
/// in MiB.
///
/// A dense model should sit on the card whole: the card less 1 GiB for the
/// desktop, and a quarter of the rest for the context's cache. A mixture of
/// experts keeps its experts in RAM, so a third of the RAM is added - on 12
/// GB and 32 GB that offers 23.2 GB, which takes a 35B-A3B at Q4_K_M (21.7)
/// and refuses Q5_K_M (25.3), the line measured on that machine.
pub fn budget(moe: bool, vram_mib: u64, ram_mib: u64) -> u64 {
    let card = vram_mib.saturating_sub(1024);
    let mib = if moe { card + ram_mib / 3 } else { card * 3 / 4 };
    mib * 1024 * 1024
}

/// The quant to fetch: the one named, or else the largest quantised one
/// within `budget`. With nothing within it, the smallest - the bool says
/// whether the pick fits.
pub fn pick<'a>(quants: &'a [Quant], want: Option<&str>, budget: u64) -> Result<(&'a Quant, bool), String> {
    if quants.is_empty() {
        return Err("the repository has no GGUF model files".to_string());
    }
    if let Some(want) = want {
        let found = quants
            .iter()
            .find(|q| q.label.eq_ignore_ascii_case(want))
            .or_else(|| quants.iter().find(|q| q.label.trim_start_matches("UD-").eq_ignore_ascii_case(want)));
        return match found {
            Some(q) => Ok((q, q.size() <= budget)),
            None => Err(format!(
                "no {want} quant; there are: {}",
                quants.iter().map(|q| q.label.as_str()).collect::<Vec<_>>().join(", ")
            )),
        };
    }
    let quantised: Vec<&Quant> = quants.iter().filter(|q| !q.is_full()).collect();
    let pool = if quantised.is_empty() { quants.iter().collect() } else { quantised };
    match pool.iter().rev().find(|q| q.size() <= budget) {
        Some(q) => Ok((q, true)),
        None => Ok((pool[0], false)),
    }
}

/// The preset name for a repository: `unsloth/Qwen3.5-9B-GGUF` ->
/// `qwen3.5-9b`, `bartowski/Qwen_Qwen3.6-35B-A3B-GGUF` -> `qwen3.6-35b-a3b`.
pub fn preset_name(repo: &str) -> String {
    let name = repo.rsplit('/').next().unwrap_or(repo);
    let lower = name.to_ascii_lowercase();
    let name = ["-gguf", "_gguf", ".gguf"]
        .iter()
        .find_map(|s| lower.strip_suffix(s).map(|_| &name[..name.len() - s.len()]))
        .unwrap_or(name);
    // A mirror's `Org_Model` prefix, when the model's own name repeats it.
    let name = match name.split_once('_') {
        Some((org, model)) if model.to_ascii_lowercase().starts_with(&org.to_ascii_lowercase()) => model,
        _ => name,
    };
    name.to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect()
}

/// Sampling defaults from a model's `generation_config.json`, as preset
/// keys, in a fixed order. Only what the file sets.
pub fn sampling(text: &str) -> Vec<(&'static str, String)> {
    let Ok(doc) = json::parse(text) else { return Vec::new() };
    let mut out = Vec::new();
    for (key, flag) in [("temperature", "temp"), ("top_p", "top-p"), ("top_k", "top-k"), ("min_p", "min-p")] {
        if let Some(Value::Number { text, .. }) = doc.get(key) {
            out.push((flag, text.clone()));
        }
    }
    out
}

/// The context a freshly pulled model starts at: its trained context, capped
/// at 32768 until the tuner measures how far the card goes.
pub const START_CTX: u64 = 32768;

/// The preset section for a pulled model, with the `; Download:` lines
/// `up`, `pull` and the console fetch its files from.
pub fn section(name: &str, repo: &Repo, quant: &Quant, projector: Option<&File>, sampling: &[(&str, String)]) -> String {
    let mut out = String::new();
    out.push_str(&format!("; Pulled from {} ({}).\n", repo.id, quant.label));
    for file in quant.files.iter().chain(projector) {
        out.push_str(&format!("; Download: {} -> {}\n", repo.id, file.path));
    }
    out.push_str(&format!("[{name}]\n"));
    out.push_str(&format!("model = {}\n", quant.files[0].name()));
    if let Some(p) = projector {
        out.push_str(&format!("mmproj = {}\n", p.name()));
    }
    let ctx = repo.ctx_train.map_or(START_CTX, |c| c.min(START_CTX));
    out.push_str(&format!("ctx-size = {ctx}\n"));
    for (key, value) in sampling {
        out.push_str(&format!("{key} = {value}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape of the Hub's answer, trimmed from unsloth/Qwen3.5-9B-GGUF
    /// (2026-10-25) and given the BF16 shards unsloth's 35B repo has.
    const NINE_B: &str = r#"{
      "id": "unsloth/Qwen3.5-9B-GGUF", "sha": "3885219b68aa",
      "cardData": {"base_model": ["Qwen/Qwen3.5-9B"], "license": "apache-2.0"},
      "gguf": {"total": 8953803264, "architecture": "qwen35", "context_length": 262144},
      "siblings": [
        {"rfilename": ".gitattributes", "size": 3193},
        {"rfilename": "BF16/Qwen3.5-9B-BF16-00001-of-00002.gguf", "size": 9000000000},
        {"rfilename": "BF16/Qwen3.5-9B-BF16-00002-of-00002.gguf", "size": 8920697312},
        {"rfilename": "Qwen3.5-9B-Q4_K_M.gguf", "size": 5680522464},
        {"rfilename": "Qwen3.5-9B-Q6_K.gguf", "size": 7458301152},
        {"rfilename": "Qwen3.5-9B-Q8_0.gguf", "size": 9527502048},
        {"rfilename": "Qwen3.5-9B-UD-Q4_K_XL.gguf", "size": 5966095584},
        {"rfilename": "Qwen3.5-9B-UD-Q6_K_XL.gguf", "size": 8756929760},
        {"rfilename": "mmproj-BF16.gguf", "size": 921705024},
        {"rfilename": "mmproj-F16.gguf", "size": 918166080},
        {"rfilename": "mmproj-F32.gguf", "size": 1824062016}
      ]
    }"#;

    const MOE: &str = r#"{
      "id": "ornith-ai/Ornith-1.5-35B-A3B-GGUF", "sha": "12393612fd",
      "cardData": {"license": "mit"},
      "gguf": {"total": 35505251456, "architecture": "qwen35moe", "context_length": 262144},
      "siblings": [
        {"rfilename": "Ornith-1.5-35B-BF16.gguf", "size": 71066994400},
        {"rfilename": "Ornith-1.5-35B-Q4_K_M.gguf", "size": 21713463040},
        {"rfilename": "Ornith-1.5-35B-Q5_K_M.gguf", "size": 25347532544},
        {"rfilename": "Ornith-1.5-35B-Q6_K.gguf", "size": 29208731392},
        {"rfilename": "Ornith-1.5-35B-Q8_0.gguf", "size": 37802149280},
        {"rfilename": "mmproj-Ornith-1.5-35B-BF16.gguf", "size": 902822240}
      ]
    }"#;

    #[test]
    fn a_reference_is_a_repo_and_maybe_a_quant() {
        let r = |s| parse_ref(s).expect(s);
        assert_eq!(r("unsloth/Qwen3.5-9B-GGUF"), Ref { repo: "unsloth/Qwen3.5-9B-GGUF".into(), quant: None });
        assert_eq!(r("unsloth/Qwen3.5-9B-GGUF:Q6_K").quant.as_deref(), Some("Q6_K"));
        assert_eq!(r("hf.co/unsloth/Qwen3.5-9B-GGUF:Q6_K").repo, "unsloth/Qwen3.5-9B-GGUF");
        assert_eq!(r("https://huggingface.co/unsloth/Qwen3.5-9B-GGUF/").repo, "unsloth/Qwen3.5-9B-GGUF");
        for bad in ["qwen3.5-9b", "a/b/c", "/repo", "owner/", "own er/repo"] {
            assert!(parse_ref(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_answer_gives_the_files_and_the_model_facts() {
        let repo = parse_repo(NINE_B).expect("parse");
        assert_eq!(repo.id, "unsloth/Qwen3.5-9B-GGUF");
        assert_eq!(repo.arch.as_deref(), Some("qwen35"));
        assert_eq!((repo.ctx_train, repo.params), (Some(262144), Some(8953803264)));
        assert_eq!(repo.base_model.as_deref(), Some("Qwen/Qwen3.5-9B"));
        assert_eq!(repo.license.as_deref(), Some("apache-2.0"));
        assert_eq!(repo.files.len(), 11);
        assert!(!is_moe(&repo));
        assert!(is_moe(&parse_repo(MOE).expect("parse")));
        assert_eq!(parse_repo(r#"{"error": "Repository not found"}"#).unwrap_err(), "Repository not found");
    }

    #[test]
    fn quants_are_labelled_grouped_and_exclude_companions() {
        let quants = quants(&parse_repo(NINE_B).expect("parse"));
        let labels: Vec<&str> = quants.iter().map(|q| q.label.as_str()).collect();
        assert_eq!(labels, ["Q4_K_M", "UD-Q4_K_XL", "Q6_K", "UD-Q6_K_XL", "Q8_0", "BF16"]);
        let bf16 = quants.last().expect("bf16");
        assert_eq!(bf16.files.len(), 2, "two shards, one quant");
        assert_eq!(bf16.files[0].name(), "Qwen3.5-9B-BF16-00001-of-00002.gguf");
        assert!(bf16.is_full());
        assert_eq!(quant_label("Model.Q8_0").as_deref(), Some("Q8_0"));
        assert_eq!(quant_label("Qwen_Qwen3.6-35B-A3B-Q4_0").as_deref(), Some("Q4_0"));
        assert_eq!(quant_label("readme"), None);
    }

    #[test]
    fn the_projector_prefers_f16() {
        let repo = parse_repo(NINE_B).expect("parse");
        assert_eq!(projector(&repo).map(File::name), Some("mmproj-F16.gguf"));
        let moe = parse_repo(MOE).expect("parse");
        assert_eq!(projector(&moe).map(File::name), Some("mmproj-Ornith-1.5-35B-BF16.gguf"));
    }

    #[test]
    fn the_pick_follows_the_machine_and_a_named_quant_wins() {
        let (vram, ram) = (12288, 32768);
        let moe = parse_repo(MOE).expect("parse");
        let q = quants(&moe);
        // The measured line: Q4_K_M on 12 GB + 32 GB, not Q5_K_M.
        assert_eq!(pick(&q, None, budget(true, vram, ram)).expect("pick").0.label, "Q4_K_M");
        let dense = quants(&parse_repo(NINE_B).expect("parse"));
        // 8.25 GiB for a dense model's weights on a 12 GB card.
        assert_eq!(pick(&dense, None, budget(false, vram, ram)).expect("pick").0.label, "UD-Q6_K_XL");
        // Named, with or without Unsloth's prefix, in any case.
        assert_eq!(pick(&dense, Some("q8_0"), 0).expect("pick").0.label, "Q8_0");
        assert_eq!(pick(&dense, Some("Q4_K_XL"), u64::MAX).expect("pick").0.label, "UD-Q4_K_XL");
        assert!(pick(&dense, Some("Q2_K"), u64::MAX).unwrap_err().contains("UD-Q4_K_XL"));
        // Nothing fits: the smallest quantised one, flagged.
        let (smallest, fits) = pick(&dense, None, 1).expect("pick");
        assert_eq!((smallest.label.as_str(), fits), ("Q4_K_M", false));
    }

    #[test]
    fn a_preset_name_comes_from_the_repo() {
        assert_eq!(preset_name("unsloth/Qwen3.5-9B-GGUF"), "qwen3.5-9b");
        assert_eq!(preset_name("bartowski/Qwen_Qwen3.6-35B-A3B-GGUF"), "qwen3.6-35b-a3b");
        assert_eq!(preset_name("ornith-ai/Ornith-1.5-35B-A3B-GGUF"), "ornith-1.5-35b-a3b");
        assert_eq!(preset_name("someone/Plain Model"), "plain-model");
    }

    #[test]
    fn sampling_comes_from_the_generation_config_as_written() {
        let config = r#"{"do_sample": true, "temperature": 0.7, "top_p": 0.8, "top_k": 20, "repetition_penalty": 1.1}"#;
        assert_eq!(
            sampling(config),
            vec![("temp", "0.7".to_string()), ("top-p", "0.8".to_string()), ("top-k", "20".to_string())]
        );
        assert!(sampling("not json").is_empty());
    }

    #[test]
    fn the_section_serves_the_quant_and_says_where_it_came_from() {
        let repo = parse_repo(NINE_B).expect("parse");
        let all = quants(&repo);
        let q = all.iter().find(|q| q.label == "UD-Q4_K_XL").expect("quant");
        let text = section("qwen3.5-9b", &repo, q, projector(&repo), &[("temp", "0.7".to_string())]);
        assert_eq!(
            text,
            "; Pulled from unsloth/Qwen3.5-9B-GGUF (UD-Q4_K_XL).\n\
             ; Download: unsloth/Qwen3.5-9B-GGUF -> Qwen3.5-9B-UD-Q4_K_XL.gguf\n\
             ; Download: unsloth/Qwen3.5-9B-GGUF -> mmproj-F16.gguf\n\
             [qwen3.5-9b]\n\
             model = Qwen3.5-9B-UD-Q4_K_XL.gguf\n\
             mmproj = mmproj-F16.gguf\n\
             ctx-size = 32768\n\
             temp = 0.7\n"
        );
        // The section round-trips through the preset reader: the downloads
        // belong to it, and the model is its own.
        let parsed = crate::ini::parse(&text);
        let s = parsed.get("qwen3.5-9b").expect("section");
        assert_eq!(s.downloads.len(), 2);
        assert_eq!(parsed.value("qwen3.5-9b", "model"), Some("Qwen3.5-9B-UD-Q4_K_XL.gguf"));
    }
}
