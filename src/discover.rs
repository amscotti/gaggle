//! AI component discovery: run the discover recipe, parse the agent's JSON
//! component proposals, validate/normalize them (ported from the Go
//! NormalizeDiscovery). The agent invents; the harness decides what is
//! usable. Deterministic guards only — no model judgment here.

use crate::goose;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

/// Hard caps for AI-proposed components (guards runaway splits).
pub const MAX_COMPONENTS: usize = 40;
pub const MIN_COMPONENTS: usize = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredComponent {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub paths: Vec<String>,
    pub tier: String,
    pub priority: u32,
    /// Shell commands that check this slice. Empty = repo-wide `verify`.
    pub verify: Vec<String>,
}

/// Full discovery result: project-wide gates plus components.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub verify: Vec<String>,
    pub final_verify: Vec<String>,
    pub components: Vec<DiscoveredComponent>,
}

/// Run the discovery recipe and return validated components plus the
/// project-wide verify commands the agent copied from this repo's own
/// test/CI docs. The harness does not invent those commands.
///
/// `existing_file` is a path the recipe should open — never inline
/// checklist text (newlines/`=` are rejected by `--params`).
pub fn discover(repo: &Path, project_name: &str, existing_file: &Path) -> Result<Discovery> {
    let recipe = crate::recipes::path(repo, "discover.yaml")?;
    let existing = existing_file.to_string_lossy();
    let params = [
        ("project_name", project_name),
        ("existing", existing.as_ref()),
    ];
    let outcome = goose::run_recipe(repo, &recipe, &params, Some(80))?;
    let payload = parse_discovery_payload(&outcome.result)?;
    Ok(Discovery {
        verify: payload.verify,
        final_verify: payload.final_verify,
        components: normalize(payload.components)?,
    })
}

/// Write the first-pass proposal so the validate recipe can open it.
pub fn write_proposal(path: &Path, d: &Discovery) -> Result<()> {
    let body = serde_json::json!({
        "verify": d.verify,
        "final_verify": d.final_verify,
        "components": d.components,
    });
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string_pretty(&body)? + "\n")?;
    Ok(())
}

/// Second agent pass: check the proposal against the tree and return a
/// corrected payload. If the validator flakes or returns no components,
/// the original proposal is kept.
pub fn validate(
    repo: &Path,
    project_name: &str,
    proposal: &Path,
    original: Discovery,
) -> Result<Discovery> {
    let recipe = crate::recipes::path(repo, "discover-validate.yaml")?;
    let proposal_s = proposal.to_string_lossy().to_string();
    let params = [
        ("project_name", project_name),
        ("proposal", proposal_s.as_str()),
    ];
    let outcome = match goose::run_recipe(repo, &recipe, &params, Some(80)) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("  warning: discovery validate failed ({e:#}) — keeping first-pass proposal");
            return Ok(original);
        }
    };
    let payload = match parse_discovery_payload(&outcome.result) {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "  warning: discovery validate returned unusable JSON ({e:#}) — keeping first-pass proposal"
            );
            return Ok(original);
        }
    };
    if payload.components.is_empty() {
        eprintln!(
            "  warning: discovery validate returned no components — keeping first-pass proposal"
        );
        return Ok(original);
    }
    let ok = outcome
        .result
        .get("ok")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let components = normalize(payload.components)?;
    if ok {
        println!(
            "  discovery validate: ok ({} component(s))",
            components.len()
        );
    } else {
        println!(
            "  discovery validate: corrected to {} component(s)",
            components.len()
        );
    }
    Ok(Discovery {
        verify: if payload.verify.is_empty() {
            original.verify
        } else {
            payload.verify
        },
        final_verify: if payload.final_verify.is_empty() {
            original.final_verify
        } else {
            payload.final_verify
        },
        components,
    })
}

#[derive(Debug)]
pub(crate) struct DiscoveryPayload {
    pub verify: Vec<String>,
    pub final_verify: Vec<String>,
    pub components: Vec<RawItem>,
}

fn string_cmds(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|item| item.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Some(other) => {
            eprintln!("  discover: ignoring non-array `{other}` for a command list");
            Vec::new()
        }
    }
}

