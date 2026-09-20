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
}

impl GovernanceEngine {
    pub fn new(
        stop_rules: HashMap<CompactStr, String>,
        skills: HashMap<CompactStr, String>,
    ) -> Self {
        Self { stop_rules, skills }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.stop_rules.is_empty()
    }

    #[inline]
    pub fn get_skill_path(&self, tool_or_key: &str) -> Option<&str> {
        self.skills.get(tool_or_key).map(|s| s.as_str())
    }

    /// Evaluates if a given scope or target triggers an architectural STOP rule
    pub fn evaluate_guard(&self, target_or_scope: &str) -> Option<RsahResponse> {
        let lower = target_or_scope.to_lowercase();

        for (guarded_key, rule_description) in &self.stop_rules {
            if lower.contains(guarded_key.as_str()) {
                return Some(Self::build_rsah_response(
                    guarded_key.as_str(),
                    rule_description,
                ));
            }
        }

        None
    }

    fn build_rsah_response(guarded_key: &str, rule_description: &str) -> RsahResponse {
        match guarded_key {
            "proto-registry" => RsahResponse {
                status: "GOVERNANCE_BLOCKED".to_string(),
                policy: "CONTRACT_FIRST_CASCADE_CI".to_string(),
                violation: format!("Guarded boundary '{}' accessed. {rule_description}", guarded_key),
                required_workflow: RsahWorkflow {
                    step_1: "Validate proto contract syntax via 'buf lint' in 'proto-registry'".to_string(),
                    step_2: "Commit changes exclusively inside 'proto-registry'".to_string(),
                    step_3: "Open a Pull Request on 'proto-registry' and await GitHub Actions CI stub generation".to_string(),
                    step_4: "DO NOT modify 'api-gateway' or 'services/*' until the published packages are available.".to_string(),
                },
                agent_next_action: "STOP_AND_REPORT_TO_USER".to_string(),
                message_to_user: "J'ai détecté un accès au contrat Protobuf dans 'proto-registry'. Conformément à la gouvernance d'architecture, je m'arrête ici : vous devez soumettre la PR du contrat pour que la CI génère les stubs avant d'adapter les microservices.".to_string(),
            },
            "k8s-infrastructure" => RsahResponse {
                status: "GOVERNANCE_BLOCKED".to_string(),
                policy: "INFRASTRUCTURE_AS_CODE_REVIEW".to_string(),
                violation: format!("Guarded infrastructure repository '{}' accessed. {rule_description}", guarded_key),
                required_workflow: RsahWorkflow {
                    step_1: "Review Kubernetes resource limits and security context".to_string(),
                    step_2: "Run 'helm lint' or 'kubeconform' on modified manifests".to_string(),
                    step_3: "Submit changes to DevOps review board".to_string(),
                    step_4: "Await ArgoCD / Flux deployment sync".to_string(),
                },
                agent_next_action: "STOP_AND_REPORT_TO_USER".to_string(),
                message_to_user: "Modification des manifests K8s sous revue DevOps obligatoire. Veuillez valider les manifests avec l'équipe Infrastructure avant déploiement.".to_string(),
            },
            _ => RsahResponse {
                status: "GOVERNANCE_BLOCKED".to_string(),
                policy: "ACTIVE_GOVERNANCE_POLICY".to_string(),
                violation: format!("Action blocked by active rule for '{guarded_key}': {rule_description}"),
                required_workflow: RsahWorkflow {
                    step_1: format!("Verify policy for '{guarded_key}'"),
                    step_2: "Obtain human confirmation".to_string(),
                    step_3: "Apply isolated modifications".to_string(),
                    step_4: "Verify downstream dependencies".to_string(),
                },
                agent_next_action: "STOP_AND_REPORT_TO_USER".to_string(),
                message_to_user: format!("Cette action nécessite une validation humaine conformément à la règle: {rule_description}"),
            },
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

        let engine = GovernanceEngine::new(rules, HashMap::new());
        let result = engine.evaluate_guard("services/proto-registry/auth.proto");
        assert!(result.is_some());

        let rsah = result.unwrap();
        assert_eq!(rsah.status, "GOVERNANCE_BLOCKED");
        assert_eq!(rsah.policy, "CONTRACT_FIRST_CASCADE_CI");
        assert_eq!(rsah.agent_next_action, "STOP_AND_REPORT_TO_USER");
    }

    #[test]
    fn test_rsah_clean_scope() {
        let mut rules = HashMap::new();
        rules.insert(
            CompactStr::new("proto-registry"),
            "Stop Cascade CI".to_string(),
        );

        let engine = GovernanceEngine::new(rules, HashMap::new());
        let result = engine.evaluate_guard("services/billing/BillingController.java");
        assert!(result.is_none());
    }
}
