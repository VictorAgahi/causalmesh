use mesh_core::{AuditEntry, AuditLogger, AuditMetrics, IndexCacheTotals};
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fewest timed calls for which a p95 is reported. Below this, the nearest-rank
/// p95 is simply the slowest (or second slowest) call, which the 7.0.0 output
/// presented as a p95 even over a single sample.
pub const MIN_P95_SAMPLES: usize = 20;

/// Per-tool counts and latencies, sorted by call volume for display.
#[derive(serde::Serialize)]
pub struct ToolStats {
    pub tool: String,
    pub calls: usize,
    /// Calls whose audit status is not `SUCCESS`: exactly the calls answered with
    /// `isError: true` (`ToolRegistry::invoke` audits `ERROR` for every `Err`).
    pub errors: usize,
    /// Calls with a recorded latency (only calls made by 7.0.0+ are timed).
    pub latency_samples: usize,
    pub p50_us: Option<u64>,
    /// `None` below [`MIN_P95_SAMPLES`] timed calls.
    pub p95_us: Option<u64>,
}

/// Calls audited under one session id (one agent session: see
/// `mesh_server::new_session_id`).
#[derive(serde::Serialize)]
pub struct SessionStats {
    pub session_id: String,
    pub calls: usize,
    pub errors: usize,
    /// ISO 8601 UTC timestamps of the session's first and last call in the window.
    pub first_call: String,
    pub last_call: String,
}

/// `meshd` starts recorded for one workspace (its roots, `;`-joined).
#[derive(serde::Serialize)]
pub struct DaemonStarts {
    pub workspace: String,
    pub starts: usize,
}

/// Local usage summary computed from the audit log (RFC-001 item 15). Nothing
/// here leaves the machine: it is derived purely from an already-local SQLite
/// file, read read-only. Every figure is counted or measured; nothing is estimated.
#[derive(serde::Serialize)]
pub struct StatsSummary {
    pub total_calls: usize,
    pub total_errors: usize,
    pub per_tool: Vec<ToolStats>,
    /// Ordered by first call, oldest first.
    pub sessions: Vec<SessionStats>,
    /// `(file, number of calls whose result included it)`, top 10.
    pub top_targets: Vec<(String, usize)>,
    /// `false` when the database predates latency recording (table absent).
    pub latency_recorded: bool,
    /// `None` when the database predates index cache recording (table absent).
    #[serde(serialize_with = "serialize_index_cache")]
    pub index_cache: Option<IndexCacheTotals>,
    /// `None` when the database predates process-start recording (table absent).
    pub daemon_starts: Option<Vec<DaemonStarts>>,
}

impl StatsSummary {
    /// Restarts = starts after the first one of each workspace within the window.
    pub fn daemon_restarts(&self) -> Option<usize> {
        self.daemon_starts
            .as_ref()
            .map(|d| d.iter().map(|w| w.starts.saturating_sub(1)).sum())
    }
}

fn serialize_index_cache<S: serde::Serializer>(
    totals: &Option<IndexCacheTotals>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    match totals {
        None => serializer.serialize_none(),
        Some(t) => {
            let mut st = serializer.serialize_struct("IndexCacheTotals", 4)?;
            st.serialize_field("passes", &t.passes)?;
            st.serialize_field("hits", &t.hits)?;
            st.serialize_field("misses", &t.misses)?;
            st.serialize_field("errors", &t.errors)?;
            st.end()
        }
    }
}

/// A workspace as `meshd` records it (roots `;`-joined, absolute) made readable:
/// roots print relative to their deepest common ancestor, as
/// `<ancestor>/{a, b/c, … (+N)} (M roots)`.
fn compact_workspace(workspace: &str) -> String {
    let roots: Vec<&Path> = workspace.split(';').map(Path::new).collect();
    if roots.len() < 2 {
        return workspace.to_string();
    }
    let mut ancestor: PathBuf = roots[0].to_path_buf();
    while !roots.iter().all(|r| r.starts_with(&ancestor)) {
        if !ancestor.pop() {
            return workspace.to_string();
        }
    }
    if ancestor.parent().is_none() {
        // Only the filesystem root in common: nothing gained.
        return workspace.to_string();
    }
    const SHOWN: usize = 4;
    let names: Vec<String> = roots
        .iter()
        .take(SHOWN)
        .map(|r| {
            r.strip_prefix(&ancestor)
                .map_or_else(|_| r.display().to_string(), |rel| rel.display().to_string())
        })
        .collect();
    let more = if roots.len() > SHOWN {
        format!(", … (+{})", roots.len() - SHOWN)
    } else {
        String::new()
    };
    format!(
        "{}/{{{}{more}}} ({} roots)",
        ancestor.display(),
        names.join(", "),
        roots.len()
    )
}

