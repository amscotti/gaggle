//! Agent driver: runs a goose recipe as a subprocess and captures the
//! structured result. This is the GDK embedding surface — today that means
//! shelling out to `goose run --recipe <file> --params k=v --output-format
//! json` (the goose CLI is built on the GDK Rust API; the published
//! `goose-sdk` crate is still early/experimental ACP wire types). Swapping
//! this for the in-process GDK API later changes only this module.
//!
//! OUTPUT SHAPE (critical): with `--output-format json`, goose prints a
//! single JSON envelope `{"messages": [...]}`. The recipe's final JSON line
//! (e.g. `{"findings": [...]}` or `{"outcome": "fixed"}`) appears as the
//! LAST line of the LAST assistant text block INSIDE the envelope. This
//! module parses the envelope and extracts that line — the same contract
//! the old `extract_result.py` implemented for the PE pipeline.
//!
//! Model: configurable, never hard-coded. Optional top-level `provider` /
//! `model` keys in `.review/config.toml` are exported as GOOSE_PROVIDER /
//! GOOSE_MODEL for each run; when absent, goose resolves its own default
//! (its config.yaml, or GOOSE_* inherited from the shell). Recipe files do
//! NOT pin a model — goose recipe `settings:` would override both.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

/// The agent model pin resolved from `.review/config.toml`.
/// Both fields optional; absence defers to goose's configured default.
#[derive(Debug, Default, PartialEq)]
pub struct ModelPin {
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Load the optional `provider` / `model` keys from `.review/config.toml`.
///
/// Resolution for a given phase: `[model.<phase>]` → `[model]` (or
/// top-level keys) → unset (goose's configured default). Keys unset in a
/// phase section inherit from the base. Per-phase sections require the
/// `[model]` SECTION form for the base — a bare top-level `model = "..."`
/// key conflicts with `[model.fix]` as a table, and TOML rejects the
/// file outright, so the conflict is surfaced with a targeted message.
///
/// A missing file or missing keys is fine (empty pin). A malformed file
/// is an error — the run would silently use a different model than
/// configured.
pub fn model_pin_for(repo: &Path, phase: &str) -> Result<ModelPin> {
    let cfg = repo.join(".review/config.toml");
    let text = match std::fs::read_to_string(&cfg) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ModelPin::default()),
        Err(e) => return Err(e).with_context(|| format!("failed to read `{}`", cfg.display())),
    };
    let t: toml::Value = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            // Targeted messages for the known footguns around bare
            // top-level model keys + [model.<phase>] tables.
            let es = e.to_string();
            if text.contains("[model.")
                && (es.contains("duplicate key") || es.contains("attempted to extend non-table"))
            {
                bail!(
                    "{} failed to parse ({e}) — a bare top-level `model =` key cannot \
                     coexist with `[model.<phase>]` sections. Move the base keys INSIDE \
                     `[model]` (section form).",
                    cfg.display()
                );
            }
            return Err(anyhow::anyhow!(e))
                .with_context(|| format!("failed to parse `{}`", cfg.display()));
        }
    };
    let base = t.get("model").filter(|v| v.is_table());
    let phase_section = base.and_then(|m| m.get(phase)).filter(|v| v.is_table());

    // Unknown phase sub-tables are almost certainly typos (e.g. [model.fx]);
    // a silently-ignored section would run the wrong model. Validate against
    // the known recipe names once.
    if let Some(m) = base {
        if let Some(table) = m.as_table() {
            for key in table.keys() {
                if !KNOWN_PHASES.contains(&key.as_str()) && key != "provider" && key != "model" {
                    bail!(
                        "`[model.{key}]` in {} is not a known phase — expected one of: {} \
                         (or the base keys provider/model)",
                        cfg.display(),
                        KNOWN_PHASES.join(", ")
                    );
                }
            }
        }
    }

    let get = |scope: Option<&toml::Value>, key: &str, where_: &str| -> Result<Option<String>> {
        match scope.and_then(|s| s.get(key)) {
            None => Ok(None),
            Some(v) => match v.as_str() {
                Some(s) => {
                    let s = s.trim();
                    if s.is_empty() {
                        // Explicit empty string ≈ unset; harmless.
                        Ok(None)
                    } else {
                        Ok(Some(s.to_string()))
                    }
                }
                None => bail!(
                    "`{where_}.{key}` in {} must be a string, found `{}` — refusing to \
                     silently fall back to a different model",
                    cfg.display(),
                    v.type_str()
                ),
            },
        }
    };

    // Base: [model] section, or bare top-level keys (the two cannot
    // coexist with phase sections present — TOML rejects the duplicate).
    let base_provider = match get(Some(&t), "provider", "top-level")? {
        Some(v) => Some(v),
        None => get(base, "provider", "[model]")?,
    };
    let base_model = match base {
        Some(_) => get(base, "model", "[model]")?,
        None => get(Some(&t), "model", "top-level")?,
    };

    // Phase overrides inherit per-key from the base.
    let provider = match get(phase_section, "provider", &format!("[model.{phase}]"))? {
        Some(v) => Some(v),
        None => base_provider,
    };
    let model = match get(phase_section, "model", &format!("[model.{phase}]"))? {
        Some(v) => Some(v),
        None => base_model,
    };
    Ok(ModelPin { provider, model })
}

