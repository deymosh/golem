//! `golem install`: put the pinned engine build on this machine.
//!
//! It fetches the pinned release rather than building llama.cpp from source.
//! Two reasons, and the second is the one that decides it:
//!
//! - **A build needs a toolchain.** git, cmake and a compiler, on a machine
//!   whose owner wanted to run a model - and the failure when one is missing
//!   arrives ten minutes in, as a cmake error.
//! - **The pin is the point.** `build = b11379` is the artifact the 200K gate
//!   was measured on. A build from whatever upstream `master` is today is a
//!   different binary, and the gate that adopted the pin does not describe it.
//!
//! So there is one acquisition path on every platform: the release named by
//! the adapter for this platform and this backend. `up` does it itself when
//! the server is not on disk yet, which leaves two callers for the command:
//! a container image, which wants the engine baked in rather than downloaded
//! on first run, and a machine being prepared before the model arrives.

use std::path::Path;

use crate::launch;
use crate::release;

/// `golem install`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    if let Err(message) = parse_args(args) {
        eprintln!("[ERROR] {message}");
        return 1;
    }
    let plan = match launch::plan(home, &crate::config::Overrides::from_env()) {
        Ok(plan) => plan,
        Err(message) => {
            eprintln!("[ERROR] {message}");
            return 1;
        }
    };
    let mut out = std::io::stdout();
    match release::ensure(home, &plan.pinned(), &mut out) {
        Ok(installed) => {
            println!(
                "[ OK ] {} {} ({}) is installed at {}",
                plan.engine,
                installed.tag,
                plan.backend,
                installed.dir.display()
            );
            // Whether the build finds the card: an archive that installs
            // cleanly can still lack the runtime its backend loads.
            if let Some(advice) = launch::probe(home, &crate::config::Overrides::from_env(), &mut out) {
                eprintln!("[WARN] {advice}");
            }
            0
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_takes_no_arguments() {
        assert!(parse_args(&[]).is_ok());
        let err = parse_args(&["--native".to_string()]).expect_err("a flag is not ignored");
        assert_eq!(err, "unexpected argument: --native");
        let err = parse_args(&["a".to_string(), "b".to_string()]).expect_err("the first is named");
        assert_eq!(err, "unexpected argument: a");
    }

    // What the command *does* is `release::ensure`, which has its own tests
    // against a real release and a scratch home; what is left here is the
    // argument check and the resolution, which `launch::plan`'s tests cover.
}