/// Nearest-rank percentile of an ascending-sorted sample: the value at 1-based rank
/// `ceil(p / 100 * n)` (rank 1 at least). No interpolation, so the result is always
/// an observed value. `None` for an empty sample.
pub fn percentile_nearest_rank(sorted: &[u64], p: u64) -> Option<u64> {
    let n = sorted.len() as u64;
    if n == 0 {
        return None;
    }
    let rank = (p.min(100) * n).div_ceil(100).max(1);
    sorted.get((rank - 1) as usize).copied()
}

fn percent(num: u64, den: u64) -> String {
    if den == 0 {
        "n/a".to_string()
    } else {
        format!("{:.1}%", num as f64 * 100.0 / den as f64)
    }
}

fn millis(us: Option<u64>) -> String {
    match us {
        Some(us) => format!("{}.{:03}ms", us / 1000, us % 1000),
        None => "-".to_string(),
    }
}

pub struct StatsCommand;

impl StatsCommand {
    /// Reads `audit.db` and prints a local usage summary: text on stderr, or JSON
    /// on stdout with `json` (a CLI subcommand, never the MCP server, so stdout is
    /// free here — same as `doctor --json`). `db_path` overrides
    /// [`AuditLogger::default_db_path`]; `since` accepts `<N>s|m|h|d` or `all`
    /// (default: `7d`); `session` keeps only that session's calls.
    pub fn run(
        db_path: Option<&Path>,
        since: Option<&str>,
        session: Option<&str>,
        json: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path: PathBuf = db_path
            .map(PathBuf::from)
            .unwrap_or_else(AuditLogger::default_db_path);

        let since_label = since.unwrap_or("7d");
        let cutoff = Self::parse_since(since_label)?;

        let mut entries = AuditLogger::read_entries(&path, cutoff)?;
        if let Some(session) = session {
            entries.retain(|e| e.session_id == session);
        }
        let metrics = AuditLogger::read_metrics_for_session(&path, cutoff, session)?;

        let summary = Self::summarize(&entries, &metrics);
        if json {
            println!("{}", serde_json::to_string_pretty(&summary)?);
            return Ok(());
        }
        if entries.is_empty() {
            eprintln!(
                "ℹ No audit entries found in {} (window: {since_label}{})",
                path.display(),
                session.map_or(String::new(), |s| format!(", session: {s}"))
            );
            return Ok(());
        }
        eprint!("{}", Self::render(&path, since_label, &summary));

        Ok(())
    }

