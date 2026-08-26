//! The main loop: review once → fix that work order → verify → commit on
//! green. Confirmation only asks whether those findings are still open; it
//! does not invent new ones. A later red verify restores the last green
//! commit instead of wiping the component. Goose flakes and red checks
//! retry / send the fixer; quarantine is only after the attempt budget
//! is spent. A red full-suite gate is the same (classify → fix → re-run).

use crate::checklist::{self, Component};
use crate::commit;
use crate::discover;
use crate::goose::{self, field};
use crate::recipes;
use crate::state::{self, Phase, State};
use crate::status::{self, Phase as StatusPhase};
use crate::verify;
use anyhow::{Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// How many review→fix cycles before a component is quarantined.
const MAX_FIX_CYCLES: usize = 3;

/// Harness verify result, optionally classified by the verify recipe.
struct VerifyVerdict {
    passed: bool,
    cause: Option<String>,
    diagnostics: Option<String>,
}

/// Cap the verify output we embed in state detail so it stays readable.
const DETAIL_TAIL: usize = 1024;

pub struct Engine {
    pub repo: PathBuf,
    pub review_dir: PathBuf,
    /// Run-level token/cost accumulator (all recipe runs this process).
    /// Interior mutability: phase methods take `&self` (state is threaded
    /// separately as `&mut State`), and usage is telemetry, not state.
    usage_total: std::cell::RefCell<goose::Usage>,
    /// Per-component usage breakdown for the final report.
    usage_by_component: std::cell::RefCell<std::collections::BTreeMap<String, goose::Usage>>,
    /// Dedicated run branch name when `[branch] dedicated = true` (set at
    /// run start; recorded in the final report). RefCell for the same
    /// `&self` reason as the usage fields.
    run_branch: std::cell::RefCell<Option<String>>,
}

impl Engine {
    pub fn new(repo: &Path) -> Self {
        Self {
            repo: repo.to_path_buf(),
            review_dir: repo.join(crate::REVIEW_DIR),
            usage_total: std::cell::RefCell::new(goose::Usage::default()),
            usage_by_component: std::cell::RefCell::new(std::collections::BTreeMap::new()),
            run_branch: std::cell::RefCell::new(None),
        }
    }

    fn state_path(&self) -> PathBuf {
        self.review_dir.join("state.json")
    }

    /// Review-only pass: review EVERY checklist component and record
    /// findings files, but never fix, verify, or commit. The on-disk state
    /// machine is untouched (a later `gaggle run` proceeds normally);
    /// findings land in `.review/findings/<slug>.txt` exactly as the full
    /// loop writes them.
    ///
    /// Unlike `run`, a dirty worktree is allowed — nothing here commits.
    /// A single component's review failure does not abort the pass; it is
    /// counted and reported (and reflected in the exit code).
    pub fn run_review_only(&self) -> Result<()> {
        recipes::ensure_config(&self.repo)?;
        recipes::ensure_gitignore(&self.repo)?;
        let overrides = recipes::list_overrides(&self.repo);
        if !overrides.is_empty() {
            println!("recipe overrides: {}", overrides.join(", "));
        }
        if !commit::dirty_paths(&self.repo)?.is_empty() {
            println!(
                "note: worktree is dirty — reviewing the working tree as-is (review-only never commits)"
            );
        }

        let components = checklist::load(&self.review_dir.join("checklist.md"))?;
        if components.is_empty() {
            bail!("checklist is empty — run `gaggle init` first");
        }
        // Scratch state (never saved): name/paths lookup per component.
        let mut scratch = State::default();
        scratch.sync(&components);

        // Same order `state.next()` would pick: tier rank, then slug.
        let mut ordered = components;
        ordered.sort_by(|a, b| {
            (checklist::tier_rank(&a.tier), &a.slug).cmp(&(checklist::tier_rank(&b.tier), &b.slug))
        });

        status::report(
            &self.review_dir,
            StatusPhase::Picking,
            "-",
            &format!("review-only pass started ({} components)", ordered.len()),
        )?;

        let mut total_findings = 0usize;
        let mut clean = 0usize;
        let mut errors = 0usize;
        for comp in &ordered {
            println!("\n=== {} — {} (review only) ===", comp.slug, comp.name);
            status::report(
                &self.review_dir,
                StatusPhase::Reviewing,
                &comp.slug,
                "review agent starting",
            )?;
            match self.review_component(&scratch, &comp.slug) {
                Ok(findings) => {
                    println!("  review: {} finding(s)", findings.len());
                    if findings.is_empty() {
                        clean += 1;
                        println!("  ✓ clean");
                    } else {
                        let file = self.write_findings_file(&comp.slug, &findings)?;
                        total_findings += findings.len();
                        for (i, f) in findings.iter().take(3).enumerate() {
                            let one_line: String =
                                f.lines().next().unwrap_or("").chars().take(100).collect();
                            println!("    {}. {}", i + 1, one_line);
                        }
                        if findings.len() > 3 {
                            println!("    … and {} more", findings.len() - 3);
                        }
                        println!("  → full list: {}", file.display());
                    }
                }
                Err(e) => {
                    errors += 1;
                    eprintln!("  ✗ review failed: {e:#}");
                    // A failed review must not leave a PREVIOUS pass's
                    // findings file looking current — remove the stale
                    // artifact so `.review/findings/<slug>.txt` presence
                    // always reflects THIS pass.
                    let stale = self.findings_path(&comp.slug);
                    if stale.exists() {
                        let _ = std::fs::remove_file(&stale);
                        println!(
                            "  (removed stale findings from an earlier pass: {})",
                            stale.display()
                        );
                    }
                }
            }
        }

        status::report(
            &self.review_dir,
            StatusPhase::Idle,
            "-",
            &format!(
                "review-only pass complete: {} finding(s), {} clean, {} error(s)",
                total_findings, clean, errors
            ),
        )?;
        println!(
            "\nreview-only pass complete: {} component(s) — {} finding(s), {} clean, {} error(s)",
            ordered.len(),
            total_findings,
            clean,
            errors
        );
        println!("findings: {}", self.review_dir.join("findings").display());
        recipes::cleanup_temp();
        if errors > 0 {
            bail!("{errors} component review(s) failed — see output above");
        }
        Ok(())
    }

    /// Run the loop until every component is done (or failed).
    pub fn run(&self) -> Result<()> {
        // No-HEAD guard: every commit/reset path (`git diff HEAD`,
        // `reset --hard`, `rev-parse HEAD`) fails confusingly on a repo
        // with zero commits. Require at least one commit up front.
        commit::require_head(&self.repo)?;
        recipes::ensure_config(&self.repo)?;
        recipes::ensure_gitignore(&self.repo)?;
        verify::load_commands(&self.repo)?;
        let overrides = recipes::list_overrides(&self.repo);
        if !overrides.is_empty() {
            println!("recipe overrides: {}", overrides.join(", "));
        }

        let components = checklist::load(&self.review_dir.join("checklist.md"))?;
        if components.is_empty() {
            bail!("checklist is empty — run `gaggle init` first");
        }
        let mut state = State::load(&self.state_path())?;
        state.sync(&components);

        // Crash mid-component: wipe uncommitted fixer dirt (commits from
        // earlier green cycles stay at HEAD) and restart from review.
        let has_active = state.components.values().any(|c| {
            matches!(
                c.phase,
                Phase::Reviewing | Phase::Fixing | Phase::Verifying | Phase::Committing
            )
        });
        if has_active {
            commit::reset_worktree(&self.repo)?;
            for c in state.components.values_mut() {
                if matches!(
                    c.phase,
                    Phase::Reviewing | Phase::Fixing | Phase::Verifying | Phase::Committing
                ) {
                    c.phase = Phase::Pending;
                }
            }
        } else {
            let dirty = commit::dirty_paths(&self.repo)?;
            let blocking: Vec<&str> = dirty
                .iter()
                .map(|s| s.as_str())
                .filter(|p| *p != ".gitignore")
                .collect();
            if !blocking.is_empty() {
                // Cap the listing: a missing build-output gitignore can
                // dirty hundreds of paths (target/, node_modules/) — name
                // the first few and hint at the common cause.
                let shown: Vec<&str> = blocking.iter().take(5).copied().collect();
                let more = blocking.len() - shown.len();
                bail!(
                    "worktree is dirty — commit or stash before `gaggle run` (dirty: {}{}). \
                     {}",
                    shown.join(", "),
                    if more > 0 {
                        format!(" … and {more} more")
                    } else {
                        String::new()
                    },
                    if more > 0 || blocking.iter().any(|p| p.starts_with("target/")) {
                        "If these are build outputs, gitignore them (e.g. /target/) \
                         and commit the .gitignore first."
                    } else {
                        ""
                    }
                );
            }
        }
        state.save(&self.state_path())?;

        // Dedicated run branch ([branch] dedicated = true): switch AFTER
        // the dirty guard (branch creation needs a clean tree) and BEFORE
        // any processing (all commits then land on it). Resume-aware —
        // an existing gaggle/run-* branch is reused, not forked.
        *self.run_branch.borrow_mut() = commit::ensure_run_branch(&self.repo)?;

        status::report(&self.review_dir, StatusPhase::Picking, "-", "loop started")?;

        while let Some(next) = state.next().cloned() {
            let comp = Component::new(&next.slug, &next.name, &next.tier);
            self.process_component(&mut state, &comp)?;
            state.save(&self.state_path())?;
            self.persist_checklist(&state)?;
        }

        // End-of-run FULL gate (`final_verify` from config, or the regular
        // `verify` list when unset). Per-cycle verify is the component's
        // discovered commands (then the repo `verify` list). This gate
        // catches what those runs never executed. A red result is a work
        // order: classify the owning component, send the fixer, re-run.
        // Only after that budget is spent is the run red. Prior component
        // commits stay; a failed extra fix is wiped, not rolled back.
        let final_gate = self.close_final_gate(&mut state)?;
        state.save(&self.state_path())?;
        self.persist_checklist(&state)?;

        let quarantined: Vec<&str> = state
            .components
            .values()
            .filter(|c| c.phase == Phase::Failed)
            .map(|c| c.slug.as_str())
            .collect();
        if !final_gate.passed {
            status::report(
                &self.review_dir,
                StatusPhase::Failed,
                "-",
                &format!(
                    "loop finished with a RED final verify: {}quarantined — final gate `{}` failed",
                    if quarantined.is_empty() {
                        "no components ".to_string()
                    } else {
                        format!("{} ", quarantined.join(", "))
                    },
                    final_gate.failed_command.clone().unwrap_or_default()
                ),
            )?;
        } else if !quarantined.is_empty() {
            // Gate is green; leftover quarantine is leftover work, not a
            // failed run. `gaggle status` used to show Failed here and
            // look like the suite never passed.
            status::report(
                &self.review_dir,
                StatusPhase::Done,
                "-",
                &format!(
                    "final verify green; quarantined (requeue to retry): {}",
                    quarantined.join(", ")
                ),
            )?;
        } else {
            status::report(
                &self.review_dir,
                StatusPhase::Done,
                "-",
                "all components done, final verify green",
            )?;
        }
        if let Err(e) = self.generate_report(&state, &final_gate) {
            eprintln!("  warning: final report generation failed: {e:#}");
        }
        recipes::cleanup_temp();
        // Archive this run's artifacts for `gaggle history` — post-mortem
        // and debugging material (what ran, what it cost, what it left
        // behind). Best-effort: archiving must never fail the run.
        if let Err(e) = self.archive_run(&state, &final_gate) {
            eprintln!("  warning: run archive failed (continuing): {e:#}");
        }
        let usage = self.usage_total.borrow();
        if !usage.is_empty() {
            println!("\nrun usage: {}", usage.summary());
        }
        // A red full gate is a completed run with a bad outcome — the
        // report is already on disk, but the PROCESS exit code must
        // reflect it (CI scripts gate on this).
        if !final_gate.passed {
            bail!(
                "final verify is RED (`{}`) — see {} and {}",
                final_gate.failed_command.clone().unwrap_or_default(),
                self.review_dir.join("verify-diagnostics.txt").display(),
                self.review_dir.join("final-report.md").display()
            );
        }
        Ok(())
    }

    /// Archive this run to `.review/runs/<timestamp>/`: report, ledger,
    /// state snapshot, and a small index.json (outcome, counts, usage,
    /// model, commits) so `gaggle history` can answer "what happened on
    /// that run two weeks ago" without re-parsing markdown.
    fn archive_run(&self, state: &State, final_gate: &verify::RunResult) -> Result<()> {
        let ts = chrono::Utc::now().format("%Y%m%d-%H%M%S");
        let dir = self.review_dir.join("runs").join(ts.to_string());
        std::fs::create_dir_all(&dir)?;

        // Copy the report, ledger, and verify diagnostics as-of-run-end.
        for name in ["final-report.md", "run-ledger.md", "verify-diagnostics.txt"] {
            let src = self.review_dir.join(name);
            if src.exists() {
                std::fs::copy(&src, dir.join(name))
                    .map_err(|e| anyhow::anyhow!("archiving {name}: {e}"))?;
            }
        }
        // Snapshot the terminal state (components + phases + commits).
        std::fs::write(
            dir.join("state.json"),
            serde_json::to_string_pretty(state)? + "\n",
        )?;

        // Machine-readable index for `gaggle history`.
        let done = state
            .components
            .values()
            .filter(|c| c.phase == Phase::Done)
            .count();
        let failed = state
            .components
            .values()
            .filter(|c| c.phase == Phase::Failed)
            .count();
        let commits: Vec<String> = state
            .components
            .values()
            .filter_map(|c| c.commit.clone())
            .collect();
        let leftovers: usize = state
            .components
            .values()
            .filter(|c| c.phase == Phase::Done && c.findings > 0)
            .map(|c| c.findings)
            .sum();
        let usage_json = {
            let u = self.usage_total.borrow();
            serde_json::json!({
                "cost_usd": u.cost_usd,
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
                "cache_read_input_tokens": u.cache_read_input_tokens,
            })
        };
        let outcome = if final_gate.passed { "green" } else { "red" };
        let index = serde_json::json!({
            "ts": ts.to_string(),
            "outcome": outcome,
            "components": state.components.len(),
            "done": done,
            "failed": failed,
            "leftover_findings": leftovers,
            "commits": commits,
            "model": goose::effective_model(&self.repo),
            "usage": usage_json,
        });
        std::fs::write(
            dir.join("index.json"),
            serde_json::to_string_pretty(&index)? + "\n",
        )?;
        Ok(())
    }

    /// Run the configured `final_verify` list (falls back to `verify`)
    /// once. Never aborts: a red gate is a run outcome, not a harness
    /// failure. Status is owned by the caller so a red result can still
    /// enter the fixer instead of marking the run failed immediately.
    fn run_final_verify(&self) -> verify::RunResult {
        println!("\n=== final verify (full gate) ===");
        status::report(
            &self.review_dir,
            StatusPhase::Verifying,
            "-",
            "final full-suite verify running",
        )
        .ok();
        match verify::run_final(&self.repo) {
            Ok(r) if r.passed => {
                println!("  final verify: PASS");
                r
            }
            Ok(r) => {
                let cmd = r.failed_command.clone().unwrap_or_default();
                println!("  final verify: FAIL (`{cmd}`)");
                let diag = persist_final_diagnostics(&self.review_dir, &r);
                eprintln!(
                    "  ⚠ the per-cycle (scoped) verifies passed but the full gate is red — \
                     sending the fixer; full output: {}",
                    diag.display()
                );
                r
            }
            Err(e) => {
                // Spawning/timeout harness error: treat as red with the
                // error named, not a crash.
                eprintln!("  final verify: ERROR ({e:#})");
                let r = verify::RunResult {
                    passed: false,
                    failed_command: Some("(final verify errored)".to_string()),
                    output: format!("{e:#}"),
                    kill: None,
                };
                let diag = persist_final_diagnostics(&self.review_dir, &r);
                eprintln!("  full output: {}", diag.display());
                r
            }
        }
    }

    /// Close a red full-suite gate the same way a component's red verify
    /// is closed: classify, fix, re-run. Does not re-review. Exhausting
    /// the cycle budget (or a persistent environmental failure) leaves
    /// the gate red for the caller to fail the process.
    fn close_final_gate(&self, state: &mut State) -> Result<verify::RunResult> {
        let mut gate = self.run_final_verify();
        if gate.passed {
            return Ok(gate);
        }

        let originals: std::collections::BTreeMap<String, Option<Vec<u8>>> = state
            .components
            .keys()
            .map(|slug| (slug.clone(), read_optional(&self.findings_path(slug))))
            .collect();

        let mut cycles = 0usize;
        let mut env_retries = 0usize;
        while !gate.passed && cycles < MAX_FIX_CYCLES {
            if gate.kill.is_some() {
                if env_retries == 0 {
                    env_retries += 1;
                    println!("  final verify: stalled/timed out — retrying once (not a fix cycle)");
                    gate = self.run_final_verify();
                    continue;
                }
                break;
            }

            let classified = self.classify_final_gate(state, &gate)?;
            if classified.cause == "environmental" && env_retries == 0 {
                env_retries += 1;
                println!(
                    "  final verify: classified environmental ({}) — retrying once",
                    classified.component
                );
                gate = self.run_final_verify();
                continue;
            }

            cycles += 1;
            println!("\n=== full-gate fix {cycles}/{MAX_FIX_CYCLES} ===");

            let slug = classified.component.clone();
            status::report(
                &self.review_dir,
                StatusPhase::Fixing,
                &slug,
                &format!("full-gate fix {cycles}/{MAX_FIX_CYCLES} — {slug}"),
            )?;
            let finding = gate_finding(&gate, &classified.diagnostics);
            let outcome = match self.fix_component(state, &slug, &[finding]) {
                Ok(o) => o,
                Err(e) => {
                    commit::reset_worktree(&self.repo)?;
                    eprintln!("  gate fix failed: {e:#} — re-running the full gate");
                    gate = self.run_final_verify();
                    continue;
                }
            };
            println!("  gate fix {cycles}: {outcome}");

            let owned = self.component_paths_vec(state, &slug);
            let tampered = gate_edits_outside_component(&self.repo, &owned)?;
            if !tampered.is_empty() {
                commit::reset_worktree(&self.repo)?;
                eprintln!(
                    "  gate fix modified verify gate file(s): {} — wiping and retrying",
                    tampered.join(", ")
                );
                gate = self.run_final_verify();
                continue;
            }

            status::report(
                &self.review_dir,
                StatusPhase::Verifying,
                &slug,
                "re-running full-suite verify after gate fix",
            )?;
            gate = self.run_final_verify();
            if !gate.passed {
                continue;
            }

            let msg = format!("gaggle({slug}): fix full-suite verify");
            match commit::commit_dirty(&self.repo, &msg) {
                Ok(h) if !h.is_empty() => {
                    println!("  commit: {h}");
                    restore_optional(
                        &self.findings_path(&slug),
                        originals.get(&slug).and_then(|o| o.as_deref()),
                    );
                    if let Some(c) = state.components.get_mut(&slug) {
                        c.commit = Some(h.clone());
                        c.detail = format!("fixed full-suite verify {h}");
                    }
                    state.save(&self.state_path())?;
                }
                Ok(_) => {
                    println!("  commit: nothing new (tree already matches HEAD)");
                    restore_optional(
                        &self.findings_path(&slug),
                        originals.get(&slug).and_then(|o| o.as_deref()),
                    );
                }
                Err(e) => {
                    eprintln!("  commit FAILED after green full gate: {e:#}");
                    return Err(e);
                }
            }
        }

        if !gate.passed {
            commit::reset_worktree(&self.repo)?;
        }
        Ok(gate)
    }

    /// Ask the gate recipe which component owns the red full-suite
    /// failure. Classifier flakes and unknown slugs fall back to a
    /// path match against the catalog so a red gate still reaches the
    /// fixer.
    fn classify_final_gate(&self, state: &State, gate: &verify::RunResult) -> Result<GateClass> {
        let diagnostics = {
            let mut d = gate.output.clone();
            if let Some(cmd) = &gate.failed_command {
                d = format!("command `{cmd}` failed\n{d}");
            }
            d
        };
        let diag_file = persist_final_diagnostics(&self.review_dir, gate);
        let catalog = write_component_catalog(&self.review_dir, state)?;
        let recipe = recipes::path(&self.repo, "gate.yaml")?;
        let diag_str = diag_file.to_string_lossy().to_string();
        let catalog_str = catalog.to_string_lossy().to_string();
        let params = [
            ("diagnostics_file", diag_str.as_str()),
            ("components_file", catalog_str.as_str()),
        ];
        let parsed = match goose::run_recipe(&self.repo, &recipe, &params, Some(60)) {
            Ok(outcome) => {
                self.record_usage("-", &outcome.usage);
                parse_gate_class(&outcome.result, &diagnostics)
            }
            Err(e) => {
                eprintln!("  warning: gate classifier failed: {e:#} — treating as a fix");
                GateClass {
                    cause: "fix".to_string(),
                    component: String::new(),
                    diagnostics: diagnostics.clone(),
                }
            }
        };
        let slug = resolve_gate_component(state, &parsed.component, &parsed.diagnostics);
        println!("  gate: cause={} component={slug}", parsed.cause);
        Ok(GateClass {
            cause: parsed.cause,
            component: slug,
            diagnostics: parsed.diagnostics,
        })
    }

    /// Write checklist.md from current state (checkboxes + paths).
    fn persist_checklist(&self, state: &State) -> Result<()> {
        persist_checklist(&self.review_dir, state)
    }
    /// Generate `.review/final-report.md`. Strict format:
    ///
    /// ```text
    /// # Final Review Report
    /// ## Run summary        ← computed BY THE HARNESS from state (facts:
    ///                         counts, slugs, commits, model — cannot drift)
    /// ## OPEN QUESTIONS     ← synthesized by the report agent from the
    ///                         ledger + findings (judgment work only)
    /// ```
    ///
    /// The deterministic summary is written even when the agent recipe
    /// FAILS — an unreportable run must still leave the facts on disk;
    /// only the questions section degrades to a placeholder.
    fn generate_report(&self, state: &State, final_gate: &verify::RunResult) -> Result<()> {
        let recipe = recipes::path(&self.repo, "report.yaml")?;

        let ledger_path = self.review_dir.join("run-ledger.md");
        let mut ledger = String::from("# Run ledger\n\n");
        ledger.push_str("| slug | tier | phase | findings | commit | detail |\n");
        ledger.push_str("|------|------|-------|----------|--------|--------|\n");
        for c in state.components.values() {
            ledger.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                c.slug,
                c.tier,
                c.phase.as_str(),
                c.findings,
                c.commit.as_deref().unwrap_or("-"),
                c.detail
                    .replace('|', "\\|")
                    .replace('\n', " ")
                    .chars()
                    .take(80)
                    .collect::<String>(),
            ));
        }
        std::fs::write(&ledger_path, &ledger)?;

        // ---- Deterministic summary (harness-computed facts only) ----
        let total = state.components.len();
        let done: Vec<&crate::state::ComponentState> = state
            .components
            .values()
            .filter(|c| c.phase == Phase::Done)
            .collect();
        let failed: Vec<&crate::state::ComponentState> = state
            .components
            .values()
            .filter(|c| c.phase == Phase::Failed)
            .collect();
        let committed: Vec<&crate::state::ComponentState> = state
            .components
            .values()
            .filter(|c| c.commit.is_some())
            .collect();
        let clean: Vec<&&crate::state::ComponentState> =
            done.iter().filter(|c| c.findings == 0).collect();
        let leftover: Vec<&&crate::state::ComponentState> =
            done.iter().filter(|c| c.findings > 0).collect();

        let mut out = String::from("# Final Review Report\n\n## Run summary\n\n");
        out.push_str(&format!(
            "- reviewed: {} of {} components\n",
            done.len() + failed.len(),
            total
        ));
        let committed_list = committed
            .iter()
            .map(|c| format!("{} ({})", c.slug, c.commit.clone().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "- fixed+committed: {}{}\n",
            committed.len(),
            if committed_list.is_empty() {
                String::new()
            } else {
                format!(": {committed_list}")
            }
        ));
        let clean_list = clean
            .iter()
            .map(|c| c.slug.clone())
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!(
            "- clean: {}{}\n",
            clean.len(),
            if clean_list.is_empty() {
                String::new()
            } else {
                format!(": {clean_list}")
            }
        ));
        // needs-decision = quarantined + done-with-leftovers (a done
        // component can still carry unresolved findings).
        let mut needs: Vec<String> = failed
            .iter()
            .map(|c| format!("{} (failed, {} open)", c.slug, c.findings))
            .collect();
        needs.extend(
            leftover
                .iter()
                .map(|c| format!("{} (done, {} leftover)", c.slug, c.findings)),
        );
        out.push_str(&format!(
            "- needs-decision: {}{}\n",
            needs.len(),
            if needs.is_empty() {
                String::new()
            } else {
                format!(": {}", needs.join(", "))
            }
        ));
        out.push_str(&format!("- commits: {}\n", committed.len()));
        // End-of-run full gate result: scoped per-cycle verifies can pass
        // while the full suite is red (cross-component fallout, fixer-
        // added tests) — the report must never claim green over a red gate.
        out.push_str(&format!(
            "- final verify: {}\n",
            if final_gate.passed {
                "PASS".to_string()
            } else {
                format!(
                    "FAIL (`{}`)",
                    final_gate.failed_command.clone().unwrap_or_default()
                )
            }
        ));
        // The recipe agent cannot know the effective model — only the
        // harness does. Recorded here, once, for reproducibility, with
        // this run's accumulated token/cost usage (as reported by the
        // provider; absent fields mean the provider didn't report them).
        out.push_str(&format!(
            "- model: {}\n",
            goose::effective_model(&self.repo)
        ));
        if let Some(branch) = self.run_branch.borrow().as_deref() {
            out.push_str(&format!(
                "- branch: {branch} (dedicated — merge or revert as a unit)\n"
            ));
        }
        // Scoped borrows: the questions recipe below calls record_usage →
        // borrow_mut on the same cells; guards must drop before that.
        {
            let usage = self.usage_total.borrow();
            if !usage.is_empty() {
                out.push_str(&format!("- usage: {}\n", usage.summary()));
                // Per-component breakdown: the biggest spenders are the
                // useful signal (which components are expensive to
                // review/fix).
                let by_comp = self.usage_by_component.borrow();
                let mut rows: Vec<(&String, &goose::Usage)> = by_comp.iter().collect();
                rows.sort_by_key(|(_, u)| {
                    std::cmp::Reverse(u.cost_usd.map(|c| (c * 1e6) as u64).unwrap_or(0))
                });
                for (slug, u) in rows.iter().take(5) {
                    out.push_str(&format!("  - {slug}: {}\n", u.summary()));
                }
            }
        }

        // ---- Questions (agent-synthesized judgment) ----
        let findings_dir = self.review_dir.join("findings");
        let ledger_str = ledger_path.to_string_lossy().to_string();
        let findings_str = findings_dir.to_string_lossy().to_string();
        let params: [(&str, &str); 2] = [
            ("run_ledger", ledger_str.as_str()),
            ("findings_dir", findings_str.as_str()),
        ];

        out.push_str("\n## OPEN QUESTIONS\n\n");
        match goose::run_recipe(&self.repo, &recipe, &params, Some(60)) {
            Ok(outcome) => {
                self.record_usage("-", &outcome.usage);
                let items: Vec<String> = outcome
                    .result
                    .get("questions")
                    .and_then(|q| q.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|q| q.as_str())
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                if items.is_empty() {
                    if needs.is_empty() {
                        out.push_str("(none)\n");
                    } else {
                        out.push_str(
                            "(the report agent returned no questions — \
                          see the run ledger and findings files)\n",
                        );
                    }
                } else {
                    // Harness-owned numbering: the format stays strict no
                    // matter how the agent formatted its items.
                    for (i, q) in items.iter().enumerate() {
                        out.push_str(&format!("{}. {}\n\n", i + 1, q));
                    }
                }
            }
            Err(e) => {
                // Facts are already written; questions degrade to a
                // placeholder instead of losing the whole report.
                out.push_str(&format!(
                    "(questions unavailable — report agent failed: {e:#})\n"
                ));
            }
        }

        let report_path = self.review_dir.join("final-report.md");
        std::fs::write(&report_path, &out)?;
        println!("\n════════════════════════════════════════════");
        println!("  FINAL REVIEW REPORT → {}", report_path.display());
        println!("════════════════════════════════════════════");
        Ok(())
    }

    fn process_component(&self, state: &mut State, comp: &Component) -> Result<()> {
        let slug = comp.slug.clone();
        println!("\n=== {} — {} ===", slug, comp.name);

        state::transition(state, &slug, Phase::Reviewing)?;
        state.save(&self.state_path())?;
        status::report(
            &self.review_dir,
            StatusPhase::Reviewing,
            &slug,
            "review agent starting",
        )?;

        // Goose dying is a retry, not a quarantine. Only after the
        // attempt budget is spent do we park this component and move on.
        // Reset the worktree between attempts so a half-applied tool
        // call cannot leak into the next try (or the next component).
        let mut findings = None;
        let mut review_err = String::new();
        for attempt in 1..=MAX_FIX_CYCLES {
            match self.review_component(state, &slug) {
                Ok(f) => {
                    findings = Some(f);
                    break;
                }
                Err(e) => {
                    commit::reset_worktree(&self.repo)?;
                    review_err = format!("{e:#}");
                    eprintln!(
                        "  review attempt {attempt}/{MAX_FIX_CYCLES} failed: {review_err} — retrying"
                    );
                    status::report(
                        &self.review_dir,
                        StatusPhase::Reviewing,
                        &slug,
                        &format!("review agent failed ({attempt}/{MAX_FIX_CYCLES}) — retrying"),
                    )?;
                }
            }
        }
        let mut findings = match findings {
            Some(f) => f,
            None => {
                let why = format!("review failed after {MAX_FIX_CYCLES} attempts: {review_err}");
                return self.quarantine(state, &slug, &why);
            }
        };
        println!("  review: {} finding(s)", findings.len());
        state.set_findings(&slug, findings.len())?;

        if findings.is_empty() {
            // Remove any stale findings file from an earlier pass —
            // generate_report reads this directory, so a leftover file
            // would resurface findings this component no longer has.
            let stale = self.findings_path(&slug);
            if stale.exists() {
                let _ = std::fs::remove_file(&stale);
            }
            state::transition(state, &slug, Phase::Done)?;
            state.set_detail(&slug, "clean review — no findings")?;
            status::report(&self.review_dir, StatusPhase::Done, &slug, "no findings")?;
            return Ok(());
        }

        let mut cycles = 0;
        let mut last_verify_ok = false;
        let mut committed_once = false;
        let mut last_hash = String::new();

        while !findings.is_empty() && cycles < MAX_FIX_CYCLES {
            cycles += 1;
            // Already Fixing when the previous cycle's agent died before
            // verify — skip the self-transition (not in the machine).
            if state.get(&slug).map(|c| c.phase) != Some(Phase::Fixing) {
                state::transition(state, &slug, Phase::Fixing)?;
            }
            state.save(&self.state_path())?;
            status::report(
                &self.review_dir,
                StatusPhase::Fixing,
                &slug,
                &format!(
                    "fix cycle {cycles}/{} — {} finding(s)",
                    MAX_FIX_CYCLES,
                    findings.len()
                ),
            )?;

            let work_order = findings.clone();
            // Goose flake/kill: spend this cycle, keep the work order,
            // try again. Quarantine only when the budget is gone.
            let outcome = match self.fix_component(state, &slug, &findings) {
                Ok(o) => o,
                Err(e) => {
                    commit::reset_worktree(&self.repo)?;
                    eprintln!("  fix {cycles}: agent failed: {e:#} — retrying (cycle spent)");
                    state
                        .set_detail(&slug, &format!("fix agent failed in cycle {cycles}: {e:#}"))?;
                    continue;
                }
            };
            println!("  fix {cycles}: {outcome}");

            state::transition(state, &slug, Phase::Verifying)?;
            state.save(&self.state_path())?;
            status::report(
                &self.review_dir,
                StatusPhase::Verifying,
                &slug,
                "verify commands running",
            )?;

            let v = self.verify_until_stable(state, &slug)?;
            if v.passed {
                // Gate-tampering: rewriting a verify script that is not
                // this component's work is cheating. Editing a gate file
                // that this component owns (CI owns verify-artifacts.sh)
                // is the review — verify already passed with that edit.
                let owned = self.component_paths_vec(state, &slug);
                let tampered = gate_edits_outside_component(&self.repo, &owned)?;
                if !tampered.is_empty() {
                    let why = format!(
                        "fixer modified verify gate file(s): {} — refusing to commit a \
                         green result produced by a gate the agent changed",
                        tampered.join(", ")
                    );
                    return self.quarantine(state, &slug, &why);
                }
                last_verify_ok = true;
                println!("  verify: PASS");
                match self.commit_green(&slug) {
                    Ok(Some(h)) => {
                        committed_once = true;
                        last_hash = h.clone();
                        println!("  commit: {h}");
                    }
                    Ok(None) => {
                        committed_once = true;
                        println!("  commit: nothing new (tree already matches HEAD)");
                    }
                    Err(e) => {
                        eprintln!("  commit FAILED: {e:#}");
                        let why = format!("verify passed but commit failed: {e:#}");
                        return self.quarantine(state, &slug, &why);
                    }
                }
                status::report(
                    &self.review_dir,
                    StatusPhase::Reviewing,
                    &slug,
                    "confirming work-order findings",
                )?;
                // A confirm recipe flake must not close the work order.
                // The commit already landed — treat findings as still
                // open so the next cycle retries rather than claiming
                // closure.
                let still = match self.confirm_component(state, &slug, &work_order) {
                    Ok(s) => s,
                    Err(e) => {
                        eprintln!(
                            "  warning: confirm failed: {e:#} — treating work-order findings as still open"
                        );
                        work_order.clone()
                    }
                };
                println!("  confirm: {} still open", still.len());
                state.set_findings(&slug, still.len())?;
                findings = still;
                if !findings.is_empty() {
                    state.set_detail(
                        &slug,
                        &format!(
                            "committed{}; {} still open after confirm",
                            if last_hash.is_empty() {
                                String::new()
                            } else {
                                format!(" {last_hash}")
                            },
                            findings.len()
                        ),
                    )?;
                }
                continue;
            }

            last_verify_ok = false;
            let cause = v.cause.as_deref().unwrap_or("fix");
            let diagnostics = v.diagnostics.as_deref().unwrap_or("");
            println!("  verify: FAIL (cause={cause}) — sending the fixer");
            let mut next = vec![format!(
                "VERIFY FAILED (cause: {cause}). Diagnostics from the failed check:\n{}",
                verify::tail_bytes(diagnostics, DETAIL_TAIL),
            )];
            next.extend(work_order);
            findings = next;
            state.set_findings(&slug, findings.len())?;
            state.set_detail(
                &slug,
                &format!(
                    "verify fail cycle {cycles} (cause: {cause})\n{}",
                    verify::tail_bytes(diagnostics, DETAIL_TAIL),
                ),
            )?;
        }

        if committed_once || last_verify_ok {
            if !last_verify_ok {
                commit::reset_worktree(&self.repo)?;
            }
            self.finish_done(state, &slug, &findings, &last_hash)
        } else {
            let why = if findings.is_empty() {
                "fix cycles exhausted but verify never passed".to_string()
            } else {
                format!(
                    "{} finding(s) unresolved after {cycles} fix cycles",
                    findings.len()
                )
            };
            self.quarantine(state, &slug, &why)
        }
    }

    fn finish_done(
        &self,
        state: &mut State,
        slug: &str,
        leftover: &[String],
        last_hash: &str,
    ) -> Result<()> {
        self.write_findings_file(slug, leftover)?;
        state.set_findings(slug, leftover.len())?;
        let detail = if leftover.is_empty() {
            if last_hash.is_empty() {
                "fixed + verified".to_string()
            } else {
                format!("fixed + committed {last_hash}")
            }
        } else {
            format!(
                "kept last green{}; {} still open after confirm",
                if last_hash.is_empty() {
                    String::new()
                } else {
                    format!(" {last_hash}")
                },
                leftover.len()
            )
        };
        state.set_detail(slug, &detail)?;
        // Record the kept commit (if any) for the deterministic report.
        if let Some(c) = state.components.get_mut(slug) {
            c.commit = if last_hash.is_empty() {
                None
            } else {
                Some(last_hash.to_string())
            };
        }
        state::transition(state, slug, Phase::Committing)?;
        state::transition(state, slug, Phase::Done)?;
        status::report(&self.review_dir, StatusPhase::Done, slug, &detail)?;
        println!("  ✓ {slug}: {detail}");
        Ok(())
    }

    fn commit_green(&self, slug: &str) -> Result<Option<String>> {
        // No out-of-scope warning: components are STARTING POINTS, not
        // boundaries — the fix recipe explicitly instructs updating call
        // sites/tests/registrations wherever completeness requires, so
        // cross-file edits under the component's commit are expected and
        // attribution stays correct (each component starts from a clean
        // tree, so the dirty set is this fixer's work).
        let msg = format!("gaggle({slug}): fix review findings");
        let h = commit::commit_dirty(&self.repo, &msg)?;
        if h.is_empty() { Ok(None) } else { Ok(Some(h)) }
    }

    /// Wipe the worktree and park the component in Failed. Only called
    /// after the attempt budget is spent. Reset is fatal: a leftover
    /// dirty tree would leak into the next component's commit.
    fn quarantine(&self, state: &mut State, slug: &str, why: &str) -> Result<()> {
        commit::reset_worktree(&self.repo)?;
        state.set_detail(slug, why)?;
        state::transition(state, slug, Phase::Failed)?;
        status::report(&self.review_dir, StatusPhase::Failed, slug, why)?;
        println!("  ✗ {slug}: {why}");
        Ok(())
    }

    fn review_component(&self, state: &State, slug: &str) -> Result<Vec<String>> {
        let recipe = recipes::path(&self.repo, "review.yaml")?;
        let name = self
            .component_name(state, slug)
            .unwrap_or_else(|| slug.to_string());
        let paths = self.component_paths(state, slug);
        let params = [
            ("component", slug),
            ("component_name", &name),
            ("component_paths", &paths),
        ];
        // 100 turns (was 60): a large component review legitimately reads
        // many files; 60 proved too tight and the run died turn-capped
        // with no final JSON. Turns, not wall-clock: there is no default
        // goose duration cap.
        let outcome = goose::run_recipe(&self.repo, &recipe, &params, Some(100))?;
        self.record_usage(slug, &outcome.usage);
        parse_findings(&outcome.result)
    }

    /// Accumulate a recipe run's usage into the run total and the
    /// component breakdown. `slug` = "-" for run-level phases (report).
    fn record_usage(&self, slug: &str, usage: &goose::Usage) {
        if usage.is_empty() {
            return; // provider reported nothing — nothing to record
        }
        self.usage_total.borrow_mut().add(usage);
        self.usage_by_component
            .borrow_mut()
            .entry(slug.to_string())
            .or_default()
            .add(usage);
    }

    /// Write findings to a file and pass only the path — recipe `--params`
    /// cannot carry newlines or `=`.
    fn fix_component(&self, state: &State, slug: &str, findings: &[String]) -> Result<String> {
        let recipe = recipes::path(&self.repo, "fix.yaml")?;
        let findings_file = self.write_findings_file(slug, findings)?;
        let name = self
            .component_name(state, slug)
            .unwrap_or_else(|| slug.to_string());
        let paths = self.component_paths(state, slug);
        let findings_file_str = findings_file.to_string_lossy().to_string();
        let params = [
            ("component", slug),
            ("component_name", &name),
            ("component_paths", &paths),
            ("findings_file", &findings_file_str),
        ];
        let outcome = goose::run_recipe(&self.repo, &recipe, &params, Some(120))?;
        self.record_usage(slug, &outcome.usage);
        Ok(field(&outcome.result, "outcome")
            .unwrap_or("unknown")
            .to_string())
    }

    fn confirm_component(
        &self,
        state: &State,
        slug: &str,
        findings: &[String],
    ) -> Result<Vec<String>> {
        let recipe = recipes::path(&self.repo, "confirm.yaml")?;
        let findings_file = self.write_findings_file(slug, findings)?;
        let name = self
            .component_name(state, slug)
            .unwrap_or_else(|| slug.to_string());
        let paths = self.component_paths(state, slug);
        let findings_file_str = findings_file.to_string_lossy().to_string();
        let params = [
            ("component", slug),
            ("component_name", &name),
            ("component_paths", &paths),
            ("findings_file", &findings_file_str),
        ];
        let outcome = goose::run_recipe(&self.repo, &recipe, &params, Some(40))?;
        self.record_usage(slug, &outcome.usage);
        parse_still_open(&outcome.result)
    }

    /// Write a component's findings file. Contract: file PRESENCE means
    /// unresolved items exist — an empty findings list REMOVES the file so
    /// the report agent (and humans) can trust "no file = fully clean".
    fn write_findings_file(&self, slug: &str, findings: &[String]) -> Result<PathBuf> {
        let findings_file = self.findings_path(slug);
        if findings.is_empty() {
            if findings_file.exists() {
                std::fs::remove_file(&findings_file)?;
            }
            return Ok(findings_file);
        }
        let parent = findings_file.parent().ok_or_else(|| {
            anyhow::anyhow!("findings path has no parent directory: {findings_file:?}")
        })?;
        std::fs::create_dir_all(parent)?;
        std::fs::write(
            &findings_file,
            findings
                .iter()
                .enumerate()
                .map(|(i, f)| format!("{}. {f}", i + 1))
                .collect::<Vec<_>>()
                .join("\n\n"),
        )?;
        Ok(findings_file)
    }

    fn findings_path(&self, slug: &str) -> PathBuf {
        // Defense in depth: slugs are validated at parse/init, but a
        // hand-edited state.json must still not escape `.review/findings/`.
        let safe: String = slug
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe = if safe.is_empty() {
            "unknown"
        } else {
            safe.as_str()
        };
        self.review_dir.join("findings").join(format!("{safe}.txt"))
    }

    /// Retry an environmental / stall failure once. Still red after that
    /// is a fix work order — a red check means repair, not park.
    fn verify_until_stable(&self, state: &State, slug: &str) -> Result<VerifyVerdict> {
        let first = self.verify_component(state, slug)?;
        if first.passed || first.cause.as_deref() != Some("environmental") {
            return Ok(first);
        }
        println!("  verify: FAIL (environmental) — retrying once");
        let second = self.verify_component(state, slug)?;
        if second.passed {
            return Ok(second);
        }
        if second.cause.as_deref() == Some("environmental") {
            println!("  verify: still failing after retry — sending the fixer");
            return Ok(VerifyVerdict {
                passed: false,
                cause: Some("fix".to_string()),
                diagnostics: second.diagnostics,
            });
        }
        Ok(second)
    }

    /// Per-component commands from the checklist first (discovered), then
    /// the repo-wide `verify` list. Missing per-component commands → the
    /// repo list once. Pass/fail is the exit code; the recipe only
    /// classifies failures.
    fn verify_component(&self, state: &State, slug: &str) -> Result<VerifyVerdict> {
        let per = state
            .get(slug)
            .map(|c| c.verify.clone())
            .unwrap_or_default();
        let harness = if per.is_empty() {
            verify::run(&self.repo)?
        } else {
            let scoped = verify::run_list(&self.repo, &per)?;
            if scoped.passed {
                let full = verify::run(&self.repo)?;
                if full.passed { scoped } else { full }
            } else {
                scoped
            }
        };
        if harness.passed {
            return Ok(VerifyVerdict {
                passed: true,
                cause: None,
                diagnostics: None,
            });
        }
        let mut diagnostics = harness.output;
        if let Some(cmd) = &harness.failed_command {
            diagnostics = format!("command `{cmd}` failed\n{diagnostics}");
        }
        // Stall (and optional wall-clock timeout) are harness facts, not
        // fixer bugs — skip the classifier so we do not burn a fix cycle.
        if harness.kill.is_some() {
            let diag_file = self.review_dir.join("verify-diagnostics.txt");
            let _ = std::fs::write(&diag_file, &diagnostics);
            return Ok(VerifyVerdict {
                passed: false,
                cause: Some("environmental".to_string()),
                diagnostics: Some(diagnostics),
            });
        }
        let name = self
            .component_name(state, slug)
            .unwrap_or_else(|| slug.to_string());
        let paths = self.component_paths(state, slug);
        let diag_file = self.review_dir.join("verify-diagnostics.txt");
        std::fs::write(&diag_file, &diagnostics)?;
        let diag_str = diag_file.to_string_lossy().to_string();
        let recipe = recipes::path(&self.repo, "verify.yaml")?;
        let params = [
            ("component", slug),
            ("component_name", &name),
            ("component_paths", &paths),
            ("diagnostics_file", &diag_str),
        ];
        match goose::run_recipe(&self.repo, &recipe, &params, Some(60)) {
            Ok(outcome) => {
                self.record_usage(slug, &outcome.usage);
                let cause = field(&outcome.result, "cause").unwrap_or("fix").to_string();
                let classified = field(&outcome.result, "diagnostics")
                    .map(str::to_string)
                    .unwrap_or(diagnostics);
                Ok(VerifyVerdict {
                    passed: false,
                    cause: Some(cause),
                    diagnostics: Some(classified),
                })
            }
            Err(e) => {
                eprintln!("  warning: verify classifier failed: {e:#}");
                Ok(VerifyVerdict {
                    passed: false,
                    cause: Some("fix".to_string()),
                    diagnostics: Some(diagnostics),
                })
            }
        }
    }

    fn component_name(&self, state: &State, slug: &str) -> Option<String> {
        state.get(slug).map(|c| c.name.clone())
    }

    /// Resolve a component slug to file paths.
    ///
    /// Priority: discovered/checklist paths in state, then `src-<module>`.
    fn component_paths_vec(&self, state: &State, slug: &str) -> Vec<String> {
        if let Some(c) = state.get(slug) {
            if !c.paths.is_empty() {
                return c.paths.clone();
            }
        }
        let resolved = resolve_src_paths(&self.repo, slug);
        if resolved.is_empty() {
            Vec::new()
        } else {
            vec![resolved]
        }
    }

    fn component_paths(&self, state: &State, slug: &str) -> String {
        self.component_paths_vec(state, slug).join(", ")
    }
}

