//! `golem install`: build llama.cpp with CUDA or ROCm.
//!
//! The port of `scripts/install-llama-cpp.sh`, plus the two guards that live
//! in the `golem` script's `cmd_install`: this is native mode only. Windows
//! mode fetches a pinned release on the first `up` and docker mode pulls the
//! pinned image, so neither builds anything here - and saying so is the
//! point, because a silently ignored `install` looks like it worked.
//!
//! The output is the script's, word for word: the same `[INFO] ` / `[ OK ] `
//! / `[ERROR] ` prefixes, because that output is what the user reads while a
//! build runs for ten minutes. The children - git, cmake - inherit this
//! process's terminal instead of being captured, so the build is visible live
//! rather than looking like it hung.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::state;

/// The banner's rule, 60 `=` as in the script.
const RULE: &str = "============================================================";

/// Where llama.cpp is cloned and built, under the home.
const SRC_DIR: &str = ".local/src/llama.cpp";

/// Where the built binaries are installed, under the home.
const BIN_DIR: &str = ".local/bin";

/// The upstream the script clones from.
const REPO: &str = "https://github.com/ggml-org/llama.cpp.git";

/// The build tools the script refuses to go without, and what it says when
/// one is missing.
const REQUIREMENTS: [(&str, &str); 3] = [
    ("git", "git is required"),
    ("cmake", "cmake is required. Run setup-ai.sh first."),
    ("gcc", "gcc is required. Run setup-ai.sh first."),
];

/// The GPU backend a build targets.
///
/// Two, not three: `auto` is a way of choosing one of these, never a backend
/// of its own - by the time anything is built the detection has happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    Cuda,
    Rocm,
}

impl Backend {
    /// What the CMake flag turns on.
    fn cmake_flag(self) -> &'static str {
        match self {
            Backend::Cuda => "-DGGML_CUDA=ON",
            Backend::Rocm => "-DGGML_HIP=ON",
        }
    }

    /// The word a `--help` banner uses for it. A HIP build says HIP, not
    /// ROCm, which is why the verification greps a different word than the
    /// one this command's own lines print.
    fn help_word(self) -> &'static str {
        match self {
            Backend::Cuda => "cuda",
            Backend::Rocm => "hip",
        }
    }
}

impl std::fmt::Display for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Backend::Cuda => "cuda",
            Backend::Rocm => "rocm",
        })
    }
}

/// `golem install`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    // The script takes no arguments at all. The front door's mode flags never
    // arrive here - `main` pulls them out before dispatching - so anything
    // left is a mistake worth saying out loud rather than ignoring.
    if let Err(message) = parse_args(args) {
        eprintln!("[ERROR] {message}");
        return 1;
    }

    // The two mode guards come first, before anything is looked at or
    // created. In those modes there is no build to do, and the useful answer
    // is the one that names where it does happen.
    let mode = state::deploy_mode(home);
    if let Some(message) = mode_guard(&mode) {
        eprintln!("[ERROR] {message}");
        return 1;
    }

    let configured = env_or(home, "GOLEM_GPU_BACKEND", "GPU_BACKEND", "auto");
    let backend = match resolve_backend(&configured, which("nvcc"), which("hipcc")) {
        Ok(backend) => backend,
        Err(message) => {
            eprintln!("[ERROR] {message}");
            return 1;
        }
    };

    match install(home, backend) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("[ERROR] {message}");
            1
        }
    }
}

/// `install` takes no arguments; the first one is reported.
fn parse_args(args: &[String]) -> Result<(), String> {
    match args {
        [] => Ok(()),
        [other, ..] => Err(format!("unexpected argument: {other}")),
    }
}

/// Why this mode cannot install, or `None` when it can.
///
/// `cmd_install`'s two `die`s. `native` - and any mode a future `.env`
/// invents - falls through to the build, which is what the shell's two
/// `[[ ... ]] &&` guards do.
fn mode_guard(mode: &str) -> Option<&'static str> {
    match mode {
        "docker" => Some("install is native-only; in docker mode use: ./golem up --docker (pulls the image)"),
        "windows" => Some("windows mode installs the pinned llama.cpp build on first ./golem up"),
        _ => None,
    }
}

