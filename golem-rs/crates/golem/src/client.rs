//! `golem env` and `golem claude`: point a client at this server.
//!
//! llama-server speaks the Anthropic Messages API natively, so Claude Code
//! connects straight to it; `env` prints the exports for a shell, `claude`
//! runs the client with them already set. The port of `cmd_env` and
//! `cmd_claude` in the `golem` script.
//!
//! The values come from the home's `.env`, read the one way the rest of the
//! binary reads it ([`env_value`]) rather than from the process environment:
//! the shell `source`s `.env` before it prints, and a binary started without
//! that source must answer the same thing.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::state::{env_value, override_value};

/// The loopback host the exports name: `LOCAL_AI_HOST` (default
/// `127.0.0.1`), with the bind-only `0.0.0.0` rewritten - a wildcard is
/// where a server listens, not where a client dials.
fn host(home: &Path) -> String {
    let host = env_value(home, "LOCAL_AI_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
    if host == "0.0.0.0" {
        "127.0.0.1".to_string()
    } else {
        host
    }
}

/// `LOCAL_AI_PORT`, default `8000`.
fn port(home: &Path) -> String {
    env_value(home, "LOCAL_AI_PORT").unwrap_or_else(|| "8000".to_string())
}

/// `LOCAL_AI_API_KEY`, default `local-ai-key`.
fn api_key(home: &Path) -> String {
    env_value(home, "LOCAL_AI_API_KEY").unwrap_or_else(|| "local-ai-key".to_string())
}

/// `MODEL_ALIAS`, default `local-model`, with the process-environment
/// `GOLEM_MODEL_ALIAS` winning when it is set - the shell's
/// `${GOLEM_MODEL_ALIAS:-${MODEL_ALIAS:-local-model}}`, which the tuner and
/// the console's job runner both rely on to redirect one invocation.
fn alias(home: &Path) -> String {
    override_value(home, "GOLEM_MODEL_ALIAS", "MODEL_ALIAS")
        .unwrap_or_else(|| "local-model".to_string())
}

/// The four values both export blocks carry.
///
/// One reader for the two spellings: `export` for a POSIX shell and `$env:`
/// for PowerShell are the same four facts, and a second copy of the defaults
/// is how `up` would end up pointing a client at a different port than `env`
/// does.
fn values(home: &Path) -> (String, String, String, String) {
    (host(home), port(home), api_key(home), alias(home))
}

/// `golem env`: the exports, as a shell would source them.
///
/// Byte for byte what `cmd_env`'s heredoc prints, including the comment line
/// above the four exports and the trailing newline.
pub fn exports(home: &Path) -> String {
    let (host, port, key, alias) = values(home);
    format!(
        "# Point Claude Code at Golem:\n\
         export ANTHROPIC_BASE_URL=\"http://{host}:{port}\"\n\
         export ANTHROPIC_AUTH_TOKEN=\"{key}\"\n\
         export ANTHROPIC_MODEL=\"{alias}\"\n\
         export ANTHROPIC_SMALL_FAST_MODEL=\"{alias}\"\n"
    )
}

/// The same four exports, in the syntax the shell that ran the launcher
/// reads.
///
/// `golem up` prints this in Windows mode because the launcher it replaces
/// prints it: `start-windows.ps1` ends on a `$env:` block, and `up` is
/// typed in PowerShell or `cmd`, where `export` is not a command. Native
/// mode keeps [`exports`], which is what `cmd_up` ends on there.
pub fn powershell_exports(home: &Path) -> String {
    let (host, port, key, alias) = values(home);
    format!(
        "# Point Claude Code at Golem:\n\
         $env:ANTHROPIC_BASE_URL = \"http://{host}:{port}\"\n\
         $env:ANTHROPIC_AUTH_TOKEN = \"{key}\"\n\
         $env:ANTHROPIC_MODEL = \"{alias}\"\n\
         $env:ANTHROPIC_SMALL_FAST_MODEL = \"{alias}\"\n"
    )
}

/// `golem claude [args...]`.
///
/// Runs `claude` with the four variables already set and the arguments passed
/// through, the child's stdio inherited so it owns the terminal, and returns
/// its exit code. The base URL is always loopback here, as in the shell: the
/// client runs on the same machine as the server.
pub fn claude(home: &Path, args: &[String]) -> i32 {
    let mut command = Command::new("claude");
    command
        .args(args)
        .env("ANTHROPIC_BASE_URL", format!("http://127.0.0.1:{}", port(home)))
        .env("ANTHROPIC_AUTH_TOKEN", api_key(home))
        .env("ANTHROPIC_MODEL", alias(home))
        .env("ANTHROPIC_SMALL_FAST_MODEL", alias(home))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    match command.status() {
        Ok(status) => status.code().unwrap_or(-1),
        // The shell's `command -v claude` guard, at the point where the
        // missing program actually shows: nothing on PATH to run.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("[ERROR] claude (Claude Code) is not installed");
            1
        }
        Err(e) => {
            eprintln!("[ERROR] cannot run claude: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-client-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }

    const DEFAULTS: &str = "# Point Claude Code at Golem:\n\
                            export ANTHROPIC_BASE_URL=\"http://127.0.0.1:8000\"\n\
                            export ANTHROPIC_AUTH_TOKEN=\"local-ai-key\"\n\
                            export ANTHROPIC_MODEL=\"local-model\"\n\
                            export ANTHROPIC_SMALL_FAST_MODEL=\"local-model\"\n";

    #[test]
    fn the_defaults_are_the_shells_text() {
        let dir = home("defaults");
        std::fs::write(dir.join(".env"), "").expect("write");
        assert_eq!(exports(&dir), DEFAULTS);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_home_with_no_env_at_all_still_answers() {
        let dir = home("no-env");
        assert_eq!(exports(&dir), DEFAULTS);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_exports_read_env_and_rewrite_the_wildcard_bind() {
        let dir = home("values");
        std::fs::write(
            dir.join(".env"),
            "LOCAL_AI_HOST=\"0.0.0.0\"\n\
             LOCAL_AI_PORT=\"8080\"\n\
             LOCAL_AI_API_KEY=\"secret\"\n\
             MODEL_ALIAS=\"ornith-1.5-35b\"\n",
        )
        .expect("write");
        assert_eq!(
            exports(&dir),
            "# Point Claude Code at Golem:\n\
             export ANTHROPIC_BASE_URL=\"http://127.0.0.1:8080\"\n\
             export ANTHROPIC_AUTH_TOKEN=\"secret\"\n\
             export ANTHROPIC_MODEL=\"ornith-1.5-35b\"\n\
             export ANTHROPIC_SMALL_FAST_MODEL=\"ornith-1.5-35b\"\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_real_host_is_kept_as_it_is() {
        let dir = home("real-host");
        std::fs::write(dir.join(".env"), "LOCAL_AI_HOST=\"192.168.1.5\"\n").expect("write");
        assert!(exports(&dir).contains("http://192.168.1.5:8000"), "{}", exports(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_powershell_block_is_the_same_four_values() {
        // `up` in Windows mode ends on the block `start-windows.ps1` prints,
        // which is the same four facts in the syntax the shell it was typed
        // in actually reads.
        let dir = home("powershell");
        std::fs::write(
            dir.join(".env"),
            "LOCAL_AI_HOST=\"0.0.0.0\"\nLOCAL_AI_PORT=\"8080\"\n\
             LOCAL_AI_API_KEY=\"secret\"\nMODEL_ALIAS=\"ornith-1.5-35b\"\n",
        )
        .expect("write");
        assert_eq!(
            powershell_exports(&dir),
            "# Point Claude Code at Golem:\n\
             $env:ANTHROPIC_BASE_URL = \"http://127.0.0.1:8080\"\n\
             $env:ANTHROPIC_AUTH_TOKEN = \"secret\"\n\
             $env:ANTHROPIC_MODEL = \"ornith-1.5-35b\"\n\
             $env:ANTHROPIC_SMALL_FAST_MODEL = \"ornith-1.5-35b\"\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_alias_is_model_alias_from_the_file() {
        // `GOLEM_MODEL_ALIAS` is a *process-environment* override (see
        // `state::override_value`), not a `.env` key: a `GOLEM_MODEL_ALIAS`
        // line in the file is ignored, and `MODEL_ALIAS` is what is read.
        let dir = home("alias");
        std::fs::write(dir.join(".env"), "MODEL_ALIAS=\"old\"\nGOLEM_MODEL_ALIAS=\"ignored\"\n")
            .expect("write");
        let text = exports(&dir);
        assert!(text.contains("export ANTHROPIC_MODEL=\"old\""), "{text}");
        assert!(text.contains("export ANTHROPIC_SMALL_FAST_MODEL=\"old\""), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
