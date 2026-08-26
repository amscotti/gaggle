//! Embedded Goose recipes and the default `.review/config.toml`.
//!
//! YAML sources live in `workflows/` and are baked in at compile time so a
//! shipped `gaggle` binary does not need those files next to the target repo.
//! A repo may override any recipe by placing a file at
//! `.review/workflows/<name>.yaml`; otherwise the embedded copy is written
//! to a per-process temp dir for `goose run --recipe`.

use anyhow::{Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT_CONFIG: &str = include_str!("../config.toml.example");

const GITIGNORE_MARK: &str = "# gaggle: ignore the review directory";
const GITIGNORE_BLOCK: &str = "\
# gaggle: ignore the review directory
.review/
";

const NAMES: &[&str] = &[
    "discover.yaml",
    "discover-validate.yaml",
    "review.yaml",
    "confirm.yaml",
    "fix.yaml",
    "verify.yaml",
    "gate.yaml",
    "report.yaml",
];

const DISCOVER: &str = include_str!("../workflows/discover.yaml");
const DISCOVER_VALIDATE: &str = include_str!("../workflows/discover-validate.yaml");
const REVIEW: &str = include_str!("../workflows/review.yaml");
const CONFIRM: &str = include_str!("../workflows/confirm.yaml");
const FIX: &str = include_str!("../workflows/fix.yaml");
const VERIFY: &str = include_str!("../workflows/verify.yaml");
const GATE: &str = include_str!("../workflows/gate.yaml");
const REPORT: &str = include_str!("../workflows/report.yaml");

fn source(name: &str) -> Result<&'static str> {
    match name {
        "discover.yaml" => Ok(DISCOVER),
        "discover-validate.yaml" => Ok(DISCOVER_VALIDATE),
        "review.yaml" => Ok(REVIEW),
        "confirm.yaml" => Ok(CONFIRM),
        "fix.yaml" => Ok(FIX),
        "verify.yaml" => Ok(VERIFY),
        "gate.yaml" => Ok(GATE),
        "report.yaml" => Ok(REPORT),
        other => bail!("unknown embedded recipe: {other}"),
    }
}

fn override_path(repo: &Path, name: &str) -> PathBuf {
    repo.join(".review/workflows").join(name)
}

/// Known recipe names that have a file in `.review/workflows/`.
pub fn list_overrides(repo: &Path) -> Vec<&'static str> {
    NAMES
        .iter()
        .copied()
        .filter(|name| override_path(repo, name).is_file())
        .collect()
}

/// Resolve a recipe: repo override if present, otherwise the embedded copy.
pub fn path(repo: &Path, name: &str) -> Result<PathBuf> {
    let _ = source(name)?;
    let over = override_path(repo, name);
    if over.is_file() {
        return Ok(over);
    }
    materialize_embedded(name)
}

/// Per-process temp dir for materialized embedded recipes. Lazily created
/// with create_dir (fails if the path exists — combined with the pid+nanos
/// name this is race- and symlink-safe: an attacker cannot pre-plant the
/// exact directory), and removable by [`cleanup_temp`] (later phases that
/// still need recipes simply re-create it).
static TEMP_DIR: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

fn temp_recipe_dir() -> Result<PathBuf> {
    let mut guard = TEMP_DIR.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(d) = guard.as_ref() {
        if d.is_dir() {
            return Ok(d.clone());
        }
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("gaggle-recipes-{}-{nanos}", std::process::id()));
    fs::create_dir(&dir)?;
    *guard = Some(dir.clone());
    Ok(dir)
}

/// Best-effort removal of this process's materialized recipe temp dir.
/// Called when the loop finishes; recipes re-materialize on demand if a
/// later phase still needs them.
pub fn cleanup_temp() {
    let mut guard = TEMP_DIR.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(dir) = guard.take() {
        let _ = fs::remove_dir_all(dir);
    }
}

