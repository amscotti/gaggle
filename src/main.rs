//! gaggle CLI: init | run | status | list | history | requeue
//!
//!   gaggle init [--components "slug|Name|tier,slug2|Name2|tier2"]
//!   gaggle run [--review-only]
//!   gaggle status [--tail N]
//!   gaggle list
//!   gaggle history [run-id]
//!   gaggle requeue <slug>… | --all

use anyhow::{Result, bail};
use gaggle::{goose, loop_engine};
use std::path::Path;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");
    let repo = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!("cannot determine current working directory: {e}"))?;

    match cmd {
        "init" => cmd_init(&repo, &args[2..]),
        "run" => {
            let rest = &args[2..];
            let review_only = match rest {
                [] => false,
                [f] if f == "--review-only" => true,
                _ => bail!(
                    "`run` takes no arguments except --review-only (got: {})",
                    rest.join(" ")
                ),
            };
            if review_only {
                println!(
                    "gaggle review-only pass — model: {}",
                    goose::effective_model(&repo)
                );
                println!("no fixes, no commits — findings only\n");
            } else {
                println!("gaggle loop — model: {}", goose::effective_model(&repo));
                println!("watch: gaggle status  (or tail -f .review/activity.log)\n");
            }
            // Auto-discover if no USABLE checklist yet (one-shot, then the
            // loop). "Usable" = the file exists AND parses to ≥1 component:
            // a 0-byte or gutted checklist must not reach the engine (state
            // sync would prune every component row and "finish" instantly).
            // And a state.json with recorded progress blocks silent
            // re-discovery — deleting the checklist mid-loop must not wipe
            // progress (the same guard `init` applies).
            let checklist = repo.join(gaggle::REVIEW_DIR).join("checklist.md");
            let checklist_usable = checklist.exists()
                && gaggle::checklist::load(&checklist)
                    .map(|c| !c.is_empty())
                    .unwrap_or(false);
            if !checklist_usable {
                ensure_no_recorded_progress(&repo)?;
                println!("no usable checklist found — running AI component discovery first\n");
                loop_engine::init(&repo, &[])?;
                println!();
            }
            if review_only {
                loop_engine::Engine::new(&repo).run_review_only()
            } else {
                loop_engine::Engine::new(&repo).run()
            }
        }
        "status" => {
            // Only search the subcommand's own args (skip program name and
            // "status"), so a path literally named "--tail" can't collide.
            let status_args = &args[2..];
            let tail_count = status_args.iter().filter(|a| *a == "--tail").count();
            if tail_count > 1 {
                bail!("--tail was specified {tail_count} times; provide it at most once");
            }
            let mut tail = 8;
            let mut i = 0;
            while i < status_args.len() {
                match status_args[i].as_str() {
                    "--tail" => {
                        let s = status_args.get(i + 1).ok_or_else(|| {
                            anyhow::anyhow!(
                                "--tail expects a positive integer, but no value was provided"
                            )
                        })?;
                        let n: usize = s.parse().map_err(|_| {
                            anyhow::anyhow!("--tail expects a positive integer, got `{s}`")
                        })?;
                        if n == 0 {
                            bail!("--tail must be at least 1 (0 would show no activity)");
                        }
                        tail = n;
                        i += 2;
                    }
                    // Unknown tokens are REJECTED, not silently ignored:
                    // `--tail=20` / `--taill 20` would otherwise quietly
                    // fall back to the default 8.
                    other => bail!(
                        "unrecognized status argument: {other:?} — usage: gaggle status [--tail N]"
                    ),
                }
            }
            gaggle::status::print_status(&repo.join(gaggle::REVIEW_DIR), tail)
        }
        "list" => loop_engine::list(&repo),
        "history" => {
            let rest = &args[2..];
            match rest {
                [] => loop_engine::history(&repo, None),
                [ts] => loop_engine::history(&repo, Some(ts)),
                _ => bail!("history takes at most one run-id — usage: gaggle history [run-id]"),
            }
        }
        "requeue" => cmd_requeue(&repo, &args[2..]),
        "model" => {
            println!("{}", goose::effective_model(&repo));
            Ok(())
        }
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        other => {
            bail!("unknown command: {other} — try `gaggle help`");
        }
    }
}

/// `gaggle requeue <slug>… | --all` — move quarantined (Failed) components
/// back to Pending for the next `gaggle run` to retry.
fn cmd_requeue(repo: &Path, args: &[String]) -> Result<()> {
    let mut slugs: Vec<String> = Vec::new();
    let mut all = false;
    for a in args {
        match a.as_str() {
            "--all" => all = true,
            s if s.starts_with("--") => {
                bail!("unrecognized requeue flag: {s} — usage: gaggle requeue <slug>… | --all");
            }
            s => slugs.push(s.to_string()),
        }
    }
    if all && !slugs.is_empty() {
        bail!("--all cannot be combined with explicit slugs");
    }
    if !all && slugs.is_empty() {
        bail!(
            "requeue needs one or more slugs, or --all (see `gaggle list` for quarantined components)"
        );
    }
    loop_engine::requeue(repo, &slugs, all)?;
    println!("\nrun `gaggle run` to retry the requeued component(s)");
    Ok(())
}

