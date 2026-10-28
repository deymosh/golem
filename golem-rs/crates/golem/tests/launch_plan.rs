//! What `up --dry-run` resolves, run as the real binary in its own process.
//!
//! The `GOLEM_*` variables are read once, at the CLI's edge, and become the
//! run's overrides; a child process is the honest way to test that edge, and
//! it is how a caller (a script, a container) actually sets them. What that
//! costs when it breaks is worth the test: a plan that ignored them would
//! serve the configured preset for every candidate of a sweep, and the gate
//! would score the wrong model - which happened once.
//!
//! Nothing here needs a GPU, a network or a running engine: `--dry-run` stops
//! at the command line.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A home that resolves, with two presets so a redirect is visible.
fn home(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("golem-rs-launch-plan-{tag}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("engines/llama.cpp")).expect("temp");
    std::fs::create_dir_all(dir.join("config")).expect("temp");
    // The models are here, so the dry run has nothing to refuse.
    std::fs::create_dir_all(dir.join("data/models")).expect("temp");
    for model in ["a.gguf", "b.gguf"] {
        std::fs::write(dir.join("data/models").join(model), "x").expect("model");
    }
    std::fs::write(
        dir.join("engines/llama.cpp/engine.ini"),
        "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = true\n",
    )
    .expect("adapter");
    std::fs::write(
        dir.join("config/models.ini"),
        "[*]\njinja = true\n[model-a]\nmodel = a.gguf\n",
    )
    .expect("preset");
    std::fs::write(
        dir.join("config/other.ini"),
        "[*]\njinja = true\n[model-b]\nmodel = b.gguf\n",
    )
    .expect("preset");
    // Written before the binary runs: it writes the embedded `golem.toml` into
    // a home that has none, and that would replace this one.
    std::fs::write(
        dir.join("golem.toml"),
        "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\n\
         LOCAL_AI_PORT = \"1\"\nMODEL_ALIAS = \"model-a\"\n\
         MODELS_PRESET = \"config/models.ini\"\n",
    )
    .expect("config");
    dir
}

/// `up --dry-run` with exactly these environment variables added, and the
/// overrides cleared first so nothing leaks in from the test runner.
fn dry_run(dir: &Path, overrides: &[(&str, &str)]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_golem"));
    command.env("GOLEM_HOME", dir).args(["up", "--dry-run"]);
    for key in ["GOLEM_ENGINE", "GOLEM_MODELS_PRESET", "GOLEM_MODEL_ALIAS", "GOLEM_LOCAL_AI_HOST"] {
        command.env_remove(key);
    }
    for (key, value) in overrides {
        command.env(key, value);
    }
    let out = command.output().expect("the binary runs");
    assert!(
        out.status.success(),
        "up --dry-run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_configured_preset_is_what_a_plain_run_serves() {
    let dir = home("plain");
    let out = dry_run(&dir, &[]);
    assert!(out.contains("config/models.ini"), "{out}");
    assert!(!out.contains("other.ini"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_per_run_preset_override_reaches_the_command_line() {
    // What the tuner does for each candidate it sweeps.
    let dir = home("preset-override");
    let out = dry_run(&dir, &[("GOLEM_MODELS_PRESET", "config/other.ini")]);
    assert!(out.contains("--models-preset"), "{out}");
    assert!(out.contains("other.ini"), "the candidate preset, not the configured one: {out}");
    assert!(!out.contains("models.ini"), "and not the configured one as well: {out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_per_run_alias_override_reaches_the_command_line() {
    // The alias picks the section a single-model engine serves, which is the
    // shape the gate measures a candidate in: `GOLEM_MODEL_ALIAS` names the
    // model being gated, and a plan that ignored it would serve - and report -
    // whatever `golem.toml` names.
    //
    // In router mode the alias is deliberately absent from the command line,
    // the preset carries the flags, and the alias only decides which
    // section's model must be on disk.
    let dir = home("alias-override");
    std::fs::write(
        dir.join("engines/llama.cpp/engine.ini"),
        "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = false\n",
    )
    .expect("adapter");
    // Both overrides, as the tuner sets them: the preset it sweeps and the
    // section it is gating.
    let out = dry_run(
        &dir,
        &[("GOLEM_MODELS_PRESET", "config/other.ini"), ("GOLEM_MODEL_ALIAS", "model-b")],
    );
    assert!(out.contains("serving: model-b"), "{out}");
    assert!(out.contains("b.gguf"), "the overridden section's model: {out}");
    assert!(!out.contains("a.gguf"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_per_run_host_override_reaches_the_command_line() {
    // The container's case: the mounted `golem.toml` carries a host-side
    // `LOCAL_AI_HOST` (127.0.0.1 keeps the LAN out), but a container's port
    // is published, so its engine has to bind 0.0.0.0 whatever the file
    // says - compose sets this override in the process environment.
    let dir = home("host-override");
    let plain = dry_run(&dir, &[]);
    assert!(plain.contains("--host 127.0.0.1"), "the file's host by default: {plain}");
    let out = dry_run(&dir, &[("GOLEM_LOCAL_AI_HOST", "0.0.0.0")]);
    assert!(out.contains("--host 0.0.0.0"), "the override's host: {out}");
    assert!(!out.contains("--host 127.0.0.1"), "and not the file's: {out}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_section_the_alias_names_is_refused_rather_than_served_empty() {
    // A single-model engine handed an alias the preset does not have would
    // start a server with no model on it, and the failure would only show up
    // at the first request.
    let dir = home("no-section");
    std::fs::write(
        dir.join("engines/llama.cpp/engine.ini"),
        "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = false\n",
    )
    .expect("adapter");
    let mut command = Command::new(env!("CARGO_BIN_EXE_golem"));
    command
        .env("GOLEM_HOME", &dir)
        .env_remove("GOLEM_ENGINE")
        .env_remove("GOLEM_MODELS_PRESET")
        .env("GOLEM_MODEL_ALIAS", "not-in-the-preset")
        .args(["up", "--dry-run"]);
    let out = command.output().expect("the binary runs");
    assert!(!out.status.success(), "a section that is not there is not a plan");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("[not-in-the-preset]"), "{stderr}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn with_no_preset_named_the_shipped_one_is_served() {
    // `MODELS_PRESET` unset or empty resolves to config/models.ini: the
    // engine always runs as a router.
    let dir = home("no-preset");
    std::fs::write(
        dir.join("golem.toml"),
        "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nLOCAL_AI_PORT = \"1\"\n",
    )
    .expect("config");
    let out = dry_run(&dir, &[]);

    assert!(out.contains("--models-preset"), "the shipped preset is served: {out}");
    assert!(out.contains("config/models.ini"), "{out}");
    // The single-model keys are not read for serving.
    assert!(!out.contains("--rope-freq-base"), "{out}");
    std::fs::remove_dir_all(&dir).ok();
}