/// Resolve a `src-<module>` slug to file/dir paths under `src/`.
///
/// The loop module is `loop_engine` (not `loop`); hyphens in slugs become
/// underscores when probing files.
fn resolve_src_paths_vec(repo: &Path, slug: &str) -> Vec<String> {
    if let Some(file) = slug.strip_prefix("src-") {
        let file = if file == "loop" { "loop_engine" } else { file };
        let candidates = if file.contains('-') {
            vec![file.replace('-', "_"), file.to_string()]
        } else {
            vec![file.to_string()]
        };
        for cand in &candidates {
            let rs = repo.join("src").join(format!("{cand}.rs"));
            if rs.exists() {
                return vec![rs.to_string_lossy().to_string()];
            }
            let dir = repo.join("src").join(cand);
            if dir.is_dir() {
                return vec![dir.to_string_lossy().to_string()];
            }
        }
        eprintln!(
            "  warning: could not resolve paths for slug '{slug}' (no src/{file}.rs or src/{file}/)"
        );
    } else {
        // Non-`src-` slugs carry no implicit path mapping; without this
        // warning the component would review with an empty component_paths
        // and the agent would explore blindly. Printed ONCE per slug per
        // process — component_paths_vec runs per phase, so an unguarded
        // note would repeat four times per component.
        static NOTED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> =
            std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashSet::new()));
        if NOTED
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(slug.to_string())
        {
            eprintln!(
                "  note: no path mapping for slug '{slug}' — components outside src/ keep whatever paths discovery/checklist provides (none so far)"
            );
        }
    }
    Vec::new()
}

