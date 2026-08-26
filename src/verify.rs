//! Harness-owned verify gate: run the `verify` commands from
//! `.review/config.toml` and treat their exit codes as the verdict.

use anyhow::Context;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Why the harness killed a still-running verify command.
#[derive(Debug, Clone)]
pub enum VerifyKill {
    /// No stdout/stderr bytes and no process-group CPU for `idle`.
    Stall { idle: Duration },
    /// Optional wall-clock ceiling (`verify_timeout_secs`) elapsed.
    Timeout { after: Duration },
}

impl VerifyKill {
    pub fn is_stall(&self) -> bool {
        matches!(self, Self::Stall { .. })
    }
}

/// Outcome of running the configured verify commands.
pub struct RunResult {
    pub passed: bool,
    pub failed_command: Option<String>,
    pub output: String,
    /// Set when the harness killed the command (stall or optional timeout).
    pub kill: Option<VerifyKill>,
}

/// Load a named command-array key (`verify`, `final_verify`) from
/// `.review/config.toml`. Shared validation: strings, non-empty entries.
/// An empty/missing `verify` is rejected (it would silently pass with
/// zero commands run); `final_verify` is optional and may be absent.
fn load_commands_key(repo: &Path, key: &str, required: bool) -> anyhow::Result<Vec<String>> {
    let cfg = repo.join(".review/config.toml");
    let text = match std::fs::read_to_string(&cfg) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            anyhow::bail!(
                "config not found: {} — expected a `verify` array (see config.toml.example)",
                cfg.display()
            );
        }
        Err(e) => {
            return Err(e).with_context(|| format!("failed to read `{}`", cfg.display()));
        }
    };
    let t: toml::Value = text
        .parse()
        .with_context(|| format!("failed to parse `{}`", cfg.display()))?;
    let Some(arr) = t.get(key) else {
        if required {
            anyhow::bail!(
                "`{key}` key missing from {} — expected a non-empty command array (see config.toml.example)",
                cfg.display()
            );
        }
        // Placing a top-level key at the BOTTOM of the file (a natural
        // edit) scopes it under the last [section] — it would be silently
        // ignored. Detect that exact mistake and warn.
        if key == "final_verify" {
            warn_if_key_nested_in_section(&t, key, &cfg);
        }
        return Ok(Vec::new());
    };
    let arr = arr.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "`{key}` must be an array of command strings in {} (see config.toml.example)",
            cfg.display()
        )
    })?;
    let cmds: Vec<String> = arr
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let s = v.as_str().ok_or_else(|| {
                anyhow::anyhow!(
                    "`{key}[{i}]` must be a string in {}, but found `{}` (see config.toml.example)",
                    cfg.display(),
                    v.type_str()
                )
            })?;
            let trimmed = s.trim();
            if trimmed.is_empty() {
                anyhow::bail!(
                    "`{key}[{i}]` is empty/whitespace-only in {} — entries must be non-empty command strings (see config.toml.example)",
                    cfg.display()
                );
            }
            Ok(trimmed.to_string())
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if cmds.is_empty() && required {
        anyhow::bail!(
            "`{key}` is empty in {} — an empty array is rejected (it would silently pass with zero commands run)",
            cfg.display()
        );
    }
    Ok(cmds)
}

/// Load the `verify` command list from `.review/config.toml`.
///
/// An empty or missing `verify` array is rejected: it would otherwise
/// silently pass with zero commands run.
pub fn load_commands(repo: &Path) -> anyhow::Result<Vec<String>> {
    load_commands_key(repo, "verify", true)
}

/// Load the optional `final_verify` command list — the FULL gate run once
/// at the end of a `gaggle run` (e.g. the slow e2e suite that would be
/// too expensive per fix cycle). Empty when unset: the run-end gate then
/// falls back to the regular `verify` list once more.
pub fn load_final_commands(repo: &Path) -> anyhow::Result<Vec<String>> {
    let final_cmds = load_commands_key(repo, "final_verify", false)?;
    if final_cmds.is_empty() {
        return load_commands(repo);
    }
    Ok(final_cmds)
}

