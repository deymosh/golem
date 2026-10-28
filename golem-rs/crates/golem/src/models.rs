//! The console's Models tab: what each preset is, and the edits behind its
//! buttons.
//!
//! The page reads the fields by name (`m.Present`, `m.NcMoe`, `m.ParamsB`,
//! ...), so the JSON's capitalisation is a contract with the page template.
//! Three facts come from three
//! places: the preset file says what the section *asks for* (its flags and
//! its `; Download:` source), the disk says whether the file is here and how
//! big it is, and the GGUF header says what the model *is* - parameters,
//! experts, trained context.
//!
//! The three edits refuse what would break the setup: the default preset and the default's model file cannot be deleted, so a click can
//! never leave the setup without one. Every edit that touches
//! `config/models.ini` copies it to `.bak` first, and the delete only ever
//! reaches inside `data/models`.

use std::path::{Path, PathBuf};

use golem_core::gguf;
use golem_core::ini;
use golem_core::json;

use crate::config;
use crate::state;

/// One preset, as the page's table shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    pub name: String,
    pub file: String,
    /// The `; Download:` repo `up` and `pull` fetch from.
    pub repo: String,
    /// `ctx-size`, or 0 when the section does not say.
    pub ctx: i64,
    /// `n-cpu-moe`, or -1 when the section does not say (`0` is a real value:
    /// the whole model in VRAM).
    pub nc_moe: i64,
    pub mtp: bool,
    pub present: bool,
    /// Size on disk in GB, one decimal; 0 when the file is not here.
    pub gb: f64,
    pub default: bool,
    pub params_b: f64,
    pub params_active_b: f64,
    pub moe: bool,
    /// `256x8` for a mixture of experts, empty for a dense model.
    pub experts: String,
    pub ctx_train: i64,
    /// The library's role for it (coder, everyday, ...), empty when the
    /// library does not list it.
    pub role: String,
}

/// Every preset in the home's preset file, in file order.
pub fn list(home: &Path) -> Vec<Model> {
    let Ok(doc) = ini::Document::read(&state::preset_path(home)) else { return Vec::new() };
    let alias = config::get(home, "MODEL_ALIAS").unwrap_or_default();
    let models_dir = home.join("data/models");
    let library = crate::paths::library(home);

    doc.sections
        .iter()
        .filter(|section| section.name != "*")
        .map(|section| {
            // The section's own `model` key wins; a section that never got
            // one falls back to the file its `; Download:` names, which is
            // what the page then offers to fetch.
            let file = section
                .value("model")
                .map(str::to_string)
                .or_else(|| section.downloads.first().map(|d| d.name().to_string()))
                .unwrap_or_default();
            let path = models_dir.join(&file);
            // A preset is hand-editable: a `model` that is not a plain file
            // name is not read, whatever it points at.
            let present = golem_core::hub::safe_name(&file) && path.is_file();
            // Only a file that is here can be read; a header that will not
            // parse (a download in progress, a foreign file) is not an error
            // the table should show - it is simply a model with no facts.
            let facts = if present { gguf::read(&path).ok() } else { None };
            let moe = facts.as_ref().is_some_and(|f| f.moe);
            Model {
                name: section.name.clone(),
                file,
                repo: section.downloads.first().map(|d| d.repo.clone()).unwrap_or_default(),
                ctx: number(section.value("ctx-size")).unwrap_or(0),
                nc_moe: number(section.value("n-cpu-moe")).unwrap_or(-1),
                // Any `spec-type` means "this preset uses speculative
                // decoding", whatever the value says.
                mtp: section.value("spec-type").is_some(),
                present,
                gb: if present { round(file_size_gb(&path), 1) } else { 0.0 },
                default: section.name == alias,
                params_b: facts.as_ref().map_or(0.0, |f| round(f.params_total as f64 / 1e9, 2)),
                params_active_b: facts
                    .as_ref()
                    .map_or(0.0, |f| round(f.params_active as f64 / 1e9, 2)),
                moe,
                experts: if moe {
                    format!(
                        "{}x{}",
                        facts.as_ref().map_or(0, |f| f.n_expert),
                        facts.as_ref().map_or(0, |f| f.n_expert_used)
                    )
                } else {
                    String::new()
                },
                ctx_train: facts.as_ref().map_or(0, |f| f.n_ctx_train as i64),
                role: library
                    .iter()
                    .find(|e| e.name == section.name)
                    .and_then(|e| e.role.clone())
                    .unwrap_or_default(),
            }
        })
        .collect()
}

