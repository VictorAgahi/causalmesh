use crate::config::ReadGovernanceMode;
use crate::types::CompactStr;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RsahWorkflow {
    pub step_1: String,
    pub step_2: String,
    pub step_3: String,
    pub step_4: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RsahResponse {
    pub status: String,
    pub policy: String,
    pub violation: String,
    pub required_workflow: RsahWorkflow,
    pub agent_next_action: String,
    pub message_to_user: String,
}

#[derive(Debug, Clone, Default)]
pub struct GovernanceEngine {
    stop_rules: HashMap<CompactStr, String>,
    skills: HashMap<CompactStr, String>,
    read_governance_mode: ReadGovernanceMode,
}

impl GovernanceEngine {
    pub fn new(
        stop_rules: HashMap<CompactStr, String>,
        skills: HashMap<CompactStr, String>,
        read_governance_mode: ReadGovernanceMode,
    ) -> Self {
        Self {
            stop_rules,
            skills,
            read_governance_mode,
        }
    }

    #[inline]
    pub fn read_governance_mode(&self) -> ReadGovernanceMode {
        self.read_governance_mode
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.stop_rules.is_empty()
    }

    #[inline]
    pub fn get_skill_path(&self, tool_or_key: &str) -> Option<&str> {
        self.skills.get(tool_or_key).map(|s| s.as_str())
    }

    /// Every configured `key = skill path` pair, for validation (`mesh-mcp doctor`).
    pub fn skills(&self) -> impl Iterator<Item = (&str, &str)> {
        self.skills.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }

    /// Picks the playbook to point the agent at for this call.
    ///
    /// Keys are matched exactly against the MCP tool name first (`smart_search`,
    /// `analyze_grpc`, ...), then as a case-insensitive substring of the call's
    /// subject — the scope or target the agent asked about — so
    /// `"proto-registry" = "..."` fires for any query touching that area, exactly
    /// like `[engines.policy.stop_rules]`.
    pub fn recommend_skill(&self, tool: &str, subject: Option<&str>) -> Option<&str> {
        if let Some(path) = self.get_skill_path(tool) {
            return Some(path);
        }

        let subject = subject?.to_lowercase();
        // Longest key wins, so a specific rule beats a generic one regardless of
        // HashMap iteration order.
        self.skills
            .iter()
            .filter(|(key, _)| subject.contains(key.to_lowercase().as_str()))
            .max_by_key(|(key, _)| key.len())
            .map(|(_, path)| path.as_str())
    }

    /// Evaluates if a given scope or target triggers an architectural STOP rule.
    ///
    /// Matches `guarded_key` as a whole path SEGMENT of `target_or_scope`, not a
    /// bare substring: `init --auto` now emits short, generic guarded directory
    /// names (`"proto"`, `"k8s"`, `"deploy"`, agnostic to any specific repo's
    /// naming convention — see `cli::init`), and an unanchored substring match
    /// on a word that short would block any path merely *containing* it
    /// (`src/protobuf_helpers.py`, `internal/prototype/test.go`,
    /// `cmd/redeploy/main.go`) rather than only paths actually inside the
    /// guarded directory.
    pub fn evaluate_guard(&self, target_or_scope: &str) -> Option<RsahResponse> {
        let lower = target_or_scope.to_lowercase();

        for (guarded_key, rule_description) in &self.stop_rules {
            if Self::has_path_segment(&lower, guarded_key.to_lowercase().as_str()) {
                return Some(Self::build_rsah_response(
                    guarded_key.as_str(),
                    rule_description,
                ));
            }
        }

        None
    }

    /// True if `segment` appears as a whole `/`-delimited path component of
    /// `haystack` — not merely as a substring straddling a component boundary
    /// (e.g. `"proto"` must match `"proto-registry/x.proto"` and
    /// `"src/proto/x"`, but not `"internal/prototype/test.go"`).
    fn has_path_segment(haystack: &str, segment: &str) -> bool {
        if segment.is_empty() {
            return false;
        }
        haystack.split('/').any(|part| part == segment)
    }

    fn build_rsah_response(guarded_key: &str, rule_description: &str) -> RsahResponse {
        let lower_key = guarded_key.to_lowercase();
        if lower_key.contains("proto") {
            RsahResponse {
                status: "GOVERNANCE_BLOCKED".to_string(),
                policy: "CONTRACT_FIRST_CASCADE_CI".to_string(),
                violation: format!("Guarded contract boundary '{guarded_key}' accessed. {rule_description}"),
                required_workflow: RsahWorkflow {
                    step_1: format!("Validate schema syntax and breaking changes in '{guarded_key}'"),
                    step_2: format!("Commit changes exclusively inside '{guarded_key}'"),
                    step_3: format!("Submit contract review and verify CI schema generation for '{guarded_key}'"),
                    step_4: "Do not modify downstream services until the generated contract packages are published.".to_string(),
                },
                agent_next_action: "STOP_AND_REPORT_TO_USER".to_string(),
                message_to_user: format!("Detected mutation targeting contract in '{guarded_key}'. Per active architecture governance: submit contract changes and await CI verification before adapting downstream consumers. {rule_description}"),
            }
        } else if lower_key.contains("k8s")
            || lower_key.contains("infra")
            || lower_key.contains("deploy")
        {
            RsahResponse {
                status: "GOVERNANCE_BLOCKED".to_string(),
                policy: "INFRASTRUCTURE_AS_CODE_REVIEW".to_string(),
                violation: format!("Guarded infrastructure boundary '{guarded_key}' accessed. {rule_description}"),
                required_workflow: RsahWorkflow {
                    step_1: format!("Review infrastructure resource specifications in '{guarded_key}'"),
                    step_2: "Run manifest linter and policy compliance checks".to_string(),
                    step_3: "Submit changes to infrastructure review board".to_string(),
                    step_4: "Await infrastructure deployment synchronization".to_string(),
                },
                agent_next_action: "STOP_AND_REPORT_TO_USER".to_string(),
                message_to_user: format!("Modifying infrastructure in '{guarded_key}' requires mandatory operational review. {rule_description}"),
            }
        } else {
            RsahResponse {
                status: "GOVERNANCE_BLOCKED".to_string(),
                policy: "ACTIVE_GOVERNANCE_POLICY".to_string(),
                violation: format!(
                    "Action blocked by active rule for '{guarded_key}': {rule_description}"
                ),
                required_workflow: RsahWorkflow {
                    step_1: format!("Verify policy for '{guarded_key}'"),
                    step_2: "Obtain human confirmation".to_string(),
                    step_3: "Apply isolated modifications".to_string(),
                    step_4: "Verify downstream dependencies".to_string(),
                },
                agent_next_action: "STOP_AND_REPORT_TO_USER".to_string(),
                message_to_user: format!(
                    "This action requires human validation per the active rule: {rule_description}"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rsah_trigger() {
        let mut rules = HashMap::new();
        rules.insert(
            CompactStr::new("proto-registry"),
            "Ne touche pas aux microservices avant CI".to_string(),
        );

        let engine = GovernanceEngine::new(rules, HashMap::new(), ReadGovernanceMode::AllowAll);
        let result = engine.evaluate_guard("services/proto-registry/auth.proto");
        assert!(result.is_some());

        let rsah = result.unwrap();
        assert_eq!(rsah.status, "GOVERNANCE_BLOCKED");
        assert_eq!(rsah.policy, "CONTRACT_FIRST_CASCADE_CI");
        assert_eq!(rsah.agent_next_action, "STOP_AND_REPORT_TO_USER");
    }

    #[test]
    fn test_skill_recommendation_by_tool_then_subject() {
        let mut skills = HashMap::new();
        skills.insert(
            CompactStr::new("smart_search"),
            ".agents/skills/search.md".to_string(),
        );
        skills.insert(
            CompactStr::new("proto-registry"),
            ".agents/skills/proto.md".to_string(),
        );
        let engine = GovernanceEngine::new(HashMap::new(), skills, ReadGovernanceMode::AllowAll);

        // Exact tool-name key wins over any subject match.
        assert_eq!(
            engine.recommend_skill("smart_search", Some("/w/proto-registry")),
            Some(".agents/skills/search.md")
        );
        // Otherwise the subject is matched like a stop rule.
        assert_eq!(
            engine.recommend_skill("analyze_grpc", Some("/w/Proto-Registry/auth.proto")),
            Some(".agents/skills/proto.md")
        );
        assert_eq!(
            engine.recommend_skill("analyze_grpc", Some("/w/billing")),
            None
        );
        assert_eq!(engine.recommend_skill("analyze_grpc", None), None);
    }

    #[test]
    fn test_rsah_clean_scope() {
        let mut rules = HashMap::new();
        rules.insert(
            CompactStr::new("proto-registry"),
            "Stop Cascade CI".to_string(),
        );

        let engine = GovernanceEngine::new(rules, HashMap::new(), ReadGovernanceMode::AllowAll);
        let result = engine.evaluate_guard("services/billing/BillingController.java");
        assert!(result.is_none());
    }

    #[test]
    fn test_evaluate_guard_case_insensitivity_and_generic_fallback() {
        let mut rules = HashMap::new();
        rules.insert(CompactStr::new("Proto-Registry"), "Proto check".to_string());
        rules.insert(
            CompactStr::new("Billing-DB"),
            "Database migrations require DBA approval".to_string(),
        );

        let engine = GovernanceEngine::new(rules, HashMap::new(), ReadGovernanceMode::AllowAll);

        // Lowercase path matches uppercase rule key
        let r1 = engine
            .evaluate_guard("services/proto-registry/auth.proto")
            .expect("proto match");
        assert_eq!(r1.policy, "CONTRACT_FIRST_CASCADE_CI");

        // Generic rule fallback
        let r2 = engine
            .evaluate_guard("services/billing-db/migration.sql")
            .expect("db match");
        assert_eq!(r2.policy, "ACTIVE_GOVERNANCE_POLICY");
        assert_eq!(r2.status, "GOVERNANCE_BLOCKED");
    }

    /// Regression test for the ultrareview finding on PR #6: `init --auto`
    /// emits short, generic guarded keys (`"proto"`, `"k8s"`, `"deploy"`,
    /// agnostic to any specific repo's directory-naming convention) —
    /// `evaluate_guard` must not treat these as bare substrings, or any path
    /// merely *containing* the word (not inside the guarded directory at
    /// all) gets a fabricated GOVERNANCE_BLOCKED refusal.
    #[test]
    fn evaluate_guard_matches_a_whole_path_segment_not_a_bare_substring() {
        let mut rules = HashMap::new();
        rules.insert(CompactStr::new("proto"), "Contract boundary".to_string());

        let engine = GovernanceEngine::new(rules, HashMap::new(), ReadGovernanceMode::AllowAll);

        // Real matches: "proto" is a whole path segment.
        assert!(engine.evaluate_guard("proto/user.proto").is_some());
        assert!(engine.evaluate_guard("src/proto/billing.proto").is_some());

        // False positives the old bare-substring match produced — none of
        // these have "proto" as a whole path segment.
        assert!(
            engine.evaluate_guard("src/protobuf_helpers.py").is_none(),
            "protobuf_helpers.py must not trip the 'proto' guard"
        );
        assert!(
            engine
                .evaluate_guard("internal/prototype/test.go")
                .is_none(),
            "prototype/ must not trip the 'proto' guard"
        );
        assert!(
            engine.evaluate_guard("cmd/redeploy/main.go").is_none(),
            "redeploy/ must not trip a 'deploy' guard's substring"
        );
    }
}
