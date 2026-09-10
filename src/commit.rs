//! Git commit ownership. The harness commits — the agent never does.

use anyhow::{Context, Result, bail};
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};

fn git_cmd(repo: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(repo).stdin(Stdio::null());
    cmd
}

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = git_cmd(repo, args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed (exit {:?}): {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run git expecting NUL-separated path output (`-z`), split on NUL.
///
/// All path-consuming callers must use this: line-based parsing keeps
/// git's C-quoting (paths with non-ASCII/quotes come back like
/// `"src/caf\303\251.rs"`), which then fails as a literal pathspec.
fn git_z(repo: &Path, args: &[&str]) -> Result<Vec<String>> {
    let out = git_cmd(repo, args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed (exit {:?}): {}",
            args.join(" "),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
}

/// Commit config from `.review/config.toml`. Currently only commit signing.
#[derive(Debug, Clone, Default)]
pub struct CommitConfig {
    /// Whether to sign commits. Default: false — an interactive gpg signing
    /// prompt would hang the autonomous loop, so signing is opt-in via
    /// `[commit] sign = true` in `.review/config.toml`.
    pub sign: bool,
}

impl CommitConfig {
    /// Load from `.review/config.toml` (missing file or missing section =
    /// defaults: no signing).
    pub fn load(repo: &Path) -> Result<Self> {
        let cfg_path = repo.join(".review/config.toml");
        let text = match std::fs::read_to_string(&cfg_path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read `{}`", cfg_path.display()));
            }
        };
        let t: toml::Value = text
            .parse()
            .with_context(|| format!("failed to parse `{}`", cfg_path.display()))?;
        let mut cfg = Self::default();
        if let Some(commit) = t.get("commit") {
            match commit.get("sign") {
                None => {}
                Some(v) => match v.as_bool() {
                    Some(b) => cfg.sign = b,
                    None => bail!(
                        "`commit.sign` in {} must be a boolean (true/false), found `{}` — refusing to guess intent",
                        cfg_path.display(),
                        v.type_str()
                    ),
                },
            }
        }
        Ok(cfg)
    }
}

pub(crate) fn is_harness_path(p: &str) -> bool {
    let p = p.trim().trim_start_matches("./").replace('\\', "/");
    p == ".review" || p.starts_with(".review/")
}

/// Refuse to operate on a repo with no commits: every harness git path
/// (`git diff HEAD`, `reset --hard`, `rev-parse HEAD`) fails with
/// confusing errors when HEAD doesn't exist. One clear message up front.
pub fn require_head(repo: &Path) -> Result<()> {
    let out = git_cmd(repo, &["rev-parse", "--verify", "--quiet", "HEAD"])
        .output()
        .with_context(|| "failed to run git rev-parse")?;
    if !out.status.success() {
        bail!(
            "this repository has no commits yet — gaggle needs at least one commit \
             (its diff/reset/commit machinery keys off HEAD). Make an initial commit \
             and re-run."
        );
    }
    Ok(())
}

/// Branch placement config from `.review/config.toml`.
///
/// `[branch] dedicated = true` → the run creates and commits on its own
/// `gaggle/run-<timestamp>` branch instead of the current branch, so a
/// human merges the run's work via PR/merge and `git revert` undoes a
/// whole run cleanly. Default: commit on the current branch.
#[derive(Debug, Clone, Default)]
pub struct BranchConfig {
    pub dedicated: bool,
}

impl BranchConfig {
    pub fn load(repo: &Path) -> Result<Self> {
        let cfg_path = repo.join(".review/config.toml");
        let text = match std::fs::read_to_string(&cfg_path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => {
                return Err(e).with_context(|| format!("failed to read `{}`", cfg_path.display()));
            }
        };
        let t: toml::Value = text
            .parse()
            .with_context(|| format!("failed to parse `{}`", cfg_path.display()))?;
        let mut cfg = Self::default();
        if let Some(b) = t.get("branch") {
            match b.get("dedicated") {
                None => {}
                Some(v) => match v.as_bool() {
                    Some(x) => cfg.dedicated = x,
                    None => bail!(
                        "`branch.dedicated` in {} must be a boolean, found `{}`",
                        cfg_path.display(),
                        v.type_str()
                    ),
                },
            }
        }
        Ok(cfg)
    }
}

/// Prefix for dedicated run branches.
pub const RUN_BRANCH_PREFIX: &str = "gaggle/run-";

/// Ensure the run has its own branch when `[branch] dedicated = true`.
///
/// Resume-aware: if HEAD is already on a `gaggle/run-*` branch (a resumed
/// run, or `gaggle run` invoked twice), REUSE it — creating a fresh branch
/// would strand the earlier commits of the same logical run. Returns the
/// branch name when dedicated mode is on and active, None otherwise.
pub fn ensure_run_branch(repo: &Path) -> Result<Option<String>> {
    let cfg = BranchConfig::load(repo)?;
    if !cfg.dedicated {
        return Ok(None);
    }
    let current = current_branch(repo)?;
    if let Some(name) = current {
        if name.starts_with(RUN_BRANCH_PREFIX) {
            println!("  branch: reusing dedicated run branch {name}");
            return Ok(Some(name));
        }
    }
    let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
    // Second-resolution timestamps collide when two runs start in the same
    // second (or a stale branch survived): `switch -c` would fail outright.
    // Suffix instead — the branch is still unique and sortable.
    let name = run_branch_name(repo, &ts.to_string())?;
    git(repo, &["switch", "-c", &name])?;
    println!(
        "  branch: created dedicated run branch {name} (commits land here, not your working branch)"
    );
    Ok(Some(name))
}

/// Pick a fresh `gaggle/run-<ts>[-n]` branch name. Separated from
/// [`ensure_run_branch`] (which stamps the current time) so the collision
/// walk is unit-testable with a fixed timestamp.
fn run_branch_name(repo: &Path, ts: &str) -> Result<String> {
    let base = format!("{RUN_BRANCH_PREFIX}{ts}");
    if !branch_exists(repo, &base) {
        return Ok(base);
    }
    for n in 2..=1000u32 {
        let name = format!("{base}-{n}");
        if !branch_exists(repo, &name) {
            return Ok(name);
        }
    }
    bail!("too many {base}* branches exist — delete stale gaggle/run-* branches and retry")
}

/// True when a local branch already exists (probe, not an error, so a
/// missing ref or a bare directory simply means "available").
fn branch_exists(repo: &Path, name: &str) -> bool {
    git_cmd(
        repo,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ],
    )
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

/// Current branch name (None in detached-HEAD state).
pub fn current_branch(repo: &Path) -> Result<Option<String>> {
    let out = git_cmd(repo, &["branch", "--show-current"])
        .output()
        .with_context(|| "failed to run git branch --show-current")?;
    if !out.status.success() {
        bail!(
            "git branch --show-current failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(if name.is_empty() { None } else { Some(name) })
}

/// Paths with uncommitted changes (tracked diffs vs HEAD + untracked,
/// excluding gitignored files and `.review/` harness state).
pub fn dirty_paths(repo: &Path) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    // `-z` (NUL-separated) everywhere: line output C-quotes paths with
    // non-ASCII/quotes/backslashes, which then fail as literal pathspecs.
    let tracked = git_z(repo, &["diff", "--name-only", "-z", "HEAD"])?;
    for line in tracked {
        if !is_harness_path(&line) {
            paths.push(line);
        }
    }
    let untracked = git_z(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
    for line in untracked {
        if !is_harness_path(&line) && !paths.iter().any(|p| p == &line) {
            paths.push(line);
        }
    }
    Ok(paths)
}

/// Best-effort `git reset -- path`. Used to drop harness files from the
/// index after a blanket `git add -A`. Failure is ignored: the path may
/// not be staged.
fn unstage(repo: &Path, path: &str) {
    let _ = git_cmd(repo, &["reset", "--quiet", "--", path]).output();
}

/// Commit whatever the worktree currently has dirty. Used after a fix that
/// started from a clean tree, so the dirty set is this component's edits
/// (adds, edits, and deletions). `git add -A` stages all of that; a
/// vanished file is a deletion, not a pathspec error.
///
/// `.review/` is never part of the commit (gitignore plus an unstage, so
/// already-tracked checklist/config files cannot sneak in). Harness-authored
/// `.gitignore` edits are also left out of the product commit.
pub fn commit_dirty(repo: &Path, message: &str) -> Result<String> {
    git(repo, &["add", "-A"])?;
    unstage(repo, ".review");
    unstage(repo, ".gitignore");

    let cfg = CommitConfig::load(repo)?;
    let probe = git_cmd(repo, &["diff", "--cached", "--quiet"])
        .output()
        .with_context(|| "failed to check staged diff")?;
    match probe.status.code() {
        Some(0) => return Ok(String::new()),
        Some(1) => {}
        _ => bail!(
            "git diff --cached --quiet failed (exit {:?}): {}",
            probe.status.code(),
            String::from_utf8_lossy(&probe.stderr).trim()
        ),
    }
    let commit_args: Vec<&str> = if cfg.sign {
        vec!["commit", "-m", message]
    } else {
        vec!["-c", "commit.gpgsign=false", "commit", "-m", message]
    };
    git(repo, &commit_args)?;
    git(repo, &["rev-parse", "--short", "HEAD"])
}

/// Reset the entire worktree: unstage, drop untracked files/dirs, discard
/// tracked modifications. Gitignored files are left alone.
///
/// `.review/` is exempt from `git clean` (harness state must survive a
/// reset), and `.gitignore` is exempt from the tracked discard
/// (ensure_gitignore may have appended our block this run).
pub fn reset_worktree(repo: &Path) -> Result<()> {
    git(repo, &["reset", "--quiet", "--", "."])?;
    // Clean first: `checkout -- .` fails on leftover untracked files.
    git(repo, &["clean", "-f", "-d", "-e", ".review"])?;
    let dirty = git_z(repo, &["diff", "--name-only", "-z", "HEAD"])?;
    let targets: Vec<&str> = dirty
        .iter()
        .map(String::as_str)
        .filter(|p| *p != ".gitignore")
        .collect::<Vec<_>>();
    if !targets.is_empty() {
        let mut args: Vec<&str> = vec!["checkout", "--"];
        args.extend(targets);
        git(repo, &args)?;
    }
    Ok(())
}

/// True if the worktree has changes outside `.review/` harness state.
pub fn is_dirty(repo: &Path) -> Result<bool> {
    // `-z`: NUL-separated records, no C-quoting. `-uall` overrides the
    // user's status.showUntrackedFiles config (with `no`, untracked files
    // would vanish from the output and a dirty tree would read clean).
    let records = git_z(repo, &["status", "--porcelain", "-z", "-uall"])?;
    let mut i = 0;
    while i < records.len() {
        let record = &records[i];
        if record.len() < 3 {
            i += 1;
            continue;
        }
        let status = &record[..2];
        let path = &record[3..];
        let is_rename = status.contains('R') || status.contains('C');
        if is_rename {
            // Rename/copy records are `R  <new>` followed by a bare `<old>`
            // record. The rename is invisible to the harness only when
            // BOTH ends are harness paths; a user file moved into or out
            // of .review/ is a real change either way.
            let old = records.get(i + 1).map(String::as_str).unwrap_or("");
            let both_harness = is_harness_path(path) && !old.is_empty() && is_harness_path(old);
            if !both_harness {
                return Ok(true);
            }
            i += 2; // consume the rename pair
            continue;
        }
        if !path.is_empty() && !is_harness_path(path) {
            return Ok(true);
        }
        i += 1;
    }
    Ok(false)
}

#[cfg(test)]
mod test_helpers {
    use super::*;

    pub fn unique_dir(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "gaggle-commit-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    pub fn init_repo(tag: &str) -> std::path::PathBuf {
        let dir = unique_dir(tag);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        git(&dir, &["config", "user.email", "gaggle@test"]).unwrap();
        git(&dir, &["config", "user.name", "gaggle"]).unwrap();
        // Deterministic line endings: Windows runners default to
        // core.autocrlf=true, which checks text files out as CRLF and
        // breaks byte-exact content assertions (local-only setting).
        git(&dir, &["config", "core.autocrlf", "false"]).unwrap();
        dir
    }

    pub fn base_commit(dir: &Path) {
        std::fs::write(dir.join("base.txt"), "base\n").unwrap();
        git(dir, &["add", "-A"]).unwrap();
        git(
            dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        )
        .unwrap();
    }
}

#[cfg(test)]
mod config_tests {
    use super::CommitConfig;
    use super::test_helpers::unique_dir;
    use crate::commit::BranchConfig;

    fn write_config(dir: &Path, body: &str) {
        let review = dir.join(".review");
        std::fs::create_dir_all(&review).unwrap();
        std::fs::write(review.join("config.toml"), body).unwrap();
    }
    use std::path::Path;

    #[test]
    fn commit_config_defaults_without_file() {
        let dir = unique_dir("cfg-missing");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!CommitConfig::load(&dir).unwrap().sign);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_config_reads_sign_true() {
        let dir = unique_dir("cfg-sign");
        std::fs::create_dir_all(&dir).unwrap();
        write_config(&dir, "[commit]\nsign = true\n");
        assert!(CommitConfig::load(&dir).unwrap().sign);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_config_rejects_non_bool_sign() {
        let dir = unique_dir("cfg-sign-bad");
        std::fs::create_dir_all(&dir).unwrap();
        write_config(&dir, "[commit]\nsign = \"yes\"\n");
        let err = CommitConfig::load(&dir).unwrap_err().to_string();
        assert!(err.contains("commit.sign"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn commit_config_rejects_corrupt_toml() {
        let dir = unique_dir("cfg-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        write_config(&dir, "not toml [[[\n");
        assert!(CommitConfig::load(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_config_defaults_and_reads_dedicated() {
        let dir = unique_dir("branch-missing");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!BranchConfig::load(&dir).unwrap().dedicated);
        write_config(&dir, "[branch]\ndedicated = true\n");
        assert!(BranchConfig::load(&dir).unwrap().dedicated);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn branch_config_rejects_non_bool() {
        let dir = unique_dir("branch-bad");
        std::fs::create_dir_all(&dir).unwrap();
        write_config(&dir, "[branch]\ndedicated = 1\n");
        let err = BranchConfig::load(&dir).unwrap_err().to_string();
        assert!(err.contains("branch.dedicated"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod git_tests {
    use super::test_helpers::{base_commit, init_repo, unique_dir};
    use crate::commit::*;

    #[test]
    fn require_head_rejects_empty_repo() {
        let dir = init_repo("no-head");
        let err = require_head(&dir).unwrap_err().to_string();
        assert!(err.contains("no commits yet"), "{err}");
        base_commit(&dir);
        require_head(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_branch_names_fresh_repo() {
        let dir = init_repo("branch");
        base_commit(&dir);
        let name = current_branch(&dir).unwrap().expect("branch name");
        assert!(!name.is_empty(), "expected a branch name");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn is_dirty_ignores_review_dir() {
        let dir = init_repo("dirty");
        base_commit(&dir);
        assert!(!is_dirty(&dir).unwrap());
        // Harness-only dirt is invisible.
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(dir.join(".review/state.json"), "{}\n").unwrap();
        assert!(!is_dirty(&dir).unwrap());
        // Product dirt is visible (tracked edit and untracked file).
        std::fs::write(dir.join("base.txt"), "changed\n").unwrap();
        assert!(is_dirty(&dir).unwrap());
        git(&dir, &["checkout", "--", "base.txt"]).unwrap();
        std::fs::write(dir.join("new.txt"), "new\n").unwrap();
        assert!(is_dirty(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dirty_paths_lists_product_changes_only() {
        let dir = init_repo("dirty-paths");
        base_commit(&dir);
        std::fs::write(dir.join("edit.txt"), "a\n").unwrap();
        git(&dir, &["add", "-A"]).unwrap();
        git(
            &dir,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "-m",
                "add edit",
            ],
        )
        .unwrap();
        std::fs::write(dir.join("edit.txt"), "b\n").unwrap();
        std::fs::write(dir.join("untracked.txt"), "u\n").unwrap();
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(dir.join(".review/note.txt"), "n\n").unwrap();
        let mut paths = dirty_paths(&dir).unwrap();
        paths.sort();
        assert_eq!(paths, vec!["edit.txt", "untracked.txt"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reset_worktree_restores_tracked_and_drops_untracked() {
        let dir = init_repo("reset");
        base_commit(&dir);
        std::fs::write(dir.join("base.txt"), "dirty\n").unwrap();
        std::fs::write(dir.join("scratch.txt"), "tmp\n").unwrap();
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(dir.join(".review/state.json"), "{}\n").unwrap();
        reset_worktree(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("base.txt")).unwrap(),
            "base\n"
        );
        assert!(!dir.join("scratch.txt").exists());
        assert!(dir.join(".review/state.json").exists());
        assert!(!is_dirty(&dir).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ensure_run_branch_off_returns_none() {
        let dir = init_repo("nobranch");
        base_commit(&dir);
        assert_eq!(ensure_run_branch(&dir).unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_branch_name_suffixes_on_collision() {
        let dir = init_repo("branchname");
        base_commit(&dir);
        // No collision: the plain timestamp name.
        assert_eq!(
            run_branch_name(&dir, "20240101-000000").unwrap(),
            "gaggle/run-20240101-000000"
        );
        // Two runs in the same second (or a stale branch): walk -2, -3…
        git(&dir, &["branch", "gaggle/run-20240101-000000"]).unwrap();
        git(&dir, &["branch", "gaggle/run-20240101-000000-2"]).unwrap();
        assert_eq!(
            run_branch_name(&dir, "20240101-000000").unwrap(),
            "gaggle/run-20240101-000000-3"
        );
        // Outside any repo every name is "available" — never an error.
        let bare = unique_dir("branchname-bare");
        std::fs::create_dir_all(&bare).unwrap();
        assert_eq!(
            run_branch_name(&bare, "20240101-000000").unwrap(),
            "gaggle/run-20240101-000000"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&bare);
    }

    #[test]
    fn ensure_run_branch_creates_and_reuses() {
        let dir = init_repo("runbranch");
        base_commit(&dir);
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(
            dir.join(".review/config.toml"),
            "[branch]\ndedicated = true\n",
        )
        .unwrap();
        let first = ensure_run_branch(&dir).unwrap().expect("branch name");
        assert!(first.starts_with(RUN_BRANCH_PREFIX), "{first}");
        let second = ensure_run_branch(&dir).unwrap().expect("branch name");
        assert_eq!(first, second, "resumed run must reuse its branch");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_paths() {
        assert!(is_harness_path(".review/checklist.md"));
        assert!(is_harness_path(".review/state.json"));
        assert!(is_harness_path("./.review/foo"));
        assert!(!is_harness_path("src/main.rs"));
        assert!(!is_harness_path(".gitignore"));
    }

    #[test]
    fn porcelain_extracts_path() {
        // Line form (kept for reference/debugging); is_dirty itself uses -z.
        let line = |s: &str| s.get(3..).unwrap_or("").to_string();
        assert_eq!(line(" M src/foo.rs"), "src/foo.rs");
    }

    #[test]
    fn commit_dirty_stages_deletions_and_skips_review_dir() {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-commit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "a\n").unwrap();
        std::fs::write(dir.join("src/b.rs"), "b\n").unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        git(&dir, &["config", "user.email", "gaggle@test"]).unwrap();
        git(&dir, &["config", "user.name", "gaggle"]).unwrap();
        git(&dir, &["add", "-A"]).unwrap();
        git(
            &dir,
            &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "base"],
        )
        .unwrap();

        std::fs::remove_file(dir.join("src/b.rs")).unwrap();
        std::fs::write(dir.join("src/c.rs"), "c\n").unwrap();
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(dir.join(".review/checklist.md"), "nope\n").unwrap();

        let hash = commit_dirty(&dir, "fix").unwrap();
        assert!(!hash.is_empty(), "expected a commit hash");

        let names = git(
            &dir,
            &["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"],
        )
        .unwrap();
        assert!(names.contains("src/c.rs"), "{names}");
        assert!(
            names.contains("src/b.rs"),
            "deletion should be in the commit: {names}"
        );
        assert!(
            !names.contains(".review"),
            "harness files must not be committed: {names}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