/// Repo files the verify gate depends on: path-looking tokens extracted
/// from the `verify` and `final_verify` command strings that exist in the
/// repo (e.g. `./slowcheck.sh`, `bin/ameba`, `scripts/check.sh`).
///
/// The loop refuses to commit fixer changes to these files: an agent that
/// cannot make a failing gate pass may rewrite the gate itself (observed
/// in the wild — a fixer replaced a deliberately-hanging verify script
/// with a test suite for a utility binary it wrote). The agent does not
/// get a vote on the gate.
pub fn gate_files(repo: &Path) -> Vec<String> {
    let cmds: Vec<String> = load_commands(repo)
        .unwrap_or_default()
        .into_iter()
        .chain(load_final_commands(repo).unwrap_or_default())
        .collect();
    let mut files: Vec<String> = Vec::new();
    for cmd in &cmds {
        // Split on whitespace and common shell separators; strip quotes.
        for token in cmd.split(|c: char| {
            c.is_whitespace() || matches!(c, '&' | '|' | ';' | '>' | '<' | '(' | ')')
        }) {
            let token = token.trim_matches(|c| c == '\'' || c == '"');
            if token.is_empty() {
                continue;
            }
            // Path-looking: contains a '/', or starts with './'. Bare
            // command names (cargo, go, npm) are not files.
            if !token.contains('/') || token.starts_with("http") {
                continue;
            }
            let rel = token.trim_start_matches("./");
            if rel.is_empty() || rel.contains('*') {
                continue;
            }
            // Go's `./...` package wildcard is not a file. On Windows,
            // trailing dots are stripped from path components, so
            // `join("...")` is the repo root and would spuriously match.
            if rel.chars().all(|c| c == '.') {
                continue;
            }
            // Only count it when the repo actually has such a file/dir.
            if repo.join(rel).exists() && !files.iter().any(|f| f == rel) {
                files.push(rel.to_string());
            }
        }
    }
    files
}

/// Run every configured verify command from the repo root. Stop at the
/// first failure. Exit codes are the only pass/fail signal.
pub fn run(repo: &Path) -> anyhow::Result<RunResult> {
    run_commands(repo, &load_commands(repo)?)
}

/// Run the end-of-run FULL gate: `final_verify` from config when set
/// (e.g. the slow e2e suite), otherwise the regular `verify` list.
/// Command output is echoed live so a 40-minute suite is inspectable
/// instead of a single FAIL line after the fact.
pub fn run_final(repo: &Path) -> anyhow::Result<RunResult> {
    run_commands_timed(
        repo,
        &load_final_commands(repo)?,
        load_verify_timeout(repo),
        load_verify_stall(repo),
        true,
    )
}

/// Run an explicit command list (per-component `verify:` from the
/// checklist). Same runner as the repo-wide `verify` list — no language
/// knowledge here.
pub fn run_list(repo: &Path, cmds: &[String]) -> anyhow::Result<RunResult> {
    run_commands(repo, cmds)
}

fn run_commands(repo: &Path, cmds: &[String]) -> anyhow::Result<RunResult> {
    run_commands_timed(
        repo,
        cmds,
        load_verify_timeout(repo),
        load_verify_stall(repo),
        true,
    )
}

fn run_commands_timed(
    repo: &Path,
    cmds: &[String],
    timeout: Option<Duration>,
    stall: Option<Duration>,
    echo: bool,
) -> anyhow::Result<RunResult> {
    let mut output = String::new();
    for cmd in cmds {
        let mut result = run_shell(repo, cmd, timeout, stall, echo)
            .with_context(|| format!("failed to spawn verify command: {cmd}"))?;
        append_output(&mut output, &result.stdout, &result.stderr);
        if let Some(kill) = result.kill.clone() {
            let last = last_nonempty_line(&output).map(str::to_string);
            note_kill(&mut output, cmd, &kill, last.as_deref());
            // Early stall (hung almost immediately) is often a lock or a
            // spawn glitch — retry once. A stall after real work must not
            // restart a multi-hour compile. Wall-clock timeout is a CI
            // budget: no retry.
            // Use time-to-kill, not time-to-return: after SIGTERM the
            // pipe-drain join can sit up to 5s, which would otherwise
            // push a 1s hang past the 2×window "early" bound.
            let early_stall = match (&kill, stall) {
                (VerifyKill::Stall { .. }, Some(window)) => result.ran_for <= window * 2,
                _ => false,
            };
            if early_stall {
                eprintln!("  ⚠ verify stalled early — retrying once: {cmd}");
                output.push_str("[gaggle] verify stalled early — retrying once\n");
                result = run_shell(repo, cmd, timeout, stall, echo)
                    .with_context(|| format!("failed to spawn verify command: {cmd}"))?;
                append_output(&mut output, &result.stdout, &result.stderr);
                if let Some(kill) = result.kill.clone() {
                    let last = last_nonempty_line(&output).map(str::to_string);
                    note_kill(&mut output, cmd, &kill, last.as_deref());
                    return Ok(RunResult {
                        passed: false,
                        failed_command: Some(cmd.clone()),
                        output,
                        kill: Some(kill),
                    });
                }
            } else {
                return Ok(RunResult {
                    passed: false,
                    failed_command: Some(cmd.clone()),
                    output,
                    kill: Some(kill),
                });
            }
        }
        if !result.success {
            return Ok(RunResult {
                passed: false,
                failed_command: Some(cmd.clone()),
                output,
                kill: None,
            });
        }
    }
    Ok(RunResult {
        passed: true,
        failed_command: None,
        output,
        kill: None,
    })
}

