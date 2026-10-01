//! Skill-level verification — runs success criteria after skill execution.
//!
//! Uses the shared typed verification boundary without owning a second executor.

use std::path::PathBuf;
use std::sync::Arc;

use astra_services::{VerificationCriterion, VerificationResult, VerificationRunner, VerifierKind};

use crate::manifest::SkillManifest;

/// Runs verification criteria declared in a skill manifest.
///
/// Reuses the shared verification runner and its typed verifier kinds.
pub struct SkillVerifier {
    runner: VerificationRunner,
}

impl SkillVerifier {
    /// Create a verifier for the given working directory.
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            runner: VerificationRunner::new(work_dir),
        }
    }

    /// Create a verifier with LLM judge support for semantic checks.
    pub fn with_llm_judge(work_dir: PathBuf, judge: Arc<dyn astra_services::LlmJudge>) -> Self {
        Self {
            runner: VerificationRunner::with_llm_judge(work_dir, judge),
        }
    }

    /// Run all success criteria declared in the skill manifest.
    ///
    /// Returns `(all_required_passed, results)`.
    /// If the manifest has no criteria, returns `(true, [])`.
    pub async fn verify(&self, manifest: &SkillManifest) -> (bool, Vec<VerificationResult>) {
        if manifest.success_criteria.is_empty() {
            return (true, Vec::new());
        }
        // Convert serde_json::Value to VerificationCriterion
        let criteria: Vec<VerificationCriterion> = manifest
            .success_criteria
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        self.verify_criteria(&criteria).await
    }

    /// Verify a read-only child without opening a second shell execution path.
    ///
    /// Filesystem observers and an already-authorized LLM judge remain
    /// available. Any command-backed criterion, including one nested inside a
    /// composite, is reported as unsuccessful instead of being executed.
    pub async fn verify_read_only(
        &self,
        manifest: &SkillManifest,
    ) -> (bool, Vec<VerificationResult>) {
        if manifest.success_criteria.is_empty() {
            return (true, Vec::new());
        }
        let criteria: Vec<VerificationCriterion> = manifest
            .success_criteria
            .iter()
            .filter_map(|v| serde_json::from_value(v.clone()).ok())
            .collect();
        self.verify_criteria_read_only(&criteria).await
    }

    /// Run a specific set of criteria.
    ///
    /// Returns `(all_required_passed, results)`.
    pub async fn verify_criteria(
        &self,
        criteria: &[VerificationCriterion],
    ) -> (bool, Vec<VerificationResult>) {
        self.verify_criteria_with_policy(criteria, false).await
    }

    /// Read-only counterpart of [`Self::verify_criteria`].
    pub async fn verify_criteria_read_only(
        &self,
        criteria: &[VerificationCriterion],
    ) -> (bool, Vec<VerificationResult>) {
        self.verify_criteria_with_policy(criteria, true).await
    }

    async fn verify_criteria_with_policy(
        &self,
        criteria: &[VerificationCriterion],
        read_only: bool,
    ) -> (bool, Vec<VerificationResult>) {
        let mut results = Vec::with_capacity(criteria.len());

        for criterion in criteria {
            if read_only && verifier_requires_shell(&criterion.verifier) {
                results.push(VerificationResult {
                    criterion_id: criterion.id.clone(),
                    passed: false,
                    evidence: String::new(),
                    expected: "verification without shell execution".to_string(),
                    duration_ms: 0,
                    error: Some(
                        "command-backed verification is unavailable in a read-only child"
                            .to_string(),
                    ),
                });
                continue;
            }
            // Skip LlmJudge if no judge is configured
            if matches!(criterion.verifier, VerifierKind::LlmJudge { .. })
                && self.runner.llm_judge.is_none()
            {
                results.push(VerificationResult {
                    criterion_id: criterion.id.clone(),
                    passed: !criterion.required, // skip advisory, fail required
                    evidence: String::new(),
                    expected: "LLM judge evaluation".to_string(),
                    duration_ms: 0,
                    error: Some("LLM judge not configured, skipped".to_string()),
                });
                continue;
            }
            results.push(self.runner.run_criterion(criterion).await);
        }

        let all_required_passed = criteria
            .iter()
            .zip(results.iter())
            .all(|(c, r)| !c.required || r.passed);

        (all_required_passed, results)
    }
}