/// The backend to build, from `GPU_BACKEND` and the toolchain that is here.
///
/// The tools are booleans rather than a lookup so the decision itself can be
/// tested without a GPU: `nvcc` wins an `auto` race, as the script's `if`
/// does, and an explicit value is never second-guessed - someone who set
/// `cuda` on a machine with only `hipcc` gets a CUDA build that fails in
/// cmake, not a silent switch.
fn resolve_backend(configured: &str, nvcc: bool, hipcc: bool) -> Result<Backend, String> {
    match configured {
        "auto" => {
            if nvcc {
                Ok(Backend::Cuda)
            } else if hipcc {
                Ok(Backend::Rocm)
            } else {
                Err(
                    "no CUDA (nvcc) or ROCm (hipcc) toolchain found; install one or set GPU_BACKEND in .env"
                        .to_string(),
                )
            }
        }
        "cuda" => Ok(Backend::Cuda),
        "rocm" => Ok(Backend::Rocm),
        other => Err(format!("GPU_BACKEND must be auto, cuda or rocm (got: {other})")),
    }
}

/// The CMake arguments that configure the build.
///
/// `AMDGPU_TARGETS` pins the ROCm target (e.g. `gfx1100`); an empty one is
/// left out so the toolkit builds for the GPU it detects. The script builds
/// the flag as one string and lets the shell split it, which is the same
/// argument list as this.
fn configure_args(backend: Backend, amdgpu_targets: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> =
        vec!["-S".to_string(), ".".to_string(), "-B".to_string(), "build".to_string()];
    args.push(backend.cmake_flag().to_string());
    if backend == Backend::Rocm {
        if let Some(targets) = amdgpu_targets.filter(|targets| !targets.is_empty()) {
            args.push(format!("-DAMDGPU_TARGETS={targets}"));
        }
    }
    args.push("-DCMAKE_BUILD_TYPE=Release".to_string());
    args.push("-DLLAMA_CURL=ON".to_string());
    args
}

/// The build itself, in the order the script does it.
fn install(home: &Path, backend: Backend) -> Result<(), String> {
    let src = home.join(SRC_DIR);
    let bin = home.join(BIN_DIR);

    println!();
    println!("{RULE}");
    println!(" LOCAL AI - LLAMA.CPP INSTALLATION");
    println!("{RULE}");
    println!();

    create_dir(&bin)?;
    if let Some(parent) = src.parent() {
        create_dir(parent)?;
    }

    for (tool, message) in REQUIREMENTS {
        if !which(tool) {
            return Err(message.to_string());
        }
    }

    // Advisory, not fatal: the CUDA toolkit is sometimes exposed through the
    // build environment without `nvcc` on PATH, and the script carries on.
    if backend == Backend::Cuda && !which("nvcc") {
        log("nvcc is not installed.");
        log("This is acceptable if the CUDA toolkit is exposed through the build environment.");
    }

    if !src.join(".git").is_dir() {
        log("Cloning llama.cpp...");
        let mut clone = Command::new("git");
        clone.args(["clone", "--depth", "1", REPO]).arg(&src).current_dir(home);
        run_step(clone, "git clone")?;
    } else {
        log("Updating existing llama.cpp checkout...");
        let mut pull = Command::new("git");
        pull.arg("-C").arg(&src).args(["pull", "--ff-only"]);
        run_step(pull, "git pull")?;
    }

    // A stale build directory is the script's `rm -rf build`: cmake would
    // otherwise keep whatever the last configuration left in the cache.
    std::fs::remove_dir_all(src.join("build")).ok();

    log(&format!("Configuring {backend} build..."));
    let mut configure = Command::new("cmake");
    configure
        .args(configure_args(backend, state::env_value(home, "AMDGPU_TARGETS").as_deref()))
        .current_dir(&src);
    run_step(configure, "cmake configure")?;

    // The script's `nproc`.
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    log(&format!("Building llama.cpp ({backend})..."));
    let mut build = Command::new("cmake");
    build
        .args(["--build", "build", "--config", "Release", "--parallel"])
        .arg(cores.to_string())
        .current_dir(&src);
    run_step(build, "cmake --build")?;

    let built_server = src.join("build/bin/llama-server");
    if !built_server.is_file() {
        return Err("llama-server was not built successfully".to_string());
    }

    // The CLI is copied only when the build produced one; the server is the
    // one binary the launchers need.
    let built_cli = src.join("build/bin/llama-cli");
    if built_cli.is_file() {
        install_binary(&built_cli, &bin.join("llama-cli"))?;
    }
    let server = bin.join("llama-server");
    install_binary(&built_server, &server)?;

    ok("llama-server installed");
    ok(&format!("Binary: {}", server.display()));

    println!();
    // Best effort, as the script's `|| true`: a binary that cannot answer
    // `--version` is still installed, and its own output says why.
    let _ = Command::new(&server)
        .arg("--version")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    println!();

    log(&format!("Checking {backend} support..."));

    // CUDA builds mention CUDA in `--help`; HIP builds say HIP, not ROCm.
    // Both streams are searched, which is the script's `2>&1`.
    let help = Command::new(&server).arg("--help").stdin(Stdio::null()).output();
    let found = help
        .map(|out| {
            let mut text = String::from_utf8_lossy(&out.stdout).to_lowercase();
            text.push_str(&String::from_utf8_lossy(&out.stderr).to_lowercase());
            text.contains(backend.help_word())
        })
        .unwrap_or(false);
    if found {
        ok(&format!("{backend}-related support detected in llama-server"));
    } else {
        log(&format!("Could not verify {backend} support from --help."));
        log(&format!("The binary was built with the {backend} backend; runtime validation comes next."));
    }

    println!();
    println!("Installation complete.");
    println!();
    Ok(())
}