fn resolve_src_paths(repo: &Path, slug: &str) -> String {
    resolve_src_paths_vec(repo, slug).join(", ")
}

/// Classifier result for a red full-suite gate.
struct GateClass {
    cause: String,
    component: String,
    diagnostics: String,
}

fn parse_gate_class(result: &Value, fallback_diag: &str) -> GateClass {
    let raw_cause = field(result, "cause").unwrap_or("fix").trim();
    let cause = if raw_cause.eq_ignore_ascii_case("environmental") {
        "environmental"
    } else {
        "fix"
    };
    let component = field(result, "component")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("")
        .to_string();
    let diagnostics = field(result, "diagnostics")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| fallback_diag.to_string());
    GateClass {
        cause: cause.to_string(),
        component,
        diagnostics,
    }
}

/// Prefer the classifier's slug when it is a real component; otherwise
/// the longest catalog path that appears in the diagnostics; otherwise
/// a synthetic slug so the fixer still runs.
fn resolve_gate_component(state: &State, suggested: &str, diagnostics: &str) -> String {
    if state.components.contains_key(suggested) {
        return suggested.to_string();
    }
    match_component_from_diagnostics(state, diagnostics).unwrap_or_else(|| "final-gate".to_string())
}

/// Longest component path (then slug) that appears as a substring of
/// `diagnostics`. Language-agnostic: just string contains.
fn match_component_from_diagnostics(state: &State, diagnostics: &str) -> Option<String> {
    let mut best: Option<(usize, &str, &str)> = None;
    for c in state.components.values() {
        for p in &c.paths {
            let p = p.trim().trim_end_matches('/');
            if p.is_empty() {
                continue;
            }
            if diagnostics.contains(p) {
                let score = p.len();
                let take = match best {
                    None => true,
                    Some((s, slug, _)) => score > s || (score == s && c.slug.as_str() < slug),
                };
                if take {
                    best = Some((score, c.slug.as_str(), p));
                }
            }
        }
    }
    best.map(|(_, slug, _)| slug.to_string())
}