fn note_kill(output: &mut String, cmd: &str, kill: &VerifyKill, last_line: Option<&str>) {
    let last = last_line
        .map(|l| format!("; last output: {l}"))
        .unwrap_or_default();
    let msg = match kill {
        VerifyKill::Stall { idle } => format!(
            "[gaggle] verify stalled after {}s with no output and no CPU (killed){last}: {cmd}\n",
            idle.as_secs()
        ),
        VerifyKill::Timeout { after } => {
            format!(
                "[gaggle] verify command timed out after {}s and was killed{last}: {cmd}\n",
                after.as_secs()
            )
        }
    };
    match kill {
        VerifyKill::Stall { idle } => eprintln!(
            "  ⚠ verify stalled after {}s with no output and no CPU{last}: {cmd}",
            idle.as_secs()
        ),
        VerifyKill::Timeout { after } => eprintln!(
            "  ⚠ verify command timed out after {}s (killed){last}: {cmd}",
            after.as_secs()
        ),
    }
    output.push_str(&msg);
}

fn last_nonempty_line(s: &str) -> Option<&str> {
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("[gaggle]"))
}

/// Cap per-stream bytes we keep in memory from a verify command; a
/// runaway command writing forever would otherwise grow the buffer
/// without bound. (Diagnostics only ever surface the tail.)
const VERIFY_OUTPUT_CAP: usize = 10 * 1024 * 1024;

/// Tables a top-level key can accidentally land in when appended after a
/// `[section]` header. Keep in sync with the documented config sections.
const CONFIG_SECTIONS: [&str; 3] = ["commit", "model", "branch"];

fn warn_if_key_nested_in_section(t: &toml::Value, key: &str, cfg: &Path) {
    for section in CONFIG_SECTIONS {
        if t.get(section).is_some_and(|s| s.get(key).is_some()) {
            eprintln!(
                "  ⚠ `{key}` found inside [{section}] in {} — keys appended after a \
                 [section] header belong to that section. Move it to the TOP of the \
                 file (before any [section]) for it to take effect.",
                cfg.display()
            );
        }
    }
}

/// Optional `verify_timeout_secs` from `.review/config.toml`. `None` when
/// unset, unreadable, or invalid — and `None` means *no timeout*, not a
/// default. Warns rather than failing the run: a bad timeout must not
/// skip the gate.
fn load_timeout_from_config(repo: &Path) -> Option<u64> {
    let cfg = repo.join(".review/config.toml");
    let text = std::fs::read_to_string(&cfg).ok()?;
    let t: toml::Value = text.parse().ok()?;
    match t.get("verify_timeout_secs") {
        None => {
            warn_if_key_nested_in_section(&t, "verify_timeout_secs", &cfg);
            None
        }
        Some(v) => match v.as_integer() {
            Some(n) if n >= 1 => Some(n as u64),
            Some(_) => {
                eprintln!(
                    "  ⚠ `verify_timeout_secs` must be a positive integer in {} — leaving timeout unset",
                    cfg.display()
                );
                None
            }
            None => {
                eprintln!(
                    "  ⚠ `verify_timeout_secs` must be a number of seconds in {} (found `{}`) — leaving timeout unset",
                    cfg.display(),
                    v.type_str()
                );
                None
            }
        },
    }
}

/// Env `GAGGLE_VERIFY_TIMEOUT_SECS` (if set and valid) wins over config.
/// Unset env and unset config means no timeout: the command runs until
/// it exits. An invalid env value is warned about and ignored so a typo
/// does not skip a repo-level setting.
fn resolve_verify_timeout(
    env_raw: Option<&str>,
    config_secs: Option<u64>,
) -> Option<std::time::Duration> {
    if let Some(raw) = env_raw {
        match raw.trim().parse::<u64>() {
            Ok(secs) if secs >= 1 => return Some(std::time::Duration::from_secs(secs)),
            Ok(_) => {
                // `0` is an explicit "no timeout"; env still wins over config.
                return None;
            }
            Err(_) => {
                eprintln!(
                    "  ⚠ GAGGLE_VERIFY_TIMEOUT_SECS={raw:?} is not a number of seconds — ignoring"
                );
            }
        }
    }
    config_secs.map(|secs| std::time::Duration::from_secs(secs.max(1)))
}

fn load_verify_timeout(repo: &Path) -> Option<Duration> {
    resolve_verify_timeout(
        std::env::var("GAGGLE_VERIFY_TIMEOUT_SECS").ok().as_deref(),
        load_timeout_from_config(repo),
    )
}

/// Default idle window before a silent, idle process tree is considered hung.
const DEFAULT_STALL: Duration = Duration::from_secs(15 * 60);

