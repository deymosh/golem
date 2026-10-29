//! The engine adapter's contract: turning a preset's flags into the arguments
//! an engine is actually started with.
//!
//! Downloading a release and spawning a process live in the binary; what is
//! here is the translation, and it has three rules:
//!
//! - a value of `false` means "do not pass this flag at all";
//! - `flag.<key> = --other` renames a canonical flag, and `flag.<key> = -`
//!   drops it with a warning (the engine has no equivalent);
//! - `map.<key>.<value> = <args...>` replaces the whole flag *and* its value,
//!   and `map.<key>.<value> = -` drops it with a warning.
//!
//! The distinction between `flag` and `map` is not decoration: `--mlock` is a
//! single flag with no argument, where `--spec-type draft-mtp` is two, so a
//! rename cannot express it and a map can.
//!
//! Everything is warning-carrying rather than silent. A flag the engine cannot
//! take is the difference between "it runs" and "it runs the way you asked",
//! so the start says so out loud.

use crate::ini::Document;

/// One engine's argument list, and what had to be dropped to get it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandLine {
    pub args: Vec<String>,
    /// `(key, value)` pairs the adapter has no equivalent for. The wording
    /// says which engine and which flag, because a dropped flag is a silent
    /// difference in behaviour if nobody says it.
    pub dropped: Vec<(String, String)>,
}

/// Translate `flags` (in the order the preset gave them) through `adapter`.
///
/// `adapter` is the parsed `config/engines/<name>.ini`; `flags` is what
/// `record::flags_of` produces for the section being served.
pub fn translate(flags: &[(String, String)], adapter: &Document) -> CommandLine {
    let mut out = CommandLine::default();
    for (key, value) in flags {
        // `false` means the flag is not wanted, not that it takes the word.
        if value == "false" {
            continue;
        }
        let mapped = adapter.global(&format!("map.{key}.{value}"));
        if let Some(replacement) = mapped {
            if replacement == "-" {
                out.dropped.push((key.clone(), value.clone()));
            } else {
                out.args.extend(replacement.split_whitespace().map(str::to_string));
            }
            continue;
        }
        let name = match adapter.global(&format!("flag.{key}")) {
            Some("-") => {
                out.dropped.push((key.clone(), value.clone()));
                continue;
            }
            Some(renamed) => renamed.to_string(),
            None => format!("--{key}"),
        };
        if value == "true" {
            out.args.push(name);
        } else {
            out.args.push(name);
            out.args.push(value.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(text: &str) -> Document {
        crate::ini::parse(text)
    }

    fn flags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_canonical_flag_is_passed_as_itself() {
        let line = translate(&flags(&[("ctx-size", "249344"), ("jinja", "true")]), &adapter(""));
        assert_eq!(line.args, vec!["--ctx-size", "249344", "--jinja"]);
        assert!(line.dropped.is_empty());
    }

    #[test]
    fn a_false_value_passes_nothing() {
        // `false` means "not wanted", which is not the same as "wanted false":
        // llama.cpp takes `--no-mmproj`, not `--mmproj false`.
        let line = translate(&flags(&[("mmproj", "false"), ("jinja", "true")]), &adapter(""));
        assert_eq!(line.args, vec!["--jinja"]);
    }

    #[test]
    fn a_rename_keeps_the_value_with_it() {
        let line = translate(
            &flags(&[("n-gpu-layers", "99")]),
            &adapter("flag.n-gpu-layers = --gpu-layers\n"),
        );
        assert_eq!(line.args, vec!["--gpu-layers", "99"]);
    }

    #[test]
    fn a_dropped_flag_is_named_rather_than_silently_ignored() {
        // llama.cpp has no `--sleep-idle-seconds`; the adapter says so.
        let line = translate(
            &flags(&[("sleep-idle-seconds", "600"), ("jinja", "true")]),
            &adapter("flag.sleep-idle-seconds = -\n"),
        );
        assert_eq!(line.args, vec!["--jinja"]);
        assert_eq!(line.dropped, vec![("sleep-idle-seconds".to_string(), "600".to_string())]);
    }

    #[test]
    fn a_map_replaces_the_flag_and_its_value_together() {
        // `--mlock` is one flag with no argument; a rename cannot express it.
        let line = translate(
            &flags(&[("load-mode", "mlock"), ("spec-type", "draft-mtp")]),
            &adapter(
                "map.load-mode.mlock = --mlock\nmap.spec-type.draft-mtp = --spec-type mtp\n",
            ),
        );
        assert_eq!(line.args, vec!["--mlock", "--spec-type", "mtp"]);
    }

    #[test]
    fn a_map_to_dash_drops_the_pair_and_says_which() {
        let line = translate(
            &flags(&[("load-mode", "auto")]),
            &adapter("map.load-mode.auto = -\n"),
        );
        assert!(line.args.is_empty());
        assert_eq!(line.dropped, vec![("load-mode".to_string(), "auto".to_string())]);
    }

    #[test]
    fn a_map_for_one_value_does_not_catch_the_others() {
        // `mmap` and `auto` both exist as values; only the one the adapter
        // names is replaced, and the other goes through as itself.
        let adapter = adapter("map.load-mode.mlock = --mlock\n");
        let line = translate(&flags(&[("load-mode", "mmap")]), &adapter);
        assert_eq!(line.args, vec!["--load-mode", "mmap"]);
    }

    #[test]
    fn the_presets_order_is_the_command_lines_order() {
        // The tuner's `write_candidate` puts its overrides at the top of the
        // section, and flags later in the file win in llama.cpp - so the order
        // is behaviour, not tidiness.
        let line = translate(
            &flags(&[("n-cpu-moe", "35"), ("ctx-size", "249344"), ("fit", "off")]),
            &adapter(""),
        );
        assert_eq!(
            line.args,
            vec!["--n-cpu-moe", "35", "--ctx-size", "249344", "--fit", "off"]
        );
    }
}