/// Contract: `{"verify": [...], "final_verify": [...], "components": [ ... ]}`.
/// A bare ARRAY of components is accepted (gates then empty). A single
/// component OBJECT is a truncated answer — not a list.
pub(crate) fn parse_discovery_payload(result: &serde_json::Value) -> Result<DiscoveryPayload> {
    let verify = string_cmds(result.get("verify"));
    let final_verify = string_cmds(result.get("final_verify"));
    let raw = if let Some(c) = result.get("components") {
        if !c.is_array() {
            bail!("discovery \"components\" must be an array, got {c}");
        }
        c.clone()
    } else if result.is_array() {
        eprintln!(
            "  discover: agent returned a bare array (not wrapped in {{\"components\": …}}) — accepting"
        );
        result.clone()
    } else if result.get("slug").is_some() && result.get("paths").is_some() {
        bail!(
            "discovery returned a single component object instead of {{\"components\": [...]}} \
             (truncated answer)"
        );
    } else {
        bail!("discovery result missing \"components\" key: {result}");
    };
    let components = serde_json::from_value(raw)
        .map_err(|e| anyhow::anyhow!("failed to parse discovery components: {e}"))?;
    Ok(DiscoveryPayload {
        verify,
        final_verify,
        components,
    })
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawItem {
    #[serde(default)]
    slug: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    /// Lenient: the agent plausibly sends a single string ("paths":
    /// "src/x.rs") — a custom deserializer accepts string-or-array so one
    /// odd item never aborts the whole discovery.
    #[serde(default, deserialize_with = "de_paths")]
    paths: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)] // singular alias
    path: String,
    #[serde(default, deserialize_with = "de_paths")]
    verify: Vec<String>,
    #[serde(default)]
    tier: String,
    /// Lenient by design ("agent invents, harness decides"): a plausible
    /// variant like "80" (string) or 80.0 (float) must not abort the whole
    /// discovery. Everything else in RawItem is already lenient.
    #[serde(default, deserialize_with = "de_priority")]
    priority: u32,
}

fn de_paths<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    match v {
        serde_json::Value::Null => Ok(Vec::new()),
        serde_json::Value::String(s) => Ok(vec![s]),
        serde_json::Value::Array(a) => Ok(a
            .into_iter()
            .filter_map(|item| match item {
                serde_json::Value::String(s) => Some(s),
                // Non-string array entries are dropped, not fatal.
                other => {
                    eprintln!("  discover: dropping non-string path entry {other}");
                    None
                }
            })
            .collect()),
        other => {
            // Any other shape (number, object) — degrade to empty rather
            // than aborting; the no-usable-paths rejection handles it.
            eprintln!("  discover: unparseable paths {other} — treating as empty");
            Ok(Vec::new())
        }
    }
}

fn de_priority<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v = serde_json::Value::deserialize(deserializer)?;
    // Fully lenient — a single odd priority variant must never abort the
    // whole discovery: numbers clamp, numeric strings ("80", "80.0", "1e2",
    // "+5") parse via f64, and anything unparseable degrades to 0 (the
    // tier default then applies downstream).
    let parsed = match &v {
        serde_json::Value::Null => Some(0.0),
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    match parsed {
        Some(f) => Ok(f.clamp(0.0, u32::MAX as f64) as u32),
        None => {
            eprintln!("  discover: unparseable priority {v} — treating as 0");
            Ok(0)
        }
    }
}