fn write_component_catalog(review_dir: &Path, state: &State) -> Result<PathBuf> {
    let path = review_dir.join("gate-components.txt");
    let mut body = String::from(
        "# slug | name | phase | verify | paths\n\
         # Pick the slug whose paths own the failing file.\n",
    );
    for c in state.components.values() {
        let verify = if c.verify.is_empty() {
            "(repo verify list)".to_string()
        } else {
            c.verify.join("; ")
        };
        let paths = if c.paths.is_empty() {
            "(none)".to_string()
        } else {
            c.paths.join(", ")
        };
        body.push_str(&format!(
            "{} | {} | {} | {} | {}\n",
            c.slug,
            c.name,
            c.phase.as_str(),
            verify,
            paths
        ));
    }
    std::fs::write(&path, body)?;
    Ok(path)
}

/// Verify-script edits that are **not** this component's files.
/// Changing a gate file you own is the review; changing one you don't is
/// rewriting the exam.
fn gate_edits_outside_component(repo: &Path, owned: &[String]) -> Result<Vec<String>> {
    let gates = verify::gate_files(repo);
    if gates.is_empty() {
        return Ok(Vec::new());
    }
    Ok(commit::dirty_paths(repo)?
        .into_iter()
        .filter(|p| gates.iter().any(|g| p == g))
        .filter(|p| !path_in_component(p, owned))
        .collect())
}