fn materialize_embedded(name: &str) -> Result<PathBuf> {
    let body = source(name)?;
    let dir = temp_recipe_dir()?;
    let dest = dir.join(name);
    if dest.exists() {
        let _ = fs::remove_file(&dest);
    }
    // O_EXCL semantics: fails instead of following a pre-planted symlink.
    let mut f = fs::File::create_new(&dest)?;
    std::io::Write::write_all(&mut f, body.as_bytes())?;
    Ok(dest)
}

/// Config text for a fresh `.review/config.toml`. Discovery copies the
/// repo's real test commands into `verify` / `final_verify`. Until then
/// the placeholder fails closed.
pub fn default_config_for(_repo: &Path) -> String {
    DEFAULT_CONFIG.to_string()
}

/// Create `.review/config.toml` from the baked-in template when missing.
pub fn ensure_config(repo: &Path) -> Result<()> {
    let path = repo.join(".review/config.toml");
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, default_config_for(repo))?;
    Ok(())
}

/// Write discovery's project-wide gate commands into `.review/config.toml`.
/// Empty `verify` leaves the placeholder and warns. Empty `final_verify`
/// copies `verify` so both gates match unless the agent named a slower one.
pub fn apply_discovered_gates(
    repo: &Path,
    verify: &[String],
    final_verify: &[String],
) -> Result<()> {
    if verify.is_empty() {
        eprintln!(
            "  ⚠ discovery did not name project-wide verify commands — \
             edit `verify` in .review/config.toml before `gaggle run`"
        );
        return Ok(());
    }
    let path = repo.join(".review/config.toml");
    let text = fs::read_to_string(&path)?;
    let final_cmds = if final_verify.is_empty() {
        verify
    } else {
        final_verify
    };
    let text = replace_array_key(&text, "verify", verify);
    let text = replace_array_key(&text, "final_verify", final_cmds);
    fs::write(&path, text)?;
    println!("  project verify: {}", verify.join(" ; "));
    if final_cmds != verify {
        println!("  final verify: {}", final_cmds.join(" ; "));
    }
    Ok(())
}

fn replace_array_key(text: &str, key: &str, cmds: &[String]) -> String {
    let quoted: Vec<String> = cmds
        .iter()
        .map(|c| format!("\"{}\"", c.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    let new_line = format!("{key} = [{}]", quoted.join(", "));
    let active = format!("{key} = [");
    let commented = format!("# {key} = [");
    let mut result = String::new();
    let mut replaced = false;
    for line in text.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let trimmed = content.trim_start();
        if !replaced && (trimmed.starts_with(&active) || trimmed.starts_with(&commented)) {
            replaced = true;
            result.push_str(&new_line);
            if line.ends_with("\r\n") {
                result.push_str("\r\n");
            } else if line.ends_with('\n') {
                result.push('\n');
            }
        } else {
            result.push_str(line);
        }
    }
    if !replaced {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&new_line);
        result.push('\n');
    }
    result
}

