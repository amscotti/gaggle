//! Harness-owned verify gate: run the `verify` commands from
//! `.review/config.toml` and treat their exit codes as the verdict.

use anyhow::Context;
use std::io::ErrorKind;
use std::path::Path;
use std::process::{Command, Stdio};

/// Outcome of running the configured verify commands.
pub struct RunResult {
    pub passed: bool,
    pub failed_command: Option<String>,
    pub output: String,
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
        // Placing `final_verify` at the BOTTOM of the file (a natural
        // edit) scopes it under the last [section] — it would be silently
        // ignored. Detect that exact mistake and warn.
        if key == "final_verify" {
            for section in ["commit", "model", "branch"] {
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
pub fn run_final(repo: &Path) -> anyhow::Result<RunResult> {
    run_commands(repo, &load_final_commands(repo)?)
}

/// Run a scoped check when we can derive one from `paths` (Go packages).
/// `Ok(None)` = no scoped command derivable — the CALLER decides to run
/// the full suite. (The old internal fallback made every green cycle on a
/// non-Go repo run the full suite twice: once here, once in the caller's
/// follow-up `run`.)
pub fn run_scoped(repo: &Path, paths: &[String]) -> anyhow::Result<Option<RunResult>> {
    match scoped_commands(repo, paths) {
        Some(cmds) => Ok(Some(run_commands(repo, &cmds)?)),
        None => Ok(None),
    }
}

/// `go test ./pkg...` for unique package dirs among `paths` (Go), or
/// `cargo test -p crate…` for unique workspace crates among `paths`
/// (Cargo workspace). None when no scoped command can be derived.
pub fn scoped_commands(repo: &Path, paths: &[String]) -> Option<Vec<String>> {
    if paths.is_empty() {
        return None;
    }
    if repo.join("go.mod").exists() {
        let pkgs = go_test_packages(repo, paths);
        if pkgs.is_empty() {
            return None;
        }
        // Shell-safe quoting: the command string goes through `sh -c` on
        // Unix and `cmd /C` on Windows, so the quote character differs.
        #[cfg(windows)]
        let quote = |p: &str| format!("\"{p}\"");
        #[cfg(not(windows))]
        let quote = |p: &str| format!("'{p}'");
        return Some(vec![format!(
            "go test {}",
            pkgs.iter().map(|p| quote(p)).collect::<Vec<_>>().join(" ")
        )]);
    }
    if repo.join("Cargo.toml").exists() {
        let crates = workspace_crates_touched(repo, paths);
        if crates.is_empty() {
            return None;
        }
        // Quoted `-p` flags: `cargo test -p 'crate' …` runs unit + bin +
        // integration tests of exactly the touched crates — fast on big
        // workspaces, and it catches fixer-added tests (redacter lesson).
        #[cfg(windows)]
        let quote = |p: &str| format!("\"{p}\"");
        #[cfg(not(windows))]
        let quote = |p: &str| format!("'{p}'");
        return Some(vec![format!(
            "cargo test {}",
            crates
                .iter()
                .map(|c| format!("-p {}", quote(c)))
                .collect::<Vec<_>>()
                .join(" ")
        )]);
    }
    None
}

/// Map touched paths to unique workspace member crate names by reading
/// each member's Cargo.toml `name` and matching paths under its directory.
/// Returns empty when the repo is a single-crate project (no members) —
/// the caller then falls back to the configured full suite.
pub(crate) fn workspace_crates_touched(repo: &Path, paths: &[String]) -> Vec<String> {
    // Parse the workspace members from the root Cargo.toml.
    let Ok(text) = std::fs::read_to_string(repo.join("Cargo.toml")) else {
        return Vec::new();
    };
    let Ok(t) = text.parse::<toml::Value>() else {
        return Vec::new();
    };
    let Some(members) = t
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())
    else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for m in members.iter().filter_map(|v| v.as_str()) {
        let member_dir = m.trim_end_matches('/').replace('\\', "/");
        // `name` comes from the member manifest (fallback: dir basename).
        let manifest = repo.join(format!("{member_dir}/Cargo.toml"));
        let name = std::fs::read_to_string(&manifest)
            .ok()
            .and_then(|mt| mt.parse::<toml::Value>().ok())
            .and_then(|mt| {
                mt.get("package")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    .map(str::to_string)
            })
            .unwrap_or_else(|| {
                member_dir
                    .rsplit('/')
                    .next()
                    .unwrap_or(member_dir.as_str())
                    .to_string()
            });
        // Touched if any path lies under the member directory.
        let prefix = format!("{member_dir}/");
        if paths.iter().any(|p| {
            let p = p.trim_start_matches("./").replace('\\', "/");
            p == member_dir || p.starts_with(&prefix)
        }) && !out.contains(&name)
        {
            out.push(name);
        }
    }
    out
}

pub(crate) fn go_test_packages(repo: &Path, paths: &[String]) -> Vec<String> {
    let mut pkgs = Vec::new();
    for p in paths {
        let rel = p.replace('\\', "/");
        let rel = rel.trim_start_matches("./");
        if rel.is_empty() || rel == "go.mod" || rel == "go.sum" {
            continue;
        }
        // Only map REAL Go packages: `.go` files contribute their
        // directory, and existing directories contribute themselves.
        // Anything else (Makefile, LICENSE, Dockerfile, README…) is not a
        // package — passing it to `go test` fails the whole command with
        // 'matched no packages' and spuriously fails a good fix.
        let dir = if rel.ends_with(".go") {
            match rel.rsplit_once('/') {
                Some((d, _)) => d.to_string(),
                None => ".".to_string(), // .go file in repo root → root package
            }
        } else if repo.join(rel).is_dir() {
            rel.trim_end_matches('/').to_string()
        } else {
            continue;
        };
        // Deleted/renamed source: a `.go` path whose parent dir no longer
        // exists would yield a nonexistent package and fail `go test` with
        // 'matched no packages', spuriously failing a good fix.
        if !repo.join(&dir).is_dir() {
            continue;
        }
        if dir.is_empty() {
            continue;
        }
        let pkg = format!("./{dir}");
        if !pkgs.contains(&pkg) {
            pkgs.push(pkg);
        }
    }
    pkgs
}

fn run_commands(repo: &Path, cmds: &[String]) -> anyhow::Result<RunResult> {
    let mut output = String::new();
    for cmd in cmds {
        let result = run_shell(repo, cmd)
            .with_context(|| format!("failed to spawn verify command: {cmd}"))?;
        append_output(&mut output, &result.stdout, &result.stderr);
        if result.timed_out {
            // A timeout is a FAILED VERIFY, not a harness abort: the loop's
            // fix-cycle logic (and the verify classifier recipe) handles a
            // red result — an Err here would kill the whole run instead.
            output.push_str(&format!(
                "[gaggle] verify command timed out after {}s and was killed\n",
                verify_timeout().as_secs()
            ));
            return Ok(RunResult {
                passed: false,
                failed_command: Some(cmd.clone()),
                output,
            });
        }
        if !result.success {
            return Ok(RunResult {
                passed: false,
                failed_command: Some(cmd.clone()),
                output,
            });
        }
    }
    Ok(RunResult {
        passed: true,
        failed_command: None,
        output,
    })
}

/// Wall-clock ceiling for ONE verify command. A hung command (waiting on
/// a network resource, a stuck prompt) must not block the loop forever.
/// Override with `GAGGLE_VERIFY_TIMEOUT_SECS`.
const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Cap per-stream bytes we keep in memory from a verify command; a
/// runaway command writing forever would otherwise grow the buffer
/// without bound. (Diagnostics only ever surface the tail.)
const VERIFY_OUTPUT_CAP: usize = 10 * 1024 * 1024;

fn verify_timeout() -> std::time::Duration {
    match std::env::var("GAGGLE_VERIFY_TIMEOUT_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) => std::time::Duration::from_secs(secs.max(1)),
            // A user-specified override that fails to parse must not be
            // silently ignored — warn (but keep the run going on default).
            Err(_) => {
                eprintln!(
                    "  ⚠ GAGGLE_VERIFY_TIMEOUT_SECS={raw:?} is not a number of seconds — using default {}s",
                    VERIFY_TIMEOUT.as_secs()
                );
                VERIFY_TIMEOUT
            }
        },
        Err(_) => VERIFY_TIMEOUT,
    }
}

/// Outcome of one shell command: exit status + drained output, or a
/// timeout marker (the run continues as a FAILED verify, not an abort,
/// so the verify recipe can classify/retry it).
struct ShellOutcome {
    success: bool,
    timed_out: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_shell(repo: &Path, cmd: &str) -> anyhow::Result<ShellOutcome> {
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
    fn drain(mut r: std::process::ChildStdout) -> Vec<u8> {
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
                    Ok(_) => continue,
                }
            }
            match r.read(&mut chunk) {
                Ok(0) => break,
                // EINTR is not EOF — retry; breaking here would silently
                // truncate diagnostics (possibly the failing error text).
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf
    }
    // ChildStderr is a distinct type; duplicate the body.
    fn drain_err(mut r: std::process::ChildStderr) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        use std::io::Read;
        loop {
            if buf.len() >= VERIFY_OUTPUT_CAP {
                match r.read(&mut chunk) {
                    Ok(0) => break,
                    Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                    Ok(_) => continue,
                }
            }
            match r.read(&mut chunk) {
                Ok(0) => break,
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf
    }
    let out_handle = child
        .stdout
        .take()
        .map(|r| std::thread::spawn(move || drain(r)));
    let err_handle = child
        .stderr
        .take()
        .map(|r| std::thread::spawn(move || drain_err(r)));

    let timeout = verify_timeout();
    let deadline = std::time::Instant::now() + timeout;
    let mut timed_out = false;
    let status_success;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                status_success = status.success();
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    kill_tree(&mut child);
                    let _ = child.wait(); // reap so OUR pipes close
                    status_success = false;
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
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

    if timed_out {
        eprintln!(
            "  ⚠ verify command timed out after {}s (killed): {cmd}",
            timeout.as_secs()
        );
    }
    Ok(ShellOutcome {
        success: status_success,
        timed_out,
        stdout,
        stderr,
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

    #[test]
    fn go_packages_from_paths() {
        let dir = temp_repo();
        std::fs::create_dir_all(dir.join("internal/game")).unwrap();
        std::fs::create_dir_all(dir.join("internal/p2p")).unwrap();
        std::fs::write(dir.join("internal/game/game.go"), "package game\n").unwrap();
        std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
        std::fs::write(dir.join("LICENSE"), "MIT\n").unwrap();
        let pkgs = go_test_packages(
            &dir,
            &[
                "internal/game/game.go".into(),
                "internal/game/game_test.go".into(),
                "internal/p2p".into(),
                "go.mod".into(),
                "mise.toml".into(),
                // Extension-less NON-directory files must NOT become
                // packages ('go test ./Makefile' fails with 'matched no
                // packages').
                "Makefile".into(),
                "LICENSE".into(),
                "Dockerfile".into(),
                ".gitignore".into(),
            ],
        );
        assert_eq!(pkgs, vec!["./internal/game", "./internal/p2p"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_commands_quote_packages() {
        let dir = temp_repo();
        std::fs::write(dir.join("go.mod"), "module x\n").unwrap();
        std::fs::create_dir_all(dir.join("my pkg")).unwrap();
        let cmds =
            scoped_commands(&dir, &["my pkg".into()]).expect("package dir maps to a command");
        assert_eq!(cmds.len(), 1);
        #[cfg(windows)]
        assert_eq!(cmds[0], r#"go test "./my pkg""#);
        #[cfg(not(windows))]
        assert_eq!(cmds[0], r"go test './my pkg'");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_commands_none_without_gomod() {
        let dir = temp_repo();
        assert!(scoped_commands(&dir, &["internal/game/game.go".into()]).is_none());
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn rust_workspace_scopes_to_touched_crates() {
        let dir = temp_repo();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/core\", \"crates/cli\"]\n",
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("crates/core/src")).unwrap();
        std::fs::create_dir_all(dir.join("crates/cli/src")).unwrap();
        std::fs::write(
            dir.join("crates/core/Cargo.toml"),
            "[package]\nname = \"redact-core\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("crates/cli/Cargo.toml"),
            "[package]\nname = \"redact-cli\"\n",
        )
        .unwrap();
        // Touched core only → scoped command tests core only.
        let touched = vec!["crates/core/src/engine.rs".to_string()];
        let cmds = scoped_commands(&dir, &touched).expect("workspace derives scoped cmds");
        assert_eq!(cmds.len(), 1);
        #[cfg(not(windows))]
        assert_eq!(cmds[0], "cargo test -p 'redact-core'");
        #[cfg(windows)]
        assert_eq!(cmds[0], "cargo test -p \"redact-core\"");
        // Touched both → both crates, deterministic order (member order).
        let touched = vec![
            "crates/cli/src/main.rs".to_string(),
            "crates/core/src/lib.rs".to_string(),
        ];
        let cmds = scoped_commands(&dir, &touched).expect("scoped");
        #[cfg(not(windows))]
        assert_eq!(cmds[0], "cargo test -p 'redact-core' -p 'redact-cli'");
        #[cfg(windows)]
        assert_eq!(cmds[0], "cargo test -p \"redact-core\" -p \"redact-cli\"");
        // No touched paths under members → None (caller falls back).
        assert!(scoped_commands(&dir, &["docs/README.md".to_string()]).is_none());
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