/// The list as the page's `fetch("api/models")` expects it.
///
/// The field names, capitals and all, are what the page's own template
/// reads.
pub fn to_json(models: &[Model]) -> String {
    let items: Vec<String> = models
        .iter()
        .map(|m| {
            format!(
                "{{\"Name\": {}, \"File\": {}, \"Repo\": {}, \"Ctx\": {}, \"NcMoe\": {}, \
                 \"Mtp\": {}, \"Present\": {}, \"Gb\": {}, \"Default\": {}, \"ParamsB\": {}, \
                 \"ParamsActiveB\": {}, \"Moe\": {}, \"Experts\": {}, \"CtxTrain\": {},                  \"Role\": {}}}",
                json::quote(&m.name),
                json::quote(&m.file),
                json::quote(&m.repo),
                m.ctx,
                m.nc_moe,
                m.mtp,
                m.present,
                number_text(m.gb, 1),
                m.default,
                number_text(m.params_b, 2),
                number_text(m.params_active_b, 2),
                m.moe,
                json::quote(&m.experts),
                m.ctx_train,
                json::quote(&m.role),
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// The fields `POST /api/addmodel` sends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewModel {
    pub name: String,
    pub repo: String,
    pub file: String,
    /// Kept as text: only digits are accepted, anything else falls back to
    /// the default, and `""` is not a number.
    pub ctx: String,
    pub ncpu: String,
    pub mtp: bool,
}

/// `POST /api/addmodel`: append a section to the preset file.
///
/// The name is sanitised rather than rejected (a console user typing
/// "My Model" gets `my-model`), the repo and file are checked against the
/// shape `up` and `pull` can fetch, and an existing preset name is refused so
/// a click cannot silently retune a model that is already there. Answers
/// `added` / `bad-input`, which the page reads.
pub fn add(home: &Path, form: &NewModel) -> Result<&'static str, std::io::Error> {
    let name = sanitize(&form.name);
    let repo = form.repo.trim().to_string();
    let file = form.file.trim().to_string();
    if name.is_empty()
        || !is_repo(&repo)
        || !is_file(&file)
        || preset_names(home).iter().any(|n| n == &name)
    {
        return Ok("bad-input");
    }
    let ctx = number(Some(form.ctx.as_str())).unwrap_or(32_768);
    let ncpu = number(Some(form.ncpu.as_str())).unwrap_or(32);

    let preset = state::preset_path(home);
    backup(&preset)?;
    let Ok(doc) = ini::Document::read(&preset) else { return Ok("bad-input") };
    // The leading blank line and the comment are what make the section look
    // hand-written in the file.
    let mut block = String::from("\n");
    block.push_str(&format!("; Added from the web console {}\n", golem_core::time::today()));
    block.push_str(&format!("; Download: {repo} -> {file}\n"));
    block.push_str(&format!("[{name}]\n"));
    block.push_str(&format!("model = {file}\n"));
    block.push_str(&format!("ctx-size = {ctx}\n"));
    block.push_str("load-mode = mlock\n");
    block.push_str(&format!("n-cpu-moe = {ncpu}\n"));
    if form.mtp {
        block.push_str("spec-type = draft-mtp\n");
    }
    block.push_str("batch-size = 2048\nubatch-size = 2048\ntemp = 0.6\ntop-k = 20\nmin-p = 0");
    std::fs::write(&preset, doc.append(&block))?;
    Ok("added")
}

/// `POST /api/delmodel`: delete a preset's model file from disk.
///
/// The preset stays (it is re-downloadable), and the default's file is
/// refused - the running setup cannot be broken by a click. Only a `.gguf`
/// inside `data/models` is ever removed, whatever the section says.
pub fn delete_file(home: &Path, name: &str) -> Result<&'static str, std::io::Error> {
    let alias = config::get(home, "MODEL_ALIAS").unwrap_or_default();
    let Some(model) = list(home).into_iter().find(|m| m.name == name) else {
        return Ok("bad-input");
    };
    if !model.present || name == alias {
        return Ok("bad-input");
    }
    let models_dir = home.join("data/models");
    let path = models_dir.join(&model.file);
    if !model.file.ends_with(".gguf") || !inside(&models_dir, &path) {
        return Ok("bad-input");
    }
    std::fs::remove_file(&path)?;
    // The checksum the download left beside it, when there is one. Its
    // absence is not a failure: a hand-copied file never had one.
    let _ = std::fs::remove_file(models_dir.join(format!("{}.sha256", model.file)));
    Ok("deleted")
}

/// `POST /api/delpreset`: remove a preset from the preset file.
///
/// The model file stays on disk, the default is refused, and the section's
/// own comment block goes with it. Answers `removed` / `bad-input`.
pub fn delete_preset(home: &Path, name: &str) -> Result<&'static str, std::io::Error> {
    let alias = config::get(home, "MODEL_ALIAS").unwrap_or_default();
    if name.is_empty() || name == alias || !preset_names(home).iter().any(|n| n == name) {
        return Ok("bad-input");
    }
    let preset = state::preset_path(home);
    let Ok(doc) = ini::Document::read(&preset) else { return Ok("bad-input") };
    backup(&preset)?;
    std::fs::write(&preset, doc.remove(name))?;
    Ok("removed")
}