/// Append gaggle's `.review/` gitignore rule when the repo does not
/// already ignore that directory.
pub fn ensure_gitignore(repo: &Path) -> Result<()> {
    let path = repo.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if gitignore_has_review_rules(&existing) {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(GITIGNORE_BLOCK);
    fs::write(path, out)?;
    Ok(())
}

/// True when `.review/` is already ignored (directory entry, contents
/// glob, or a prior gaggle/sift marker). Appending would be redundant.
fn gitignore_has_review_rules(text: &str) -> bool {
    const LEGACY_MARK: &str = "# sift: keep durable config; ignore run state";
    const LEGACY_GAGGLE_MARK: &str = "# gaggle: keep durable config; ignore run state";
    if text.contains(GITIGNORE_MARK)
        || text.contains(LEGACY_MARK)
        || text.contains(LEGACY_GAGGLE_MARK)
    {
        return true;
    }
    text.lines().any(|l| {
        matches!(
            l.trim(),
            ".review" | "/.review" | ".review/" | "/.review/" | ".review/*" | "/.review/*"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_repo() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-recipes-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn all_recipes_embed_and_materialize() {
        let repo = temp_repo();
        for name in NAMES {
            let src = source(name).unwrap();
            assert!(src.contains("version:"), "{name} looks empty");
            let p = path(&repo, name).unwrap();
            assert!(p.exists(), "{name} was not written");
            assert!(
                !p.starts_with(repo.join(".review")),
                "{name} should use the embedded temp copy when no override exists"
            );
        }
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn unknown_recipe_errors() {
        let repo = temp_repo();
        assert!(path(&repo, "nope.yaml").is_err());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn repo_override_wins() {
        let repo = temp_repo();
        let dir = repo.join(".review/workflows");
        fs::create_dir_all(&dir).unwrap();
        let custom = dir.join("review.yaml");
        fs::write(&custom, "version: 1.0.0\nid: custom-review\n").unwrap();
        let resolved = path(&repo, "review.yaml").unwrap();
        assert_eq!(resolved, custom);
        assert_eq!(list_overrides(&repo), vec!["review.yaml"]);
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn unknown_name_does_not_read_override() {
        let repo = temp_repo();
        let dir = repo.join(".review/workflows");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("nope.yaml"), "id: nope\n").unwrap();
        assert!(path(&repo, "nope.yaml").is_err());
        assert!(list_overrides(&repo).is_empty());
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn apply_discovered_gates_writes_both_keys() {
        let repo = temp_repo();
        fs::create_dir_all(repo.join(".review")).unwrap();
        fs::write(
            repo.join(".review/config.toml"),
            "verify = [\"false\"]\n# final_verify = [\"./slow.sh\"]\n",
        )
        .unwrap();
        apply_discovered_gates(&repo, &["./scripts/check.sh".to_string()], &[]).unwrap();
        let cfg = fs::read_to_string(repo.join(".review/config.toml")).unwrap();
        assert!(
            cfg.lines()
                .any(|l| l == "verify = [\"./scripts/check.sh\"]"),
            "{cfg}"
        );
        assert!(
            cfg.lines()
                .any(|l| l == "final_verify = [\"./scripts/check.sh\"]"),
            "{cfg}"
        );
        let _ = fs::remove_dir_all(&repo);
    }

    #[test]
    fn gitignore_appends_once() {
        let repo = temp_repo();
        fs::write(repo.join(".gitignore"), "/target\n").unwrap();
        ensure_gitignore(&repo).unwrap();
        ensure_gitignore(&repo).unwrap();
        let text = fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert_eq!(text.matches(GITIGNORE_MARK).count(), 1);
        assert!(text.contains(".review/"));
        assert!(!text.contains("!.review/checklist.md"));
        let _ = fs::remove_dir_all(&repo);
    }
}

#[cfg(test)]
mod gitignore_negation_tests {
    use super::*;

    #[test]
    fn any_review_ignore_counts_as_covered() {
        assert!(gitignore_has_review_rules(".review/\n/target\n"));
        assert!(gitignore_has_review_rules(".review\n"));
        assert!(gitignore_has_review_rules(".review/*\n/target\n"));
        assert!(gitignore_has_review_rules(
            ".review/*\n!.review/config.toml\n"
        ));
        assert!(gitignore_has_review_rules(
            "# gaggle: ignore the review directory\n.review/\n"
        ));
        assert!(gitignore_has_review_rules(
            "# gaggle: keep durable config; ignore run state\n.review/*\n"
        ));
        assert!(!gitignore_has_review_rules("/target\n"));
    }
}

#[cfg(test)]
mod final_verify_template_tests {
    use super::*;

    #[test]
    fn default_config_is_language_neutral() {
        let dir = std::env::temp_dir().join(format!("gaggle-fv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg = default_config_for(&dir);
        assert!(
            cfg.lines().any(|l| l == "verify = [\"false\"]"),
            "verify line wrong:\n{cfg}"
        );
        assert!(
            cfg.contains("# final_verify = [\"./scripts/e2e.sh\"]"),
            "final_verify example corrupted:\n{cfg}"
        );
        assert!(
            cfg.lines().any(|l| l == "# verify_stall_secs = 900"),
            "verify_stall_secs example missing:\n{cfg}"
        );
        assert!(
            cfg.lines().any(|l| l == "# verify_timeout_secs = 14400"),
            "verify_timeout_secs example missing:\n{cfg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