/// Recipe-name-derived phases that may carry their own model section.
/// `run_recipe` derives the phase from the recipe file stem; repo
/// overrides must keep the same file names, so this set is closed.
pub const KNOWN_PHASES: &[&str] = &["discover", "review", "confirm", "fix", "verify", "report"];

/// Backwards-compatible base-pin lookup (no phase overrides).
pub fn model_pin(repo: &Path) -> Result<ModelPin> {
    model_pin_for(repo, "")
}

/// Derive the phase name from a recipe path (`…/fix.yaml` → "fix").
/// Unknown stems (a custom-named recipe) resolve to "" = base pin only.
fn phase_from_recipe(recipe: &Path) -> String {
    recipe
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| {
            if KNOWN_PHASES.contains(&s) {
                s.to_string()
            } else {
                String::new()
            }
        })
        .unwrap_or_default()
}

/// Human-readable effective model for CLI display (`gaggle model`, run
/// header, report). Shows the base pin and any per-phase overrides.
pub fn effective_model(repo: &Path) -> String {
    let base = match model_pin(repo) {
        Ok(pin) => pin,
        Err(e) => return format!("unusable .review/config.toml: {e:#}"),
    };
    let fmt = |p: &ModelPin| -> String {
        let provider = p.provider.as_deref().unwrap_or("(goose default)");
        let model = p.model.as_deref().unwrap_or("(goose default)");
        format!("{provider} / {model}")
    };
    let mut out = fmt(&base);
    if base.provider.is_some() || base.model.is_some() {
        out.push_str(" — from .review/config.toml");
    }
    // Per-phase overrides, in canonical phase order.
    let mut overrides: Vec<String> = Vec::new();
    for phase in KNOWN_PHASES {
        if let Ok(pin) = model_pin_for(repo, phase) {
            if (pin.provider != base.provider) || (pin.model != base.model) {
                overrides.push(format!("{phase}: {}", fmt(&pin)));
            }
        }
    }
    if !overrides.is_empty() {
        out.push_str(&format!(" [{}]", overrides.join("; ")));
    }
    out
}

/// Wall-clock ceiling for one goose recipe run. Sized for the slowest
/// legit phase observed in the wild: a 120-turn fix on a large component
/// of a 16k-LOC workspace ran ~35 min (redacter). 60 min leaves generous
/// headroom; a hung subprocess must not block the driver forever.
/// Override with `GAGGLE_GOOSE_TIMEOUT_SECS`.
const GOOSE_RUN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60 * 60);

fn goose_run_timeout() -> std::time::Duration {
    match std::env::var("GAGGLE_GOOSE_TIMEOUT_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) => std::time::Duration::from_secs(secs.max(1)),
            // A user-specified override that fails to parse must not be
            // silently ignored — warn (but keep the run going on default).
            Err(_) => {
                eprintln!(
                    "  ⚠ GAGGLE_GOOSE_TIMEOUT_SECS={raw:?} is not a number of seconds — using default {}s",
                    GOOSE_RUN_TIMEOUT.as_secs()
                );
                GOOSE_RUN_TIMEOUT
            }
        },
        Err(_) => GOOSE_RUN_TIMEOUT,
    }
}