fn verifier_requires_shell(verifier: &VerifierKind) -> bool {
    match verifier {
        VerifierKind::Command { .. }
        | VerifierKind::CommandOutput { .. }
        | VerifierKind::BuildPass { .. }
        | VerifierKind::TestPass { .. } => true,
        VerifierKind::Composite { criteria, .. } => criteria
            .iter()
            .any(|criterion| verifier_requires_shell(&criterion.verifier)),
        VerifierKind::FileExists { .. }
        | VerifierKind::GrepCheck { .. }
        | VerifierKind::ReadFileContains { .. }
        | VerifierKind::LlmJudge { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    /// Helper to convert VerificationCriterion to serde_json::Value for tests.
    fn criterion_to_value(c: VerificationCriterion) -> serde_json::Value {
        serde_json::to_value(c).unwrap()
    }

    #[tokio::test]
    async fn test_empty_criteria_passes() {
        let manifest = SkillManifest::default();
        let verifier = SkillVerifier::new(PathBuf::from("/tmp"));
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed);
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_file_exists_criterion() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("output.txt");
        std::fs::File::create(&file_path)
            .unwrap()
            .write_all(b"hello")
            .unwrap();

        let mut manifest = SkillManifest::default();
        manifest
            .success_criteria
            .push(criterion_to_value(VerificationCriterion {
                id: "output-exists".to_string(),
                description: "Output file must exist".to_string(),
                verifier: VerifierKind::FileExists {
                    paths: vec![file_path.to_string_lossy().to_string()],
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            }));

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed);
        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
    }

    #[tokio::test]
    async fn test_required_criterion_fails() {
        let dir = TempDir::new().unwrap();

        let mut manifest = SkillManifest::default();
        manifest
            .success_criteria
            .push(criterion_to_value(VerificationCriterion {
                id: "missing-file".to_string(),
                description: "File must exist".to_string(),
                verifier: VerifierKind::FileExists {
                    paths: vec![
                        dir.path()
                            .join("nonexistent.txt")
                            .to_string_lossy()
                            .to_string(),
                    ],
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            }));

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(!passed);
        assert!(!results[0].passed);
    }

    #[tokio::test]
    async fn test_advisory_criterion_doesnt_block() {
        let dir = TempDir::new().unwrap();

        let mut manifest = SkillManifest::default();
        manifest
            .success_criteria
            .push(criterion_to_value(VerificationCriterion {
                id: "advisory-check".to_string(),
                description: "Nice to have".to_string(),
                verifier: VerifierKind::FileExists {
                    paths: vec![
                        dir.path()
                            .join("optional.txt")
                            .to_string_lossy()
                            .to_string(),
                    ],
                },
                required: false, // advisory
                timeout_sec: 10,
                global_only: false,
            }));

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed); // advisory failure doesn't block
        assert!(!results[0].passed); // but it's still recorded as failed
    }

    #[tokio::test]
    async fn test_command_output_criterion() {
        let dir = TempDir::new().unwrap();

        let mut manifest = SkillManifest::default();
        manifest
            .success_criteria
            .push(criterion_to_value(VerificationCriterion {
                id: "echo-check".to_string(),
                description: "Echo contains expected text".to_string(),
                verifier: VerifierKind::CommandOutput {
                    cmd: "echo 'all tests passed'".to_string(),
                    contains: vec!["tests passed".to_string()],
                    not_contains: vec![],
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            }));

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify(&manifest).await;
        assert!(passed);
        assert!(results[0].passed);
    }

    #[tokio::test]
    async fn read_only_verification_blocks_shell_criteria_without_running_them() {
        let dir = TempDir::new().unwrap();
        let marker = dir.path().join("marker");
        let mut manifest = SkillManifest::default();
        manifest
            .success_criteria
            .push(criterion_to_value(VerificationCriterion {
                id: "command-check".to_string(),
                description: "Command must pass".to_string(),
                verifier: VerifierKind::Command {
                    cmd: format!("touch {}", marker.display()),
                    expected_exit: 0,
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            }));

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify_read_only(&manifest).await;
        assert!(!passed);
        assert!(!results[0].passed);
        assert!(
            results[0]
                .error
                .as_deref()
                .is_some_and(|error| error.contains("read-only"))
        );
        assert!(!marker.exists());
    }

    #[tokio::test]
    async fn read_only_verification_keeps_typed_file_observers() {
        let dir = TempDir::new().unwrap();
        let file_path = dir.path().join("output.txt");
        std::fs::write(&file_path, "ready").unwrap();
        let mut manifest = SkillManifest::default();
        manifest
            .success_criteria
            .push(criterion_to_value(VerificationCriterion {
                id: "output-exists".to_string(),
                description: "Output file must exist".to_string(),
                verifier: VerifierKind::FileExists {
                    paths: vec![file_path.to_string_lossy().to_string()],
                },
                required: true,
                timeout_sec: 10,
                global_only: false,
            }));

        let verifier = SkillVerifier::new(dir.path().to_path_buf());
        let (passed, results) = verifier.verify_read_only(&manifest).await;
        assert!(passed);
        assert!(results[0].passed);
    }
}