    /// Pure aggregation step, kept separate from I/O so it is directly testable.
    pub fn summarize(entries: &[AuditEntry], metrics: &AuditMetrics) -> StatsSummary {
        let mut per_tool: HashMap<String, (usize, usize)> = HashMap::new();
        let mut per_target: HashMap<String, usize> = HashMap::new();
        let mut per_session: HashMap<&str, SessionStats> = HashMap::new();

        for entry in entries {
            // Entries come in `entry_seq` order: the first one seen is the first call.
            let session = per_session
                .entry(entry.session_id.as_str())
                .or_insert_with(|| SessionStats {
                    session_id: entry.session_id.clone(),
                    calls: 0,
                    errors: 0,
                    first_call: mesh_core::normalize_audit_timestamp(&entry.timestamp),
                    last_call: String::new(),
                });
            session.calls += 1;
            if entry.status != "SUCCESS" {
                session.errors += 1;
            }
            session.last_call = mesh_core::normalize_audit_timestamp(&entry.timestamp);

            let counter = per_tool.entry(entry.tool.clone()).or_insert((0, 0));
            counter.0 += 1;
            if entry.status != "SUCCESS" {
                counter.1 += 1;
            }
            for file in &entry.files_accessed {
                *per_target.entry(file.clone()).or_insert(0) += 1;
            }
        }

        let mut latencies: HashMap<&str, Vec<u64>> = HashMap::new();
        for (tool, us) in metrics.tool_latencies_us.iter().flatten() {
            latencies.entry(tool.as_str()).or_default().push(*us);
        }

        let mut per_tool: Vec<ToolStats> = per_tool
            .into_iter()
            .map(|(tool, (calls, errors))| {
                let mut sample = latencies.remove(tool.as_str()).unwrap_or_default();
                sample.sort_unstable();
                let p95_us = if sample.len() >= MIN_P95_SAMPLES {
                    percentile_nearest_rank(&sample, 95)
                } else {
                    None
                };
                ToolStats {
                    latency_samples: sample.len(),
                    p50_us: percentile_nearest_rank(&sample, 50),
                    p95_us,
                    tool,
                    calls,
                    errors,
                }
            })
            .collect();
        per_tool.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.tool.cmp(&b.tool)));

        let mut top_targets: Vec<(String, usize)> = per_target.into_iter().collect();
        top_targets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top_targets.truncate(10);

        let mut sessions: Vec<SessionStats> = per_session.into_values().collect();
        // ISO 8601 UTC strings of one fixed width: lexical order is time order.
        sessions.sort_by(|a, b| {
            a.first_call
                .cmp(&b.first_call)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });

        let daemon_starts = metrics.process_starts.as_ref().map(|starts| {
            let mut by_ws: BTreeMap<&str, usize> = BTreeMap::new();
            for (process, workspace) in starts {
                if process == "meshd" {
                    *by_ws.entry(workspace.as_str()).or_insert(0) += 1;
                }
            }
            by_ws
                .into_iter()
                .map(|(workspace, starts)| DaemonStarts {
                    workspace: workspace.to_string(),
                    starts,
                })
                .collect()
        });

        StatsSummary {
            total_calls: entries.len(),
            total_errors: per_tool.iter().map(|t| t.errors).sum(),
            per_tool,
            sessions,
            top_targets,
            latency_recorded: metrics.tool_latencies_us.is_some(),
            index_cache: metrics.index_cache,
            daemon_starts,
        }
    }

    /// Renders the summary as the text `run` prints (to stderr).
    pub fn render(db_path: &Path, since_label: &str, summary: &StatsSummary) -> String {
        let mut o = String::new();
        let _ = writeln!(
            o,
            "MeshMCP local audit stats — {} ({} calls, window: {since_label})",
            db_path.display(),
            summary.total_calls
        );
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "isError rate: {}/{} ({})",
            summary.total_errors,
            summary.total_calls,
            percent(summary.total_errors as u64, summary.total_calls as u64)
        );
        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "Per tool (latency: nearest-rank percentiles over timed calls):"
        );
        let _ = writeln!(
            o,
            "  {:<20} {:>6} {:>8} {:>8} {:>7} {:>12} {:>12}",
            "tool", "calls", "isError", "rate", "timed", "p50", "p95"
        );
        for t in &summary.per_tool {
            let _ = writeln!(
                o,
                "  {:<20} {:>6} {:>8} {:>8} {:>7} {:>12} {:>12}",
                t.tool,
                t.calls,
                t.errors,
                percent(t.errors as u64, t.calls as u64),
                t.latency_samples,
                millis(t.p50_us),
                millis(t.p95_us)
            );
        }
        if !summary.latency_recorded {
            let _ = writeln!(
                o,
                "  (latency not recorded by this audit db: it predates MeshMCP 7.0.0)"
            );
        } else if summary
            .per_tool
            .iter()
            .any(|t| t.latency_samples > 0 && t.p95_us.is_none())
        {
            let _ = writeln!(
                o,
                "  (p95 shown as '-' below {MIN_P95_SAMPLES} timed calls: too few samples)"
            );
        }

        let _ = writeln!(o);
        const SHOWN_SESSIONS: usize = 10;
        let _ = writeln!(
            o,
            "Sessions: {} (one per agent connection; last {} shown, filter with --session <id>)",
            summary.sessions.len(),
            summary.sessions.len().min(SHOWN_SESSIONS)
        );
        let skip = summary.sessions.len().saturating_sub(SHOWN_SESSIONS);
        for sess in summary.sessions.iter().skip(skip) {
            let _ = writeln!(
                o,
                "  {:<32} {:>6} calls {:>4} isError  {} → {}",
                sess.session_id, sess.calls, sess.errors, sess.first_call, sess.last_call
            );
        }

        let _ = writeln!(o);
        match &summary.index_cache {
            Some(c) => {
                let lookups = c.hits + c.misses;
                let _ = writeln!(
                    o,
                    "Index cache: hit rate {} ({} hits / {} lookups over {} indexing passes, {} lookup errors counted as misses)",
                    percent(c.hits, lookups),
                    c.hits,
                    lookups,
                    c.passes,
                    c.errors
                );
            }
            None => {
                let _ = writeln!(
                    o,
                    "Index cache: not recorded by this audit db (it predates MeshMCP 7.0.0)"
                );
            }
        }

        match &summary.daemon_starts {
            Some(ws) => {
                let starts: usize = ws.iter().map(|w| w.starts).sum();
                let _ = writeln!(
                    o,
                    "Daemon: {} restarts ({starts} meshd starts over {} workspaces; a restart is any start after a workspace's first in the window)",
                    summary.daemon_restarts().unwrap_or(0),
                    ws.len()
                );
                for w in ws {
                    let _ = writeln!(
                        o,
                        "  {:<6} starts  {}",
                        w.starts,
                        compact_workspace(&w.workspace)
                    );
                }
            }
            None => {
                let _ = writeln!(
                    o,
                    "Daemon: restarts not recorded by this audit db (it predates MeshMCP 7.0.0)"
                );
            }
        }

        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "Most-returned files (top {}, by number of calls whose result included the file):",
            summary.top_targets.len()
        );
        if summary.top_targets.is_empty() {
            let _ = writeln!(o, "  (no files_accessed recorded in this window)");
        } else {
            for (target, count) in &summary.top_targets {
                let _ = writeln!(o, "  {count:<6} {target}");
            }
        }

        let _ = writeln!(o);
        let _ = writeln!(
            o,
            "Note: everything above is counted or measured locally; nothing is estimated (no \
             \"tokens saved\"). audit_entries stores no per-call result size, so an \
             empty-result rate cannot be computed from this data today."
        );
        let _ = writeln!(
            o,
            "Nothing above leaves this machine — this is a local, read-only SQLite scan."
        );
        o
    }

    /// Parses `--since`: `all`, or `<amount><unit>` where unit is one of
    /// `s`/`m`/`h`/`d`. Returns the cutoff as Unix-epoch seconds, or `None` for
    /// no filtering (`all`).
    fn parse_since(spec: &str) -> Result<Option<f64>, Box<dyn std::error::Error>> {
        if spec.eq_ignore_ascii_case("all") {
            return Ok(None);
        }

        if spec.is_empty() {
            return Err("invalid --since value: expected e.g. 7d, 24h, 30m, or 'all'".into());
        }

        // Split off the last *character*, not the last byte: `7é` used to panic
        // slicing inside the two-byte `é`.
        let unit_start = spec.char_indices().next_back().map_or(0, |(i, _)| i);
        let (num_part, unit) = spec.split_at(unit_start);
        let amount: f64 = num_part.parse().map_err(|_| {
            format!("invalid --since value: {spec} (expected e.g. 7d, 24h, 30m, or 'all')")
        })?;

        let seconds = match unit {
            "s" => amount,
            "m" => amount * 60.0,
            "h" => amount * 3_600.0,
            "d" => amount * 86_400.0,
            _ => {
                return Err(format!(
                    "invalid --since unit in '{spec}': expected one of s, m, h, d, or 'all'"
                )
                .into())
            }
        };

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64();

        Ok(Some(now - seconds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_summarize_counts_calls_errors_and_targets() {
        let entries = vec![
            AuditEntry {
                entry_seq: 0,
                prev_hash: AuditLogger::GENESIS_HASH.to_string(),
                timestamp: "1.000".to_string(),
                session_id: "s".to_string(),
                trace_id: None,
                tool: "smart_search".to_string(),
                args_digest: "d".to_string(),
                status: "SUCCESS".to_string(),
                files_accessed: vec!["src/main.rs".to_string()],
                secrets_redacted_count: 0,
                entry_hash: "h0".to_string(),
                chain_version: 2,
            },
            AuditEntry {
                entry_seq: 1,
                prev_hash: "h0".to_string(),
                timestamp: "2.000".to_string(),
                session_id: "s".to_string(),
                trace_id: None,
                tool: "smart_search".to_string(),
                args_digest: "d".to_string(),
                status: "ERROR".to_string(),
                files_accessed: vec![],
                secrets_redacted_count: 0,
                entry_hash: "h1".to_string(),
                chain_version: 2,
            },
            AuditEntry {
                entry_seq: 2,
                prev_hash: "h1".to_string(),
                timestamp: "3.000".to_string(),
                session_id: "s".to_string(),
                trace_id: None,
                tool: "find_dependents".to_string(),
                args_digest: "d".to_string(),
                status: "SUCCESS".to_string(),
                files_accessed: vec!["src/main.rs".to_string(), "src/lib.rs".to_string()],
                secrets_redacted_count: 0,
                entry_hash: "h2".to_string(),
                chain_version: 2,
            },
        ];

        let summary = StatsCommand::summarize(&entries, &AuditMetrics::default());
        assert_eq!(summary.total_calls, 3);

        assert_eq!(summary.per_tool.len(), 2);
        let smart_search = summary
            .per_tool
            .iter()
            .find(|t| t.tool == "smart_search")
            .expect("smart_search present");
        assert_eq!(smart_search.calls, 2);
        assert_eq!(smart_search.errors, 1);

        let find_dependents = summary
            .per_tool
            .iter()
            .find(|t| t.tool == "find_dependents")
            .expect("find_dependents present");
        assert_eq!(find_dependents.calls, 1);
        assert_eq!(find_dependents.errors, 0);

        // src/main.rs was accessed twice across the two SUCCESS entries.
        assert_eq!(
            summary
                .top_targets
                .iter()
                .find(|(t, _)| t == "src/main.rs")
                .map(|(_, c)| *c),
            Some(2)
        );
    }

    #[test]
    fn test_run_reports_counts_from_seeded_audit_db() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("stats_audit.db");

        let logger = AuditLogger::new(Some(db_file.clone())).expect("init logger");
        for _ in 0..3 {
            logger
                .record_entry(
                    "sess-1",
                    None,
                    "smart_search",
                    "{}",
                    "SUCCESS",
                    vec!["src/main.rs".to_string()],
                    0,
                )
                .expect("seed smart_search entry");
        }
        logger
            .record_entry("sess-1", None, "find_dependents", "{}", "ERROR", vec![], 0)
            .expect("seed find_dependents entry");

        let entries = AuditLogger::read_entries(&db_file, None).expect("read seeded entries");
        assert_eq!(entries.len(), 4);

        let summary = StatsCommand::summarize(&entries, &AuditMetrics::default());
        assert_eq!(summary.total_calls, 4);
        assert_eq!(
            summary
                .per_tool
                .iter()
                .find(|t| t.tool == "smart_search")
                .map(|t| t.calls),
            Some(3)
        );
        assert_eq!(
            summary
                .per_tool
                .iter()
                .find(|t| t.tool == "find_dependents")
                .map(|t| t.errors),
            Some(1)
        );

        // `run` must succeed end-to-end against the same seeded db (`--since all`
        // to avoid depending on wall-clock timing in CI).
        StatsCommand::run(Some(&db_file), Some("all"), None, false)
            .expect("stats run should succeed");
    }

    #[test]
    fn test_percentile_nearest_rank_edge_cases() {
        // 0 samples: no percentile, never a fabricated 0.
        assert_eq!(percentile_nearest_rank(&[], 50), None);
        assert_eq!(percentile_nearest_rank(&[], 95), None);
        // 1 sample: every percentile is that sample.
        assert_eq!(percentile_nearest_rank(&[7], 50), Some(7));
        assert_eq!(percentile_nearest_rank(&[7], 95), Some(7));
        // 2 samples: p50 -> rank ceil(1.0) = 1, p95 -> rank ceil(1.9) = 2.
        assert_eq!(percentile_nearest_rank(&[3, 9], 50), Some(3));
        assert_eq!(percentile_nearest_rank(&[3, 9], 95), Some(9));
        // 20 samples 1..=20: p50 -> rank 10, p95 -> rank 19 (not interpolated).
        let twenty: Vec<u64> = (1..=20).collect();
        assert_eq!(percentile_nearest_rank(&twenty, 50), Some(10));
        assert_eq!(percentile_nearest_rank(&twenty, 95), Some(19));
        assert_eq!(percentile_nearest_rank(&twenty, 100), Some(20));
        assert_eq!(percentile_nearest_rank(&twenty, 0), Some(1));
    }

    /// Plan 4 step 4.8 exit criterion: `stats` on a fixture audit db with known values.
    #[test]
    fn test_stats_on_fixture_audit_db_with_known_values() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("fixture_audit.db");
        let logger = AuditLogger::new(Some(db_file.clone())).expect("init logger");

        // smart_search: 10 successful calls timed 1..=10 ms, recorded out of order.
        for ms in [7u64, 2, 9, 4, 1, 10, 3, 8, 6, 5] {
            let e = logger
                .record_entry("s", None, "smart_search", "{}", "SUCCESS", vec![], 0)
                .expect("seed smart_search");
            logger
                .record_tool_latency(e.entry_seq, ms * 1000)
                .expect("seed latency");
        }
        // find_dependents: 2 calls (1 isError), 300 us and 900 us.
        let e = logger
            .record_entry("s", None, "find_dependents", "{}", "ERROR", vec![], 0)
            .expect("seed error");
        logger.record_tool_latency(e.entry_seq, 300).expect("lat");
        let e = logger
            .record_entry("s", None, "find_dependents", "{}", "SUCCESS", vec![], 0)
            .expect("seed ok");
        logger.record_tool_latency(e.entry_seq, 900).expect("lat");
        // analyze_grpc: 1 call audited but never timed (e.g. written by an older build).
        logger
            .record_entry("s", None, "analyze_grpc", "{}", "SUCCESS", vec![], 0)
            .expect("seed untimed");

        // Two indexing passes: (30 hits, 10 misses) + (5 hits, 5 misses, 1 error).
        logger.record_index_cache_pass(30, 10, 0).expect("pass 1");
        logger.record_index_cache_pass(5, 5, 1).expect("pass 2");

        // meshd: 3 starts on /ws/a, 1 on /ws/b; a standalone mesh-mcp start is not a daemon.
        for _ in 0..3 {
            logger
                .record_process_start("meshd", "/ws/a")
                .expect("start");
        }
        logger
            .record_process_start("meshd", "/ws/b")
            .expect("start");
        logger
            .record_process_start("mesh-mcp", "/ws/a")
            .expect("start");

        let entries = AuditLogger::read_entries(&db_file, None).expect("entries");
        let metrics = AuditLogger::read_metrics(&db_file, None).expect("metrics");
        let summary = StatsCommand::summarize(&entries, &metrics);

        assert_eq!(summary.total_calls, 13);
        assert_eq!(summary.total_errors, 1);
        assert!(summary.latency_recorded);

        let tool = |name: &str| {
            summary
                .per_tool
                .iter()
                .find(|t| t.tool == name)
                .expect("tool present")
        };
        let ss = tool("smart_search");
        assert_eq!((ss.calls, ss.errors, ss.latency_samples), (10, 0, 10));
        // nearest rank: p50 -> rank 5 -> 5 ms; p95 withheld: 10 < MIN_P95_SAMPLES.
        assert_eq!(ss.p50_us, Some(5_000));
        assert_eq!(ss.p95_us, None);

        let fd = tool("find_dependents");
        assert_eq!((fd.calls, fd.errors, fd.latency_samples), (2, 1, 2));
        assert_eq!(fd.p50_us, Some(300));
        assert_eq!(fd.p95_us, None);

        let ag = tool("analyze_grpc");
        assert_eq!((ag.calls, ag.latency_samples), (1, 0));
        assert_eq!((ag.p50_us, ag.p95_us), (None, None));

        assert_eq!(
            summary.index_cache,
            Some(mesh_core::IndexCacheTotals {
                passes: 2,
                hits: 35,
                misses: 15,
                errors: 1
            })
        );
        assert_eq!(summary.daemon_restarts(), Some(2));

        let out = StatsCommand::render(&db_file, "all", &summary);
        assert!(out.contains("isError rate: 1/13 (7.7%)"), "{out}");
        assert!(out.contains("5.000ms"), "{out}");
        assert!(out.contains("0.300ms"), "{out}");
        assert!(
            out.contains("p95 shown as '-' below 20 timed calls"),
            "{out}"
        );
        assert!(out.contains("Sessions: 1"), "{out}");
        assert!(out.contains("50.0%"), "{out}");
        assert!(
            out.contains(
                "Index cache: hit rate 70.0% (35 hits / 50 lookups over 2 indexing passes"
            ),
            "{out}"
        );
        assert!(
            out.contains("Daemon: 2 restarts (4 meshd starts over 2 workspaces"),
            "{out}"
        );
        assert!(!out.to_lowercase().contains("tokens saved:"), "{out}");
        eprint!("{out}");
    }

    #[test]
    fn test_p95_is_reported_from_min_samples_on() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("p95.db");
        let logger = AuditLogger::new(Some(db_file.clone())).expect("init logger");
        for ms in 1..=20u64 {
            let e = logger
                .record_entry("s", None, "smart_search", "{}", "SUCCESS", vec![], 0)
                .expect("seed");
            logger
                .record_tool_latency(e.entry_seq, ms * 1000)
                .expect("lat");
        }
        let entries = AuditLogger::read_entries(&db_file, None).expect("entries");
        let metrics = AuditLogger::read_metrics(&db_file, None).expect("metrics");
        let summary = StatsCommand::summarize(&entries, &metrics);
        // nearest rank over 1..=20 ms: p95 -> rank 19.
        assert_eq!(summary.per_tool[0].p95_us, Some(19_000));
    }

    /// 7.0.0 pilot feedback: every call was audited as `active-session`, so runs
    /// could not be separated. Sessions are now counted, and `--session` keeps one
    /// session's calls and latencies only.
    #[test]
    fn test_sessions_are_counted_and_filterable() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let db_file = temp_dir.path().join("sessions.db");
        let logger = AuditLogger::new(Some(db_file.clone())).expect("init logger");
        for (session, status, us) in [
            ("run-a", "SUCCESS", 100u64),
            ("run-a", "ERROR", 200),
            ("run-b", "SUCCESS", 900),
        ] {
            let e = logger
                .record_entry(session, None, "smart_search", "{}", status, vec![], 0)
                .expect("seed");
            logger.record_tool_latency(e.entry_seq, us).expect("lat");
        }

        let entries = AuditLogger::read_entries(&db_file, None).expect("entries");
        let metrics = AuditLogger::read_metrics(&db_file, None).expect("metrics");
        let summary = StatsCommand::summarize(&entries, &metrics);
        let sessions: Vec<(&str, usize, usize)> = summary
            .sessions
            .iter()
            .map(|s| (s.session_id.as_str(), s.calls, s.errors))
            .collect();
        assert_eq!(sessions, [("run-a", 2, 1), ("run-b", 1, 0)]);

        let only_b =
            AuditLogger::read_metrics_for_session(&db_file, None, Some("run-b")).expect("metrics");
        assert_eq!(
            only_b.tool_latencies_us,
            Some(vec![("smart_search".to_string(), 900)])
        );

        let json = serde_json::to_value(&summary).expect("json");
        assert_eq!(json["sessions"][0]["session_id"], "run-a");
        assert_eq!(json["per_tool"][0]["calls"], 3);

        StatsCommand::run(Some(&db_file), Some("all"), Some("run-b"), true).expect("json run");
    }

    #[test]
    fn test_compact_workspace() {
        assert_eq!(compact_workspace("/ws/a"), "/ws/a");
        assert_eq!(
            compact_workspace("/ws/a;/ws/b;/ws/c;/ws/d;/ws/e;/ws/f"),
            "/ws/{a, b, c, d, … (+2)} (6 roots)"
        );
        assert_eq!(compact_workspace("/x/a;/y/b"), "/x/a;/y/b");
        assert_eq!(
            compact_workspace("/r/crates/a;/r/crates/b;/r/docs"),
            "/r/{crates/a, crates/b, docs} (3 roots)"
        );
    }

    #[test]
    fn test_render_declares_unrecorded_metrics_instead_of_zero() {
        let summary = StatsCommand::summarize(&[], &AuditMetrics::default());
        let out = StatsCommand::render(Path::new("x.db"), "7d", &summary);
        assert!(out.contains("latency not recorded"), "{out}");
        assert!(out.contains("Index cache: not recorded"), "{out}");
        assert!(out.contains("Daemon: restarts not recorded"), "{out}");
        assert!(out.contains("isError rate: 0/0 (n/a)"), "{out}");
    }

    #[test]
    fn test_parse_since_variants() {
        assert!(StatsCommand::parse_since("all")
            .expect("all parses")
            .is_none());
        assert!(StatsCommand::parse_since("7d")
            .expect("7d parses")
            .is_some());
        assert!(StatsCommand::parse_since("24h")
            .expect("24h parses")
            .is_some());
        assert!(StatsCommand::parse_since("bogus").is_err());
        assert!(StatsCommand::parse_since("").is_err());
        // Multi-byte units and lone multi-byte input are errors, never panics.
        assert!(StatsCommand::parse_since("7é").is_err());
        assert!(StatsCommand::parse_since("é").is_err());
        assert!(StatsCommand::parse_since("7日").is_err());
    }
}