/// Kill the child and (on Unix) its whole process group so goose's own
/// children (MCP servers, uvx runtimes) don't survive a timeout holding
/// the pipe write-ends.
fn kill_process_group(child: &mut std::process::Child) {
    #[cfg(not(windows))]
    {
        // process_group(0) made the child a group leader: pgid == pid.
        let pgid = child.id();
        let killed = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pgid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if killed {
            return;
        }
    }
    let _ = child.kill();
}

/// Token/cost usage for one goose recipe run, parsed from the response
/// envelope's top-level `metadata` block (present when the provider
/// reports usage). All fields optional — providers differ in what they
/// emit; goose computes `cost_usd` from its model pricing tables.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Usage {
    pub total_tokens: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_write_input_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl Usage {
    fn from_metadata(meta: &Value) -> Self {
        let u64_field = |k: &str| meta.get(k).and_then(|v| v.as_u64());
        Self {
            total_tokens: u64_field("total_tokens"),
            input_tokens: u64_field("input_tokens"),
            output_tokens: u64_field("output_tokens"),
            cache_read_input_tokens: u64_field("cache_read_input_tokens"),
            cache_write_input_tokens: u64_field("cache_write_input_tokens"),
            cost_usd: meta.get("cost_usd").and_then(|v| v.as_f64()),
        }
    }

    /// True when nothing was reported (no metadata block at all).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Sum two usage records (per-field: None + Some = Some; None + None
    /// stays None so absent fields don't masquerade as zeros).
    pub fn add(&mut self, other: &Usage) {
        let sum = |a: &mut Option<u64>, b: Option<u64>| {
            if let Some(b) = b {
                *a = Some(a.unwrap_or(0) + b);
            }
        };
        sum(&mut self.total_tokens, other.total_tokens);
        sum(&mut self.input_tokens, other.input_tokens);
        sum(&mut self.output_tokens, other.output_tokens);
        sum(
            &mut self.cache_read_input_tokens,
            other.cache_read_input_tokens,
        );
        sum(
            &mut self.cache_write_input_tokens,
            other.cache_write_input_tokens,
        );
        if let Some(c) = other.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + c);
        }
    }

    /// One-line human summary, e.g. `$0.0131 · 426,940 in / 35,511 out /
    /// 412,672 cache-read`. Fields absent from the provider report are
    /// simply omitted.
    pub fn summary(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(c) = self.cost_usd {
            parts.push(format!("${c:.4}"));
        }
        if let Some(i) = self.input_tokens {
            parts.push(format!("{i} in"));
        }
        if let Some(o) = self.output_tokens {
            parts.push(format!("{o} out"));
        }
        if let Some(r) = self.cache_read_input_tokens {
            parts.push(format!("{r} cache-read"));
        }
        if parts.is_empty() {
            "(usage not reported)".to_string()
        } else {
            parts.join(" / ")
        }
    }
}

/// A recipe run's payload plus its reported usage.
pub struct RecipeOutcome {
    pub result: Value,
    pub usage: Usage,
}

/// Run a goose recipe headless. `params` are `key=value` pairs.
///
/// Returns the recipe's final JSON output (parsed from the response
/// envelope) together with the envelope's usage/cost metadata (empty
/// `Usage` when the provider reports none). Exit code 0 + parseable
/// JSON = success.
///
/// A clean-exit run that produces NO final JSON is retried ONCE: thinking
/// models (e.g. GLM at high effort) intermittently end their final turn
/// with a bare thinking block and no text answer — goose exits 0 with
/// nothing to parse. A fresh run usually answers normally.
pub fn run_recipe(
    repo: &Path,
    recipe: &Path,
    params: &[(&str, &str)],
    max_turns: Option<u32>,
) -> Result<RecipeOutcome> {
    // Phase-aware pin: `[model.<phase>]` overrides the `[model]` base for
    // this recipe (e.g. a thorough reviewer + a fast fixer).
    let pin = model_pin_for(repo, &phase_from_recipe(recipe))?;
    let mut last_usage = Usage::default();
    for attempt in 1..=2u8 {
        match run_recipe_once(repo, recipe, params, max_turns, &pin, attempt)? {
            Some((v, usage)) => {
                last_usage.add(&usage);
                return Ok(RecipeOutcome {
                    result: v,
                    usage: last_usage,
                });
            }
            None => {
                if attempt == 1 {
                    eprintln!(
                        "  note: goose run ended without a text answer (model stopped after thinking) — retrying once"
                    );
                }
            }
        }
    }
    bail!(
        "goose produced no parseable JSON in stdout for recipe {} after 2 attempts \
         (the model ended both runs without a text answer)",
        recipe.display()
    )
}

