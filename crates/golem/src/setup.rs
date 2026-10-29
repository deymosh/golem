//! `golem setup`: inspect the machine and get it ready to build and serve.
//!
//! A section report, two guards (not root, Linux only), the package install
//! and the directory scaffolding, on Linux (native, WSL2, or a
//! GPU-passthrough VM) - no `/etc/os-release` means no report. On Windows it
//! is the app's Start-menu and desktop shortcuts ([`crate::shortcut`]).
//!
//! Everything that reads the machine is best effort: a tool that is not
//! installed produces a warning, never a panic and never a crash. The one
//! long-running child - the package manager - inherits this terminal so the
//! install is visible while it happens.
//!
//! Every line goes to `logs/setup-<stamp>.log` as well as the terminal
//! ([`Report`]).

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use golem_core::time;

/// The banner rule, 60 `=`.
const RULE: &str = "============================================================";

// The report's colours, enabled only when stdout is a terminal.
const RESET: &str = "\x1b[0m";
const RED: &str = "\x1b[0;31m";
const GREEN: &str = "\x1b[0;32m";
const YELLOW: &str = "\x1b[1;33m";
const BLUE: &str = "\x1b[0;34m";
const CYAN: &str = "\x1b[0;36m";

/// The Debian/Ubuntu build dependencies, for `apt-get`.
const APT_PACKAGES: [&str; 9] = [
    "build-essential",
    "cmake",
    "git",
    "pkg-config",
    "curl",
    "wget",
    "ca-certificates",
    "unzip",
    "jq",
];

/// The Fedora/RHEL build dependencies, for `dnf`.
const DNF_PACKAGES: [&str; 11] = [
    "gcc",
    "gcc-c++",
    "make",
    "cmake",
    "git",
    "pkgconfig",
    "curl",
    "wget",
    "ca-certificates",
    "unzip",
    "jq",
];

/// The Arch build dependencies, for `pacman`.
const PACMAN_PACKAGES: [&str; 9] = [
    "base-devel",
    "cmake",
    "git",
    "pkgconf",
    "curl",
    "wget",
    "ca-certificates",
    "unzip",
    "jq",
];

/// What `--features desktop` builds against - the webview, the tray and
/// its menu (WebKitGTK, an appindicator, libxdo) - per package manager.
/// Installed only on a graphical session: a server has no window to open.
const APT_DESKTOP: [&str; 3] = ["libwebkit2gtk-4.1-dev", "libayatana-appindicator3-dev", "libxdo-dev"];
const DNF_DESKTOP: [&str; 3] = ["webkit2gtk4.1-devel", "libappindicator-gtk3-devel", "libxdo-devel"];
const PACMAN_DESKTOP: [&str; 3] = ["webkit2gtk-4.1", "libayatana-appindicator", "xdotool"];

/// Where the report looks for `libcuda.so`, in order.
const SEARCH_PATHS: [&str; 6] = [
    "/usr/lib",
    "/usr/lib64",
    "/usr/local/cuda/lib64",
    "/usr/local/cuda/lib",
    "/usr/lib/x86_64-linux-gnu",
    "/usr/lib/wsl/lib",
];

/// The image the docker GPU test runs.
const CUDA_IMAGE: &str = "nvidia/cuda:13.0.0-base-ubuntu24.04";

/// The tools the toolchain section reports on, in order.
const TOOLCHAIN: [&str; 7] = ["gcc", "g++", "make", "cmake", "git", "curl", "wget"];