/// The preset file's section names, without the shared `[*]` one.
fn preset_names(home: &Path) -> Vec<String> {
    ini::Document::read(&state::preset_path(home)).map(|doc| doc.preset_names()).unwrap_or_default()
}

/// Copy the preset file beside itself (`.bak`) before an edit.
///
/// A failed copy is a failed edit: rewriting the file with no way back is
/// worse than refusing the click.
fn backup(preset: &Path) -> Result<(), std::io::Error> {
    let bak = PathBuf::from(format!("{}.bak", preset.display()));
    std::fs::copy(preset, bak).map(|_| ())
}

/// Whether `path` really is inside `dir`, after both are resolved.
///
/// A section's `model` value comes from a file the user can edit, so it can
/// say `../../golem.toml`; canonicalising both sides is what makes the check about
/// the file that would actually be deleted rather than about the string.
fn inside(dir: &Path, path: &Path) -> bool {
    match (dir.canonicalize(), path.canonicalize()) {
        (Ok(dir), Ok(path)) => path.starts_with(&dir),
        _ => false,
    }
}

/// A preset name in the shape `up` and `pull` expect: lower case, and nothing
/// but letters, digits, dot, dash and underscore - one dash per bad
/// character.
fn sanitize(name: &str) -> String {
    name.trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect()
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')
}

/// `owner/repo`: exactly one slash, and no surprises on either side.
fn is_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) => {
            !owner.is_empty()
                && !name.is_empty()
                && owner.chars().all(is_word)
                && name.chars().all(is_word)
        }
        _ => false,
    }
}

/// `something.gguf`, and nothing else.
fn is_file(file: &str) -> bool {
    file.len() > ".gguf".len() && file.ends_with(".gguf") && file.chars().all(is_word)
}