/// One goose recipe attempt. `Ok(None)` = clean exit but no final JSON in
/// the envelope (retryable flake); errors are hard failures. On success
/// returns `(final_json, usage_from_envelope_metadata)`.
fn run_recipe_once(
    repo: &Path,
    recipe: &Path,
    params: &[(&str, &str)],
    max_turns: Option<u32>,
    pin: &ModelPin,
    attempt: u8,
) -> Result<Option<(Value, Usage)>> {
    let mut cmd = Command::new("goose");
    cmd.arg("run")
        .arg("--recipe")
        .arg(recipe)
        .arg("--no-session")
        .arg("--output-format")
        .arg("json");
    for (k, v) in params {
        // goose's --params splits on the FIRST '=' only, so a value
        // containing '=' would be silently mis-split (e.g. a base64 blob,
        // a URL with a query string, embedded JSON). Fail loud rather than
        // corrupt the recipe substitution. Newlines and carriage returns —
        // in keys OR values — are equally unsafe: they'd inject into the
        // YAML/instruction text.
        if k.contains('=')
            || k.contains('\n')
            || k.contains('\r')
            || v.contains('=')
            || v.contains('\n')
            || v.contains('\r')
        {
            bail!(
                "unsafe recipe param {k:?}: keys/values with '=' or line breaks cannot be passed via --params (use a file instead)"
            );
        }
        cmd.arg("--params").arg(format!("{k}={v}"));
    }
    if let Some(t) = max_turns {
        cmd.arg("--max-turns").arg(t.to_string());
    }
    // Pin the model ONLY when configured; otherwise leave GOOSE_PROVIDER /
    // GOOSE_MODEL untouched so goose resolves its own default (its
    // config.yaml, or the caller's exported env).
    if let Some(p) = &pin.provider {
        cmd.env("GOOSE_PROVIDER", p);
    }
    if let Some(m) = &pin.model {
        cmd.env("GOOSE_MODEL", m);
    }
    // Thinking-effort cap for harness children. A user's global goose
    // config may set effort=high; for GLM-class thinking models that can
    // burn the ENTIRE per-response output-token budget on reasoning, so
    // the run ends with outputTokenLimitReached and never writes the
    // required final JSON line (observed repeatedly at effort=high).
    // Review/fix answers are terse JSON — medium is plenty. An explicitly
    // exported GOOSE_THINKING_EFFORT always wins.
    if std::env::var_os("GOOSE_THINKING_EFFORT").is_none() {
        cmd.env("GOOSE_THINKING_EFFORT", "medium");
    }
    // Per-response output budget. The provider's default cap (observed
    // ~4k tokens for glm-5.3) can be consumed by a single long thinking
    // turn on large components, truncating the answer (same
    // outputTokenLimitReached signature). GLM-class models accept much
    // larger outputs — 64k ensures even the longest reasoning turn plus
    // the final JSON line fits. An explicitly exported GOOSE_MAX_TOKENS
    // always wins.
    if std::env::var_os("GOOSE_MAX_TOKENS").is_none() {
        cmd.env("GOOSE_MAX_TOKENS", "65536");
    }
    cmd.env("GOOSE_MODE", "auto")
        .env_remove("PYTHONPATH") // Hermes venv pollution breaks uvx MCP servers
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(repo);
    // Own process group: killing only goose on timeout leaves its children
    // (MCP servers, uvx runtimes) alive holding the pipe write-ends — the
    // reader threads below would then block forever anyway.
    #[cfg(not(windows))]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    // `--max-turns` bounds agent turns, not wall-clock time: a hung goose
    // (stuck provider call, wedged MCP server) would block the driver
    // forever. Drain pipes on threads (a full pipe would otherwise block
    // the child forever — deadlock), poll `try_wait` against a deadline,
    // and kill the whole group on expiry. Joins are bounded too: a
    // grandchild that inherits the write-ends can outlive the direct
    // child even on a NORMAL exit.
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn goose for recipe {}", recipe.display()))?;
    let mut stdout_pipe = child.stdout.take().expect("stdout piped above");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped above");
    let t_out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout_pipe, &mut s);
        s
    });
    let t_err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr_pipe, &mut s);
        s
    });
    // Bounded join via a channel proxy: returns whatever the thread
    // produced within `limit`, else empty (the thread is abandoned and
    // exits whenever the last pipe write-end closes).
    let join_bounded = |h: std::thread::JoinHandle<String>, limit: std::time::Duration| -> String {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = std::thread::spawn(move || {
            let _ = tx.send(h.join().unwrap_or_default());
        });
        rx.recv_timeout(limit).unwrap_or_default()
    };

    let timeout = goose_run_timeout();
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    kill_process_group(&mut child);
                    let _ = child.wait(); // reap so OUR pipe ends close
                    let _ = join_bounded(t_out, std::time::Duration::from_secs(5));
                    let _ = join_bounded(t_err, std::time::Duration::from_secs(5));
                    bail!(
                        "goose recipe {} timed out after {}s (killed)",
                        recipe.display(),
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                kill_process_group(&mut child);
                let _ = child.wait();
                return Err(e).with_context(|| {
                    format!("failed to wait for goose recipe {}", recipe.display())
                });
            }
        }
    };

    let stdout = join_bounded(t_out, std::time::Duration::from_secs(10));
    let stderr = join_bounded(t_err, std::time::Duration::from_secs(10));
    if !stderr.is_empty() {
        eprintln!("[goose stderr] {}", stderr.trim_end());
    }

    // A non-zero exit is a failure regardless of whether we managed to
    // parse a (possibly stale/earlier) assistant JSON message from stdout.
    // We check this BEFORE extracting result JSON so that a failed run is
    // never masked by a successful parse of stale stdout.
    if !status.success() {
        bail!(
            "goose recipe {} failed (exit {:?})\nstdout: {}\nstderr: {}",
            recipe.display(),
            status.code(),
            stdout.chars().take(2000).collect::<String>(),
            stderr.chars().take(2000).collect::<String>(),
        );
    }

    // Clean exit: parse the envelope ONCE — the final JSON comes from its
    // last assistant text block, and usage/cost from its top-level
    // `metadata`. (The old code ran extract_final_json twice: once as an
    // is-none check and once for the value.)
    let envelope = extract_last_envelope(&stdout);
    let final_json = envelope
        .as_ref()
        .and_then(extract_from_envelope)
        .or_else(|| scan_trailing_json(&stdout));
    let Some(final_json) = final_json else {
        // No final JSON anywhere: the model ended without a text answer
        // (bare thinking block) or the envelope is truncated. Ok(None)
        // lets the caller retry once; keep a breadcrumb for the terminal
        // failure case.
        eprintln!(
            "  [goose] attempt {attempt}: no final JSON in envelope \
             (stdout tail: {})",
            stdout
                .chars()
                .rev()
                .take(400)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>(),
        );
        return Ok(None);
    };
    let usage = envelope
        .as_ref()
        .and_then(|e| e.get("metadata"))
        .map(Usage::from_metadata)
        .unwrap_or_default();
    Ok(Some((final_json, usage)))
}