fn path_in_component(path: &str, owned: &[String]) -> bool {
    owned.iter().any(|root| {
        let root = root.trim().trim_end_matches('/');
        !root.is_empty() && (path == root || path.starts_with(&format!("{root}/")))
    })
}

fn gate_finding(gate: &verify::RunResult, diagnostics: &str) -> String {
    let cmd = gate.failed_command.clone().unwrap_or_default();
    format!(
        "FULL-SUITE VERIFY FAILED (command: `{cmd}`). This is the work — \
         make this command pass. Do not dismiss it as pre-existing or out of \
         scope; the per-component check already passed and missed it.\n\
         Full command output is in .review/verify-diagnostics.txt — open it.\n\n{}",
        verify::tail_bytes(diagnostics, DETAIL_TAIL)
    )
}

/// Write `.review/verify-diagnostics.txt` from a final-gate result so a
/// red run is inspectable. Per-cycle verify already writes this file;
/// the full gate used to drop its output on the floor.
fn read_optional(path: &Path) -> Option<Vec<u8>> {
    std::fs::read(path).ok()
}

fn restore_optional(path: &Path, prior: Option<&[u8]>) {
    match prior {
        Some(bytes) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, bytes);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn persist_final_diagnostics(
    review_dir: &std::path::Path,
    result: &verify::RunResult,
) -> std::path::PathBuf {
    let path = review_dir.join("verify-diagnostics.txt");
    let mut body = result.output.clone();
    if let Some(cmd) = &result.failed_command {
        body = format!("command `{cmd}` failed\n{body}");
    }
    if let Err(e) = std::fs::write(&path, &body) {
        eprintln!("  warning: failed to write {}: {e}", path.display());
    }
    path
}

/// Parse findings from the review recipe's final JSON.
///   {"findings": ["...", "..."]}   (empty array = clean)
/// Missing key / null / unexpected shapes are sentinel review errors.
fn parse_findings(result: &Value) -> Result<Vec<String>> {
    match result.get("findings") {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                let s = item.as_str().unwrap_or("");
                if !s.trim().is_empty() {
                    out.push(s.trim().to_string());
                }
            }
            Ok(out)
        }
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(vec![s.trim().to_string()]),
        None => {
            eprintln!("  review: missing findings key, flagging as review error");
            Ok(vec![
                "review result was malformed (missing findings key, expected an array)".to_string(),
            ])
        }
        Some(Value::Null) | Some(Value::String(_)) => {
            eprintln!("  review: findings value is null/empty-string, flagging as review error");
            Ok(vec![
                "review result was malformed (findings value was null or empty-string, expected an array)".to_string(),
            ])
        }
        Some(other) => {
            eprintln!("  review: unexpected findings shape ({other}), flagging as review error");
            Ok(vec![format!(
                "review result was malformed (unexpected findings shape: {other})"
            )])
        }
    }
}

