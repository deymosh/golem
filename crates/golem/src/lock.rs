//! One engine-touching command per home: `bench`, `eval`, `tune` and
//! `install` hold `.local/golem.lock` while they run, and `up` and
//! `restart` from outside them refuse while it is held (`down` does not: it
//! fails a measurement loudly, and is how the card is freed in a hurry). Two measurements at
//! once measure each other, and an engine restarted under a running tune
//! turns its numbers into noise.
//!
//! The lock is a file created with `create_new` - atomic on every platform -
//! holding the holder's pid and image name and the verb. A holder that died
//! without removing it (a killed terminal) leaves a stale file: a pid no
//! longer running under that name is taken over. A command's own children
//! (`proc::golem`: the tuner's benches and gate) inherit the lock through
//! [`HOLDER_ENV`] rather than wait on their parent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// The environment value a holder's children are started with: the pid in
/// the lock file. Not configuration - a handoff from parent to child.
pub const HOLDER_ENV: &str = "GOLEM_LOCK_HOLDER";

/// The pid of the lock this process holds or inherited, 0 for none.
static HELD: AtomicU32 = AtomicU32::new(0);

/// The pid of the lock this process holds or inherited, for its children.
pub fn held() -> Option<u32> {
    Some(HELD.load(Ordering::Relaxed)).filter(|&pid| pid != 0)
}

/// The lock, held until dropped.
#[derive(Debug)]
pub struct Guard {
    path: PathBuf,
    owned: bool,
}

impl Drop for Guard {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn path(home: &Path) -> PathBuf {
    crate::paths::lock_file(home)
}

/// What a lock file records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Holder {
    pid: u32,
    image: String,
    verb: String,
}

impl Holder {
    fn text(&self) -> String {
        format!("{}\n{}\n{}\n", self.pid, self.image, self.verb)
    }

    fn parse(text: &str) -> Option<Holder> {
        let mut lines = text.lines();
        Some(Holder {
            pid: lines.next()?.trim().parse().ok()?,
            image: lines.next().unwrap_or("").trim().to_string(),
            verb: lines.next().unwrap_or("").trim().to_string(),
        })
    }
}

/// What to do about a lock file that is already there.
#[derive(Debug, PartialEq, Eq)]
enum Found {
    /// This process's parent holds it and handed it down.
    Inherited,
    /// Another command holds it and runs.
    Busy,
    /// Its holder is gone (or the file is not a lock's): take it over.
    Stale,
}

/// The decision, given what is on disk, what was handed down, and whether
/// the recorded holder still runs.
fn judge(holder: Option<&Holder>, handed_down: Option<u32>, running: impl Fn(&Holder) -> bool) -> Found {
    match holder {
        Some(h) if handed_down == Some(h.pid) => Found::Inherited,
        Some(h) if running(h) => Found::Busy,
        _ => Found::Stale,
    }
}

fn running(holder: &Holder) -> bool {
    let image = Some(holder.image.as_str()).filter(|image| !image.is_empty());
    crate::launch::is_engine(holder.pid, image)
}

fn handed_down() -> Option<u32> {
    std::env::var(HOLDER_ENV).ok()?.parse().ok()
}

fn busy_message(home: &Path, holder: &Holder) -> String {
    format!(
        "golem {} (pid {}) is using this home's engine - one engine-touching command at a time; \
         wait for it, or remove {} if no golem command is running",
        holder.verb,
        holder.pid,
        path(home).display()
    )
}

/// Take the home's lock for `verb`, or say who holds it.
pub fn acquire(home: &Path, verb: &str) -> Result<Guard, String> {
    let path = path(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let me = Holder {
        pid: std::process::id(),
        image: std::env::current_exe()
            .ok()
            .and_then(|exe| exe.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default(),
        verb: verb.to_string(),
    };
    // Twice at most: a stale file is removed once, and a second loss of the
    // race to create it is another command that got there first.
    for _ in 0..2 {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                use std::io::Write;
                file.write_all(me.text().as_bytes()).map_err(|e| format!("cannot write {}: {e}", path.display()))?;
                HELD.store(me.pid, Ordering::Relaxed);
                return Ok(Guard { path, owned: true });
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let holder = std::fs::read_to_string(&path).ok().and_then(|t| Holder::parse(&t));
                match judge(holder.as_ref(), handed_down(), running) {
                    Found::Inherited => {
                        HELD.store(holder.map_or(0, |h| h.pid), Ordering::Relaxed);
                        return Ok(Guard { path, owned: false });
                    }
                    Found::Busy => return Err(busy_message(home, holder.as_ref().expect("busy has a holder"))),
                    Found::Stale => {
                        let _ = std::fs::remove_file(&path);
                    }
                }
            }
            Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
        }
    }
    Err(format!("cannot take {}: another command took it first", path.display()))
}

/// `Err` naming the holder when another command holds the lock and runs -
/// for a verb that does not take the lock itself but must not cut in.
pub fn check(home: &Path) -> Result<(), String> {
    let Some(holder) = std::fs::read_to_string(path(home)).ok().and_then(|t| Holder::parse(&t)) else { return Ok(()) };
    match judge(Some(&holder), handed_down(), running) {
        Found::Busy => Err(busy_message(home, &holder)),
        Found::Inherited | Found::Stale => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(pid: u32) -> Holder {
        Holder { pid, image: "golem.exe".into(), verb: "tune".into() }
    }

    #[test]
    fn a_lock_file_reads_back() {
        let h = holder(4242);
        assert_eq!(Holder::parse(&h.text()), Some(h));
        assert_eq!(Holder::parse("not a pid\n"), None);
        assert_eq!(Holder::parse(""), None);
    }

    #[test]
    fn a_lock_is_inherited_held_or_stale() {
        let h = holder(4242);
        assert_eq!(judge(Some(&h), Some(4242), |_| true), Found::Inherited);
        assert_eq!(judge(Some(&h), None, |_| true), Found::Busy);
        assert_eq!(judge(Some(&h), Some(7), |_| true), Found::Busy, "another holder's child is not ours");
        assert_eq!(judge(Some(&h), None, |_| false), Found::Stale);
        assert_eq!(judge(None, None, |_| true), Found::Stale, "a file that is not a lock's");
    }

    #[test]
    fn the_lock_is_taken_released_and_refused_while_held() {
        let home = std::env::temp_dir().join(format!("golem-lock-{}", std::process::id()));
        let guard = acquire(&home, "bench").expect("free");
        assert!(path(&home).is_file());
        // This process runs, so its own lock reads as busy to a command
        // that did not inherit it.
        let busy = Holder::parse(&std::fs::read_to_string(path(&home)).expect("lock")).expect("holder");
        assert_eq!(busy.pid, std::process::id());
        assert!(running(&busy));
        drop(guard);
        assert!(!path(&home).exists(), "released on drop");
        // A dead holder's file is taken over.
        std::fs::write(path(&home), holder(0).text()).expect("stale");
        let taken = acquire(&home, "eval").expect("stale lock taken over");
        drop(taken);
        std::fs::remove_dir_all(&home).ok();
    }
}
