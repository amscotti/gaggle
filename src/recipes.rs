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

const GITIGNORE_MARK: &str = "# gaggle: keep durable config; ignore run state";
const GITIGNORE_BLOCK: &str = "\
# gaggle: keep durable config; ignore run state
.review/*
!.review/config.toml
!.review/checklist.md
!.review/workflows/
!.review/workflows/**
";

const NAMES: &[&str] = &[
    "discover.yaml",
    "review.yaml",
    "confirm.yaml",
    "fix.yaml",
    "verify.yaml",
    "report.yaml",
];

const DISCOVER: &str = include_str!("../workflows/discover.yaml");
const REVIEW: &str = include_str!("../workflows/review.yaml");
const CONFIRM: &str = include_str!("../workflows/confirm.yaml");
const FIX: &str = include_str!("../workflows/fix.yaml");
const VERIFY: &str = include_str!("../workflows/verify.yaml");
const REPORT: &str = include_str!("../workflows/report.yaml");

fn source(name: &str) -> Result<&'static str> {
    match name {
        "discover.yaml" => Ok(DISCOVER),
        "review.yaml" => Ok(REVIEW),
        "confirm.yaml" => Ok(CONFIRM),
        "fix.yaml" => Ok(FIX),
        "verify.yaml" => Ok(VERIFY),
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

/// Detect verify commands from repo markers.
///
/// Honest defaults: only claim a command when the repo actually supports
/// it. For package.json we require a `test` script (else `npm test` fails
/// with 'Missing script: test'). When NO language is recognized we emit an
/// explicitly-failing placeholder (`false`) rather than a wrong-language
/// command — a verify gate that spuriously fails every fix is preferable
/// to one that silently fakes green or blames the wrong toolchain; init
/// warns loudly and the config comment tells the user to edit it.
pub fn detect_verify_commands(repo: &Path) -> Vec<String> {
    if repo.join("go.mod").exists() {
        vec!["go test ./...".to_string()]
    } else if repo.join("Cargo.toml").exists() {
        vec!["cargo build".to_string(), "cargo test".to_string()]
    } else if repo.join("package.json").exists() {
        if package_json_has_test_script(repo) {
            vec!["npm test".to_string()]
        } else {
            vec![UNKNOWN_REPO_PLACEHOLDER.to_string()]
        }
    } else if repo.join("pyproject.toml").exists()
        || repo.join("pytest.ini").exists()
        || repo.join("setup.py").exists()
    {
        vec!["pytest".to_string()]
    } else {
        vec![UNKNOWN_REPO_PLACEHOLDER.to_string()]
    }
}

/// Placeholder for unrecognized repos: fails with a message pointing at
/// the config instead of running a wrong-language toolchain.
const UNKNOWN_REPO_PLACEHOLDER: &str =
    "false # gaggle: no verify command detected — edit verify in .review/config.toml";

/// Minimal check that package.json declares a REAL `test` script. An empty
/// string (or non-string) value would make `npm test` run an empty command
/// and exit 0 — a silently fake-green verify gate.
fn package_json_has_test_script(repo: &Path) -> bool {
    let Ok(text) = fs::read_to_string(repo.join("package.json")) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| {
            v.get("scripts")
                .and_then(|s| s.get("test"))
                .and_then(|t| t.as_str())
                .map(|s| !s.trim().is_empty())
        })
        .unwrap_or(false)
}

/// Config text for a fresh `.review/config.toml`: `config.toml.example`
/// (DEFAULT_CONFIG) with its `verify = [...]` line swapped for commands
/// detected from the repo. Keeping the example as the single template
/// means its comments/settings always match what init writes.
pub fn default_config_for(repo: &Path) -> String {
    let cmds = detect_verify_commands(repo);
    let quoted: Vec<String> = cmds.iter().map(|c| format!("\"{c}\"")).collect();
    let out = DEFAULT_CONFIG;
    let new_line = format!("verify = [{}]", quoted.join(", "));
    // Walk lines (not byte offsets): `lines().map(|l| l.len() + 1)` assumes
    // LF, so on a CRLF checkout (Windows `core.autocrlf`) the replacement
    // landed mid-comment. `split_inclusive` keeps the original ending.
    let mut result = String::new();
    let mut replaced = false;
    for line in out.split_inclusive('\n') {
        let content = line.trim_end_matches(['\n', '\r']);
        let is_verify = content.trim_start_matches('#').trim() == "verify = []"
            || content.starts_with("verify = [");
        if is_verify && !replaced {
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
        // The example template drifted (no verify line) — append so
        // the config is still valid rather than silently unverified.
        eprintln!("  ⚠ config.toml.example has no `verify = [` line — appending one");
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&new_line);
        result.push('\n');
    }
    result
}

/// Create `.review/config.toml` from a language-detected default when missing.
pub fn ensure_config(repo: &Path) -> Result<()> {
    let path = repo.join(".review/config.toml");
    if path.exists() {
        return Ok(());
    }
    if detect_verify_commands(repo)
        .iter()
        .any(|c| c == UNKNOWN_REPO_PLACEHOLDER)
    {
        eprintln!(
            "  ⚠ no verify command recognized for this repo — writing a FAILING placeholder; \
             edit `verify` in .review/config.toml before `gaggle run`"
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, default_config_for(repo))?;
    Ok(())
}

/// Append gaggle's `.review/` gitignore rules when the repo does not
/// already handle that directory correctly.
pub fn ensure_gitignore(repo: &Path) -> Result<()> {
    let path = repo.join(".gitignore");
    let existing = fs::read_to_string(&path).unwrap_or_default();
    if gitignore_has_review_rules(&existing) {
        return Ok(());
    }
    // A BARE `.review` (or `/.review`, `.review/`, `/.review/`) entry
    // ignores the directory itself — and git cannot re-include files whose
    // parent directory is excluded, so our negations (`!.review/config.toml`
    // etc.) would be dead rules and the durable config would stay
    // untrackable. Replace the bare entry with the proper block.
    let bare_variants = [".review", "/.review", ".review/", "/.review/"];
    let has_bare = existing.lines().any(|l| bare_variants.contains(&l.trim()));
    let mut out = existing;
    if has_bare {
        eprintln!(
            "  ⚠ .gitignore ignores `.review` as a whole directory — git cannot re-include \
             files under it; replacing that entry with keep-config rules"
        );
        // Drop the bare lines (and any trailing blank duplication), then
        // append the full block.
        out = out
            .lines()
            .filter(|l| !bare_variants.contains(&l.trim()))
            .collect::<Vec<_>>()
            .join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
    }
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

/// True when the existing gitignore already covers `.review` the way the
/// keep-config block does (`.review/*` + negations) — i.e. appending would
/// be redundant. Bare directory-only entries do NOT count (see
/// [`ensure_gitignore`]).
fn gitignore_has_review_rules(text: &str) -> bool {
    // LEGACY_MARK: repos bootstrapped before the sift→gaggle rename carry the
    // old marker comment atop an identical rule block — treat it as present so
    // we never append a second block.
    const LEGACY_MARK: &str = "# sift: keep durable config; ignore run state";
    if text.contains(GITIGNORE_MARK) || text.contains(LEGACY_MARK) {
        return true;
    }
    // `.review/*` ignores only the CONTENTS, so negations CAN re-include —
    // but only if negations actually exist. A lone `.review/*` with no
    // `!.review/…` lines keeps the durable config ignored forever; append
    // our full block in that case.
    let has_star = text
        .lines()
        .any(|l| matches!(l.trim(), ".review/*" | "/.review/*"));
    let has_negation = text.lines().any(|l| {
        let t = l.trim();
        t.starts_with("!") && (t.contains(".review/") || t == "!.review")
    });
    has_star && has_negation
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
    fn detect_go_vs_cargo() {
        let repo = temp_repo();
        fs::write(repo.join("go.mod"), "module x\n").unwrap();
        assert_eq!(detect_verify_commands(&repo), vec!["go test ./..."]);
        fs::remove_file(repo.join("go.mod")).unwrap();
        fs::write(repo.join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert_eq!(
            detect_verify_commands(&repo),
            vec!["cargo build", "cargo test"]
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
        assert!(text.contains("!.review/checklist.md"));
        let _ = fs::remove_dir_all(&repo);
    }
}

#[cfg(test)]
mod gitignore_negation_tests {
    use super::*;

    #[test]
    fn lone_review_star_does_not_count_without_negations() {
        // A lone `.review/*` keeps config untracked — must NOT be treated
        // as covered; ensure_gitignore should append the keep-config block.
        assert!(!gitignore_has_review_rules(".review/*\n/target\n"));
        // With a negation, the user has handled it deliberately.
        assert!(gitignore_has_review_rules(
            ".review/*\n!.review/config.toml\n"
        ));
        assert!(gitignore_has_review_rules(
            "/.review/*\n!.review/checklist.md\n"
        ));
        // Marker comment always counts (ours or the legacy sift one).
        assert!(gitignore_has_review_rules(
            "# gaggle: keep durable config; ignore run state\n.review/*\n"
        ));
    }
}

#[cfg(test)]
mod final_verify_template_tests {
    use super::*;

    #[test]
    fn default_config_swaps_verify_not_final_verify() {
        let dir = std::env::temp_dir().join(format!("gaggle-fv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("go.mod"), "module x\n").unwrap();
        let cfg = default_config_for(&dir);
        // The ACTIVE verify line carries the detected command.
        // Line-based: a CRLF checkout must not fail a `\n…\n` substring check.
        assert!(
            cfg.lines().any(|l| l == "verify = [\"go test ./...\"]"),
            "verify line wrong:\n{cfg}"
        );
        // The commented final_verify example survives untouched.
        assert!(
            cfg.contains(
                "# final_verify = [\"cargo build --workspace\", \"cargo test --workspace\"]"
            ),
            "final_verify example corrupted:\n{cfg}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