/// Find the LAST `{"messages": [...]}`-shaped envelope object in stdout.
///
/// Tries each '{' as a candidate start: a leading banner/progress line may
/// contain an unbalanced '{' (e.g. `[info] config {`); if we only tried
/// the first, match_brace might never reach depth 0 and envelope parsing
/// would be skipped entirely. The LAST envelope wins — the documented
/// contract is the last assistant block of the run; the first could be a
/// stale intermediate envelope.
fn extract_last_envelope(stdout: &str) -> Option<Value> {
    let mut last_envelope: Option<Value> = None;
    let mut search_from = 0;
    while let Some(f) = stdout[search_from..].find('{') {
        let abs_start = search_from + f;
        if let Some(l) = match_brace(&stdout[abs_start..]) {
            // `l` is RELATIVE to the slice start; the absolute end index
            // is `abs_start + l` (inclusive).
            let abs_end = abs_start + l;
            if let Ok(obj) = serde_json::from_str::<Value>(&stdout[abs_start..=abs_end]) {
                if obj.get("messages").and_then(|m| m.as_array()).is_some() {
                    last_envelope = Some(obj);
                }
                // Parsed, but not an envelope — try the next '{'.
            }
        }
        // Either brace-match failed (unbalanced '{' in a banner) or the
        // slice wasn't a valid envelope. Advance past this '{' and retry.
        search_from = abs_start + 1;
    }
    last_envelope
}