/// Block operations that would silently reset recorded component
/// progress (init, auto-discovery in `run`). A CORRUPT state.json is an
/// error here — treating it as "no progress" (the old `.ok()` swallow)
/// would let init wipe an in-progress loop whose state file merely failed
/// to parse.
fn ensure_no_recorded_progress(repo: &Path) -> Result<()> {
    let state_path = repo.join(gaggle::REVIEW_DIR).join("state.json");
    if !state_path.exists() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&state_path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", state_path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| {
        anyhow::anyhow!(
            "{} is corrupt ({e}) — fix or remove it before re-initializing \
             (State::load would back it up as state.json.bad on a normal run)",
            state_path.display()
        )
    })?;
    let has_progress = v
        .get("components")
        .and_then(|c| c.as_object())
        .map(|m| {
            m.values().any(|s| {
                s.get("phase")
                    .and_then(|p| p.as_str())
                    .is_some_and(|p| p != "pending")
            })
        })
        .unwrap_or(false);
    if has_progress {
        bail!(
            ".review/state.json records components with progress (done/failed/active) — \
             this operation would reset it. Move or delete .review/state.json first if \
             you really want a fresh checklist."
        );
    }
    Ok(())
}

fn cmd_init(repo: &Path, args: &[String]) -> Result<()> {
    // Re-running init would overwrite state.json/checklist.md and silently
    // reset all component phases of an in-progress loop. Require a
    // deliberate delete first.
    ensure_no_recorded_progress(repo)?;
    let mut components: Vec<(String, String, String)> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--components" => {
                let list = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--components needs a value"))?;
                if list.is_empty() {
                    bail!("--components value must not be empty");
                }
                if list.starts_with("--") {
                    bail!("--components needs a value, but got flag `{list}`");
                }
                if !list.contains('|') {
                    bail!(
                        "--components needs a value like `slug|Name|tier`, but got `{list}` (missing `|` separator)"
                    );
                }
                for entry in list.split(',') {
                    let entry = entry.trim();
                    if entry.is_empty() {
                        bail!("--components contains an empty entry (check for stray commas)");
                    }
                    let parts: Vec<&str> = entry.split('|').collect();
                    if parts.len() < 2 || parts.len() > 3 {
                        bail!("bad component entry: {entry} (want slug|Name|tier)");
                    }
                    let slug = parts[0].trim();
                    let name = parts[1].trim();
                    if slug.is_empty() {
                        bail!("component slug must not be empty (entry: `{entry}`)");
                    }
                    // Same validation discovery applies: a slug like
                    // `../evil` or `a/b` would otherwise reach
                    // `.review/findings/{slug}.txt` path construction and
                    // write outside .review/.
                    if !gaggle::discover::valid_slug(slug) {
                        bail!(
                            "invalid slug {slug:?} in entry `{entry}` — lowercase a-z0-9 hyphen-joined segments (e.g. `loop-engine`)"
                        );
                    }
                    if name.is_empty() {
                        bail!("component name must not be empty (entry: `{entry}`)");
                    }
                    let tier = parts
                        .get(2)
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .unwrap_or("medium");
                    // Unknown tiers silently rank BELOW low (tier_rank maps
                    // them to the worst rank), inverting user intent —
                    // validate like discover's normalization does.
                    let tier_lc = tier.to_lowercase();
                    if !gaggle::checklist::TIERS.contains(&tier_lc.as_str()) {
                        bail!(
                            "unknown tier {tier:?} in entry `{entry}` — valid tiers: {} (case-insensitive)",
                            gaggle::checklist::TIERS.join("/")
                        );
                    }
                    if components.iter().any(|(s, _, _)| s == slug) {
                        bail!(
                            "duplicate component slug `{slug}` — each --components entry must be unique"
                        );
                    }
                    components.push((slug.to_string(), name.to_string(), tier_lc));
                }
                i += 2;
            }
            other => bail!("unknown init flag: {other}"),
        }
    }
    let (count, label) = if components.is_empty() {
        // No explicit components → AI discovery (agent invents the
        // checklist; the harness validates it). The returned Vec carries
        // per-component info from discovery, so surface its length rather
        // than silently dropping it.
        let discovered = loop_engine::init(repo, &[])?;
        (discovered.len(), "discovered component(s)")
    } else {
        loop_engine::init(repo, &components)?;
        (components.len(), "components")
    };
    print_init_success(count, label);
    Ok(())
}

/// Shared success banner for `cmd_init` — keeps the two branches in sync so
/// the created-file list can't silently drift.
fn print_init_success(count: usize, label: &str) {
    println!("initialized .review/ with {count} {label}");
    println!("  → .review/checklist.md");
    println!("  → .review/state.json");
    println!("  → .review/activity.log");
    println!("  → .review/config.toml");
    println!("  → .gitignore (gaggle .review/ rules, if missing)");
    println!("run `gaggle run` to start the loop");
}

fn print_help() {
    println!(
        r#"gaggle — checklist-driven autonomous review/fix loop on the Goose GDK stack

USAGE:
  gaggle init [--components "slug|Name|tier,slug2|Name2|tier2"]
  gaggle run [--review-only]
  gaggle status [--tail N]
  gaggle list
  gaggle history [run-id]
  gaggle requeue <slug>… | --all
  gaggle model

COMMANDS:
  init     create .review/ — AI-discovers components (or --components to pin)
  run      start the loop: review → fix → verify → commit → confirm, per component
           --review-only: review every component, record findings files,
           never fix or commit (state machine untouched; dirty tree OK)
  status   show live phase + recent activity (what the loop is doing NOW)
  list     component phase table
  history  past runs (outcome, cost, leftovers) — detail: gaggle history <run-id>
  requeue  move quarantined (failed) components back to pending for retry
  model    print the effective agent model + where it comes from

MODEL: optional `provider` / `model` keys in .review/config.toml; when unset,
       goose's configured default is used (GOOSE_PROVIDER/GOOSE_MODEL or its
       config.yaml). Nothing is hard-coded; recipes never pin a model.
RECIPES: baked in; override any with .review/workflows/<name>.yaml
"#,
    );
}
