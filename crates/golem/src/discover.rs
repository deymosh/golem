//! Whether a model on the Hub fits this card, before a byte of its weights
//! is fetched: the decision is `golem_core::fit`'s, and this module fetches
//! the header and asks the engine.
//!
//! One range request brings the file's first bytes - the metadata and the
//! tensor descriptions, 11 MB of the 35B's 21.7 GB - into `.local/fit/`,
//! where they are kept as a sparse file of the model's real size, so the
//! pinned build's `llama-fit-params` reads it as the model itself and sizes
//! the load without allocating anything on the card. On Windows the hole
//! stays a hole only once `fsutil sparse` marks it: a plain extend writes
//! all of it.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use golem_core::fit::{self, Breakdown, Drafting};
use golem_core::gguf::{self, GgufError, ModelFacts};
use golem_core::hub;

use crate::config::{self, Overrides};
use crate::{engine, launch, machine, paths, proc};

/// The first range request: enough for a model of the 35B's class, whose
/// header ends 10.9 MB in. A longer header is fetched again at 4x, up to
/// [`HEADER_MAX`].
const HEADER_FIRST: u64 = 16 << 20;
const HEADER_MAX: u64 = 256 << 20;

/// How long one `llama-fit-params` run may take: it reads the header and
/// reserves the graphs, 0.6 s for the 35B.
const FIT_LIMIT: Duration = Duration::from_secs(60);

/// How the load would be served, where a preset says; [`Serving::default`]
/// is a new model's: q8_0 cache, `ubatch-size 2048` (the tuner's), drafting
/// when the file has an MTP head (`pull` turns it on), and the split found
/// from the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Serving {
    pub cache_k: String,
    pub cache_v: String,
    pub ubatch: String,
    pub drafting: Option<bool>,
    pub n_cpu_moe: Option<u64>,
}

impl Default for Serving {
    fn default() -> Serving {
        Serving { cache_k: "q8_0".into(), cache_v: "q8_0".into(), ubatch: "2048".into(), drafting: None, n_cpu_moe: None }
    }
}

impl Serving {
    /// What the preset section `name` serves: the tuned split and
    /// `ubatch-size` (the engine's 512 when it sets none), the cache types,
    /// and drafting exactly when it sets `spec-type = draft-mtp`.
    pub fn of_preset(doc: &golem_core::ini::Document, name: &str) -> Serving {
        let value = |key: &str| doc.effective(name, key).map(str::to_string);
        let default = Serving::default();
        Serving {
            cache_k: value("cache-type-k").unwrap_or(default.cache_k),
            cache_v: value("cache-type-v").unwrap_or(default.cache_v),
            ubatch: value("ubatch-size").unwrap_or_else(|| "512".into()),
            drafting: Some(value("spec-type").as_deref() == Some("draft-mtp")),
            n_cpu_moe: value("n-cpu-moe").and_then(|v| v.parse().ok()),
        }
    }
}

/// The desktop's share of the card assumed while a model is loaded, when
/// the card cannot be read without it (MiB).
const DESKTOP_ASSUMED_MIB: u64 = 768;

/// A predicted fit, for `pull` and the console to say.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prediction {
    /// The context the card holds; 0 when the weights alone do not fit.
    pub ctx: u64,
    /// The trained context, which caps it.
    pub ctx_train: u64,
    /// The expert layers left in RAM (`n-cpu-moe`); 0 for a dense model.
    pub n_cpu_moe: u64,
    /// Whether the load drafts with the file's MTP head.
    pub drafting: bool,
    /// The desktop's share of the card the prediction leaves room for, and
    /// whether it was read (the engine down) or assumed.
    pub desktop_mib: u64,
    pub desktop_read: bool,
}

impl Prediction {
    /// One line: `fits 228K tokens (predicted: n-cpu-moe 35, drafting, desktop 0.8 GB read)`.
    pub fn line(&self) -> String {
        let verdict = match fit::verdict(self.ctx) {
            fit::Verdict::DoesNotFit => "does not fit this card".to_string(),
            _ if self.ctx >= self.ctx_train && self.ctx_train > 0 => {
                format!("fits its trained {}K tokens", self.ctx_train / 1024)
            }
            _ => format!("fits {}K tokens", self.ctx / 1024),
        };
        let mut how = Vec::new();
        if self.n_cpu_moe > 0 {
            how.push(format!("n-cpu-moe {}", self.n_cpu_moe));
        }
        if self.drafting {
            how.push("drafting".to_string());
        }
        how.push(format!(
            "desktop {:.1} GB {}",
            self.desktop_mib as f64 / 1024.0,
            if self.desktop_read { "read" } else { "assumed" }
        ));
        format!("{verdict} (predicted from the header: {})", how.join(", "))
    }

