//! Per-component phase state machine + JSON persistence.
//!
//! Phases: pending → reviewing → fixing → verifying → committing → done.
//! Any active phase → failed (quarantine). Resume: a component interrupted
//! mid-phase is reset to pending on startup (see `Engine::run`), so `next()`
//! only ever picks among pending components.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

/// Per-call suffix for unique temp-file names in `State::save`, so concurrent
/// runs never collide on the same temp path.
static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Pending,
    Reviewing,
    Fixing,
    Verifying,
    Committing,
    Done,
    /// Quarantine. A component enters this phase only after its attempt
    /// budget is spent (goose retries + fix cycles). A single goose flake
    /// or a red verify is not enough — those retry or send the fixer.
    /// It is *not* automatically requeued: `State::next` only selects
    /// `Pending` components, so a failed component stays parked here until an
    /// external caller explicitly requeues it via the `Failed → Pending`
    /// transition (the only legal path out of `Failed`). This is intentional —
    /// quarantine lets the engine continue with the remaining components
    /// rather than stalling the whole run on one slice.
    Failed,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Pending => "pending",
            Phase::Reviewing => "reviewing",
            Phase::Fixing => "fixing",
            Phase::Verifying => "verifying",
            Phase::Committing => "committing",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentState {
    pub slug: String,
    pub name: String,
    pub tier: String,
    pub phase: Phase,
    /// Finding count from the last review (0 = clean).
    pub findings: usize,
    /// Last known detail (e.g. fix outcome or verify failure).
    pub detail: String,
    /// Repo-relative paths this component covers (from AI discovery).
    #[serde(default)]
    pub paths: Vec<String>,
    /// Per-component verify commands from the checklist. Empty = repo-wide
    /// `verify` list in `.review/config.toml`.
    #[serde(default)]
    pub verify: Vec<String>,
    /// Short hash of this component's fix commit, when one was kept
    /// (commit-on-green). Absent = nothing was committed for it.
    #[serde(default)]
    pub commit: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub components: BTreeMap<String, ComponentState>,
}

/// Remove sibling `state.json.tmp.*` orphans from crashed saves. Only
/// files matching the exact temp-name shape are touched; removal failures
/// are ignored (best-effort hygiene, not a load error).
fn sweep_tmp_files(path: &Path) {
    let Some(dir) = path.parent() else { return };
    let Some(stem) = path.file_name().and_then(|s| s.to_str()) else {
        return;
    };
    let prefix = format!("{stem}.tmp.");
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(rest) = name.strip_prefix(&prefix) else {
            continue;
        };
        // Temp names are `tmp.<pid>.<counter>`: only sweep when the owning
        // process is DEAD. A live concurrent engine's in-flight temp file
        // must survive — deleting it would make that process's rename
        // fail with ENOENT and abort its run (the pid-scoped temp naming
        // exists precisely to allow this liveness check).
        let pid = rest.split('.').next().and_then(|p| p.parse::<u32>().ok());
        match pid {
            Some(pid) if !pid_alive(pid) => {
                let _ = fs::remove_file(entry.path());
            }
            // Unparseable pid (hand-created file?) — leave it alone.
            _ => {}
        }
    }
}