/// `golem setup`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    // `setup` takes no arguments; a stray one is a mistake worth saying out
    // loud rather than ignoring, as `install` does too.
    if let Err(message) = parse_args(args) {
        eprintln!("[ERROR] {message}");
        return 1;
    }
    // Windows has no package to install and no distribution to report on:
    // getting it ready is the app's shortcuts.
    if cfg!(windows) {
        return crate::shortcut::create(&mut std::io::stdout());
    }

    let logs_dir = home.join("logs");
    let log_path = logs_dir.join(format!("setup-{}.log", stamp()));
    // The log's directory first, so the log can open.
    let _ =std::fs::create_dir_all(&logs_dir);
    let report = Report::new(&log_path);

    // ---- header ----------------------------------------------------------
    report.section("LOCAL AI / LINUX SETUP");
    report.line(&format!("Script       : {}", script_name()));
    report.line(&format!("Project root : {}", home.display()));
    report.line(&format!("Log          : {}", log_path.display()));
    report.line(&format!("Date         : {}", date_line()));
    report.line(&format!("User         : {}", user_name()));
    report.blank();

    // ---- 1. privileges ---------------------------------------------------
    report.section("1. PRIVILEGES");
    if is_root() {
        report.error("Do not run this script as root.");
        report.blank();
        report.line("Run it as your normal Linux user:");
        report.blank();
        report.line("  golem setup");
        return 1;
    }
    report.ok(&format!("Running as user: {}", user_name()));
    if which("sudo") {
        report.ok("sudo is available");
    } else {
        report.warn("sudo is not available");
        report.warn("Package installation may require manual setup");
    }

    // ---- 2. operating system --------------------------------------------
    report.section("2. OPERATING SYSTEM");
    let os_release = Path::new("/etc/os-release");
    if !os_release.is_file() {
        report.error("Cannot identify Linux distribution.");
        return 1;
    }
    let vars = std::fs::read_to_string(os_release)
        .map(|text| parse_os_release(&text))
        .unwrap_or_default();
    let os_field = |key: &str| vars.get(key).cloned().unwrap_or_else(|| "unknown".to_string());
    report.line(&format!("Distribution : {}", os_field("PRETTY_NAME")));
    report.line(&format!("ID           : {}", os_field("ID")));
    report.line(&format!("Version      : {}", os_field("VERSION_ID")));
    report.line(&format!("Kernel       : {}", capture("uname", &["-r"]).unwrap_or_default().trim()));
    report.line(&format!("Architecture : {}", capture("uname", &["-m"]).unwrap_or_default().trim()));
    let ostype = ostype();
    if ostype.starts_with("linux-gnu") {
        report.ok("Linux detected");
    } else {
        let shown = if ostype.is_empty() { "unknown" } else { ostype.as_str() };
        report.warn(&format!("Unexpected OSTYPE: {shown}"));
    }

    // ---- 3. cpu ----------------------------------------------------------
    report.section("3. CPU");
    let mut cpu_model: Option<String> = None;
    let mut cpu_cores: Option<String> = None;
    if which("lscpu") {
        cpu_model = capture("lscpu", &[]).and_then(|text| lscpu_model(&text));
        cpu_cores = capture("nproc", &[]).map(|out| out.trim().to_string()).filter(|s| !s.is_empty());
        report.line(&format!("CPU model : {}", cpu_model.clone().unwrap_or_else(|| "unknown".to_string())));
        report.line(&format!("Threads   : {}", cpu_cores.clone().unwrap_or_else(|| "unknown".to_string())));
    } else {
        report.warn("lscpu is not available.");
    }

    // ---- 4. memory -------------------------------------------------------
    report.section("4. MEMORY");
    show(&report, "free", &["-h"]);
    let total_mem_gb = meminfo_kb("MemTotal").map(|kb| kb / 1024 / 1024).unwrap_or(0);
    report.blank();
    report.line(&format!("System memory: approximately {total_mem_gb} GB"));
    match ram_verdict(total_mem_gb) {
        (Verdict::Ok, message) => report.ok(message),
        (Verdict::Warn, message) => report.warn(message),
    }

    // ---- 5. nvidia gpu ---------------------------------------------------
    report.section("5. NVIDIA GPU");
    let nvidia_available = which("nvidia-smi");
    if nvidia_available {
        report.ok("nvidia-smi detected");
        report.blank();
        show(&report, "nvidia-smi", &[]);
        report.blank();
        report.info("GPU summary:");
        if let Some(out) = nvidia_query("name,memory.total,memory.used,memory.free,driver_version") {
            for line in out.lines() {
                report.line(line);
            }
        }
    } else {
        report.warn("nvidia-smi is not available.");
        report.warn("No NVIDIA GPU was detected through the standard NVIDIA interface.");
        // The card may still be there and readable without NVIDIA's tool:
        // the machine probe reads the registry on Windows and sysfs on Linux.
        if let Some(gpu) = crate::machine::gpu() {
            report.info(&format!("GPU detected: {} ({} MiB VRAM)", gpu.name, gpu.total_mib));
        }
    }

    // ---- 6. gpu devices --------------------------------------------------
    report.section("6. GPU DEVICES");
    let nodes = nvidia_device_nodes();
    if !nodes.is_empty() {
        report.ok("NVIDIA device nodes detected");
        let mut ls = Command::new("ls");
        ls.args(["-lah"]).args(&nodes);
        show_command(&report, &mut ls);
    } else {
        report.info("No /dev/nvidia* device nodes detected.");
        if nvidia_available {
            report.info("nvidia-smi still reports a working GPU.");
        }
    }

    // ---- 7. cuda ---------------------------------------------------------
    report.section("7. CUDA");
    if which("nvcc") {
        report.ok("CUDA compiler detected");
        show(&report, "nvcc", &["--version"]);
    } else {
        report.info("nvcc is not installed.");
    }
    report.blank();
    if let Ok(cuda_home) = std::env::var("CUDA_HOME") {
        if !cuda_home.is_empty() {
            report.line(&format!("CUDA_HOME: {cuda_home}"));
        }
    }
    if Path::new("/usr/local/cuda").is_dir() {
        report.ok("/usr/local/cuda exists");
        show(&report, "ls", &["-ld", "/usr/local/cuda"]);
    }
    report.blank();
    report.info("CUDA-related environment variables:");
    let mut cuda_vars: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| key.starts_with("CUDA_") || key.starts_with("NVIDIA_"))
        .collect();
    cuda_vars.sort();
    for (key, value) in cuda_vars {
        report.line(&format!("{key}={value}"));
    }

    // ---- 8. nvidia libraries --------------------------------------------
    report.section("8. NVIDIA LIBRARIES");
    let mut libcuda_found = false;
    for path in SEARCH_PATHS {
        let dir = Path::new(path);
        if !dir.is_dir() {
            continue;
        }
        let libs = libcuda_files(dir);
        if libs.is_empty() {
            continue;
        }
        libcuda_found = true;
        report.ok(&format!("CUDA driver library found in: {path}"));
        let mut ls = Command::new("ls");
        ls.args(["-lah"]).args(&libs);
        show_command(&report, &mut ls);
    }
    if !libcuda_found {
        report.warn("libcuda.so was not found in standard locations.");
    }

    // ---- 9. build dependencies ------------------------------------------
    report.section("9. BUILD DEPENDENCIES");
    let graphical = crate::desktop::graphical_session();
    let desktop = |packages: &'static [&'static str]| -> &'static [&'static str] {
        if graphical { packages } else { &[] }
    };
    if graphical {
        report.info("Graphical session: the app's window and tray libraries too.");
    }
    match package_manager(which("apt-get"), which("dnf"), which("pacman")) {
        Some(Manager::Apt) => {
            report.info("Debian/Ubuntu package manager detected.");
            report.run_visible(Command::new("sudo").args(["apt-get", "update"]), "apt-get update");
            let mut install = Command::new("sudo");
            install
                .args(["apt-get", "install", "-y"])
                .args(APT_PACKAGES)
                .args(desktop(&APT_DESKTOP))
                .env("DEBIAN_FRONTEND", "noninteractive");
            report.run_visible(&mut install, "apt-get install");
        }
        Some(Manager::Dnf) => {
            report.info("DNF package manager detected.");
            let mut install = Command::new("sudo");
            install.args(["dnf", "install", "-y"]).args(DNF_PACKAGES).args(desktop(&DNF_DESKTOP));
            report.run_visible(&mut install, "dnf install");
        }
        Some(Manager::Pacman) => {
            report.info("Pacman package manager detected.");
            let mut install = Command::new("sudo");
            install
                .args(["pacman", "-Sy", "--needed", "--noconfirm"])
                .args(PACMAN_PACKAGES)
                .args(desktop(&PACMAN_DESKTOP));
            report.run_visible(&mut install, "pacman -Sy");
        }
        None => {
            report.warn("No supported package manager detected.");
            report.warn("Install the build dependencies manually.");
        }
    }

    // ---- 10. toolchain ---------------------------------------------------
    report.section("10. TOOLCHAIN");
    for tool in TOOLCHAIN {
        if which(tool) {
            let out = capture(tool, &["--version"]).unwrap_or_default();
            report.line(&toolchain_line(tool, &out));
        } else {
            report.warn(&format!("{tool} is not installed."));
        }
    }

    // ---- 11. docker ------------------------------------------------------
    report.section("11. DOCKER");
    let docker_available = which("docker");
    if docker_available {
        report.ok("Docker detected");
        if let Some(out) = capture("docker", &["--version"]) {
            for line in out.lines() {
                report.line(line);
            }
        }
        if let Some(out) = capture("docker", &["compose", "version"]) {
            for line in out.lines() {
                report.line(line);
            }
            report.ok("Docker Compose available");
        } else {
            report.warn("Docker Compose is not available.");
        }
        if docker_daemon_ok() {
            report.ok("Docker daemon is accessible");
        } else {
            report.warn("Docker daemon is not accessible.");
        }
    } else {
        report.info("Docker is not installed.");
    }

    // ---- 12. docker -> nvidia gpu ---------------------------------------
    report.section("12. DOCKER -> NVIDIA GPU");
    if docker_available && docker_daemon_ok() && nvidia_available {
        report.info("Testing Docker GPU passthrough...");
        report.info(&format!("Image: {CUDA_IMAGE}"));
        if !succeeds("docker", &["image", "inspect", CUDA_IMAGE]) {
            report.info("Pulling CUDA test image...");
            if let Some(out) = output_of("docker", &["pull", CUDA_IMAGE]) {
                print_stream(&report, &out.stdout);
                print_stream(&report, &out.stderr);
            }
        }
        let mut run = Command::new("docker");
        run.args(["run", "--rm", "--gpus", "all", CUDA_IMAGE, "nvidia-smi"]);
        let ok = show_command(&report, &mut run);
        if ok {
            report.ok("Docker -> NVIDIA GPU access works.");
        } else {
            report.warn("Docker GPU access failed.");
        }
    } else {
        report.info("Docker GPU test skipped.");
    }

    // ---- 13. storage -----------------------------------------------------
    report.section("13. STORAGE");
    let root = home.display().to_string();
    show(&report, "df", &["-h", root.as_str()]);
    report.blank();
    // Printed only when `bc` is installed, although the division is done
    // here and needs no `bc`; with no `bc` the line is left out.
    if which("bc") {
        let available = df_available_kb(home)
            .map(|kb| (kb / 1024 / 1024).to_string())
            .unwrap_or_else(|| "unknown".to_string());
        report.line(&format!("Available project storage: approximately {available} GB"));
    }

    // ---- 14. local ai directories ---------------------------------------
    report.section("14. LOCAL AI DIRECTORIES");
    let data_dir = home.join("data");
    let models_dir = data_dir.join("models");
    let cache_dir = data_dir.join("cache");
    let config_dir = home.join("config");
    let local_dir = home.join(".local");
    for dir in [&models_dir, &cache_dir, &config_dir, &local_dir, &logs_dir] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            report.warn(&format!("cannot create {}: {e}", dir.display()));
        }
    }
    report.ok(&format!("Project data directory : {}", data_dir.display()));
    report.ok(&format!("Models directory       : {}", models_dir.display()));
    report.ok(&format!("Cache directory        : {}", cache_dir.display()));
    report.ok(&format!("Config directory       : {}", config_dir.display()));
    report.ok(&format!("Local build/state dir  : {}", local_dir.display()));
    report.ok(&format!("Logs directory         : {}", logs_dir.display()));
    if !home.join("golem.toml").is_file() {
        report.warn("No golem.toml yet - the binary writes one beside itself on first run:");
        report.line("    golem up   (or golem install, which only fetches the engine)");
    }
    // A desktop gets the app in its applications menu.
    if graphical {
        let mut made = Vec::new();
        crate::shortcut::create(&mut made);
        for line in String::from_utf8_lossy(&made).lines() {
            report.line(line);
        }
    }

    // ---- 15. git ---------------------------------------------------------
    report.section("15. GIT");
    if which("git") {
        report.line("Git version:");
        show(&report, "git", &["--version"]);
        report.blank();
        if let Some(name) = capture("git", &["config", "--global", "user.name"]) {
            report.line(&format!("User name : {}", name.trim()));
        } else {
            report.info("Global git user.name is not configured.");
        }
        if let Some(email) = capture("git", &["config", "--global", "user.email"]) {
            report.line(&format!("Email     : {}", email.trim()));
        } else {
            report.info("Global git user.email is not configured.");
        }
    }

    // ---- 16. gpu baseline ------------------------------------------------
    report.section("16. GPU BASELINE");
    if nvidia_available {
        if let Some(out) =
            nvidia_query("name,memory.total,memory.used,memory.free,utilization.gpu,temperature.gpu")
        {
            for line in out.lines() {
                report.line(line);
            }
        }
    } else {
        report.warn("No NVIDIA GPU information available.");
    }

    // ---- summary ---------------------------------------------------------
    report.section("SETUP SUMMARY");
    report.line("Operating system:");
    report.line(&format!("  {}", os_field("PRETTY_NAME")));
    report.blank();
    report.line("CPU:");
    report.line(&format!("  {}", cpu_model.unwrap_or_else(|| "unknown".to_string())));
    report.line(&format!("  {} threads", cpu_cores.unwrap_or_else(|| "unknown".to_string())));
    report.blank();
    report.line("Memory:");
    report.line(&format!("  ~{total_mem_gb} GB"));
    report.blank();
    report.line("GPU:");
    if nvidia_available {
        let name = nvidia_first("name");
        let vram = nvidia_first("memory.total");
        let driver = nvidia_first("driver_version");
        for line in gpu_summary_lines(&name, &vram, &driver) {
            report.line(&line);
        }
    } else {
        report.line("  NVIDIA GPU   : not detected");
    }
    report.blank();
    report.line("CUDA:");
    report.line(&format!("  nvcc         : {}", nvcc_summary_line()));
    report.blank();
    report.line("Docker:");
    if docker_available {
        report.line("  Available    : yes");
    } else {
        report.line("  Available    : no");
    }
    report.blank();
    report.line("Project:");
    report.line(&format!("  Root         : {}", home.display()));
    report.line(&format!("  Models       : {}", models_dir.display()));
    report.line(&format!("  Cache        : {}", cache_dir.display()));
    report.line(&format!("  Config       : {}", config_dir.display()));
    report.line(&format!("  Logs         : {}", logs_dir.display()));
    report.blank();
    report.line("Log file:");
    report.line(&format!("  {}", log_path.display()));
    report.blank();
    report.line("Next steps:");
    report.line("  1. golem install        (the pinned engine build, ~600 MB)");
    report.line("  2. golem pull           (the model's GGUF, ~20 GB)");
    report.line("  3. golem up             (start it; the model loads on first request)");
    report.line("  Settings live in golem.toml beside the binary; see README.md and");
    report.line("  docs/ for pointing Claude Code at the stack.");
    report.complete();

    0
}