/// Validate + normalize raw agent items into usable components.
/// Rejects bad slugs, empty path sets, escape paths, absolute paths,
/// duplicates; applies tier/priority defaults; enforces min/max bounds.
pub(crate) fn normalize(items: Vec<RawItem>) -> Result<Vec<DiscoveredComponent>> {
    let mut out: Vec<DiscoveredComponent> = Vec::new();
    // Slug → index into `out` for O(1) duplicate lookup: the raw list is
    // model-controlled and unbounded at this point (MAX_COMPONENTS applies
    // only after the loop), so a per-item linear scan is O(n²).
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for it in items {
        let slug = normalize_slug(&it.slug, &it.name, &it.paths);
        if slug.is_empty() || !valid_slug(&slug) {
            eprintln!(
                "  discover: rejected item (bad slug {:?}, name {:?})",
                it.slug, it.name
            );
            continue;
        }
        let paths = collect_paths(&it);
        if paths.is_empty() {
            eprintln!("  discover: rejected item {slug} (no usable paths)");
            continue;
        }
        if let Some(&i) = index.get(&slug) {
            // Merge paths of a duplicate slug into the existing component
            // rather than discarding the later item entirely.
            let existing = &mut out[i];
            for p in &paths {
                if !existing.paths.contains(p) {
                    existing.paths.push(p.clone());
                }
            }
            for v in collect_verify(&it) {
                if !existing.verify.contains(&v) {
                    existing.verify.push(v);
                }
            }
            // Keep the strongest tier (high > medium > low) and the higher
            // priority so the surviving component reflects agent intent
            // instead of whichever item happened to come first.
            let new_tier = crate::checklist::Tier::parse(&it.tier);
            let mut tier_upgraded = false;
            if new_tier.is_stronger_than(crate::checklist::Tier::parse(&existing.tier)) {
                existing.tier = new_tier.as_str().to_string();
                tier_upgraded = true;
            }
            if it.priority > existing.priority {
                existing.priority = it.priority;
            }
            // A tier upgrade must also raise the priority FLOOR: an
            // existing low/10 merged with a high duplicate keeps tier=high
            // but priority=10, sorting below medium/50 and risking
            // truncation at MAX_COMPONENTS. Applied ONLY on an actual tier
            // upgrade — running it on every merge would silently override
            // an explicit priority (low/5 + same-tier dup → 10).
            if tier_upgraded {
                let floor = crate::checklist::Tier::parse(&existing.tier).default_priority();
                if existing.priority < floor {
                    existing.priority = floor;
                }
            }
            eprintln!(
                "  discover: merged duplicate slug {slug} ({} paths)",
                paths.len()
            );
            continue;
        }
        let tier = crate::checklist::Tier::parse(&it.tier);
        let name = if it.name.trim().is_empty() {
            slug.clone()
        } else {
            // Newlines cannot round-trip through the line-based checklist
            // format (and could inject phantom entries) — collapse to
            // spaces instead of rejecting: the name is display-only.
            it.name.trim().replace(['\n', '\r'], " ")
        };
        let priority = if it.priority > 0 {
            it.priority
        } else {
            tier.default_priority()
        };
        index.insert(slug.clone(), out.len());
        let verify = collect_verify(&it);
        out.push(DiscoveredComponent {
            slug,
            name,
            description: it.description.trim().to_string(),
            paths,
            tier: tier.as_str().to_string(),
            priority,
            verify,
        });
    }
    if out.len() < MIN_COMPONENTS {
        bail!(
            "discovery produced {} usable component(s); need at least {MIN_COMPONENTS}",
            out.len()
        );
    }
    // Sort by priority desc (importance), then slug for stability.
    out.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.slug.cmp(&b.slug))
    });
    // Truncate after sorting so the highest-priority components survive.
    out.truncate(MAX_COMPONENTS);
    Ok(out)
}

/// Valid component slug: lowercase a-z0-9 hyphen-joined segments. Shared
/// with the CLI so `--components` entries get the same validation the
/// discovery path applies (a slug like `../evil` must never reach
/// `.review/findings/{slug}.txt` path construction).
pub fn valid_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && !slug.contains("--")
        && slug
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