/// Best-effort liveness probe for a pid. `ps -p` exits 0 when the pid
/// exists (regardless of owner), 1 when not — unambiguous, unlike
/// `kill -0` where EPERM and dead look the same through a shell. Fails
/// OPEN (assume alive) so a probe error never sweeps a live temp file.
fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(true)
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        // Sweep orphaned save temps: a crash between File::create and the
        // rename leaves state.json.tmp.<pid>.<n> siblings that nothing else
        // ever cleans; they accumulate across crashes.
        sweep_tmp_files(path);
        let raw = fs::read_to_string(path)?;
        match serde_json::from_str::<Self>(&raw) {
            Ok(state) => Ok(state),
            Err(e) => {
                // A corrupted or hand-edited state.json should not abort the
                // whole engine. Back up the offending file and fall back to a
                // fresh default, mirroring the missing-file branch above.
                // Use a numeric suffix so repeated corruption recovery never
                // overwrites a prior backup.
                let mut n = 0;
                let backup = loop {
                    if n >= 10_000 {
                        // Extremely unlikely: 10 000 backup files already
                        // exist. Fall back to a .overflow suffix rather than
                        // spinning indefinitely.
                        break path.with_extension("json.bad.overflow");
                    }
                    let candidate = if n == 0 {
                        path.with_extension("json.bad")
                    } else {
                        path.with_extension(format!("json.bad.{n}"))
                    };
                    if !candidate.exists() {
                        break candidate;
                    }
                    n += 1;
                };
                // Move the corrupt file aside BEFORE returning the default
                // state: the next save would otherwise overwrite the only
                // copy, destroying the evidence. If the move fails, fall
                // back to a copy (preserves the bytes) and warn honestly.
                if fs::rename(path, &backup).is_err() {
                    if fs::copy(path, &backup).is_ok() {
                        eprintln!(
                            "  ⚠ state file {:?} was invalid JSON ({e:#}); could not move it, copied to {:?} \
                             and starting from a default state",
                            path.as_os_str(),
                            backup.as_os_str(),
                        );
                        return Ok(Self::default());
                    }
                    eprintln!(
                        "  ⚠ state file {:?} was invalid JSON ({e:#}); could NOT back it up \
                         (rename and copy both failed) — it will be overwritten by the next save",
                        path.as_os_str(),
                    );
                } else {
                    eprintln!(
                        "  ⚠ state file {:?} was invalid JSON ({e:#}); backed up to {:?} \
                         and starting from a default state",
                        path.as_os_str(),
                        backup.as_os_str(),
                    );
                }
                Ok(Self::default())
            }
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        // Write to a temp file in the same directory then rename, so a crash
        // mid-write cannot leave a truncated state.json. The temp name embeds
        // the process id (plus a per-call counter) so concurrent runs on the
        // same review dir never race on a shared temp file.
        let tmp = path.with_extension(format!(
            "json.tmp.{}.{}",
            std::process::id(),
            NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        // Write + fsync in a closure so that on any error we remove the
        // half-written temp file instead of leaking it.
        let result = (|| -> Result<()> {
            let mut f = fs::File::create(&tmp)?;
            // Preserve an existing file's permissions: File::create gets
            // umask-default mode (typically 0644), so the atomic rename
            // would silently widen a more-restrictive state.json (e.g. 0600).
            if let Ok(meta) = fs::metadata(path) {
                let _ = f.set_permissions(meta.permissions());
            }
            use std::io::Write;
            f.write_all((serde_json::to_string_pretty(self)? + "\n").as_bytes())?;
            // fsync the temp file before the rename so the atomic-rename
            // guarantee actually holds on disk.
            f.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
            return result;
        }
        // fs::rename atomically REPLACES an existing destination on both
        // Unix and Windows — no pre-remove (which would open a window
        // where a concurrent load sees a missing file).
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            return Err(e.into());
        }
        // Directory fsync is a POSIX durability nicety; Windows cannot open
        // a directory as a File, so skip it there.
        #[cfg(unix)]
        {
            let dir = path.parent();
            match dir {
                Some(d) if !d.as_os_str().is_empty() => {
                    let d = fs::File::open(d)?;
                    d.sync_all()?;
                }
                Some(_) => {
                    let d = fs::File::open(".")?;
                    d.sync_all()?;
                }
                None => {}
            }
        }
        Ok(())
    }

    /// Ensure every component has a state row (adds missing as pending),
    /// refresh mutable fields (tier, name) for existing entries, and prune
    /// rows that are no longer present in the checklist.
    pub fn sync(&mut self, components: &[crate::checklist::Component]) {
        self.components
            .retain(|slug, _| components.iter().any(|c| &c.slug == slug));
        for c in components {
            match self.components.get_mut(&c.slug) {
                Some(existing) => {
                    existing.name = c.name.clone();
                    existing.tier = c.tier.clone();
                    if !c.paths.is_empty() {
                        existing.paths = c.paths.clone();
                    }
                    existing.verify = c.verify.clone();
                    if c.done && existing.phase == Phase::Pending {
                        existing.phase = Phase::Done;
                        if existing.detail.is_empty() {
                            existing.detail = "marked done in checklist".to_string();
                        }
                    } else if !c.done && matches!(existing.phase, Phase::Done | Phase::Failed) {
                        // Unchecking the checklist is a full redo of that
                        // slice: Done *and* quarantined rows go back to
                        // pending. Failed used to stick until `gaggle
                        // requeue`, which made a checklist reset skip the
                        // components that most needed another pass.
                        existing.phase = Phase::Pending;
                        existing.findings = 0;
                        existing.detail = String::new();
                    }
                }
                None => {
                    self.components.insert(
                        c.slug.clone(),
                        ComponentState {
                            slug: c.slug.clone(),
                            name: c.name.clone(),
                            tier: c.tier.clone(),
                            phase: if c.done { Phase::Done } else { Phase::Pending },
                            findings: 0,
                            detail: if c.done {
                                "marked done in checklist".to_string()
                            } else {
                                String::new()
                            },
                            paths: c.paths.clone(),
                            verify: c.verify.clone(),
                            commit: None,
                        },
                    );
                }
            }
        }
    }

    /// Operator reset for a new pass over the same checklist. Every
    /// component returns to `Pending` with findings/detail/commit cleared.
    /// Paths and verify commands stay. This is not a state-machine
    /// transition (Done has no legal path back to Pending); it is the
    /// same kind of bulk rewrite `sync` uses when a checkbox is unchecked.
    pub fn restart_all(&mut self) {
        for c in self.components.values_mut() {
            c.phase = Phase::Pending;
            c.findings = 0;
            c.detail = String::new();
            c.commit = None;
        }
    }

    pub fn get(&self, slug: &str) -> Option<&ComponentState> {
        self.components.get(slug)
    }

    fn set_phase(&mut self, slug: &str, phase: Phase) -> Result<()> {
        let c = self
            .components
            .get_mut(slug)
            .ok_or_else(|| anyhow::anyhow!("unknown component {slug}"))?;
        c.phase = phase;
        Ok(())
    }

    pub fn set_detail(&mut self, slug: &str, detail: &str) -> Result<()> {
        let c = self
            .components
            .get_mut(slug)
            .ok_or_else(|| anyhow::anyhow!("unknown component {slug}"))?;
        c.detail = detail.to_string();
        Ok(())
    }

    pub fn set_findings(&mut self, slug: &str, n: usize) -> Result<()> {
        let c = self
            .components
            .get_mut(slug)
            .ok_or_else(|| anyhow::anyhow!("unknown component {slug}"))?;
        c.findings = n;
        Ok(())
    }

    /// Next component to work on: the lowest-tier pending one
    /// (high < medium < low, then alphabetical). Active-phase components are
    /// reset to pending on startup by `Engine::run`, so they are picked up
    /// here naturally rather than via a separate resume branch.
    pub fn next(&self) -> Option<&ComponentState> {
        self.components
            .values()
            .filter(|c| c.phase == Phase::Pending)
            .min_by(|a, b| {
                (crate::checklist::tier_rank(&a.tier), &a.slug)
                    .cmp(&(crate::checklist::tier_rank(&b.tier), &b.slug))
            })
    }
}

/// Transition validity: from → to.
///
/// `Failed → Pending` is the only legal way out of quarantine and is not
/// taken automatically anywhere in this module — a caller must request it
/// explicitly to requeue a failed component (see [`Phase::Failed`]).
pub fn allowed(from: Phase, to: Phase) -> bool {
    matches!(
        (from, to),
        (Phase::Pending, Phase::Reviewing)
            | (Phase::Reviewing, Phase::Fixing)
            | (Phase::Reviewing, Phase::Done)
            | (Phase::Reviewing, Phase::Failed)
            | (Phase::Fixing, Phase::Verifying)
            | (Phase::Fixing, Phase::Failed)
            | (Phase::Verifying, Phase::Committing)
            | (Phase::Verifying, Phase::Fixing)
            | (Phase::Verifying, Phase::Failed)
            | (Phase::Committing, Phase::Done)
            | (Phase::Committing, Phase::Failed)
            | (Phase::Failed, Phase::Pending)
    )
}

/// Transition a component, enforcing the state machine.
pub fn transition(state: &mut State, slug: &str, to: Phase) -> Result<()> {
    let from = state
        .components
        .get(slug)
        .map(|c| c.phase)
        .ok_or_else(|| anyhow::anyhow!("unknown component {slug}"))?;
    if !allowed(from, to) {
        bail!(
            "illegal transition {slug}: {} → {}",
            from.as_str(),
            to.as_str()
        );
    }
    state.set_phase(slug, to)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checklist::Component;

    #[test]
    fn sync_honors_done_and_paths() {
        let comps = vec![Component {
            slug: "a".into(),
            name: "A".into(),
            tier: "high".into(),
            done: true,
            paths: vec!["src/a.rs".into()],
            verify: vec![],
        }];
        let mut s = State::default();
        s.sync(&comps);
        let row = s.get("a").unwrap();
        assert_eq!(row.phase, Phase::Done);
        assert_eq!(row.paths, vec!["src/a.rs"]);
    }

    #[test]
    fn sync_uncheck_requeues_done() {
        let mut s = State::default();
        s.sync(&[Component {
            slug: "a".into(),
            name: "A".into(),
            tier: "high".into(),
            done: true,
            paths: vec![],
            verify: vec![],
        }]);
        s.sync(&[Component {
            slug: "a".into(),
            name: "A".into(),
            tier: "high".into(),
            done: false,
            paths: vec![],
            verify: vec![],
        }]);
        assert_eq!(s.get("a").unwrap().phase, Phase::Pending);
    }

    #[test]
    fn sync_uncheck_requeues_failed() {
        let mut s = State::default();
        s.sync(&[Component {
            slug: "a".into(),
            name: "A".into(),
            tier: "high".into(),
            done: false,
            paths: vec![],
            verify: vec![],
        }]);
        transition(&mut s, "a", Phase::Reviewing).unwrap();
        transition(&mut s, "a", Phase::Failed).unwrap();
        s.set_findings("a", 2).unwrap();
        s.set_detail("a", "goose timed out").unwrap();
        s.sync(&[Component {
            slug: "a".into(),
            name: "A".into(),
            tier: "high".into(),
            done: false,
            paths: vec![],
            verify: vec![],
        }]);
        let row = s.get("a").unwrap();
        assert_eq!(row.phase, Phase::Pending);
        assert_eq!(row.findings, 0);
        assert!(row.detail.is_empty());
    }

    #[test]
    fn restart_all_resets_done_and_failed() {
        let mut s = State::default();
        s.sync(&[
            Component::new("a", "A", "high"),
            Component::new("b", "B", "low"),
        ]);
        transition(&mut s, "a", Phase::Reviewing).unwrap();
        transition(&mut s, "a", Phase::Done).unwrap();
        s.set_findings("a", 3).unwrap();
        s.set_detail("a", "fixed").unwrap();
        s.components.get_mut("a").unwrap().commit = Some("abc".into());
        transition(&mut s, "b", Phase::Reviewing).unwrap();
        transition(&mut s, "b", Phase::Failed).unwrap();
        s.restart_all();
        for slug in ["a", "b"] {
            let row = s.get(slug).unwrap();
            assert_eq!(row.phase, Phase::Pending);
            assert_eq!(row.findings, 0);
            assert!(row.detail.is_empty());
            assert!(row.commit.is_none());
        }
    }
}

#[cfg(test)]
mod requeue_tests {
    use super::*;

    #[test]
    fn failed_to_pending_is_legal_requeue_path() {
        assert!(allowed(Phase::Failed, Phase::Pending));
        // And the surrounding machine stays closed: Failed has no other
        // exit, and Pending cannot be re-entered from anywhere else.
        assert!(!allowed(Phase::Done, Phase::Pending));
        assert!(!allowed(Phase::Reviewing, Phase::Pending));
        assert!(!allowed(Phase::Failed, Phase::Reviewing));
        assert!(!allowed(Phase::Failed, Phase::Done));
    }

    #[test]
    fn transition_moves_failed_to_pending() {
        let mut s = State::default();
        s.sync(&[crate::checklist::Component::new("a", "A", "high")]);
        transition(&mut s, "a", Phase::Reviewing).unwrap();
        transition(&mut s, "a", Phase::Failed).unwrap();
        transition(&mut s, "a", Phase::Pending).unwrap();
        assert_eq!(s.get("a").unwrap().phase, Phase::Pending);
    }
}
