//! Component checklist: markdown parse/export, one-directional import source.
//!
//! Format:
//!
//! ```text
//! # Checklist
//!
//! ## high
//! - [ ] my-slug — Human Name
//!   paths: src/foo.rs, src/foo/
//!   verify: <the command this repo uses to check this slice>
//! - [x] done-slug — Already Done
//!   paths: src/done.rs
//! ```

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub slug: String,
    pub name: String,
    pub tier: String,
    pub done: bool,
    pub paths: Vec<String>,
    /// Optional per-component verify commands (discovered). Empty = use
    /// the repo-wide `verify` list from `.review/config.toml`.
    #[serde(default)]
    pub verify: Vec<String>,
}

impl Component {
    pub fn new(slug: &str, name: &str, tier: &str) -> Self {
        Self {
            slug: slug.to_string(),
            name: name.to_string(),
            tier: tier.to_lowercase(),
            done: false,
            paths: Vec::new(),
            verify: Vec::new(),
        }
    }
}

/// Review priority band. Unknown strings parse as Medium.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    High,
    Medium,
    Low,
}

impl Tier {
    pub const ALL: &[Tier] = &[Tier::High, Tier::Medium, Tier::Low];

    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "high" => Self::High,
            "low" => Self::Low,
            _ => Self::Medium,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    /// Sort key: 0 = highest priority.
    pub const fn rank(self) -> usize {
        match self {
            Self::High => 0,
            Self::Medium => 1,
            Self::Low => 2,
        }
    }

    /// High is stronger than Medium is stronger than Low.
    pub fn is_stronger_than(self, other: Self) -> bool {
        self.rank() < other.rank()
    }

    /// Priority floor when the agent omitted an explicit integer.
    pub fn default_priority(self) -> u32 {
        match self {
            Self::High => 100,
            Self::Medium => 50,
            Self::Low => 10,
        }
    }
}

/// Known tiers in priority order (highest first).
pub const TIERS: &[&str] = &[
    Tier::High.as_str(),
    Tier::Medium.as_str(),
    Tier::Low.as_str(),
];

/// Shared ranking of a tier string. Known tiers are ranked by priority
/// (0 = highest); unknown tiers sort last.
pub fn tier_rank(tier: &str) -> usize {
    match tier {
        "high" => Tier::High.rank(),
        "medium" => Tier::Medium.rank(),
        "low" => Tier::Low.rank(),
        _ => Tier::ALL.len(),
    }
}