/// `setup` takes no arguments; the first one is reported.
fn parse_args(args: &[String]) -> Result<(), String> {
    match args {
        [] => Ok(()),
        [other, ..] => Err(format!("unexpected argument: {other}")),
    }
}

// ---- the report ----------------------------------------------------------

/// A line sink: every line goes to the terminal and is appended to the log.
struct Report {
    log: Option<std::fs::File>,
    color: bool,
}

impl Report {
    /// A reporter whose log is `log`. A log that cannot be opened - a
    /// read-only home - is not fatal: the report still prints.
    fn new(log: &Path) -> Report {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(log).ok();
        Report { log, color: std::io::stdout().is_terminal() }
    }

    /// One line, to the terminal and the log.
    fn line(&self, text: &str) {
        println!("{text}");
        if let Some(mut file) = self.log.as_ref() {
            let _ = writeln!(file, "{text}");
        }
    }

    /// An empty line.
    fn blank(&self) {
        self.line("");
    }

    /// A colour, applied only on a terminal.
    fn painted(&self, color: &str, text: &str) -> String {
        if self.color {
            format!("{color}{text}{RESET}")
        } else {
            text.to_string()
        }
    }

    fn info(&self, message: &str) {
        self.line(&format!("{} {message}", self.painted(BLUE, "[INFO]")));
    }