/// A `key = value` number, or `None` when it is absent or not one.
///
/// Only digits count, so `4096.0` or `4096k` is not a number to fall back
/// from - it is a value to ignore.
fn number(value: Option<&str>) -> Option<i64> {
    let value = value?.trim();
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn round(value: f64, places: u32) -> f64 {
    let factor = 10f64.powi(places as i32);
    (value * factor).round() / factor
}

/// A rounded number as JSON, in the shortest form that still says it.
///
/// 3.70 comes out as `3.7` and a round 20 GB as `20`: the page shows the
/// number as it arrives, and a fixed two decimals would put `20.00` on it.
fn number_text(value: f64, places: u32) -> String {
    let text = format!("{:.*}", places as usize, round(value, places));
    match text.split_once('.') {
        Some((_, decimals)) if decimals.chars().all(|c| c == '0') => {
            text.split('.').next().unwrap_or("0").to_string()
        }
        Some((whole, decimals)) => format!("{whole}.{}", decimals.trim_end_matches('0')),
        None => text,
    }
}

fn file_size_gb(path: &Path) -> f64 {
    std::fs::metadata(path).map(|m| m.len() as f64 / 1_073_741_824.0).unwrap_or(0.0)
}


#[cfg(test)]
mod tests {
    use super::*;

    const PRESET: &str = "\
version = 1

[*]
jinja = true

; Coder - the default.
; Download: someone/model-GGUF -> model-Q4_K_M.gguf
; Quality: 79
[model-a]
model = model-Q4_K_M.gguf
spec-type = draft-mtp
ctx-size = 236544
n-cpu-moe = 36

; No file of its own yet: the section only knows where it would come from.
; Download: tiny/tiny-GGUF -> tiny.gguf
[tiny]
ctx-size = 8192
";

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-models-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::create_dir_all(dir.join("data/models")).expect("models");
        std::fs::write(dir.join("config/models.ini"), PRESET).expect("preset");
        std::fs::write(dir.join("golem.toml"), "MODEL_ALIAS=\"model-a\"\n").expect("env");
        dir
    }

    #[test]
    fn lists_every_preset_in_file_order_with_what_the_file_says() {
        let dir = home("list");
        let models = list(&dir);
        assert_eq!(models.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), vec!["model-a", "tiny"]);

        let a = &models[0];
        assert_eq!(a.file, "model-Q4_K_M.gguf");
        assert_eq!(a.repo, "someone/model-GGUF");
        assert_eq!(a.ctx, 236544);
        assert_eq!(a.nc_moe, 36);
        assert!(a.mtp, "any spec-type means the preset uses one");
        assert!(a.default, "the configured alias names it");
        assert!(!a.present, "the file is not on disk in this fixture");
        assert_eq!(a.gb, 0.0);
        assert_eq!(a.params_b, 0.0, "no file, so no facts to read");

        // A section with no `model` key falls back to its download's file.
        let tiny = &models[1];
        assert_eq!(tiny.file, "tiny.gguf");
        assert_eq!(tiny.repo, "tiny/tiny-GGUF");
        assert_eq!(tiny.nc_moe, -1, "absent is -1, not 0: 0 is a real setting");
        assert!(!tiny.mtp);
        assert!(!tiny.default);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_present_file_brings_its_size_and_its_gguf_facts() {
        let dir = home("facts");
        let path = dir.join("data/models/model-Q4_K_M.gguf");
        std::fs::write(&path, moe_gguf()).expect("gguf");

        let a = &list(&dir)[0];
        assert!(a.present);
        assert!(a.moe, "256 experts is a mixture of experts");
        assert_eq!(a.experts, "256x8");
        assert_eq!(a.ctx_train, 262144);
        // The size is read, and a header is a few hundred bytes - which is
        // 0.0 GB at the one decimal the page prints.
        assert!(file_size_gb(&path) > 0.0);
        assert_eq!(a.gb, 0.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A GGUF header with a MoE and a trained context: magic, version,
    /// tensor count, metadata count, then the keys the table reads. The
    /// counts are UINT32 - a string "256" would read back as 0, which is
    /// exactly how a dense model looks.
    fn moe_gguf() -> Vec<u8> {
        let mut g = Vec::new();
        g.extend_from_slice(b"GGUF");
        g.extend_from_slice(&3u32.to_le_bytes());
        g.extend_from_slice(&0u64.to_le_bytes()); // tensors
        g.extend_from_slice(&4u64.to_le_bytes()); // metadata pairs
        put_kv_str(&mut g, "general.architecture", "qwen3_moe");
        put_kv_u32(&mut g, "qwen3_moe.expert_count", 256);
        put_kv_u32(&mut g, "qwen3_moe.expert_used_count", 8);
        put_kv_u32(&mut g, "qwen3_moe.context_length", 262_144);
        g.extend_from_slice(&[0u8; 8]);
        g
    }

    fn put_string(out: &mut Vec<u8>, value: &str) {
        out.extend_from_slice(&(value.len() as u64).to_le_bytes());
        out.extend_from_slice(value.as_bytes());
    }

    fn put_kv_str(out: &mut Vec<u8>, key: &str, value: &str) {
        put_string(out, key);
        out.extend_from_slice(&8u32.to_le_bytes());
        put_string(out, value);
    }

    fn put_kv_u32(out: &mut Vec<u8>, key: &str, value: u32) {
        put_string(out, key);
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn numbers_round_and_print_in_their_shortest_form() {
        assert_eq!(round(21.712_345, 1), 21.7);
        assert_eq!(round(35.507, 2), 35.51);
        assert_eq!(round(0.0, 1), 0.0);
        assert_eq!(number_text(20.56, 1), "20.6");
        assert_eq!(number_text(20.0, 1), "20", "a round size has no decimal point");
        assert_eq!(number_text(3.7, 2), "3.7", "and no padding zeros");
        assert_eq!(number_text(3.70, 2), "3.7");
        assert_eq!(number_text(35.507, 2), "35.51");
        assert_eq!(number_text(0.0, 2), "0");
        assert_eq!(number_text(34.66, 2), "34.66");
    }

    #[test]
    fn the_json_is_the_shape_the_page_reads() {
        let dir = home("json");
        let text = to_json(&list(&dir));
        // Field names and types the template indexes by name.
        for field in [
            "\"Name\": \"model-a\"",
            "\"File\": \"model-Q4_K_M.gguf\"",
            "\"Repo\": \"someone/model-GGUF\"",
            "\"Ctx\": 236544",
            "\"NcMoe\": 36",
            "\"Mtp\": true",
            "\"Present\": false",
            "\"Default\": true",
            "\"Moe\": false",
            "\"Experts\": \"\"",
            "\"CtxTrain\": 0",
        ] {
            assert!(text.contains(field), "{field} missing from {text}");
        }
        // Numbers, not strings: the page formats them.
        assert!(text.contains("\"Gb\": 0"), "{text}");
        assert!(text.contains("\"ParamsB\": 0"), "{text}");
        // The shared `[*]` section is not a model.
        assert!(!text.contains("\"Name\": \"*\""), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adding_a_model_appends_a_hand_written_looking_section() {
        let dir = home("add");
        let form = NewModel {
            name: "My Model".into(),
            repo: "unsloth/Thing-GGUF".into(),
            file: "Thing-Q4_K_M.gguf".into(),
            ctx: "65536".into(),
            ncpu: "20".into(),
            mtp: true,
        };
        assert_eq!(add(&dir, &form).expect("write"), "added");

        let text = std::fs::read_to_string(dir.join("config/models.ini")).expect("read");
        let doc = ini::parse(&text);
        assert_eq!(doc.preset_names(), vec!["model-a", "tiny", "my-model"]);
        let added = doc.get("my-model").expect("the new section");
        assert_eq!(added.value("model"), Some("Thing-Q4_K_M.gguf"));
        assert_eq!(added.value("ctx-size"), Some("65536"));
        assert_eq!(added.value("n-cpu-moe"), Some("20"));
        assert_eq!(added.value("spec-type"), Some("draft-mtp"));
        assert_eq!(added.value("load-mode"), Some("mlock"));
        assert_eq!(added.value("ubatch-size"), Some("2048"));
        assert_eq!(added.downloads.first().map(|d| d.file.as_str()), Some("Thing-Q4_K_M.gguf"));
        assert!(text.contains("; Added from the web console 2"), "a dated comment:\n{text}");
        // The file is backed up before it is touched.
        assert!(dir.join("config/models.ini.bak").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adding_refuses_what_could_not_be_fetched() {
        let dir = home("add-bad");
        let good = NewModel {
            name: "ok".into(),
            repo: "owner/repo".into(),
            file: "x.gguf".into(),
            ctx: String::new(),
            ncpu: String::new(),
            mtp: false,
        };
        // The name is already a preset.
        let taken = NewModel { name: "model-a".into(), ..good.clone() };
        assert_eq!(add(&dir, &taken).expect("no write"), "bad-input");
        // Not owner/repo, or not a gguf.
        for repo in ["owner", "owner/repo/extra", "own er/repo", ""] {
            let form = NewModel { repo: repo.into(), ..good.clone() };
            assert_eq!(add(&dir, &form).expect("no write"), "bad-input", "repo {repo:?}");
        }
        for file in ["thing.bin", "x.gguf.exe", ".gguf", "a b.gguf"] {
            let form = NewModel { file: file.into(), ..good.clone() };
            assert_eq!(add(&dir, &form).expect("no write"), "bad-input", "file {file:?}");
        }
        // Nothing was written, and no backup was left behind by a refusal.
        assert_eq!(std::fs::read_to_string(dir.join("config/models.ini")).expect("read"), PRESET);
        assert!(!dir.join("config/models.ini.bak").exists());

        // The defaults when ctx/ncpu are not numbers.
        assert_eq!(add(&dir, &good).expect("write"), "added");
        let doc = ini::Document::read(&dir.join("config/models.ini")).expect("read");
        assert_eq!(doc.value("ok", "ctx-size"), Some("32768"));
        assert_eq!(doc.value("ok", "n-cpu-moe"), Some("32"));
        assert_eq!(doc.value("ok", "spec-type"), None, "no mtp, no spec-type");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_name_is_sanitised_rather_than_refused() {
        assert_eq!(sanitize("  My Model  "), "my-model");
        assert_eq!(sanitize("a/b\\c"), "a-b-c");
        assert_eq!(sanitize("Qwen2.5-0.5B"), "qwen2.5-0.5b");
        assert_eq!(sanitize("!!!"), "---");
    }

    #[test]
    fn deleting_a_model_file_leaves_the_preset_and_takes_the_checksum() {
        let dir = home("delmodel");
        let file = dir.join("data/models/model-Q4_K_M.gguf");
        std::fs::write(&file, b"weights").expect("write");
        std::fs::write(dir.join("data/models/model-Q4_K_M.gguf.sha256"), b"abc").expect("write");

        // The default's file is refused: the running setup stays usable.
        assert_eq!(delete_file(&dir, "model-a").expect("no delete"), "bad-input");
        assert!(file.is_file());

        std::fs::write(dir.join("golem.toml"), "MODEL_ALIAS=\"other\"\n").expect("env");
        assert_eq!(delete_file(&dir, "model-a").expect("delete"), "deleted");
        assert!(!file.exists());
        assert!(!dir.join("data/models/model-Q4_K_M.gguf.sha256").exists());
        // The preset is still there, and a second click is a no-op.
        assert!(dir.join("config/models.ini").is_file());
        assert_eq!(delete_file(&dir, "model-a").expect("no delete"), "bad-input");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deleting_a_model_never_reaches_outside_data_models() {
        let dir = home("escape");
        std::fs::write(dir.join("secret.gguf"), b"keep me").expect("write");
        // A preset whose `model` climbs out of the models directory.
        let text = std::fs::read_to_string(dir.join("config/models.ini")).expect("read");
        let escaped = text.replace("model = model-Q4_K_M.gguf", "model = ../secret.gguf");
        std::fs::write(dir.join("config/models.ini"), escaped).expect("write");
        std::fs::write(dir.join("golem.toml"), "MODEL_ALIAS=\"other\"\n").expect("env");

        assert_eq!(delete_file(&dir, "model-a").expect("no delete"), "bad-input");
        assert!(dir.join("secret.gguf").is_file(), "the file outside data/models survives");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn removing_a_preset_takes_its_comment_block_and_leaves_the_file() {
        let dir = home("delpreset");
        std::fs::write(dir.join("data/models/tiny.gguf"), b"weights").expect("write");

        // The default cannot be removed, and neither can a name that is not
        // a preset.
        assert_eq!(delete_preset(&dir, "model-a").expect("no edit"), "bad-input");
        assert_eq!(delete_preset(&dir, "nope").expect("no edit"), "bad-input");
        assert_eq!(delete_preset(&dir, "").expect("no edit"), "bad-input");

        assert_eq!(delete_preset(&dir, "tiny").expect("remove"), "removed");
        let text = std::fs::read_to_string(dir.join("config/models.ini")).expect("read");
        assert!(!text.contains("[tiny]"));
        assert!(!text.contains("only knows where it would come from"), "its comment goes:\n{text}");
        assert!(text.contains("; Quality: 79"), "the other section's stays:\n{text}");
        assert!(dir.join("data/models/tiny.gguf").is_file(), "the model file stays");
        assert!(dir.join("config/models.ini.bak").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_preset_file_lists_nothing_rather_than_failing() {
        let dir = std::env::temp_dir().join(format!("golem-models-{}-none", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert!(list(&dir).is_empty());
        assert_eq!(to_json(&[]), "[]");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_added_comment_carries_todays_date() {
        let dir = home("dated");
        let form = NewModel {
            name: "dated".into(),
            repo: "owner/repo".into(),
            file: "x.gguf".into(),
            ctx: String::new(),
            ncpu: String::new(),
            mtp: false,
        };
        assert_eq!(add(&dir, &form).expect("write"), "added");
        let text = std::fs::read_to_string(dir.join("config/models.ini")).expect("read");
        let stamp = golem_core::time::today();
        assert!(text.contains(&format!("; Added from the web console {stamp}")), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