/// Parse a markdown checklist into components. H2 headings are tiers.
pub fn parse(text: &str) -> Result<Vec<Component>> {
    let mut items: Vec<Component> = Vec::new();
    let mut tier = "medium".to_string();
    // A `paths:` line binds to the component IMMEDIATELY above it (blank
    // lines tolerated). Headings, skipped checkbox lines (bad marker,
    // empty slug), and any other non-blank prose break the binding —
    // without that, `- [n] bar` + `  paths: …` would silently attach
    // bar's paths to the previous component.
    let mut have_current = false;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue; // blank lines do not break adjacency
        }
        if let Some(h) = t.strip_prefix("## ") {
            let candidate = h.trim().to_lowercase();
            // Always capture the heading as the current tier so that items
            // under an unknown/typo'd heading are not silently merged into
            // the previous tier.
            tier = candidate;
            have_current = false;
            continue;
        }
        if let Some(rest) = t.strip_prefix("paths:") {
            if have_current {
                if let Some(last) = items.last_mut() {
                    last.paths = rest
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            } else {
                eprintln!("  ⚠ checklist: orphan `paths:` line ignored: {t}");
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("verify:") {
            let cmd = rest.trim();
            if have_current {
                if !cmd.is_empty() {
                    if let Some(last) = items.last_mut() {
                        last.verify.push(cmd.to_string());
                    }
                }
            } else {
                eprintln!("  ⚠ checklist: orphan `verify:` line ignored: {t}");
            }
            continue;
        }
        if let Some(rest) = t.strip_prefix("- [") {
            // Consume the marker character, then strip the closing `]`.
            // Iterate by chars (not bytes) so multi-byte markers like `✓`
            // don't panic on `split_at`.
            let mut chars = rest.chars();
            let marker = chars.next().unwrap_or('\u{0}');
            let after = chars.as_str();
            // Validate the marker: only ' ', 'x', 'X' are legal checkbox
            // states, and the marker must be followed by ']'. Without this,
            // a line like "- [notes here]" would be accepted with marker
            // 'n' and silently create a phantom entry.
            if !matches!(marker, ' ' | 'x' | 'X') || !after.starts_with(']') {
                have_current = false;
                continue;
            }
            let done = marker == 'x' || marker == 'X';
            let body = after[1..].trim();
            // The canonical separator is the em-dash " — " (U+2014). Accept the
            // ASCII fallback " - " (with required surrounding spaces) so a
            // source that uses it does not collapse the whole line into the
            // slug with an empty name. Bare "--" is intentionally NOT accepted:
            // it would mis-split slugs that legitimately contain a double
            // hyphen (e.g. `my--slug`).
            let (slug, name) = body
                .split_once(" — ")
                .or_else(|| body.split_once(" - "))
                .map(|(s, n)| (s.trim(), n.trim()))
                .unwrap_or((body, ""));
            if slug.is_empty() {
                have_current = false;
                continue;
            }
            // Same rule discovery and `--components` apply: a slug like
            // `../evil` must never reach `.review/findings/{slug}.txt`.
            if !crate::discover::valid_slug(slug) {
                bail!(
                    "invalid checklist slug {slug:?} — lowercase a-z0-9 hyphen-joined segments (e.g. `loop-engine`)"
                );
            }
            items.push(Component {
                slug: slug.to_string(),
                name: name.to_string(),
                tier: tier.clone(),
                done,
                paths: Vec::new(),
                verify: Vec::new(),
            });
            have_current = true;
            continue;
        }
        // Any other non-blank line (prose) breaks paths: adjacency.
        have_current = false;
    }
    Ok(items)
}

/// Render components back to markdown.
pub fn render(components: &[Component]) -> Result<String> {
    if components.is_empty() {
        bail!("cannot render an empty checklist — would produce a header-only file");
    }
    // Line-format guards BEFORE rendering: parse() is line-based, so a
    // newline in a name or a comma inside a path would silently corrupt
    // the next load (truncated names, split paths, injected entries).
    for c in components {
        if c.slug.contains(['\n', '\r'])
            || c.name.contains(['\n', '\r'])
            || c.tier.contains(['\n', '\r'])
        {
            bail!(
                "component `{}` name/slug/tier contains a newline — cannot round-trip through the checklist format",
                c.slug.trim()
            );
        }
        // The slug must not contain the separators parse() splits on, or
        // `- [ ] a — b — Name` reloads as slug `a`, name `b — Name`.
        if c.slug.contains(" — ") || c.slug.contains(" - ") {
            bail!(
                "component slug {c:?} contains the list separator (— or ' - ') — cannot round-trip"
            );
        }
        // An empty/whitespace tier renders a `## ` line that re-parses as
        // no heading at all — the item silently falls into the previous
        // tier on reload. parse() lowercases headings, so canonicalize
        // here too for exact round-trips.
        if c.tier.trim().is_empty() {
            bail!(
                "component `{}` has an empty tier — cannot round-trip through a `## ` heading",
                c.slug
            );
        }
        for p in &c.paths {
            if p.contains(['\n', '\r']) {
                bail!("component `{}` has a path with a newline: {p:?}", c.slug);
            }
            if p.contains(',') {
                bail!(
                    "component `{}` has a path containing a comma: {p:?} — paths are comma-separated in the checklist format and would not round-trip",
                    c.slug
                );
            }
        }
        for v in &c.verify {
            if v.contains(['\n', '\r']) {
                bail!(
                    "component `{}` has a verify command with a newline: {v:?}",
                    c.slug
                );
            }
        }
    }
    let mut out = String::from("# Checklist\n");
    let mut tiers: Vec<String> = Vec::new();
    for c in components {
        if !tiers.contains(&c.tier) {
            tiers.push(c.tier.clone());
        }
    }
    // Render tiers in priority order (highest first). Known tiers sort by their
    // defined priority; unknown tiers all map to the same rank (TIERS.len())
    // and therefore sort last, preserving their first-seen insertion order
    // because slice::sort_by_key is stable. This makes parse→render round-trips
    // deterministic.
    tiers.sort_by_key(|t| tier_rank(t));
    for tier in tiers {
        out.push_str(&format!("\n## {tier}\n"));
        for c in components.iter().filter(|c| c.tier == tier) {
            let box_ = if c.done { "x" } else { " " };
            if c.name.is_empty() {
                out.push_str(&format!("- [{box_}] {}\n", c.slug));
            } else {
                out.push_str(&format!("- [{box_}] {} — {}\n", c.slug, c.name));
            }
            if !c.paths.is_empty() {
                out.push_str(&format!("  paths: {}\n", c.paths.join(", ")));
            }
            for v in &c.verify {
                out.push_str(&format!("  verify: {v}\n"));
            }
        }
    }
    Ok(out)
}

/// Load components from a checklist file.
pub fn load(path: &Path) -> Result<Vec<Component>> {
    if !path.exists() {
        bail!(
            "checklist not found at {} — run `gaggle init` first",
            path.display()
        );
    }
    let text = fs::read_to_string(path)?;
    let items = parse(&text)?;
    // A non-empty file that parses to ZERO components is a wrong/corrupted
    // file (e.g. task-syntax `* [ ]`), not a valid empty checklist —
    // returning an empty Vec would make State::sync prune all component
    // progress permanently.
    if items.is_empty() && !text.trim().is_empty() {
        bail!(
            "checklist at {} has content but no parseable components — refusing to \
             treat it as empty (that would wipe recorded component progress)",
            path.display()
        );
    }
    Ok(items)
}

/// Save components to a checklist file (export mirror). Atomic: write a
/// temp file next to the target then rename — a crash mid-write must not
/// leave a truncated checklist that parse() accepts (State::sync would
/// then prune the missing components' progress permanently).
pub fn save(path: &Path, components: &[Component]) -> Result<()> {
    if components.is_empty() {
        bail!("cannot save an empty checklist — refusing to persist a no-op file");
    }
    let tmp = path.with_extension(format!("md.tmp.{}", std::process::id()));
    let result = fs::write(&tmp, render(components)?);
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_paths_and_done_roundtrip() {
        let md = "# Checklist\n\n## high\n- [x] foo — Foo\n  paths: src/foo.rs, src/foo/\n";
        let c = parse(md).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].slug, "foo");
        assert!(c[0].done);
        assert_eq!(c[0].paths, vec!["src/foo.rs", "src/foo/"]);
        let out = render(&c).unwrap();
        let again = parse(&out).unwrap();
        assert_eq!(again[0].paths, c[0].paths);
        assert!(again[0].done);
        assert_eq!(again[0].name, "Foo");
    }

    #[test]
    fn unchecked_has_no_paths() {
        let md = "# Checklist\n\n## medium\n- [ ] bar — Bar\n";
        let c = parse(md).unwrap();
        assert!(!c[0].done);
        assert!(c[0].paths.is_empty());
        assert!(c[0].verify.is_empty());
    }

    #[test]
    fn parse_verify_lines_roundtrip() {
        let md = "# Checklist\n\n## high\n- [ ] db — Database\n  paths: crates/db\n  verify: go test ./db\n  verify: go vet ./db\n";
        let c = parse(md).unwrap();
        assert_eq!(c[0].verify, vec!["go test ./db", "go vet ./db"]);
        let out = render(&c).unwrap();
        let again = parse(&out).unwrap();
        assert_eq!(again[0].verify, c[0].verify);
    }
}