fn normalize_slug(slug: &str, name: &str, paths: &[String]) -> String {
    let s = slug.trim().to_lowercase().replace(['_', ' '], "-");
    let s = s
        .split('-')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if !s.is_empty() && valid_slug(&s) {
        return s;
    }
    // Derive from first path.
    if let Some(p) = paths.first() {
        let stem = Path::new(p)
            .file_stem()
            .or_else(|| Path::new(p).file_name())
            .map(|x| x.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        // Apply the SAME normalization pipeline as the slug/name branches:
        // replace '_'/' ' with '-', strip '.', then collapse run-hyphens.
        // (Without the collapse, 'foo__bar.rs' → 'foo--bar' fails valid_slug;
        // without the dot-strip, 'foo.bar.rs' → 'foo.bar' fails too — both
        // would silently drop the item even though an equivalent slug/name
        // would be accepted.)
        let mut s = stem.replace(['_', ' ', '.'], "-");
        s = s
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        if !s.is_empty() && s != "main" && valid_slug(&s) {
            return s;
        }
    }
    if !name.trim().is_empty() {
        let n = name.trim().to_lowercase().replace(['_', ' '], "-");
        let n = n
            .split('-')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("-");
        if valid_slug(&n) {
            return n;
        }
    }
    String::new()
}

fn collect_paths(it: &RawItem) -> Vec<String> {
    let mut raw: Vec<String> = it.paths.clone();
    if !it.path.trim().is_empty() {
        raw.push(it.path.clone());
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for p in raw {
        // Raw pre-check: normalization strips leading '/' and '.' segments,
        // which would ERASE the evidence — "/abs/x" → "abs/x" and
        // "./C:/evil" → "C:/evil". The raw form must be rejected first…
        let trimmed_raw = p.trim().replace('\\', "/");
        if trimmed_raw.starts_with('/') || Path::new(&trimmed_raw).is_absolute() {
            continue;
        }
        let p = normalize_path(&p);
        if p.is_empty() || seen.contains(&p) {
            continue;
        }
        // …and the normalized first segment re-checked for a drive prefix
        // (catches "./C:/evil", which the raw check cannot see past the
        // leading "./").
        if p.split('/').next().is_some_and(is_drive_prefixed) {
            continue;
        }
        // Reject escapes (any '..' segment), newlines (would corrupt the
        // line-based checklist format), commas (paths are comma-joined
        // there and would not round-trip), and other control bytes (a NUL
        // would make a later Command::arg fail with an opaque error).
        if p.split('/').any(|seg| seg == "..") || p.chars().any(|c| c.is_control() || c == ',') {
            continue;
        }
        seen.insert(p.clone());
        out.push(p);
    }
    out
}

fn collect_verify(it: &RawItem) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in &it.verify {
        let cmd = raw.trim();
        if cmd.is_empty() || cmd.chars().any(|c| c.is_control()) {
            continue;
        }
        if seen.contains(cmd) {
            continue;
        }
        seen.insert(cmd.to_string());
        out.push(cmd.to_string());
    }
    out
}

/// True when the path starts with a Windows drive-letter prefix
/// (e.g. `C:/Users/x`) — an absolute path on the agent's machine that
/// Unix-side checks cannot see.
fn is_drive_prefixed(p: &str) -> bool {
    let bytes = p.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Collapse to canonical relative segments: strip ONE optional "./"-style
/// lead is not enough — filter '.' and empty segments so './src', 'src',
/// 'src//a', and 'src/./a' all normalize identically (the seen-set and
/// merge dedupes depend on exact-string equality).
fn normalize_path(p: &str) -> String {
    p.trim()
        .replace('\\', "/")
        .split('/')
        .filter(|seg| !seg.is_empty() && *seg != ".")
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod priority_tests {
    use super::*;

    fn raw(priority: serde_json::Value) -> Vec<RawItem> {
        let mut item = serde_json::json!({
            "slug": "a", "name": "A", "tier": "high",
            "paths": ["src/a.rs"]
        });
        item["priority"] = priority;
        vec![serde_json::from_value(item).unwrap()]
    }

    #[test]
    fn priority_accepts_numeric_string_variants() {
        for v in ["80", "80.0", "1e2", "+5"] {
            let out = normalize(raw(serde_json::json!(v))).unwrap();
            assert!(out[0].priority >= 5 && out[0].priority <= 102, "v={v}");
        }
        let out = normalize(raw(serde_json::json!(80))).unwrap();
        assert!(out[0].priority >= 5 && out[0].priority <= 102);
        let out = normalize(raw(serde_json::json!(80.5))).unwrap();
        assert!(out[0].priority >= 5 && out[0].priority <= 102);
    }

    #[test]
    fn priority_unparseable_degrades_to_tier_default() {
        let out = normalize(raw(serde_json::json!("high"))).unwrap();
        assert_eq!(out[0].priority, 100); // tier high default
    }

    #[test]
    fn duplicate_merge_raises_priority_floor_with_tier() {
        let first = serde_json::json!({
            "slug": "a", "name": "A", "tier": "low", "paths": ["src/a.rs"]
        }); // priority absent → low default 10
        let dup = serde_json::json!({
            "slug": "a", "name": "A", "tier": "high", "paths": ["src/b.rs"]
        }); // no explicit priority either
        let items = vec![
            serde_json::from_value(first).unwrap(),
            serde_json::from_value(dup).unwrap(),
        ];
        let out = normalize(items).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].tier, "high");
        // Floor raised to the high-tier default, not stranded at 10.
        assert_eq!(out[0].priority, 100);
        assert!(out[0].paths.contains(&"src/b.rs".to_string()));
    }

    #[test]
    fn duplicate_merge_keeps_explicit_priority_when_tier_unchanged() {
        // Explicit low/5 + same-tier duplicate with no priority → must stay
        // 5 (floor only applies on an actual tier upgrade).
        let first = serde_json::json!({
            "slug": "a", "name": "A", "tier": "low",
            "paths": ["src/a.rs"], "priority": 5
        });
        let dup = serde_json::json!({
            "slug": "a", "name": "A", "tier": "low", "paths": ["src/b.rs"]
        });
        let items = vec![
            serde_json::from_value(first).unwrap(),
            serde_json::from_value(dup).unwrap(),
        ];
        let out = normalize(items).unwrap();
        assert_eq!(out[0].tier, "low");
        assert_eq!(out[0].priority, 5);
    }

    #[test]
    fn paths_as_single_string_is_accepted() {
        // "paths": "src/x.rs" (string, not array) must not abort discovery.
        let item = serde_json::json!({
            "slug": "a", "name": "A", "tier": "high", "paths": "src/x.rs"
        });
        let it: RawItem = serde_json::from_value(item).unwrap();
        let paths = collect_paths(&it);
        assert_eq!(paths, vec!["src/x.rs"]);
    }

    #[test]
    fn parse_rejects_truncated_single_object() {
        let one = serde_json::json!({
            "slug": "helm-deploy", "name": "Helm", "paths": ["helm"], "tier": "medium"
        });
        let err = parse_discovery_payload(&one).unwrap_err().to_string();
        assert!(err.contains("truncated answer"), "{err}");
    }

    #[test]
    fn parse_reads_project_verify_gates() {
        let v = serde_json::json!({
            "verify": ["./scripts/check.sh"],
            "final_verify": ["./scripts/check.sh", "./scripts/e2e.sh"],
            "components": [
                {"slug": "core", "name": "Core", "paths": ["src"], "tier": "high"}
            ]
        });
        let p = parse_discovery_payload(&v).unwrap();
        assert_eq!(p.verify, vec!["./scripts/check.sh"]);
        assert_eq!(
            p.final_verify,
            vec!["./scripts/check.sh", "./scripts/e2e.sh"]
        );
        assert_eq!(p.components.len(), 1);
    }

    #[test]
    fn verify_commands_round_trip() {
        let item = serde_json::json!({
            "slug": "db", "name": "DB", "tier": "high",
            "paths": ["pkg/db"],
            "verify": ["pytest pkg/db", "ruff check pkg/db"]
        });
        let out = normalize(vec![serde_json::from_value(item).unwrap()]).unwrap();
        assert_eq!(out[0].verify, vec!["pytest pkg/db", "ruff check pkg/db"]);
    }

    #[test]
    fn parse_accepts_components_array() {
        let v = serde_json::json!({"components": [
            {"slug": "core", "name": "Core", "paths": ["crates/core"], "tier": "high"},
            {"slug": "server", "name": "Server", "paths": ["crates/server"], "tier": "high"}
        ]});
        let items = parse_discovery_payload(&v).unwrap();
        assert_eq!(items.components.len(), 2);
        assert!(items.verify.is_empty());
    }

    #[test]
    fn paths_dedupe_after_segment_collapse() {
        let item = serde_json::json!({
            "slug": "a", "name": "A", "tier": "high",
            "paths": ["./src//a.rs", "src/./a.rs", "src/a.rs", "/abs/x.rs", "C:\\Users\\x", "./C:/evil", "src/../etc", "bad\u{0}name"]
        });
        let it: RawItem = serde_json::from_value(item).unwrap();
        let paths = collect_paths(&it);
        // All three spellings collapse to one; absolute, drive-letter
        // (including the "./"-prefixed bypass), ..-escape, and control-byte
        // proposals are rejected.
        assert_eq!(paths, vec!["src/a.rs"]);
    }
}