    pub fn to_json(&self) -> String {
        format!(
            "{{\"ctx\": {}, \"ctx_train\": {}, \"n_cpu_moe\": {}, \"drafting\": {}, \"desktop_mib\": {}, \
             \"desktop_read\": {}, \"line\": {}}}",
            self.ctx,
            self.ctx_train,
            self.n_cpu_moe,
            self.drafting,
            self.desktop_mib,
            self.desktop_read,
            golem_core::json::quote(&self.line())
        )
    }
}

/// One prediction at a time: the engine's runs share a log file.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// [`predict`] for the model of the served preset's section `name`, from
/// its `; Download:` lines - the console's question about a preset that is
/// not downloaded yet.
pub fn predict_preset(home: &Path, name: &str) -> Result<Prediction, String> {
    let preset = config::models_preset(home, &Overrides::default());
    let text = std::fs::read_to_string(home.join(&preset)).map_err(|e| format!("cannot read {preset}: {e}"))?;
    let doc = golem_core::ini::parse(&text);
    let section = doc.sections.iter().find(|s| s.name == name).ok_or_else(|| format!("no preset [{name}]"))?;
    let model = doc.value(name, "model").unwrap_or_default();
    let weights: Vec<_> = section.downloads.iter().filter(|d| hub::stored_name(&d.file) == model).collect();
    let serving = Serving::of_preset(&doc, name);
    match weights.as_slice() {
        [d] => predict(home, &d.repo, &d.file, None, config::get(home, "HF_TOKEN").as_deref(), &serving),
        [] => Err(format!("[{name}] has no \"; Download:\" line for its model {model}")),
        _ => Err(format!("[{name}]'s model is split into shards")),
    }
}

/// Predict the fit of `path_in_repo` in `repo` on this card. `size` is the
/// file's size when the caller already has it (the Hub's listing).
pub fn predict(
    home: &Path,
    repo: &str,
    path_in_repo: &str,
    size: Option<u64>,
    token: Option<&str>,
    serving: &Serving,
) -> Result<Prediction, String> {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let url = format!("https://huggingface.co/{repo}/resolve/main/{path_in_repo}");
    let size = match size {
        Some(size) => size,
        None => crate::pull::linked_size(&url, token).ok_or_else(|| format!("the Hub gives no size for {path_in_repo}"))?,
    };
    let name = hub::stored_name(path_in_repo);
    if !hub::safe_name(name) {
        return Err(format!("{path_in_repo:?} does not name a file golem can store"));
    }
    let (header, facts) = header(home, &url, name, size, token)?;
    predict_file(home, &header, &facts, serving)
}