/// Confirmation result. Missing/malformed `still_open` is treated as empty
/// so a green verify is not reopened by a bad confirm payload.
fn parse_still_open(result: &Value) -> Result<Vec<String>> {
    match result.get("still_open") {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                let s = item.as_str().unwrap_or("");
                if !s.trim().is_empty() {
                    out.push(s.trim().to_string());
                }
            }
            Ok(out)
        }
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(vec![s.trim().to_string()]),
        None | Some(Value::Null) | Some(Value::String(_)) => Ok(Vec::new()),
        Some(other) => {
            eprintln!("  confirm: unexpected still_open shape ({other}), treating as empty");
            Ok(Vec::new())
        }
    }
}

/// Scaffold a fresh `.review/` directory.
///
/// With `components` non-empty: use them directly (slug|name|tier).
/// With `components` empty: run AI discovery and use the validated result.
pub fn init(
    repo: &Path,
    components: &[(String, String, String)],
) -> Result<Vec<discover::DiscoveredComponent>> {
    let review = repo.join(crate::REVIEW_DIR);
    std::fs::create_dir_all(review.join("findings"))?;
    recipes::ensure_config(repo)?;
    recipes::ensure_gitignore(repo)?;

    if !components.is_empty() {
        let list: Vec<Component> = components
            .iter()
            .map(|(slug, name, tier)| Component::new(slug, name, tier))
            .collect();
        let mut state = State::default();
        state.sync(&list);
        for c in &list {
            let resolved = resolve_src_paths_vec(repo, &c.slug);
            if !resolved.is_empty() {
                if let Some(st) = state.components.get_mut(&c.slug) {
                    st.paths = resolved;
                }
            }
        }
        let list = checklist_from_state(&state, &list);
        warn_missing_paths(repo, &list);
        checklist::save(&review.join("checklist.md"), &list)?;
        state.save(&review.join("state.json"))?;
        status::report(
            &review,
            StatusPhase::Idle,
            "-",
            "initialized (explicit components)",
        )?;
        return Ok(Vec::new());
    }

    println!("AI component discovery…");
    let project = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let existing_path = review.join("existing-checklist.md");
    let existing_text = load_existing_slugs(repo).unwrap_or_else(|| {
        "No existing checklist. Invent fresh slugs for every component.".to_string()
    });
    std::fs::write(&existing_path, existing_text)?;
    let discovered = discover::discover(repo, &project, &existing_path)?;
    let proposal = review.join("discovery-proposal.json");
    discover::write_proposal(&proposal, &discovered)?;
    println!("  validating discovery…");
    let discovered = discover::validate(repo, &project, &proposal, discovered)?;
    recipes::apply_discovered_gates(repo, &discovered.verify, &discovered.final_verify)?;
    println!("  discovered {} component(s):", discovered.components.len());
    for c in &discovered.components {
        println!(
            "    {:<24} {:<6} pri={:<4} {}",
            c.slug, c.tier, c.priority, c.name
        );
    }

    let list: Vec<Component> = discovered
        .components
        .iter()
        .map(|c| {
            let mut comp = Component::new(&c.slug, &c.name, &c.tier);
            comp.paths = c.paths.clone();
            comp.verify = c.verify.clone();
            comp
        })
        .collect();
    warn_missing_paths(repo, &list);
    checklist::save(&review.join("checklist.md"), &list)?;
    let mut state = State::default();
    state.sync(&list);
    state.save(&review.join("state.json"))?;
    status::report(
        &review,
        StatusPhase::Idle,
        "-",
        "initialized (ai-discovery)",
    )?;
    Ok(discovered.components)
}

