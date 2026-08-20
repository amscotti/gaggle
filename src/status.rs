//! Live observability: what is the loop doing right now?
//!
//! The engine writes a phase transition after every step via [`report`]:
//!   .review/status.json   — overwritten each call (instantaneous state)
//!   .review/activity.log  — append-only timeline of every transition
//!
//! Human view: `gaggle status` prints current phase + recent activity.
//! The engine also calls this before/after each agent run so you can watch
//! "reviewing component X" / "fixing component X" live from another shell
//! (`tail -f .review/activity.log`).

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Controlled vocabulary of loop phases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Picking,
    Reviewing,
    Fixing,
    Verifying,
    Committing,
    Done,
    Failed,
    Idle,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Picking => "picking",
            Phase::Reviewing => "reviewing",
            Phase::Fixing => "fixing",
            Phase::Verifying => "verifying",
            Phase::Committing => "committing",
            Phase::Done => "done",
            Phase::Failed => "failed",
            Phase::Idle => "idle",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub phase: String,
    pub component: String,
    pub detail: String,
    pub ts: String,
}

impl Status {
    fn new(phase: Phase, component: &str, detail: &str) -> Self {
        // activity.log is one-line-per-entry: component/detail are
        // interpolated into it verbatim below, so a multi-line detail (e.g.
        // an anyhow chain or verify diagnostics) must be flattened FIRST —
        // newlines would corrupt the timeline format and inflate tail
        // output with continuation fragments.
        let flatten = |s: &str| -> String {
            s.chars()
                .map(|c| match c {
                    '\n' | '\r' | '\t' => ' ',
                    // Other C0 controls are invisible junk in a log line.
                    c if c.is_control() => ' ',
                    c => c,
                })
                .collect()
        };
        Self {
            phase: phase.as_str().to_string(),
            component: flatten(component),
            detail: flatten(detail).chars().take(300).collect(),
            ts: Utc::now().to_rfc3339(),
        }
    }
}