/// [`predict`] for a file already on disk - a header in `.local/fit/` or a
/// whole model.
pub fn predict_file(home: &Path, file: &Path, facts: &ModelFacts, serving: &Serving) -> Result<Prediction, String> {
    let tool = fit_params(home)?;
    let drafting = Drafting::of(facts).filter(|_| serving.drafting != Some(false));
    let card = machine::gpu().ok_or("no GPU found: there is no card to fit")?;
    let headroom = config::get(home, "TUNE_VRAM_HEADROOM_MIB").and_then(|v| v.parse().ok()).unwrap_or(1024);
    let (desktop_mib, desktop_read) = match engine::status(home).up {
        false => (machine::vram_used_mib().unwrap_or(card.used_mib), true),
        true => (DESKTOP_ASSUMED_MIB, false),
    };
    let room = card.total_mib.saturating_sub(headroom).saturating_sub(desktop_mib);
    let probe = if facts.n_ctx_train > 0 { facts.n_ctx_train.min(262_144) } else { 32_768 };
    let ctx_at = |b: &Breakdown| {
        fit::engine_context(b, probe, facts, &serving.cache_k, &serving.cache_v, drafting.as_ref(), room)
            .ok_or_else(|| format!("the cache type {}/{} has no known size", serving.cache_k, serving.cache_v))
    };
    let ask_at = |ctx: u64, n_cpu_moe: u64| ask(&tool, file, ctx, n_cpu_moe, serving, home);
    let ask = |n_cpu_moe: u64| ask_at(probe, n_cpu_moe);

    // A dense model is placed whole, and a tuned split is kept. Otherwise a
    // mixture of experts is asked once with every layer's experts in RAM;
    // the header then says what each split keeps on the card, and the
    // fewest experts in RAM that leave a 200K cache room (or the trained
    // context, when shorter) is asked again.
    let n_cpu_moe = if let Some(n) = serving.n_cpu_moe {
        n
    } else if facts.moe {
        let all = ask(facts.n_layer)?;
        let target = facts.n_ctx_train.min(200_000).max(1);
        let mtp = drafting.map_or(0, |d| d.weights);
        let on_card = |n: u64| Breakdown {
            model: (fit::card_weight_bytes(facts, n).saturating_sub(mtp) >> 20).max(all.model),
            ..all
        };
        let mut chosen = facts.n_layer;
        for n in 0..=facts.n_layer {
            if ctx_at(&on_card(n))? >= target {
                chosen = n;
                break;
            }
        }
        chosen
    } else {
        0
    };
    // The compute buffer grows with the context (the 9B's reading climbed
    // 22.6 KB a token from 64K to 256K, against a 17.4 KB cache), so one
    // sized at the probe is too large for a shorter answer: the engine is
    // asked again at the answer, and that breakdown sizes the prediction.
    let first = ctx_at(&ask(n_cpu_moe)?)?;
    let ctx = if first > 0 && first < probe {
        let again = ask_at(first, n_cpu_moe)?;
        fit::engine_context(&again, first, facts, &serving.cache_k, &serving.cache_v, drafting.as_ref(), room)
            .unwrap_or(first)
    } else {
        first
    };
    Ok(Prediction {
        ctx,
        ctx_train: facts.n_ctx_train,
        n_cpu_moe,
        drafting: drafting.is_some(),
        desktop_mib,
        desktop_read,
    })
}

/// The pinned build's `llama-fit-params`, beside its server.
fn fit_params(home: &Path) -> Result<PathBuf, String> {
    let plan = launch::plan(home, &Overrides::default())?;
    let name = if cfg!(windows) { "llama-fit-params.exe" } else { "llama-fit-params" };
    let tool = plan.bin.join(name);
    if tool.is_file() {
        Ok(tool)
    } else {
        Err(format!("{} has no {name}: run `golem up` once to install the pinned build", plan.bin.display()))
    }
}

/// `llama-fit-params -v` on `file` at `ctx` tokens with the experts of the
/// first `n_cpu_moe` layers in RAM: the breakdown it prints. Its log goes to
/// a file beside the header, because the tool logs on stderr.
fn ask(tool: &Path, file: &Path, ctx: u64, n_cpu_moe: u64, serving: &Serving, home: &Path) -> Result<Breakdown, String> {
    let log = paths::fit_dir(home).join("fit-params.log");
    let _ = std::fs::remove_file(&log);
    // The batch is the server's 2048, or the ubatch when that is larger.
    let batch = serving.ubatch.parse::<u64>().unwrap_or(0).max(2048).to_string();
    let mut command = Command::new(tool);
    command
        .arg("-v")
        .arg("--log-file")
        .arg(&log)
        .arg("-m")
        .arg(file)
        .args(["-c", &ctx.to_string(), "-ctk", &serving.cache_k, "-ctv", &serving.cache_v, "-fa", "on"])
        .args(["-b", &batch, "-ub", &serving.ubatch, "-ngl", "99", "--n-cpu-moe", &n_cpu_moe.to_string()]);
    proc::output_within(command, FIT_LIMIT, 1 << 20)
        .map_err(|e| format!("cannot run {}: {e}", tool.display()))?
        .ok_or_else(|| format!("{} did not finish in {}s", tool.display(), FIT_LIMIT.as_secs()))?;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    fit::breakdown(&text).ok_or_else(|| format!("llama-fit-params printed no memory breakdown (log: {})", log.display()))
}

/// The header of the file at `url` as a sparse file of its real `size` in
/// `.local/fit/`, and the facts it holds. One already there is read again
/// rather than fetched.
fn header(home: &Path, url: &str, name: &str, size: u64, token: Option<&str>) -> Result<(PathBuf, ModelFacts), String> {
    let dir = paths::fit_dir(home);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let path = dir.join(name);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() == size) {
        if let Ok(facts) = gguf::read(&path) {
            return Ok((path, facts));
        }
    }
    let mut want = HEADER_FIRST.min(size);
    loop {
        let bytes = range(url, want, token)?;
        match gguf::read_bytes(&bytes, size) {
            Ok(facts) => {
                write_sparse(&path, &bytes, size)?;
                return Ok((path, facts));
            }
            Err(GgufError::Truncated) if want < size.min(HEADER_MAX) => want = (want * 4).min(size).min(HEADER_MAX),
            Err(e) => return Err(format!("{name}: {e} in its first {} MB", bytes.len() >> 20)),
        }
    }
}