/// Optional `verify_stall_secs`. `Some(0)` means explicitly off; `None` means
/// the key is absent/invalid (caller then applies the default).
fn load_stall_from_config(repo: &Path) -> Option<u64> {
    let cfg = repo.join(".review/config.toml");
    let text = std::fs::read_to_string(&cfg).ok()?;
    let t: toml::Value = text.parse().ok()?;
    match t.get("verify_stall_secs") {
        None => {
            warn_if_key_nested_in_section(&t, "verify_stall_secs", &cfg);
            None
        }
        Some(v) => match v.as_integer() {
            Some(n) if n >= 0 => Some(n as u64),
            Some(_) => {
                eprintln!(
                    "  ⚠ `verify_stall_secs` must be ≥ 0 in {} — using default {}s",
                    cfg.display(),
                    DEFAULT_STALL.as_secs()
                );
                None
            }
            None => {
                eprintln!(
                    "  ⚠ `verify_stall_secs` must be a number of seconds in {} (found `{}`) — using default {}s",
                    cfg.display(),
                    v.type_str(),
                    DEFAULT_STALL.as_secs()
                );
                None
            }
        },
    }
}

/// Env `GAGGLE_VERIFY_STALL_SECS` wins. `0` disables stall detection.
/// Unset env + unset config → 15 minutes of no I/O and no CPU.
fn resolve_verify_stall(env_raw: Option<&str>, config_secs: Option<u64>) -> Option<Duration> {
    if let Some(raw) = env_raw {
        match raw.trim().parse::<u64>() {
            Ok(0) => return None,
            Ok(secs) => return Some(Duration::from_secs(secs)),
            Err(_) => {
                eprintln!(
                    "  ⚠ GAGGLE_VERIFY_STALL_SECS={raw:?} is not a number of seconds — ignoring"
                );
            }
        }
    }
    match config_secs {
        Some(0) => None,
        Some(secs) => Some(Duration::from_secs(secs)),
        None => Some(DEFAULT_STALL),
    }
}

fn load_verify_stall(repo: &Path) -> Option<Duration> {
    resolve_verify_stall(
        std::env::var("GAGGLE_VERIFY_STALL_SECS").ok().as_deref(),
        load_stall_from_config(repo),
    )
}

/// Outcome of one shell command: exit status + drained output, or a
/// kill marker (the run continues as a FAILED verify, not an abort).
struct ShellOutcome {
    success: bool,
    kill: Option<VerifyKill>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    /// Elapsed from spawn until the command exited or we decided to kill
    /// it. Does not include the post-kill pipe-drain wait.
    ran_for: Duration,
}