/// Record a phase transition. Writes status.json + appends activity.log.
///
/// The activity.log append is performed *before* the status.json overwrite so
/// that, if the append fails (disk full / permissions), the JSON never shows a
/// phase that the timeline does not. The append-only log is the source of
/// truth; status.json is a best-effort instantaneous snapshot.
pub fn report(dir: &Path, phase: Phase, component: &str, detail: &str) -> Result<()> {
    fs::create_dir_all(dir)?;
    let status = Status::new(phase, component, detail);
    // Append to the timeline first (source of truth).
    let line = format!(
        "{}  [{:>10}]  {:<22}  {}",
        status.ts, status.phase, status.component, status.detail
    );
    let log_path = dir.join("activity.log");
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    writeln!(f, "{line}")?;
    f.flush()?;
    drop(f);

    // Now publish the instantaneous snapshot. Write status.json atomically
    // (temp-then-rename) so a concurrent reader in `print_status` never
    // observes a partial/truncated file. On Unix this is a true atomic rename;
    // on Windows `rename` over an existing file fails with `AccessDenied`, so
    // we remove the destination first (best-effort — the window where the file
    // is absent is tiny and readers already handle a missing/partial file
    // gracefully). If the rename fails we clean up the temp file so it does
    // not leak (it would self-heal on the next call, but we avoid leaving
    // stale state behind).
    let status_path = dir.join("status.json");
    // Unique temp name per call (pid + counter): a fixed shared name lets
    // two concurrent report() calls (two engines on one repo) truncate/
    // rename each other's temp file mid-write, surfacing spurious errors
    // or publishing the wrong snapshot.
    static NEXT_TMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let tmp_path = dir.join(format!(
        "status.json.tmp.{}.{}",
        std::process::id(),
        NEXT_TMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // The snapshot is best-effort: a failed publish must NOT abort the
    // engine run (every loop caller uses `?`). Clean up the temp file on
    // ANY failure after creation so partial writes don't leak.
    if let Err(e) = fs::write(&tmp_path, serde_json::to_string_pretty(&status)? + "\n") {
        let _ = fs::remove_file(&tmp_path);
        eprintln!("  ⚠ status snapshot write failed (continuing): {e}");
        return Ok(());
    }
    #[cfg(windows)]
    {
        let _ = fs::remove_file(&status_path);
    }
    if let Err(e) = fs::rename(&tmp_path, &status_path) {
        let _ = fs::remove_file(&tmp_path);
        // Transient rename races (a reader holding status.json open on
        // Windows, AV scans) must not kill the loop — warn and move on;
        // activity.log (written above) remains the source of truth.
        eprintln!("  ⚠ status snapshot publish failed (continuing): {e}");
    }
    Ok(())
}

/// Print the current status to stdout (CLI `gaggle status`).
pub fn print_status(dir: &Path, tail: usize) -> Result<()> {
    let status_path = dir.join("status.json");
    if !status_path.exists() {
        println!("no status recorded yet");
    } else {
        match fs::read_to_string(&status_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Status>(&s).ok())
        {
            Some(s) => {
                println!("phase:     {}", s.phase);
                println!("component: {}", s.component);
                println!("detail:    {}", s.detail);
                println!("ts:        {}", s.ts);
            }
            None => {
                // The file exists but is transiently truncated/unreadable
                // (e.g. a concurrent writer caught mid-rename). Distinguish
                // this from the genuinely-missing-file case above.
                println!("status temporarily unavailable");
            }
        }
    }
    println!();
    let log_path = dir.join("activity.log");
    if log_path.exists() {
        println!("── recent activity (last {tail}) ──");
        if let Some(tail_lines) = read_last_lines(&log_path, tail)? {
            for ln in &tail_lines {
                println!("{ln}");
            }
        } else {
            println!("no activity recorded yet");
        }
    } else {
        println!("no activity recorded yet");
    }
    Ok(())
}

/// Read the last `n` lines from a file without loading the whole file into
/// memory. Returns `Ok(None)` when the file is empty.
fn read_last_lines(path: &Path, n: usize) -> Result<Option<Vec<String>>> {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        // A vanished log (raced removal, fresh init) is "no activity", not
        // a hard CLI error.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    // Check for an empty file first so callers can show the "no activity"
    // fallback even when n == 0.
    let file_len = f.seek(SeekFrom::End(0))?;
    if file_len == 0 {
        return Ok(None);
    }
    if n == 0 {
        // Non-empty file but caller asked for zero lines: return an empty
        // window rather than None, so the caller renders zero lines instead
        // of the false "no activity recorded yet" fallback.
        return Ok(Some(Vec::new()));
    }

    // Read backward in chunks until we accumulate `n` newlines (plus the text
    // before the first of those newlines).
    const CHUNK: u64 = 4096;
    let mut pos = file_len;
    let mut buf: Vec<u8> = Vec::new();
    let mut newline_count: usize = 0;

    while pos > 0 {
        let read_size = CHUNK.min(pos);
        pos -= read_size;
        f.seek(SeekFrom::Start(pos))?;
        let mut chunk = vec![0u8; read_size as usize];
        f.read_exact(&mut chunk)?;
        // Prepend chunk to buf.
        let old_len = buf.len();
        buf.resize(read_size as usize + old_len, 0);
        buf.copy_within(0..old_len, read_size as usize);
        buf[..read_size as usize].copy_from_slice(&chunk);

        // Count newlines in the newly-prepended portion. Stop only once we
        // hold MORE than n newlines (or hit the file start): with exactly
        // n, a truncated head means lines.len() == n and taking the last n
        // would return the leading partial fragment as a real line.
        newline_count += chunk.iter().filter(|&&b| b == b'\n').count();
        if newline_count > n {
            break;
        }
    }

    // We have the tail of the file in `buf`. Split into lines and keep last `n`.
    // The loop guarantees we have at least `n` complete newlines. When pos > 0
    // there is earlier file content, so buf's first "line" is a TRUNCATED
    // fragment (possibly starting mid-UTF-8, surfacing as U+FFFD) — drop it
    // before taking the last n lines. (Without this, a chunk boundary that
    // leaves exactly n newlines in buf returns the fragment as a real line.)
    Ok(Some(tail_lines_from_buf(&buf, pos > 0, n)))
}

/// Pure tail of `read_last_lines` (unit-testable without hitting exact
/// 4096-byte chunk boundaries): split the backward-accumulated buffer into
/// lines, drop the leading partial fragment when the buffer is known to be
/// truncated, keep the last `n`.
fn tail_lines_from_buf(buf: &[u8], truncated_head: bool, n: usize) -> Vec<String> {
    let text = String::from_utf8_lossy(buf);
    let mut lines: Vec<&str> = text.lines().collect();
    if truncated_head && !lines.is_empty() {
        lines.remove(0);
    }
    let start = lines.len().saturating_sub(n);
    lines[start..].iter().map(|s| s.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Invariant-respecting buffers: when truncated_head is true the loop
    /// guarantees MORE than n newlines, so dropping the glued/partial
    /// first line still leaves ≥ n real lines.
    #[test]
    fn tail_lines_exact_n_plus_fragment_drops_leading_fragment() {
        // "ment" is the tail of a line cut at a chunk boundary; the last
        // two real lines are line1/line2... wait: with n=2 we stop at >2
        // newlines, i.e. we also hold line3's predecessor. Construct:
        // fragment + 3 complete lines, n=2 → last two real lines.
        let buf = b"ment\nline1\nline2\nline3\n";
        let out = tail_lines_from_buf(buf, true, 2);
        assert_eq!(out, vec!["line2", "line3"]);
        // Exactly n+1 newlines (minimum invariant): fragment + 2 lines.
        let buf = b"ment\nline1\nline2\n";
        let out = tail_lines_from_buf(buf, true, 2);
        assert_eq!(out, vec!["line1", "line2"]);
    }

    #[test]
    fn tail_lines_untruncated_keeps_all() {
        let buf = b"line1\nline2\nline3\n";
        let out = tail_lines_from_buf(buf, false, 2);
        assert_eq!(out, vec!["line2", "line3"]);
        // Whole-file read (no truncation): asking for more than present
        // returns everything.
        let out = tail_lines_from_buf(buf, false, 10);
        assert_eq!(out, vec!["line1", "line2", "line3"]);
    }
}