fn checklist_from_state(state: &State, list: &[Component]) -> Vec<Component> {
    list.iter()
        .map(|c| {
            let mut out = c.clone();
            if let Some(st) = state.get(&c.slug) {
                out.paths = st.paths.clone();
            }
            out
        })
        .collect()
}

fn warn_missing_paths(repo: &Path, list: &[Component]) {
    for c in list {
        for p in &c.paths {
            if !repo.join(p).exists() {
                eprintln!(
                    "  warning: path `{p}` for component `{}` does not exist",
                    c.slug
                );
            }
        }
    }
}

/// Render existing slugs as a "reuse these" hint for discovery (re-init).
fn load_existing_slugs(repo: &Path) -> Option<String> {
    let path = repo.join(".review/checklist.md");
    let comps = checklist::load(&path).ok()?;
    if comps.is_empty() {
        return None;
    }
    let state = State::load(&repo.join(".review/state.json")).ok();
    let mut out = Vec::new();
    for c in &comps {
        let extra = state
            .as_ref()
            .and_then(|s| s.get(&c.slug))
            .map(|st| {
                let paths = if st.paths.is_empty() {
                    c.paths.join(", ")
                } else {
                    st.paths.join(", ")
                };
                if paths.is_empty() {
                    String::new()
                } else {
                    format!("  paths: {paths}")
                }
            })
            .unwrap_or_else(|| {
                if c.paths.is_empty() {
                    String::new()
                } else {
                    format!("  paths: {}", c.paths.join(", "))
                }
            });
        out.push(format!("- {} — {} [{}]{}", c.slug, c.name, c.tier, extra));
    }
    Some(out.join("\n"))
}

/// Print a component table (CLI `gaggle list`).
pub fn list(repo: &Path) -> Result<()> {
    let components = checklist::load(&repo.join(".review/checklist.md"))?;
    let state = State::load(&repo.join(".review/state.json"))?;
    println!(
        "{:<28} {:<10} {:<10} {:<8} detail",
        "slug", "tier", "phase", "findings"
    );
    for c in &components {
        let st = state.get(&c.slug);
        let phase = st.map(|s| s.phase.as_str()).unwrap_or("pending");
        let findings = st.map(|s| s.findings).unwrap_or(0);
        let detail = st.map(|s| s.detail.as_str()).unwrap_or("");
        println!(
            "{:<28} {:<10} {:<10} {:<8} {}",
            c.slug, c.tier, phase, findings, detail
        );
    }
    Ok(())
}

/// Print past runs from `.review/runs/` (CLI `gaggle history`), most
/// recent first. `gaggle history <ts>` prints one run's full report.
pub fn history(repo: &Path, ts: Option<&str>) -> Result<()> {
    let runs_dir = repo.join(crate::REVIEW_DIR).join("runs");
    if !runs_dir.exists() {
        println!("no run history yet — every finished `gaggle run` is archived here");
        return Ok(());
    }

    // Collect + sort descending by directory name (timestamped).
    let mut entries: Vec<String> = std::fs::read_dir(&runs_dir)?
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    entries.sort();
    entries.reverse();
    if entries.is_empty() {
        println!("no run history yet");
        return Ok(());
    }

    match ts {
        Some(ts) => {
            let dir = runs_dir.join(ts);
            if !dir.is_dir() {
                bail!("no archived run named {ts:?} — try `gaggle history` for the list");
            }
            let report = dir.join("final-report.md");
            if report.exists() {
                print!("{}", std::fs::read_to_string(&report)?);
            } else {
                println!("(no report archived for {ts})");
            }
            println!("\nartifacts: {}", dir.display());
            Ok(())
        }
        None => {
            println!(
                "{:<18} {:<7} {:>5} {:>7} {:>9} {:>10}  model/usage",
                "run", "outcome", "done", "failed", "leftover", "cost"
            );
            for entry in &entries {
                let index = runs_dir.join(entry).join("index.json");
                let Ok(text) = std::fs::read_to_string(&index) else {
                    println!("{entry:<18} (no index — incomplete archive?)");
                    continue;
                };
                let v: serde_json::Value = text.parse().unwrap_or(serde_json::Value::Null);
                let get = |k: &str| v.get(k).cloned().unwrap_or(serde_json::Value::Null);
                let usage = v.get("usage").cloned().unwrap_or(serde_json::Value::Null);
                let cost = usage
                    .get("cost_usd")
                    .and_then(|c| c.as_f64())
                    .map(|c| format!("${c:.4}"))
                    .unwrap_or_else(|| "-".to_string());
                // Table wants provider/model, not the full provenance line.
                let model_full = get("model").as_str().unwrap_or("-").to_string();
                let model = model_full.split(" — ").next().unwrap_or("-").to_string();
                println!(
                    "{:<18} {:<7} {:>5} {:>7} {:>9} {:>10}  {}",
                    entry,
                    get("outcome").as_str().unwrap_or("?"),
                    get("done").as_u64().unwrap_or(0),
                    get("failed").as_u64().unwrap_or(0),
                    get("leftover_findings").as_u64().unwrap_or(0),
                    cost,
                    model
                );
            }
            println!("\ndetail: gaggle history <run-id>");
            Ok(())
        }
    }
}

/// Write checklist.md from current state: checkbox = phase Done, paths
/// refreshed from state. Shared by the engine loop and `requeue` so the
/// file always mirrors the state machine after a mutation.
fn persist_checklist(review_dir: &Path, state: &State) -> Result<()> {
    let path = review_dir.join("checklist.md");
    let mut comps = checklist::load(&path)?;
    for c in &mut comps {
        if let Some(st) = state.get(&c.slug) {
            c.done = st.phase == Phase::Done;
            if !st.paths.is_empty() {
                c.paths = st.paths.clone();
            }
            c.verify = st.verify.clone();
        }
    }
    checklist::save(&path, &comps)
}