    fn ok(&self, message: &str) {
        self.line(&format!("{} {message}", self.painted(GREEN, "[ OK ]")));
    }

    fn warn(&self, message: &str) {
        self.line(&format!("{} {message}", self.painted(YELLOW, "[WARN]")));
    }

    fn error(&self, message: &str) {
        self.line(&format!("{} {message}", self.painted(RED, "[ERROR]")));
    }

    /// A section heading: a blank line, a rule, the title, another rule, a
    /// blank line.
    fn section(&self, title: &str) {
        self.blank();
        self.line(&self.painted(CYAN, RULE));
        self.line(&self.painted(CYAN, &format!(" {title}")));
        self.line(&self.painted(CYAN, RULE));
        self.blank();
    }

    /// The closing green banner.
    fn complete(&self) {
        self.blank();
        self.line(&self.painted(GREEN, RULE));
        self.line(&self.painted(GREEN, " LINUX AI ENVIRONMENT CHECK COMPLETE"));
        self.line(&self.painted(GREEN, RULE));
    }

    /// Run a prepared command with this terminal inherited, so a package
    /// manager writes straight to the screen. A non-zero
    /// exit is reported and the report carries on: the rest of it is still
    /// worth printing.
    fn run_visible(&self, command: &mut Command, what: &str) {
        match command
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => self.warn(&format!("{what} exited {}", code_text(status))),
            Err(e) => self.warn(&format!("cannot run {what}: {e}")),
        }
    }
}