/// `[INFO] `, to stdout.
fn log(message: &str) {
    println!("[INFO] {message}");
}

/// `[ OK ] `, to stdout.
fn ok(message: &str) {
    println!("[ OK ] {message}");
}

/// Create `dir` and everything above it, reporting which one failed.
fn create_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

/// Copy a built binary into place, executable.
///
/// `std::fs::copy` carries the source's permission bits, so the executable
/// the build produced stays executable; the explicit mode is for the copy
/// that lands on a filesystem where that did not hold.
fn install_binary(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::copy(from, to)
        .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(to, std::fs::Permissions::from_mode(0o755));
    }
    Ok(())
}

/// Run a prepared command with this terminal inherited.
///
/// Inherited stdio is the point: a cmake configure or a llama.cpp build runs
/// for minutes, and watching it happen is how the user knows it is not stuck.
/// A non-zero exit is an error the caller's `[ERROR]` line carries.
fn run_step(mut command: Command, what: &str) -> Result<(), String> {
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| format!("cannot run {what}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{what} failed ({})", code_text(status)))
    }
}

/// The exit code as a word for an error line: a process killed by a signal
/// has no code, and `unwrap()` on `code()` would be the bug.
fn code_text(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// The shell's `${GOLEM_X:-${X:-default}}`: the per-invocation override, then
/// `.env`, then the default - and an empty value counts as unset, because the
/// shell's `:-` treats an empty assignment the same as an absent one.
fn env_or(home: &Path, override_key: &str, key: &str, default: &str) -> String {
    state::override_value(home, override_key, key)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Whether `tool` is on PATH - the shell's `command -v`.
///
/// The PATH walk itself rather than `where`/`which`: those are not the same
/// tool on the two platforms, and a build machine's `which` may not be
/// installed at all. Windows resolves a bare name through PATHEXT, which is
/// why the extensions are tried after it.
fn which(tool: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_lowercase)
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(&path) {
        if dir.join(tool).is_file() {
            return true;
        }
        for extension in &extensions {
            if dir.join(format!("{tool}{extension}")).is_file() {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-install-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }

    #[test]
    fn install_takes_no_arguments() {
        assert!(parse_args(&[]).is_ok());
        let err = parse_args(&["--native".to_string()]).expect_err("a flag is not ignored");
        assert_eq!(err, "unexpected argument: --native");
        let err = parse_args(&["a".to_string(), "b".to_string()]).expect_err("the first is named");
        assert_eq!(err, "unexpected argument: a");
    }

    #[test]
    fn auto_picks_the_toolchain_that_is_installed() {
        assert_eq!(resolve_backend("auto", true, false), Ok(Backend::Cuda));
        assert_eq!(resolve_backend("auto", false, true), Ok(Backend::Rocm));
        // nvcc wins the race, as the script's `if`/`elif` does.
        assert_eq!(resolve_backend("auto", true, true), Ok(Backend::Cuda));
        let err = resolve_backend("auto", false, false).expect_err("neither toolchain");
        assert_eq!(
            err,
            "no CUDA (nvcc) or ROCm (hipcc) toolchain found; install one or set GPU_BACKEND in .env"
        );
    }

    #[test]
    fn an_explicit_backend_is_taken_as_given() {
        // No second-guessing: `cuda` with only hipcc present is a CUDA build
        // that cmake will reject, not a silent switch to ROCm.
        assert_eq!(resolve_backend("cuda", false, true), Ok(Backend::Cuda));
        assert_eq!(resolve_backend("rocm", true, false), Ok(Backend::Rocm));
        let err = resolve_backend("vulkan", true, true).expect_err("not a backend");
        assert_eq!(err, "GPU_BACKEND must be auto, cuda or rocm (got: vulkan)");
    }

    #[test]
    fn the_mode_guards_say_where_install_happens_instead() {
        assert_eq!(
            mode_guard("docker"),
            Some("install is native-only; in docker mode use: ./golem up --docker (pulls the image)")
        );
        assert_eq!(
            mode_guard("windows"),
            Some("windows mode installs the pinned llama.cpp build on first ./golem up")
        );
        // native - and anything a future .env invents - falls through to the
        // build, which is what the shell's two guards do.
        assert_eq!(mode_guard("native"), None);
        assert_eq!(mode_guard(""), None);
        assert_eq!(mode_guard("weird"), None);
    }

    #[test]
    fn a_missing_requirement_keeps_the_scripts_message() {
        for (tool, message) in REQUIREMENTS {
            match tool {
                "git" => assert_eq!(message, "git is required"),
                _ => assert_eq!(message, format!("{tool} is required. Run setup-ai.sh first.")),
            }
        }
    }

    #[test]
    fn the_cmake_arguments_are_the_scripts() {
        assert_eq!(
            configure_args(Backend::Cuda, None),
            vec![
                "-S",
                ".",
                "-B",
                "build",
                "-DGGML_CUDA=ON",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DLLAMA_CURL=ON"
            ]
        );
        // ROCm without a pinned target: the toolkit builds for the GPU it
        // detects, so the flag is absent rather than empty.
        assert_eq!(
            configure_args(Backend::Rocm, None),
            vec![
                "-S",
                ".",
                "-B",
                "build",
                "-DGGML_HIP=ON",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DLLAMA_CURL=ON"
            ]
        );
        assert_eq!(
            configure_args(Backend::Rocm, Some("gfx1100")),
            vec![
                "-S",
                ".",
                "-B",
                "build",
                "-DGGML_HIP=ON",
                "-DAMDGPU_TARGETS=gfx1100",
                "-DCMAKE_BUILD_TYPE=Release",
                "-DLLAMA_CURL=ON"
            ]
        );
        // An empty value is the same as none, and a CUDA build never takes
        // the ROCm-only flag even when the key is set.
        assert_eq!(configure_args(Backend::Rocm, Some("")), configure_args(Backend::Rocm, None));
        assert!(!configure_args(Backend::Cuda, Some("gfx1100")).iter().any(|a| a.contains("AMDGPU")));
    }

    #[test]
    fn the_help_word_is_not_always_the_backends_name() {
        // A HIP build says HIP, not ROCm - this is the grep the script runs.
        assert_eq!(Backend::Cuda.help_word(), "cuda");
        assert_eq!(Backend::Rocm.help_word(), "hip");
        assert_eq!(Backend::Rocm.to_string(), "rocm");
    }

    #[test]
    fn an_empty_setting_falls_back_to_the_default() {
        let dir = home("settings");
        std::fs::write(dir.join(".env"), "GPU_BACKEND=\"rocm\"\nAMDGPU_TARGETS=\n").expect("write");
        assert_eq!(env_or(&dir, "GOLEM_GPU_BACKEND", "GPU_BACKEND", "auto"), "rocm");
        // The shell's `:-` treats an empty assignment as unset.
        assert_eq!(env_or(&dir, "GOLEM_GPU_BACKEND", "AMDGPU_TARGETS", "none"), "none");
        assert_eq!(env_or(&dir, "GOLEM_GPU_BACKEND", "MISSING", "auto"), "auto");
        std::fs::remove_dir_all(&dir).ok();
    }

    // The deploy-mode rule itself is tested where it lives now
    // (`state::deploy_mode`): `install` and `up` must not disagree about the
    // mode they are running in, so there is one reader for both.
}