#[cfg(test)]
mod parse_guard_tests {
    use super::*;

    #[test]
    fn orphan_paths_line_is_ignored_after_heading() {
        let md = "# Checklist\n\n## high\n- [ ] foo — Foo\n\n## medium\npaths: src/evil.rs\n- [ ] bar — Bar\n";
        let items = parse(md).unwrap();
        assert_eq!(items.len(), 2);
        assert!(
            items[0].paths.is_empty(),
            "paths: must not bind across a heading"
        );
        assert_eq!(items[0].tier, "high");
        assert_eq!(items[1].tier, "medium");
    }

    #[test]
    fn load_bails_on_content_with_zero_components() {
        let dir = std::env::temp_dir().join(format!("gaggle-cl-{}-z", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("checklist.md");
        std::fs::write(&p, "* [ ] task-syntax file\n").unwrap();
        assert!(load(&p).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn render_rejects_slug_containing_separator() {
        let comps = vec![Component::new("a — b", "Name", "high")];
        assert!(render(&comps).is_err());
    }

    #[test]
    fn render_rejects_newline_in_tier() {
        let mut c = Component::new("ok", "Name", "high");
        c.tier = "high\n## low".to_string();
        assert!(render(&[c]).is_err());
    }
}

#[cfg(test)]
mod adjacency_tests {
    use super::*;

    #[test]
    fn skipped_checkbox_breaks_paths_binding() {
        let md = "# Checklist\n\n## high\n- [ ] foo — Foo\n- [n] bar — Bar\n  paths: src/bar.rs\n";
        let items = parse(md).unwrap();
        assert_eq!(items.len(), 1);
        // bar's paths line must NOT attach to foo.
        assert!(items[0].paths.is_empty());
    }

    #[test]
    fn prose_breaks_paths_binding() {
        let md = "# Checklist\n\n## high\n- [ ] foo — Foo\nSome prose line\n  paths: src/x.rs\n";
        let items = parse(md).unwrap();
        assert_eq!(items.len(), 1);
        assert!(items[0].paths.is_empty());
    }

    #[test]
    fn blank_lines_preserve_paths_binding() {
        let md = "# Checklist\n\n## high\n- [ ] foo — Foo\n\n  paths: src/foo.rs\n";
        let items = parse(md).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].paths, vec!["src/foo.rs"]);
    }

    #[test]
    fn render_rejects_empty_tier() {
        let mut c = Component::new("ok", "Name", "high");
        c.tier = "   ".to_string();
        assert!(render(&[c]).is_err());
    }

    #[test]
    fn parse_rejects_path_traversal_slug() {
        let md = "# Checklist\n\n## high\n- [ ] ../evil — Evil\n";
        let err = parse(md).unwrap_err().to_string();
        assert!(err.contains("invalid checklist slug"), "{err}");
        let md = "# Checklist\n\n## high\n- [ ] foo/bar — Nested\n";
        assert!(parse(md).is_err());
    }
}