// ---- running things ------------------------------------------------------

/// Run `program` with `args`, capturing both streams. `None` when it cannot
/// be started at all (not installed).
fn output_of(program: &str, args: &[&str]) -> Option<std::process::Output> {
    Command::new(program).args(args).output().ok()
}

/// stdout of a command that succeeded, as text. `None` on a spawn failure or
/// a non-zero exit.
fn capture(program: &str, args: &[&str]) -> Option<String> {
    let out = output_of(program, args)?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a command succeeded, its output unwanted.
fn succeeds(program: &str, args: &[&str]) -> bool {
    output_of(program, args).map(|out| out.status.success()).unwrap_or(false)
}

/// Run a command and show both of its streams through the report, so they
/// reach the log too. Returns whether it succeeded.
fn show(report: &Report, program: &str, args: &[&str]) -> bool {
    match output_of(program, args) {
        Some(out) => {
            print_stream(report, &out.stdout);
            print_stream(report, &out.stderr);
            out.status.success()
        }
        None => false,
    }
}

/// The same, for a `Command` built by the caller (its arguments are not
/// `&str`s).
fn show_command(report: &Report, command: &mut Command) -> bool {
    match command.output() {
        Ok(out) => {
            print_stream(report, &out.stdout);
            print_stream(report, &out.stderr);
            out.status.success()
        }
        Err(_) => false,
    }
}

/// One captured stream's lines, each through the reporter.
fn print_stream(report: &Report, bytes: &[u8]) {
    let text = String::from_utf8_lossy(bytes);
    for line in text.lines() {
        report.line(line);
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

/// Where `program` is on PATH, if it is.
///
/// The PATH walk itself rather than `which`/`where`, which are not the same
/// tool on the two platforms; Windows resolves a bare name through PATHEXT,
/// which is why the extensions are tried after it. Mirrors `install`'s.
fn which_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
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
        let candidate = dir.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
        for extension in &extensions {
            let candidate = dir.join(format!("{program}{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Whether `program` is on PATH.
fn which(program: &str) -> bool {
    which_path(program).is_some()
}

/// The first line of an `nvidia-smi --query-gpu=<fields>` answer, or empty.
fn nvidia_first(fields: &str) -> String {
    nvidia_query(fields)
        .and_then(|out| out.lines().next().map(|line| line.trim().to_string()))
        .unwrap_or_default()
}

/// `nvidia-smi --query-gpu=<fields> --format=csv,noheader`, the whole stdout.
/// `None` when the tool is missing or answers with a failure.
fn nvidia_query(fields: &str) -> Option<String> {
    let query = format!("--query-gpu={fields}");
    capture("nvidia-smi", &[query.as_str(), "--format=csv,noheader"])
}

/// Whether the docker daemon answers `docker info`.
fn docker_daemon_ok() -> bool {
    succeeds("docker", &["info"])
}

/// The `/dev/nvidia*` device nodes, sorted.
fn nvidia_device_nodes() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else { return Vec::new() };
    let mut nodes: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().and_then(|name| name.to_str()).map(|n| n.starts_with("nvidia")).unwrap_or(false)
        })
        .collect();
    nodes.sort();
    nodes
}

/// The `libcuda.so*` files directly under `dir`, sorted.
fn libcuda_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut libs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().and_then(|name| name.to_str()).map(|n| n.starts_with("libcuda.so")).unwrap_or(false)
        })
        .collect();
    libs.sort();
    libs
}