fn run_shell(
    repo: &Path,
    cmd: &str,
    timeout: Option<Duration>,
    stall: Option<Duration>,
    echo: bool,
) -> anyhow::Result<ShellOutcome> {
    #[cfg(windows)]
    let mut child = {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(cmd);
        c
    };
    #[cfg(not(windows))]
    let mut child = {
        let mut c = Command::new("sh");
        c.arg("-c").arg(cmd);
        // Own process group: on timeout, killing only `sh` leaves its
        // descendants holding the pipe write-ends open — the drain threads
        // then block forever and the gate deadlocks anyway. Group kill
        // reaps the whole tree.
        use std::os::unix::process::CommandExt;
        c.process_group(0);
        c
    };
    child
        .current_dir(repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = child
        .spawn()
        .with_context(|| format!("failed to spawn verify command: {cmd}"))?;

    // Drain pipes on bounded reader threads: a full pipe would otherwise
    // block the child (deadlock), and the cap bounds memory. Past the cap
    // we KEEP READING AND DISCARDING to EOF — dropping the read end here
    // would SIGPIPE the child on its next write, turning a green command
    // into a spurious failed verify.
    fn echo_write(to_stderr: bool, bytes: &[u8]) {
        use std::io::Write;
        if to_stderr {
            let _ = std::io::stderr().write_all(bytes);
            let _ = std::io::stderr().flush();
        } else {
            let _ = std::io::stdout().write_all(bytes);
            let _ = std::io::stdout().flush();
        }
    }
    let io_progress = Arc::new(AtomicU64::new(0));
    fn bump_io(io: &AtomicU64, n: usize) {
        if n > 0 {
            io.fetch_add(n as u64, Ordering::Relaxed);
        }
    }
    fn drain(mut r: std::process::ChildStdout, echo: bool, io: Arc<AtomicU64>) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        use std::io::Read;
        loop {
            if buf.len() >= VERIFY_OUTPUT_CAP {
                // Discard phase: read to EOF without growing the buffer.
                match r.read(&mut chunk) {
                    Ok(0) => break,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                    Ok(n) => {
                        bump_io(&io, n);
                        if echo {
                            echo_write(false, &chunk[..n]);
                        }
                        continue;
                    }
                }
            }
            match r.read(&mut chunk) {
                Ok(0) => break,
                // EINTR is not EOF — retry; breaking here would silently
                // truncate diagnostics (possibly the failing error text).
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(n) => {
                    bump_io(&io, n);
                    if echo {
                        echo_write(false, &chunk[..n]);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
        buf
    }
    // ChildStderr is a distinct type; duplicate the body.
    fn drain_err(mut r: std::process::ChildStderr, echo: bool, io: Arc<AtomicU64>) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        use std::io::Read;
        loop {
            if buf.len() >= VERIFY_OUTPUT_CAP {
                match r.read(&mut chunk) {
                    Ok(0) => break,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                    Ok(n) => {
                        bump_io(&io, n);
                        if echo {
                            echo_write(true, &chunk[..n]);
                        }
                        continue;
                    }
                }
            }
            match r.read(&mut chunk) {
                Ok(0) => break,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(n) => {
                    bump_io(&io, n);
                    if echo {
                        echo_write(true, &chunk[..n]);
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
            }
        }
        buf
    }
    let io_out = Arc::clone(&io_progress);
    let io_err = Arc::clone(&io_progress);
    let out_handle = child
        .stdout
        .take()
        .map(|r| std::thread::spawn(move || drain(r, echo, io_out)));
    let err_handle = child
        .stderr
        .take()
        .map(|r| std::thread::spawn(move || drain_err(r, echo, io_err)));

    let spawned = Instant::now();
    let deadline = timeout.map(|t| spawned + t);
    let pgid = child.id();
    let mut last_progress = spawned;
    let mut last_io = 0u64;
    let mut last_cpu: Option<u64> = None;
    let mut last_cpu_sample = spawned;
    let mut kill = None;
    let ran_for;
    let status_success;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                ran_for = spawned.elapsed();
                status_success = status.success();
                break;
            }
            Ok(None) => {
                let now = Instant::now();
                if deadline.is_some_and(|d| now >= d) {
                    ran_for = spawned.elapsed();
                    kill = Some(VerifyKill::Timeout {
                        after: timeout.unwrap_or_default(),
                    });
                    kill_tree(&mut child);
                    let _ = child.wait(); // reap so OUR pipes close
                    status_success = false;
                    break;
                }
                let io = io_progress.load(Ordering::Relaxed);
                if io > last_io {
                    last_progress = now;
                    last_io = io;
                }
                if last_cpu.is_none()
                    || now.duration_since(last_cpu_sample) >= Duration::from_millis(250)
                {
                    if let Some(cpu) = process_group_cpu_ms(pgid) {
                        if last_cpu.is_some_and(|prev| cpu > prev) {
                            last_progress = now;
                        }
                        last_cpu = Some(cpu);
                    }
                    last_cpu_sample = now;
                }
                if stall.is_some_and(|s| now.duration_since(last_progress) >= s) {
                    ran_for = spawned.elapsed();
                    kill = Some(VerifyKill::Stall {
                        idle: stall.unwrap_or_default(),
                    });
                    kill_tree(&mut child);
                    let _ = child.wait();
                    status_success = false;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                kill_tree(&mut child);
                return Err(e).with_context(|| format!("failed waiting for verify command: {cmd}"));
            }
        }
    }

    // Bounded collection: even with the group kill, a legitimately
    // backgrounded descendant (command run with `&` that kept running)
    // can hold the pipes. Wait briefly, then give up on further output
    // rather than deadlocking the gate; the thread is abandoned (it will
    // exit whenever the last write-end closes).
    let join_bounded = |h: Option<std::thread::JoinHandle<Vec<u8>>>| -> Vec<u8> {
        h.map(|h| {
            let (tx, rx) = std::sync::mpsc::channel();
            // Proxy the join result through a channel so we can time out.
            let _ = std::thread::spawn(move || {
                let _ = tx.send(h.join().unwrap_or_default());
            });
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_default()
        })
        .unwrap_or_default()
    };
    let stdout = join_bounded(out_handle);
    let stderr = join_bounded(err_handle);

    Ok(ShellOutcome {
        success: status_success,
        kill,
        stdout,
        stderr,
        ran_for,
    })
}

/// Kill the child and (on Unix) its whole process group so descendants
/// don't survive to hold pipes or CPU.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(not(windows))]
    {
        // process_group(0) made the child a group leader: pgid == pid.
        let pgid = child.id();
        let kill = Command::new("kill")
            .arg("-TERM")
            .arg(format!("-{pgid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if kill.map(|s| s.success()).unwrap_or(false) {
            return;
        }
    }
    let _ = child.kill();
}

/// Sum CPU time of processes in `pgid`, in milliseconds. `None` when we
/// cannot sample (Windows, `ps` missing, parse failure) — stall then
/// rests on I/O alone.
fn process_group_cpu_ms(pgid: u32) -> Option<u64> {
    #[cfg(windows)]
    {
        let _ = pgid;
        None
    }
    #[cfg(not(windows))]
    {
        let out = Command::new("ps")
            .args(["-ax", "-o", "pgid=,time="])
            .stdin(Stdio::null())
            .output()
            .ok()?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let mut total = 0u64;
        let mut any = false;
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            let Some(g) = parts.next() else { continue };
            let Ok(g) = g.parse::<u32>() else { continue };
            if g != pgid {
                continue;
            }
            let Some(t) = parts.next() else { continue };
            let Some(ms) = parse_ps_time(t) else { continue };
            total = total.saturating_add(ms);
            any = true;
        }
        any.then_some(total)
    }
}

/// Parse `ps` TIME (`ss`, `mm:ss`, `mm:ss.ss`, `hh:mm:ss`) to milliseconds.
/// Compiled on Unix (stall CPU sampling) and in tests (the parser is
/// exercised on every OS so a Windows CI run still covers the formats).
#[cfg(any(test, not(windows)))]
fn parse_ps_time(s: &str) -> Option<u64> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    let to_ms = |raw: &str| -> Option<u64> {
        if let Some((whole, frac)) = raw.split_once('.') {
            let secs: u64 = whole.parse().ok()?;
            let frac = frac.trim_end_matches(|c: char| !c.is_ascii_digit());
            let millis = match frac.len() {
                0 => 0,
                1 => frac.parse::<u64>().ok()? * 100,
                2 => frac.parse::<u64>().ok()? * 10,
                _ => frac[..3].parse::<u64>().ok()?,
            };
            Some(secs.saturating_mul(1000).saturating_add(millis))
        } else {
            let secs: u64 = raw.parse().ok()?;
            Some(secs.saturating_mul(1000))
        }
    };
    match parts.as_slice() {
        [sec] => to_ms(sec),
        [min, sec] => {
            let min: u64 = min.parse().ok()?;
            Some(min.saturating_mul(60_000).saturating_add(to_ms(sec)?))
        }
        [hour, min, sec] => {
            let hour: u64 = hour.parse().ok()?;
            let min: u64 = min.parse().ok()?;
            Some(
                hour.saturating_mul(3_600_000)
                    .saturating_add(min.saturating_mul(60_000))
                    .saturating_add(to_ms(sec)?),
            )
        }
        _ => None,
    }
}

fn append_output(buf: &mut String, stdout: &[u8], stderr: &[u8]) {
    let stdout = String::from_utf8_lossy(stdout);
    let stderr = String::from_utf8_lossy(stderr);
    if !stdout.is_empty() {
        buf.push_str(&stdout);
        if !stdout.ends_with('\n') {
            buf.push('\n');
        }
    }
    if !stderr.is_empty() {
        buf.push_str(&stderr);
        if !stderr.ends_with('\n') {
            buf.push('\n');
        }
    }
}

/// Return the tail of `s` (up to `max` bytes), without splitting a UTF-8
/// character boundary.
pub fn tail_bytes(s: &str, max: usize) -> &str {
    if max == 0 {
        return "";
    }
    if s.len() <= max {
        return s;
    }
    let start = s.len() - max;
    let mut start = start;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-verify-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        dir
    }

    #[test]
    fn load_commands_rejects_empty() {
        let dir = temp_repo();
        std::fs::write(dir.join(".review/config.toml"), "verify = []\n").unwrap();
        assert!(load_commands(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_commands_rejects_missing_file() {
        let dir = temp_repo();
        assert!(load_commands(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_fails_on_nonzero_exit() {
        let dir = temp_repo();
        #[cfg(windows)]
        let cfg = "verify = [\"exit 1\"]\n";
        #[cfg(not(windows))]
        let cfg = "verify = [\"false\"]\n";
        std::fs::write(dir.join(".review/config.toml"), cfg).unwrap();
        let result = run(&dir).unwrap();
        assert!(!result.passed);
        assert!(result.failed_command.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_bytes_does_not_split_utf8() {
        let s = "abcéxyz";
        let t = tail_bytes(s, 4);
        assert!(t.is_char_boundary(0));
        assert!(s.ends_with(t));
    }
}

#[cfg(test)]
mod final_and_scoped_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-vfy-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        dir
    }

    #[test]
    fn final_commands_fall_back_to_verify_when_unset() {
        let dir = temp_repo();
        std::fs::write(dir.join(".review/config.toml"), "verify = [\"true\"]\n").unwrap();
        assert_eq!(load_final_commands(&dir).unwrap(), vec!["true"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn final_commands_override_when_set() {
        let dir = temp_repo();
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"true\"]\nfinal_verify = [\"cargo test --workspace\"]\n",
        )
        .unwrap();
        assert_eq!(
            load_final_commands(&dir).unwrap(),
            vec!["cargo test --workspace"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod gate_file_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn gate_files_extracts_existing_repo_paths_from_commands() {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-gate-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(dir.join("scripts/check.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"cargo build\", \"./scripts/check.sh\"]\nfinal_verify = [\"scripts/check.sh && cargo test --workspace\"]\n",
        )
        .unwrap();
        let gates = gate_files(&dir);
        assert_eq!(
            gates,
            vec!["scripts/check.sh"],
            "bare commands and flags excluded, real paths deduped"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_files_empty_when_commands_reference_no_files() {
        let dir = std::env::temp_dir().join(format!("gaggle-gate2-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"cargo test\", \"go test ./...\"]\n",
        )
        .unwrap();
        // `./...` is Go's recursive package wildcard, not a repo file
        // (and on Windows `join("...")` would otherwise be the repo root).
        assert!(gate_files(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod timeout_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn temp_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gaggle-timeout-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(dir.join(".review")).unwrap();
        dir
    }

    #[test]
    fn resolve_unset_means_no_timeout() {
        assert_eq!(resolve_verify_timeout(None, None), None);
        assert_eq!(
            resolve_verify_timeout(None, Some(120)),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            resolve_verify_timeout(Some("30"), Some(120)),
            Some(Duration::from_secs(30))
        );
        // Invalid env is ignored so a typo does not skip the config value.
        assert_eq!(
            resolve_verify_timeout(Some("nope"), Some(120)),
            Some(Duration::from_secs(120))
        );
        assert_eq!(resolve_verify_timeout(Some("nope"), None), None);
        // Zero env is an explicit "no timeout" and wins over config.
        assert_eq!(resolve_verify_timeout(Some("0"), Some(120)), None);
        assert_eq!(resolve_verify_timeout(Some("0"), None), None);
    }

    #[test]
    fn load_timeout_from_config_reads_positive_integer() {
        let dir = temp_repo();
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"true\"]\nverify_timeout_secs = 42\n",
        )
        .unwrap();
        assert_eq!(load_timeout_from_config(&dir), Some(42));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_timeout_from_config_ignores_nested_and_invalid() {
        let dir = temp_repo();
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"true\"]\n[commit]\nsign = false\nverify_timeout_secs = 12\n",
        )
        .unwrap();
        assert_eq!(load_timeout_from_config(&dir), None);

        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"true\"]\nverify_timeout_secs = 0\n",
        )
        .unwrap();
        assert_eq!(load_timeout_from_config(&dir), None);

        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"true\"]\nverify_timeout_secs = \"900\"\n",
        )
        .unwrap();
        assert_eq!(load_timeout_from_config(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unset_timeout_waits_for_a_slow_command() {
        let dir = temp_repo();
        #[cfg(windows)]
        let cfg = "verify = [\"ping -n 3 127.0.0.1 >nul\"]\n";
        #[cfg(not(windows))]
        let cfg = "verify = [\"sleep 1\"]\n";
        std::fs::write(dir.join(".review/config.toml"), cfg).unwrap();
        let cmds = load_commands(&dir).unwrap();
        let result = run_commands_timed(&dir, &cmds, None, None, false).unwrap();
        assert!(
            result.passed,
            "unset timeout must wait, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("timed out"),
            "must not kill when timeout is unset: {}",
            result.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wall_clock_timeout_fails_without_retry() {
        let dir = temp_repo();
        #[cfg(windows)]
        let cfg = "verify = [\"ping -n 31 127.0.0.1 >nul\"]\n";
        #[cfg(not(windows))]
        let cfg = "verify = [\"sleep 30\"]\n";
        std::fs::write(dir.join(".review/config.toml"), cfg).unwrap();
        let cmds = load_commands(&dir).unwrap();
        let result =
            run_commands_timed(&dir, &cmds, Some(Duration::from_secs(1)), None, false).unwrap();
        assert!(!result.passed);
        assert!(result.kill.is_some());
        assert!(
            result.output.contains("timed out"),
            "missing timeout note: {}",
            result.output
        );
        assert!(
            !result.output.contains("retrying once"),
            "wall-clock timeout must not retry: {}",
            result.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_stall_defaults_to_15m_zero_disables() {
        assert_eq!(
            resolve_verify_stall(None, None),
            Some(Duration::from_secs(15 * 60))
        );
        assert_eq!(resolve_verify_stall(Some("0"), Some(120)), None);
        assert_eq!(resolve_verify_stall(None, Some(0)), None);
        assert_eq!(
            resolve_verify_stall(None, Some(30)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            resolve_verify_stall(Some("12"), Some(30)),
            Some(Duration::from_secs(12))
        );
    }

    #[test]
    fn parse_ps_time_formats() {
        assert_eq!(parse_ps_time("0:00.12"), Some(120));
        assert_eq!(parse_ps_time("1:02.03"), Some(62_030));
        assert_eq!(parse_ps_time("1:02:03"), Some(3_723_000));
        assert_eq!(parse_ps_time("5"), Some(5_000));
    }

    #[test]
    fn silent_command_is_killed_as_stall() {
        let dir = temp_repo();
        #[cfg(windows)]
        let cfg = "verify = [\"ping -n 31 127.0.0.1 >nul\"]\n";
        #[cfg(not(windows))]
        let cfg = "verify = [\"sleep 30\"]\n";
        std::fs::write(dir.join(".review/config.toml"), cfg).unwrap();
        let cmds = load_commands(&dir).unwrap();
        let result =
            run_commands_timed(&dir, &cmds, None, Some(Duration::from_secs(1)), false).unwrap();
        assert!(
            !result.passed,
            "silent sleep must stall, got: {}",
            result.output
        );
        assert!(
            result.kill.as_ref().is_some_and(|k| k.is_stall()),
            "expected stall, got {:?}",
            result.kill
        );
        assert!(
            result.output.contains("stalled"),
            "missing stall note: {}",
            result.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn io_progress_prevents_stall() {
        let dir = temp_repo();
        #[cfg(windows)]
        let cfg = "verify = [\"echo tick & ping -n 2 127.0.0.1 >nul & echo tick & ping -n 2 127.0.0.1 >nul & echo tick\"]\n";
        #[cfg(not(windows))]
        let cfg = "verify = [\"sh -c 'for i in 1 2 3 4 5; do echo tick; sleep 1; done'\"]\n";
        std::fs::write(dir.join(".review/config.toml"), cfg).unwrap();
        let cmds = load_commands(&dir).unwrap();
        let result =
            run_commands_timed(&dir, &cmds, None, Some(Duration::from_secs(2)), false).unwrap();
        assert!(
            result.passed,
            "periodic output must count as progress, got: {}",
            result.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn early_stall_retries_once() {
        let dir = temp_repo();
        #[cfg(windows)]
        {
            std::fs::write(
                dir.join("hang.cmd"),
                "@echo off\r\nif exist ran exit /b 0\r\necho. > ran\r\nping -n 31 127.0.0.1 >nul\r\n",
            )
            .unwrap();
            std::fs::write(dir.join(".review/config.toml"), "verify = [\"hang.cmd\"]\n").unwrap();
        }
        #[cfg(not(windows))]
        {
            std::fs::write(
                dir.join("hang.sh"),
                "#!/bin/sh\nif [ -f ran ]; then exit 0; fi\ntouch ran\nsleep 30\n",
            )
            .unwrap();
            std::fs::write(
                dir.join(".review/config.toml"),
                "verify = [\"sh hang.sh\"]\n",
            )
            .unwrap();
        }
        let cmds = load_commands(&dir).unwrap();
        let result =
            run_commands_timed(&dir, &cmds, None, Some(Duration::from_secs(1)), false).unwrap();
        assert!(
            result.passed,
            "early stall must retry, got: {}",
            result.output
        );
        assert!(
            result.output.contains("retrying once"),
            "missing retry note: {}",
            result.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn late_stall_does_not_retry() {
        let dir = temp_repo();
        // Busy (CPU) past 2×stall, then go idle — must fail without retry.
        let cfg = "verify = [\"sh -c 'yes >/dev/null & y=$!; sleep 5; kill $y; sleep 30'\"]\n";
        std::fs::write(dir.join(".review/config.toml"), cfg).unwrap();
        let cmds = load_commands(&dir).unwrap();
        let result =
            run_commands_timed(&dir, &cmds, None, Some(Duration::from_secs(2)), false).unwrap();
        assert!(!result.passed);
        assert!(result.kill.as_ref().is_some_and(|k| k.is_stall()));
        assert!(
            !result.output.contains("retrying once"),
            "late stall must not retry: {}",
            result.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(windows))]
    #[test]
    fn cpu_progress_prevents_stall() {
        let dir = temp_repo();
        std::fs::write(
            dir.join(".review/config.toml"),
            "verify = [\"sh -c 'yes >/dev/null & y=$!; sleep 4; kill $y'\"]\n",
        )
        .unwrap();
        let cmds = load_commands(&dir).unwrap();
        let result =
            run_commands_timed(&dir, &cmds, None, Some(Duration::from_secs(2)), false).unwrap();
        assert!(
            result.passed,
            "CPU-only work must count as progress, got: {}",
            result.output
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