/// Given a string that starts with '{', return the index (relative to the
/// slice start) of the matching closing '}', accounting for braces inside
/// JSON string literals. Returns `None` if unbalanced.
fn match_brace(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Walk an already-parsed `{"messages": [...]}` envelope in reverse and
/// return the last assistant text-block JSON value, or `None`.
fn extract_from_envelope(obj: &Value) -> Option<Value> {
    let messages = obj.get("messages").and_then(|m| m.as_array())?;
    for msg in messages.iter().rev() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        if let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) {
            for block in blocks.iter().rev() {
                if block.get("type").and_then(|t| t.as_str()) != Some("text") {
                    continue;
                }
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if let Some(v) = scan_trailing_json(text) {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

/// Scan `text` from the end for the last JSON value it contains. Tries
/// (a) individual lines (reverse) for the last JSON line — this respects the
/// documented "last line of the last assistant block" contract, and avoids
/// returning a valid-but-intermediate whole-block JSON prematurely; then
/// (b) parsing the whole trimmed text as a single JSON value — so
/// multi-line/pretty-printed JSON answers are also recognised.
fn scan_trailing_json(text: &str) -> Option<Value> {
    // (a) Single-line scan (reverse) for the last JSON line. This is tried
    //     FIRST so that when a block contains multiple JSON lines (e.g. an
    //     intermediate {"intermediate":true} followed by the real answer)
    //     we return the LAST one rather than the whole block.
    for line in text.lines().rev() {
        let line = line.trim();
        if line.starts_with('{') && line.ends_with('}') {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                return Some(v);
            }
        }
    }
    // (b) Whole trimmed text. This correctly handles nested objects (e.g.
    //     {"a":{"b":1}}) and pretty-printed multi-line JSON, which the old
    //     rfind('{')-to-rfind('}') approach got wrong by slicing only the
    //     innermost object. Only reached if no individual line parsed.
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        return Some(v);
    }
    None
}

/// Convenience: read a string field from the result JSON.
pub fn field<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_pin_missing_file_is_empty() {
        let dir = std::env::temp_dir().join(format!("gaggle-pin-test-{}", std::process::id()));
        // Intentionally NOT created: missing .review/config.toml → empty pin.
        assert_eq!(model_pin(&dir).unwrap(), ModelPin::default());
    }

    #[test]
    fn model_pin_reads_keys_and_ignores_empty() {
        let dir = std::env::temp_dir().join(format!("gaggle-pin-test-2-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"cargo test\"]\nprovider = \"custom_z.ai\"\nmodel = \"glm-5.3\"\n",
        )
        .unwrap();
        let pin = model_pin(&dir).unwrap();
        assert_eq!(pin.provider.as_deref(), Some("custom_z.ai"));
        assert_eq!(pin.model.as_deref(), Some("glm-5.3"));
        // Whitespace-only values are treated as unset.
        std::fs::write(
            dir.join(".review/config.toml"),
            "provider = \"  \"\nmodel = \"glm-5.3\"\n",
        )
        .unwrap();
        let pin = model_pin(&dir).unwrap();
        assert_eq!(pin.provider, None);
        assert_eq!(pin.model.as_deref(), Some("glm-5.3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_pin_accepts_model_section_and_bottom_placement() {
        let dir = std::env::temp_dir().join(format!("gaggle-pin-test-4-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        // `[model]` section form:
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"cargo test\"]\n[commit]\nsign = false\n\n[model]\nprovider = \"openai\"\nmodel = \"gpt-5.6\"\n",
        )
        .unwrap();
        let pin = model_pin(&dir).unwrap();
        assert_eq!(pin.provider.as_deref(), Some("openai"));
        assert_eq!(pin.model.as_deref(), Some("gpt-5.6"));
        // Top-level keys and a `[model]` section cannot coexist (TOML
        // rejects the duplicate `model` key), so each form is standalone.
        std::fs::write(
            dir.join(".review/config.toml"),
            "model = \"glm-5.3\"\n[commit]\nsign = false\n",
        )
        .unwrap();
        let pin = model_pin(&dir).unwrap();
        assert_eq!(pin.model.as_deref(), Some("glm-5.3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_pin_malformed_config_errors() {
        let dir = std::env::temp_dir().join(format!("gaggle-pin-test-3-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(dir.join(".review/config.toml"), "not toml [[[").unwrap();
        assert!(model_pin(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_pin_wrong_typed_key_errors_not_silently_unsets() {
        let dir = std::env::temp_dir().join(format!("gaggle-pin-test-5-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        // Wrong type at top level → error (not silent goose-default).
        std::fs::write(dir.join(".review/config.toml"), "provider = 123\n").unwrap();
        let err = model_pin(&dir).unwrap_err().to_string();
        assert!(err.contains("must be a string"), "{err}");
        // Wrong type inside [model] → error too.
        std::fs::write(dir.join(".review/config.toml"), "[model]\nmodel = 42\n").unwrap();
        assert!(model_pin(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// JSON strings with escaped backslashes/braces must not confuse the
    /// brace matcher: the closing `"` of a string containing `\\` or `\"`
    /// must be found at the right position.
    #[test]
    fn match_brace_handles_escaped_strings() {
        // {"a":"x\\y","b":"{z}","c":[1,2]}
        let s = r#"{"a":"x\\y","b":"{z}","c":[1,2]}"#;
        assert_eq!(match_brace(s), Some(s.len() - 1));
    }

    #[test]
    fn match_brace_consecutive_backslashes() {
        // Value ends with an escaped backslash + escaped quote: "a\\\""
        // JSON string content: a \ "  → the final " is escaped, NOT a close.
        let s = r#"{"v":"a\\\""}"#;
        assert_eq!(match_brace(s), Some(s.len() - 1));
    }

    #[test]
    fn match_brace_nested() {
        let s = r#"{"m":[{"t":"text","x":1}]}"#;
        assert_eq!(match_brace(s), Some(s.len() - 1));
    }
}

#[cfg(test)]
mod usage_tests {
    use super::*;

    const ENVELOPE: &str = r#"{
  "messages": [
    {"role": "assistant", "content": [{"type": "text", "text": "done {\"outcome\":\"fixed\"}"}]}
  ],
  "metadata": {
    "total_tokens": 462451,
    "input_tokens": 426940,
    "output_tokens": 35511,
    "cache_read_input_tokens": 412672,
    "cache_write_input_tokens": 0,
    "cost_usd": 0.0130960816,
    "status": "completed"
  }
}"#;

    #[test]
    fn usage_parsed_from_envelope_metadata() {
        let env = extract_last_envelope(ENVELOPE).expect("envelope");
        let usage = Usage::from_metadata(env.get("metadata").unwrap());
        assert_eq!(usage.total_tokens, Some(462451));
        assert_eq!(usage.input_tokens, Some(426940));
        assert_eq!(usage.output_tokens, Some(35511));
        assert_eq!(usage.cache_read_input_tokens, Some(412672));
        assert_eq!(usage.cost_usd, Some(0.0130960816));
        assert!(!usage.is_empty());
    }

    #[test]
    fn usage_absent_metadata_is_empty() {
        let no_meta = r#"{"messages": [{"role":"assistant","content":[{"type":"text","text":"{\"a\":1}"}]}]}"#;
        let env = extract_last_envelope(no_meta).expect("envelope");
        let usage = env
            .get("metadata")
            .map(Usage::from_metadata)
            .unwrap_or_default();
        assert!(usage.is_empty());
        assert_eq!(usage.summary(), "(usage not reported)");
    }

    #[test]
    fn usage_add_preserves_absent_fields() {
        let mut a = Usage {
            input_tokens: Some(10),
            cost_usd: Some(0.5),
            ..Default::default()
        };
        let b = Usage {
            input_tokens: Some(5),
            output_tokens: Some(7),
            cost_usd: Some(0.25),
            ..Default::default()
        };
        a.add(&b);
        assert_eq!(a.input_tokens, Some(15));
        assert_eq!(a.output_tokens, Some(7)); // None + Some → Some
        assert_eq!(a.total_tokens, None); // absent in both stays None
        assert_eq!(a.cost_usd, Some(0.75));
    }

    #[test]
    fn summary_formats_cost_and_tokens() {
        let u = Usage {
            input_tokens: Some(1000),
            output_tokens: Some(200),
            cost_usd: Some(0.0131),
            ..Default::default()
        };
        let s = u.summary();
        assert!(s.contains("$0.0131"), "{s}");
        assert!(s.contains("1000 in"), "{s}");
        assert!(s.contains("200 out"), "{s}");
    }

    #[test]
    fn last_envelope_wins_over_earlier_one() {
        let two =
            format!("{{\"messages\":[],\"metadata\":{{\"total_tokens\":1}}}}\nbanner\n{ENVELOPE}");
        let env = extract_last_envelope(&two).expect("envelope");
        assert_eq!(
            env.get("metadata")
                .and_then(|m| m.get("total_tokens"))
                .and_then(|t| t.as_u64()),
            Some(462451)
        );
    }
}

#[cfg(test)]
mod phase_model_tests {
    use super::*;

    fn repo_with(cfg: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-phase-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(dir.join(".review/config.toml"), cfg).unwrap();
        dir
    }

    #[test]
    fn phase_override_wins_and_inherits_per_key() {
        let dir = repo_with(
            "[model]\nprovider = \"custom_z.ai\"\nmodel = \"glm-5.3\"\n\n[model.fix]\nmodel = \"deepseek-v4-flash\"\n",
        );
        // Base pin.
        let base = model_pin(&dir).unwrap();
        assert_eq!(base.provider.as_deref(), Some("custom_z.ai"));
        assert_eq!(base.model.as_deref(), Some("glm-5.3"));
        // fix overrides model, inherits provider.
        let fix = model_pin_for(&dir, "fix").unwrap();
        assert_eq!(fix.provider.as_deref(), Some("custom_z.ai"));
        assert_eq!(fix.model.as_deref(), Some("deepseek-v4-flash"));
        // review has no section → base.
        let review = model_pin_for(&dir, "review").unwrap();
        assert_eq!(review.model.as_deref(), Some("glm-5.3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_phase_section_is_rejected() {
        let dir = repo_with("[model]\nmodel = \"glm-5.3\"\n\n[model.fx]\nmodel = \"x\"\n");
        let err = model_pin_for(&dir, "fix").unwrap_err().to_string();
        assert!(err.contains("[model.fx]"), "{err}");
        assert!(err.contains("not a known phase"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bare_top_level_key_with_phase_section_gets_targeted_error() {
        let dir = repo_with("model = \"glm-5.3\"\n\n[model.fix]\nmodel = \"x\"\n");
        let err = format!("{:#}", model_pin_for(&dir, "fix").unwrap_err());
        assert!(err.contains("Move the base keys INSIDE"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn phase_from_recipe_maps_known_stems_only() {
        let p = std::path::Path::new("/tmp/x/review.yaml");
        assert_eq!(phase_from_recipe(p), "review");
        let p = std::path::Path::new("/tmp/x/fix.yaml");
        assert_eq!(phase_from_recipe(p), "fix");
        let p = std::path::Path::new("/tmp/x/custom-thing.yaml");
        assert_eq!(phase_from_recipe(p), "");
    }

    #[test]
    fn effective_model_lists_overrides() {
        let dir = repo_with(
            "[model]\nprovider = \"custom_z.ai\"\nmodel = \"glm-5.3\"\n\n[model.fix]\nmodel = \"deepseek-v4-flash\"\n",
        );
        let s = effective_model(&dir);
        assert!(s.contains("custom_z.ai / glm-5.3"), "{s}");
        assert!(s.contains("fix: custom_z.ai / deepseek-v4-flash"), "{s}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