// ---- reading the machine -------------------------------------------------

/// Whether this process is root: `id -u` prints `0` for root, and a machine
/// with no `id` (Windows) is not root.
fn is_root() -> bool {
    capture("id", &["-u"]).map(|out| out.trim() == "0").unwrap_or(false)
}

/// The value of a `/proc/meminfo` key, in KB.
fn meminfo_kb(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let needle = format!("{key}:");
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix(&needle) {
            return rest.split_whitespace().next().and_then(|value| value.parse().ok());
        }
    }
    None
}

/// The free kilobytes on the filesystem holding `path`: the fourth field of
/// the second line of `df -Pk <path>`.
fn df_available_kb(path: &Path) -> Option<u64> {
    let out = capture("df", &["-Pk", path.to_str()?])?;
    out.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()
}

/// `whoami`, with the environment as a fallback.
fn user_name() -> String {
    if let Some(name) = capture("whoami", &[]).map(|out| out.trim().to_string()).filter(|s| !s.is_empty()) {
        return name;
    }
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

/// The file name of the program this report is running as.
fn script_name() -> String {
    std::env::args()
        .next()
        .and_then(|arg| Path::new(&arg).file_name().map(|name| name.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "golem".to_string())
}

/// The report's date line: the local clock, through `date` where that is
/// safe. On Windows `date` is an interactive prompt, so the stamp is built
/// here instead - the value is the same kind of fact either way.
fn date_line() -> String {
    if !cfg!(windows) {
        if let Some(out) = capture("date", &[]) {
            let line = out.trim();
            if !line.is_empty() {
                return line.to_string();
            }
        }
    }
    let secs = secs_since_epoch();
    let (year, month, day) = time::civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC", rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// `YYYYmmdd-HHMMSS` in UTC, the stamp the log file's name carries. UTC
/// because the core's calendar is, and a log file's name is not worth a
/// timezone database.
fn stamp() -> String {
    let secs = secs_since_epoch();
    let (year, month, day) = time::civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, (rem % 3600) / 60, rem % 60)
}

fn secs_since_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The OS type the report names: `OSTYPE` when the environment has it (bash
/// sets it but does not export it), `linux-gnu` on a native Linux build,
/// and otherwise the empty string, which the report prints as `unknown`.
fn ostype() -> String {
    if let Ok(value) = std::env::var("OSTYPE") {
        if !value.is_empty() {
            return value;
        }
    }
    if cfg!(target_os = "linux") {
        "linux-gnu".to_string()
    } else {
        String::new()
    }
}

/// The CPU's `Model name` from `lscpu`: the text after the first `:`.
fn lscpu_model(text: &str) -> Option<String> {
    for line in text.lines() {
        if line.contains("Model name") {
            if let Some(value) = line.split(':').nth(1) {
                let value = value.trim();
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
    }
    None
}

/// `/etc/os-release` into its key/value pairs, the way `source` leaves them
/// in the environment: comments and blank lines skipped, the first `=`
/// splits, and one layer of matching quotes is stripped.
fn parse_os_release(text: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        vars.insert(key.to_string(), unquote(value.trim()));
    }
    vars
}

/// One layer of matching `"` or `'` removed, as `source` removes it.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    let quoted = bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''));
    if quoted {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

// ---- the pure decisions --------------------------------------------------

/// A report line's level: `[ OK ]` or `[WARN]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Ok,
    Warn,
}

/// The RAM verdict.
fn ram_verdict(gb: u64) -> (Verdict, &'static str) {
    if gb >= 32 {
        (Verdict::Ok, "32GB+ RAM detected.")
    } else if gb >= 16 {
        (Verdict::Ok, "16GB+ RAM detected.")
    } else {
        (Verdict::Warn, "Less than 16GB RAM detected.")
    }
}

/// The package manager the machine has, in priority order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Manager {
    Apt,
    Dnf,
    Pacman,
}

/// Which manager to use: `apt-get` wins, then `dnf`, then
/// `pacman`, and none present is the warning branch.
fn package_manager(apt: bool, dnf: bool, pacman: bool) -> Option<Manager> {
    if apt {
        Some(Manager::Apt)
    } else if dnf {
        Some(Manager::Dnf)
    } else if pacman {
        Some(Manager::Pacman)
    } else {
        None
    }
}

/// The toolchain line: the tool's name padded to 8, then the first line of
/// its `--version`.
fn toolchain_line(tool: &str, version_output: &str) -> String {
    format!("{tool:<8} {}", version_output.lines().next().unwrap_or(""))
}

/// The summary's three GPU lines, each `unknown` when its value is empty.
fn gpu_summary_lines(name: &str, vram: &str, driver: &str) -> [String; 3] {
    [
        format!("  Model        : {}", or_unknown(name)),
        format!("  VRAM         : {}", or_unknown(vram)),
        format!("  Driver       : {}", or_unknown(driver)),
    ]
}

/// The summary's CUDA line: where `nvcc` is on PATH, or `not installed`.
fn nvcc_summary_line() -> String {
    which_path("nvcc").map(|path| path.display().to_string()).unwrap_or_else(|| "not installed".to_string())
}

fn or_unknown(value: &str) -> &str {
    if value.is_empty() {
        "unknown"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_takes_no_arguments() {
        assert!(parse_args(&[]).is_ok());
        let err = parse_args(&["--native".to_string()]).expect_err("a flag is not ignored");
        assert_eq!(err, "unexpected argument: --native");
        let err = parse_args(&["a".to_string(), "b".to_string()]).expect_err("the first is named");
        assert_eq!(err, "unexpected argument: a");
    }

    #[test]
    fn os_release_is_key_value_pairs() {
        let text = "\
# a comment, and a blank line follows

NAME=\"Ubuntu\"
ID=ubuntu
VERSION_ID='24.04'
PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"
URL=https://ubuntu.com
EMPTY=
";
        let vars = parse_os_release(text);
        assert_eq!(vars.get("NAME").map(String::as_str), Some("Ubuntu"));
        assert_eq!(vars.get("ID").map(String::as_str), Some("ubuntu"));
        // Both quote styles are stripped, one layer only.
        assert_eq!(vars.get("VERSION_ID").map(String::as_str), Some("24.04"));
        assert_eq!(vars.get("PRETTY_NAME").map(String::as_str), Some("Ubuntu 24.04.1 LTS"));
        // An unquoted value keeps its characters.
        assert_eq!(vars.get("URL").map(String::as_str), Some("https://ubuntu.com"));
        // An empty assignment is present and empty, not missing.
        assert_eq!(vars.get("EMPTY").map(String::as_str), Some(""));
        assert_eq!(vars.get("MISSING"), None);
        // The comment is not a key.
        assert_eq!(vars.len(), 6);
    }

    #[test]
    fn the_package_manager_is_the_first_one_present() {
        assert_eq!(package_manager(true, true, true), Some(Manager::Apt));
        assert_eq!(package_manager(false, true, true), Some(Manager::Dnf));
        assert_eq!(package_manager(false, false, true), Some(Manager::Pacman));
        assert_eq!(package_manager(false, false, false), None);
    }

    #[test]
    fn each_manager_gets_its_package_list() {
        assert_eq!(APT_PACKAGES.len(), 9);
        assert_eq!(APT_PACKAGES[0], "build-essential");
        assert!(APT_PACKAGES.contains(&"pkg-config"));
        assert_eq!(DNF_PACKAGES.len(), 11);
        assert!(DNF_PACKAGES.contains(&"gcc-c++"));
        assert!(DNF_PACKAGES.contains(&"pkgconfig"));
        assert_eq!(PACMAN_PACKAGES.len(), 9);
        assert!(PACMAN_PACKAGES.contains(&"base-devel"));
        assert!(PACMAN_PACKAGES.contains(&"pkgconf"));
    }

    #[test]
    fn the_toolchain_line_pads_the_name_to_eight() {
        let gcc = toolchain_line(
            "gcc",
            "gcc (Ubuntu 13.2.0-23ubuntu4) 13.2.0\nCopyright (C) 2023 Free Software Foundation, Inc.\n",
        );
        assert_eq!(gcc, "gcc      gcc (Ubuntu 13.2.0-23ubuntu4) 13.2.0");
        assert_eq!(toolchain_line("cmake", "cmake version 3.28.3\n"), "cmake    cmake version 3.28.3");
        assert_eq!(toolchain_line("git", "git version 2.43.0\n"), "git      git version 2.43.0");
        // A tool that answers with nothing still gets its padded name.
        assert_eq!(toolchain_line("make", ""), "make     ");
    }

    #[test]
    fn the_memory_verdict_has_three_cases() {
        assert_eq!(ram_verdict(64), (Verdict::Ok, "32GB+ RAM detected."));
        assert_eq!(ram_verdict(32), (Verdict::Ok, "32GB+ RAM detected."));
        assert_eq!(ram_verdict(16), (Verdict::Ok, "16GB+ RAM detected."));
        assert_eq!(ram_verdict(8), (Verdict::Warn, "Less than 16GB RAM detected."));
    }

    #[test]
    fn the_cpu_model_comes_from_lscpus_model_name() {
        let text = "\
Architecture:                    x86_64
CPU op-mode(s):                  32-bit, 64-bit
Model name:                     12th Gen Intel(R) Core(TM) i7-12700KF
";
        assert_eq!(lscpu_model(text).as_deref(), Some("12th Gen Intel(R) Core(TM) i7-12700KF"));
        assert_eq!(lscpu_model("Architecture: x86_64\n"), None);
    }

    #[test]
    fn the_summary_gpu_lines_fall_back_to_unknown() {
        assert_eq!(
            gpu_summary_lines("NVIDIA GeForce RTX 3060", "12288 MiB", "580.65.06").join("\n"),
            "  Model        : NVIDIA GeForce RTX 3060\n  VRAM         : 12288 MiB\n  Driver       : 580.65.06"
        );
        // An empty answer is `unknown`.
        assert_eq!(
            gpu_summary_lines("", "", "").join("\n"),
            "  Model        : unknown\n  VRAM         : unknown\n  Driver       : unknown"
        );
    }

    #[test]
    fn the_log_stamp_is_date_dash_time() {
        let stamp = stamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-');
        assert!(stamp[..8].chars().all(|c| c.is_ascii_digit()), "{stamp}");
        assert!(stamp[9..].chars().all(|c| c.is_ascii_digit()), "{stamp}");
    }
}
