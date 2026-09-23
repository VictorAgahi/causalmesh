use mesh_core::{AuditEntry, AuditLogger};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-tool call/error counts, sorted by call volume for display.
pub struct ToolStats {
    pub tool: String,
    pub calls: usize,
    pub errors: usize,
}

/// Local usage summary computed from the audit log (RFC-001 item 15). Nothing
/// here leaves the machine: it is derived purely from an already-local SQLite
/// file, read read-only.
pub struct StatsSummary {
    pub total_calls: usize,
    pub per_tool: Vec<ToolStats>,
    pub top_targets: Vec<(String, usize)>,
}

pub struct StatsCommand;

impl StatsCommand {
    /// Reads `audit.db` and prints a local usage summary to stderr — stdout stays
    /// reserved for JSON-RPC framing (Commandment 3), matching `doctor`/`hooks`.
    /// `db_path` overrides the default `~/.cache/mesh-mcp/audit.db` location
    /// (used by tests); `since` accepts `<N>s|m|h|d` or `all` (default: `7d`).
    pub fn run(
        db_path: Option<&Path>,
        since: Option<&str>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let path: PathBuf = db_path
            .map(PathBuf::from)
            .unwrap_or_else(AuditLogger::default_db_path);

        let since_label = since.unwrap_or("7d");
        let cutoff = Self::parse_since(since_label)?;

        let entries = AuditLogger::read_entries(&path, cutoff)?;

        if entries.is_empty() {
            eprintln!(
                "ℹ No audit entries found in {} (window: {since_label})",
                path.display()
            );
            return Ok(());
        }

        let summary = Self::summarize(&entries);
        Self::print(&path, since_label, &summary);

        Ok(())
    }

    /// Pure aggregation step, kept separate from I/O so it is directly testable.
    pub fn summarize(entries: &[AuditEntry]) -> StatsSummary {
        let mut per_tool: HashMap<String, (usize, usize)> = HashMap::new();
        let mut per_target: HashMap<String, usize> = HashMap::new();

        for entry in entries {
            let counter = per_tool.entry(entry.tool.clone()).or_insert((0, 0));
            counter.0 += 1;
            if entry.status != "SUCCESS" {
                counter.1 += 1;
            }
            for file in &entry.files_accessed {
                *per_target.entry(file.clone()).or_insert(0) += 1;
            }
        }

        let mut per_tool: Vec<ToolStats> = per_tool
            .into_iter()
            .map(|(tool, (calls, errors))| ToolStats {
                tool,
                calls,
                errors,
            })
            .collect();
        per_tool.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.tool.cmp(&b.tool)));

        let mut top_targets: Vec<(String, usize)> = per_target.into_iter().collect();
        top_targets.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top_targets.truncate(10);

        StatsSummary {
            total_calls: entries.len(),
            per_tool,
            top_targets,
        }
    }

    fn print(db_path: &Path, since_label: &str, summary: &StatsSummary) {
        eprintln!(
            "MeshMCP local audit stats — {} ({} calls, window: {since_label})",
            db_path.display(),
            summary.total_calls
        );
        eprintln!();
        eprintln!("Calls per tool:");
        for t in &summary.per_tool {
            let error_rate = if t.calls > 0 {
                (t.errors as f64 / t.calls as f64) * 100.0
            } else {
                0.0
            };
            eprintln!(
                "  {:<20} calls={:<6} errors={:<5} ({error_rate:.1}%)",
                t.tool, t.calls, t.errors
            );
        }

        eprintln!();
        eprintln!(
            "Most-queried scopes/targets (top {}, by files_accessed):",
            summary.top_targets.len()
        );
        if summary.top_targets.is_empty() {
            eprintln!("  (no files_accessed recorded in this window)");
        } else {
            for (target, count) in &summary.top_targets {
                eprintln!("  {count:<6} {target}");
            }
        }

        eprintln!();
        eprintln!(
            "Note: audit_entries stores no per-call result-size/result-count field, so a true \
             empty-result rate cannot be computed from this data today. The error rate above and \
             the scope/target frequency are the available proxies until such a field is added."
        );
        eprintln!("Nothing above leaves this machine — this is a local, read-only SQLite scan.");
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

        let (num_part, unit) = spec.split_at(spec.len() - 1);
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

        let summary = StatsCommand::summarize(&entries);
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

        let summary = StatsCommand::summarize(&entries);
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
        StatsCommand::run(Some(&db_file), Some("all")).expect("stats run should succeed");
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
    }
}