/// Requeue quarantined (Failed) components back to Pending so the next
/// `gaggle run` retries them (CLI `gaggle requeue <slug>… | --all`).
///
/// Only Failed components are eligible here. Unchecking a component in
/// the checklist also requeues Done *and* Failed rows on the next
/// `gaggle run` (see `State::sync`). Touching an active-phase component
/// would fight the running engine. Quarantine detail is moved to a
/// `previously:` prefix rather than discarded, so the retry's history
/// stays visible in `gaggle list`.
pub fn requeue(repo: &Path, slugs: &[String], all: bool) -> Result<Vec<String>> {
    let review_dir = repo.join(crate::REVIEW_DIR);
    let state_path = review_dir.join("state.json");
    if !state_path.exists() {
        bail!(
            "no state at {} — nothing to requeue (run `gaggle run` first)",
            state_path.display()
        );
    }
    let mut state = State::load(&state_path)?;

    let targets: Vec<String> = if all {
        state
            .components
            .values()
            .filter(|c| c.phase == Phase::Failed)
            .map(|c| c.slug.clone())
            .collect()
    } else {
        slugs.to_vec()
    };
    if all && targets.is_empty() {
        println!("no quarantined components to requeue");
        return Ok(Vec::new());
    }

    let mut requeued = Vec::new();
    for slug in &targets {
        let phase = state
            .get(slug)
            .map(|c| c.phase)
            .ok_or_else(|| anyhow::anyhow!("unknown component {slug}"))?;
        if phase != Phase::Failed {
            bail!(
                "{slug} is {}, not failed — requeue applies only to quarantined components \
                 (uncheck a done component in checklist.md to redo it)",
                phase.as_str()
            );
        }
        state::transition(&mut state, slug, Phase::Pending)?;
        let prior = state
            .get(slug)
            .map(|c| c.detail.clone())
            .unwrap_or_default();
        state.set_detail(slug, &format!("requeued (previously: {})", prior.trim()))?;
        state.set_findings(slug, 0)?;
        requeued.push(slug.clone());
        println!("  requeued {slug}");
    }

    state.save(&state_path)?;
    persist_checklist(&review_dir, &state)?;
    status::report(
        &review_dir,
        StatusPhase::Idle,
        "-",
        &format!(
            "requeued {} component(s): {}",
            requeued.len(),
            requeued.join(", ")
        ),
    )?;
    Ok(requeued)
}

/// Reset every component to pending for a new pass over the same
/// checklist. Keeps `config.toml`, recipe overrides, and `.review/runs/`
/// archives. Clears current findings and the last report so the next
/// `gaggle run` is a full review, not a no-op on checked boxes.
pub fn restart(repo: &Path) -> Result<usize> {
    let review_dir = repo.join(crate::REVIEW_DIR);
    let state_path = review_dir.join("state.json");
    let checklist_path = review_dir.join("checklist.md");
    if !state_path.exists() || !checklist_path.exists() {
        bail!("no review to restart — run `gaggle init` first");
    }
    let mut state = State::load(&state_path)?;
    if state.components.is_empty() {
        bail!("state has no components — run `gaggle init` first");
    }
    let n = state.components.len();
    state.restart_all();
    state.save(&state_path)?;
    persist_checklist(&review_dir, &state)?;
    clear_current_run_artifacts(&review_dir)?;
    status::report(
        &review_dir,
        StatusPhase::Idle,
        "-",
        &format!("restarted {n} component(s) to pending"),
    )?;
    Ok(n)
}

/// Drop this run's findings/report files. History under `runs/` stays.
fn clear_current_run_artifacts(review_dir: &Path) -> Result<()> {
    for name in [
        "final-report.md",
        "run-ledger.md",
        "verify-diagnostics.txt",
        "gate-components.txt",
    ] {
        let p = review_dir.join(name);
        if p.exists() {
            std::fs::remove_file(&p)?;
        }
    }
    let findings = review_dir.join("findings");
    if findings.is_dir() {
        for entry in std::fs::read_dir(&findings)? {
            let path = entry?.path();
            if path.is_file() {
                std::fs::remove_file(&path)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_findings_is_error() {
        let v = json!({"status": "ok"});
        let f = parse_findings(&v).unwrap();
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("missing findings key"));
    }

    #[test]
    fn empty_array_is_clean() {
        let v = json!({"findings": []});
        assert!(parse_findings(&v).unwrap().is_empty());
    }

    #[test]
    fn null_findings_is_error() {
        let v = json!({"findings": null});
        let f = parse_findings(&v).unwrap();
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("malformed"));
    }

    #[test]
    fn still_open_parses_array() {
        let v = json!({"still_open": ["a", "", "b"]});
        assert_eq!(parse_still_open(&v).unwrap(), vec!["a", "b"]);
    }

    #[test]
    fn component_owns_its_own_gate_script() {
        let owned = vec![
            "scripts/verify-artifacts.sh".into(),
            ".github/workflows".into(),
        ];
        assert!(path_in_component("scripts/verify-artifacts.sh", &owned));
        assert!(path_in_component(".github/workflows/ci.yml", &owned));
        assert!(!path_in_component("src/main.rs", &owned));
    }

    #[test]
    fn missing_still_open_is_empty() {
        let v = json!({"status": "ok"});
        assert!(parse_still_open(&v).unwrap().is_empty());
    }

    #[test]
    fn findings_path_stays_inside_findings_dir() {
        let engine = Engine::new(std::path::Path::new("/tmp/gaggle-path-test"));
        let p = engine.findings_path("../evil");
        let findings = engine.review_dir.join("findings");
        assert!(p.starts_with(&findings), "{}", p.display());
        assert_eq!(p.file_name().unwrap(), "___evil.txt");
        let p = engine.findings_path("loop-engine");
        assert_eq!(p.file_name().unwrap(), "loop-engine.txt");
    }

    #[test]
    fn gate_class_defaults_unknown_cause_to_fix() {
        let v = json!({"cause": "maybe", "component": "db", "diagnostics": "boom"});
        let g = parse_gate_class(&v, "fallback");
        assert_eq!(g.cause, "fix");
        assert_eq!(g.component, "db");
        assert_eq!(g.diagnostics, "boom");
    }

    #[test]
    fn restart_unchecks_checklist_and_clears_findings() {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-restart-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let review = dir.join(".review");
        std::fs::create_dir_all(review.join("findings")).unwrap();
        let mut comps = vec![Component::new("core", "Core", "high")];
        comps[0].done = true;
        comps[0].paths = vec!["src/lib.rs".into()];
        checklist::save(&review.join("checklist.md"), &comps).unwrap();
        let mut state = State::default();
        state.sync(&comps);
        state.save(&review.join("state.json")).unwrap();
        std::fs::write(review.join("findings").join("core.txt"), "old").unwrap();
        std::fs::write(review.join("final-report.md"), "old").unwrap();
        std::fs::create_dir_all(review.join("runs").join("keep-me")).unwrap();
        std::fs::write(
            review.join("runs").join("keep-me").join("final-report.md"),
            "arch",
        )
        .unwrap();

        let n = restart(&dir).unwrap();
        assert_eq!(n, 1);
        let state = State::load(&review.join("state.json")).unwrap();
        assert_eq!(state.get("core").unwrap().phase, Phase::Pending);
        let comps = checklist::load(&review.join("checklist.md")).unwrap();
        assert!(!comps[0].done);
        assert_eq!(comps[0].paths, vec!["src/lib.rs"]);
        assert!(!review.join("findings").join("core.txt").exists());
        assert!(!review.join("final-report.md").exists());
        assert!(
            review
                .join("runs")
                .join("keep-me")
                .join("final-report.md")
                .exists(),
            "history archives must survive restart"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restart_without_init_errors() {
        let dir = std::env::temp_dir().join(format!("gaggle-restart-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = restart(&dir).unwrap_err().to_string();
        assert!(err.contains("gaggle init"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_class_accepts_environmental() {
        let v = json!({"cause": "environmental", "component": "cli"});
        let g = parse_gate_class(&v, "fallback");
        assert_eq!(g.cause, "environmental");
        assert_eq!(g.component, "cli");
        assert_eq!(g.diagnostics, "fallback");
    }

    #[test]
    fn match_picks_longest_path_in_diagnostics() {
        let mut s = State::default();
        s.sync(&[
            Component {
                slug: "db".into(),
                name: "DB".into(),
                tier: "high".into(),
                done: true,
                paths: vec!["crates/oxllm-db".into()],
                verify: vec![],
            },
            Component {
                slug: "server-pipeline".into(),
                name: "Pipeline".into(),
                tier: "high".into(),
                done: true,
                paths: vec![
                    "crates/oxllm-server/src/pipeline".into(),
                    "crates/oxllm-server/src/spend_writer.rs".into(),
                ],
                verify: vec!["cargo test -p oxllm-server".into()],
            },
        ]);
        let diag = "panicked at crates/oxllm-server/src/spend_writer.rs:882: \
                    no such column: end_user_id";
        assert_eq!(
            match_component_from_diagnostics(&s, diag).as_deref(),
            Some("server-pipeline")
        );
        assert_eq!(
            resolve_gate_component(&s, "not-a-slug", diag),
            "server-pipeline"
        );
        assert_eq!(resolve_gate_component(&s, "db", diag), "db");
        assert_eq!(
            resolve_gate_component(&s, "", "nothing matches"),
            "final-gate"
        );
    }
}