/// The first `len` bytes of `url` (`curl -r 0-<len-1>`).
fn range(url: &str, len: u64, token: Option<&str>) -> Result<Vec<u8>, String> {
    let mut command = Command::new("curl");
    command.args(["-sfL", "--retry", "3", "--max-time", "120", "-r", &format!("0-{}", len - 1)]);
    if let Some(token) = token {
        command.arg("-H").arg(format!("Authorization: Bearer {token}"));
    }
    command.arg(url);
    let bytes = proc::output_within(command, Duration::from_secs(150), len as usize + 1)
        .map_err(|e| format!("cannot run curl: {e}"))?
        .ok_or_else(|| format!("curl did not finish the range request for {url}"))?;
    if bytes.is_empty() {
        return Err(format!("the range request for {url} came back empty"));
    }
    Ok(bytes)
}

/// `bytes` at the start of a file of `size` bytes whose rest is a hole.
fn write_sparse(path: &Path, bytes: &[u8], size: u64) -> Result<(), String> {
    let fail = |e: std::io::Error| format!("cannot write {}: {e}", path.display());
    let _ = std::fs::remove_file(path);
    let mut file = OpenOptions::new().create(true).write(true).truncate(true).open(path).map_err(fail)?;
    file.write_all(bytes).map_err(fail)?;
    drop(file);
    if cfg!(windows) {
        fsutil(&["sparse", "setflag"], path, &[])?;
    }
    let file = OpenOptions::new().write(true).open(path).map_err(fail)?;
    file.set_len(size).map_err(fail)?;
    drop(file);
    if cfg!(windows) && size > bytes.len() as u64 {
        let (from, len) = (bytes.len().to_string(), (size - bytes.len() as u64).to_string());
        fsutil(&["sparse", "setrange"], path, &[&from, &len])?;
    }
    Ok(())
}

/// `fsutil <verb...> <path> <args...>`, which marks a file sparse on Windows.
fn fsutil(verb: &[&str], path: &Path, args: &[&str]) -> Result<(), String> {
    let out = Command::new("fsutil")
        .args(verb)
        .arg(path)
        .args(args)
        .output()
        .map_err(|e| format!("cannot run fsutil: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let _ = std::fs::remove_file(path);
        Err(format!("fsutil {} failed: {}", verb.join(" "), String::from_utf8_lossy(&out.stdout).trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prediction_says_what_it_assumed() {
        let p = Prediction {
            ctx: 228_352,
            ctx_train: 262_144,
            n_cpu_moe: 35,
            drafting: true,
            desktop_mib: 772,
            desktop_read: true,
        };
        assert_eq!(p.line(), "fits 223K tokens (predicted from the header: n-cpu-moe 35, drafting, desktop 0.8 GB read)");
        let whole = Prediction { ctx: 32_768, ctx_train: 32_768, n_cpu_moe: 0, drafting: false, desktop_mib: 768, desktop_read: false };
        assert_eq!(whole.line(), "fits its trained 32K tokens (predicted from the header: desktop 0.8 GB assumed)");
        let none = Prediction { ctx: 0, ..whole };
        assert!(none.line().starts_with("does not fit this card"), "{}", none.line());
        assert!(golem_core::json::parse(&p.to_json()).is_ok());
    }

    #[test]
    fn a_preset_is_sized_as_it_serves() {
        let doc = golem_core::ini::parse(
            "[*]\ncache-type-k = q8_0\ncache-type-v = q8_0\n\
             [coder]\nspec-type = draft-mtp\nn-cpu-moe = 35\nubatch-size = 2048\n\
             [everyday]\nmodel = e.gguf\n",
        );
        let coder = Serving::of_preset(&doc, "coder");
        assert_eq!((coder.drafting, coder.n_cpu_moe, coder.ubatch.as_str()), (Some(true), Some(35), "2048"));
        let everyday = Serving::of_preset(&doc, "everyday");
        assert_eq!(everyday.drafting, Some(false), "no spec-type: an MTP head is not drafted with");
        assert_eq!((everyday.n_cpu_moe, everyday.ubatch.as_str(), everyday.cache_k.as_str()), (None, "512", "q8_0"));
        assert_eq!(Serving::default().drafting, None, "a new model drafts when its file can");
    }
}
