//! Portable identity of an actually supported workspace confinement boundary.
//!
//! Providers must enumerate every readable immutable toolchain mount (including
//! its entire subtree) and retain those inputs for the execution lifetime. This
//! contract carries no host source paths and is not itself proof of isolation.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

pub const WORKSPACE_CONFINEMENT_PROFILE: &str = "linux_restricted_root_x86_64_v1";

/// Actual per-launch evidence. A configured profile alone is never a receipt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellExecutionEvidence {
    pub schema_version: u32,
    pub profile: String,
    pub execution_started: bool,
    pub setup: ShellSetupEvidence,
    pub settlement: ShellSettlementEvidence,
    pub timed_out: bool,
    pub cancelled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShellSetupEvidence {
    Verified { exit_code: i32 },
    Unverified { reason_code: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellSettlementEvidence {
    pub scope_settled: bool,
    pub ownership: Option<ShellScopeOwnership>,
    pub descendants_terminated: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShellScopeOwnership {
    InvocationCgroup,
    InvocationSupervisor,
    ForegroundProcessGroup,
}

impl ShellExecutionEvidence {
    /// A verifier outcome is usable only after verified setup and authoritative
    /// settlement. Preserve incomplete receipts for diagnosis, never score them.
    pub fn verified_exit_code(&self) -> Option<i32> {
        if self.schema_version != 1
            || self.profile != WORKSPACE_CONFINEMENT_PROFILE
            || !self.execution_started
            || !self.settlement.scope_settled
            || !matches!(
                self.settlement.ownership,
                Some(
                    ShellScopeOwnership::InvocationCgroup
                        | ShellScopeOwnership::InvocationSupervisor
                )
            )
            || self.timed_out
            || self.cancelled
        {
            return None;
        }
        match self.setup {
            ShellSetupEvidence::Verified { exit_code } => Some(exit_code),
            ShellSetupEvidence::Unverified { .. } => None,
        }
    }
}

/// Pure guest-path rules shared by the frozen contract and the Linux launcher.
pub fn validate_confined_toolchain_mount(path: &str) -> Result<(), String> {
    if !path.starts_with('/')
        || path.len() > 4096
        || path[1..].split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-+".contains(&byte))
        })
    {
        return Err("toolchain guest mount paths must be canonical absolute names".into());
    }
    if ["/", "/usr", "/opt"].contains(&path)
        || [
            "/workspace",
            "/home",
            "/tmp",
            "/proc",
            "/dev",
            "/sys",
            "/run",
            "/etc",
            "/root",
            "/var",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
        ]
        .iter()
        .any(|root| std::path::Path::new(path).starts_with(root))
    {
        return Err("ambient or reserved toolchain mount".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainInput {
    pub guest_mount_path: String,
    /// SHA256 identity of the complete immutable mounted content.
    pub content_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolchainManifest {
    pub schema_version: u32,
    pub inputs: Vec<ToolchainInput>,
    pub launcher_digest: String,
    pub supervisor_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceConfinementContract {
    pub profile_id: String,
    pub toolchain_manifest: ToolchainManifest,
}

impl<'de> Deserialize<'de> for WorkspaceConfinementContract {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            profile_id: String,
            toolchain_manifest: ToolchainManifest,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self {
            profile_id: wire.profile_id,
            toolchain_manifest: wire.toolchain_manifest,
        }
        .normalized()
        .map_err(serde::de::Error::custom)
    }
}

impl WorkspaceConfinementContract {
    pub fn validate(&self) -> Result<(), String> {
        if self.profile_id != WORKSPACE_CONFINEMENT_PROFILE {
            return Err("unsupported workspace confinement profile".into());
        }
        let manifest = &self.toolchain_manifest;
        if manifest.schema_version != 1 || manifest.inputs.is_empty() {
            return Err("confinement requires a version 1 complete toolchain manifest".into());
        }
        for digest in std::iter::once(&manifest.launcher_digest)
            .chain(std::iter::once(&manifest.supervisor_digest))
            .chain(manifest.inputs.iter().map(|input| &input.content_digest))
        {
            if !digest.strip_prefix("sha256:").is_some_and(|hex| {
                hex.len() == 64
                    && hex
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            }) {
                return Err(
                    "confinement content identities must be canonical SHA256 digests".into(),
                );
            }
        }
        for (index, input) in manifest.inputs.iter().enumerate() {
            let path = &input.guest_mount_path;
            validate_confined_toolchain_mount(path)?;
            for other in &manifest.inputs[..index] {
                let other = &other.guest_mount_path;
                if path == other
                    || path.starts_with(&format!("{other}/"))
                    || other.starts_with(&format!("{path}/"))
                {
                    return Err("toolchain guest mounts must not duplicate or overlap".into());
                }
            }
        }
        Ok(())
    }

    pub fn normalized(mut self) -> Result<Self, String> {
        self.validate()?;
        self.toolchain_manifest
            .inputs
            .sort_by(|a, b| a.guest_mount_path.cmp(&b.guest_mount_path));
        Ok(self)
    }

    pub fn fingerprint(&self) -> Result<String, String> {
        let value = serde_json::to_value(self.clone().normalized()?).map_err(|e| e.to_string())?;
        let digest = Sha256::digest(astra_core::canonical_json_string(&value).as_bytes());
        Ok(format!("sha256:{digest:x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifier_receipt_requires_setup_and_authoritative_settlement() {
        let receipt = ShellExecutionEvidence {
            schema_version: 1,
            profile: WORKSPACE_CONFINEMENT_PROFILE.into(),
            execution_started: true,
            setup: ShellSetupEvidence::Verified { exit_code: 7 },
            settlement: ShellSettlementEvidence {
                scope_settled: true,
                ownership: Some(ShellScopeOwnership::InvocationSupervisor),
                descendants_terminated: false,
            },
            timed_out: false,
            cancelled: false,
        };
        let persisted: ShellExecutionEvidence =
            serde_json::from_value(serde_json::to_value(&receipt).unwrap()).unwrap();
        assert_eq!(persisted.verified_exit_code(), Some(7));
        for mutation in 0..9 {
            let mut incomplete = receipt.clone();
            match mutation {
                0 => incomplete.schema_version = 2,
                1 => incomplete.profile = "unknown".into(),
                2 => incomplete.execution_started = false,
                3 => {
                    incomplete.setup = ShellSetupEvidence::Unverified {
                        reason_code: "setup_or_exec_unverified".into(),
                    }
                }
                4 => incomplete.settlement.scope_settled = false,
                5 => {
                    incomplete.settlement.ownership =
                        Some(ShellScopeOwnership::ForegroundProcessGroup)
                }
                6 => incomplete.settlement.ownership = None,
                7 => incomplete.timed_out = true,
                _ => incomplete.cancelled = true,
            }
            assert_eq!(incomplete.verified_exit_code(), None, "mutation {mutation}");
        }
        let mut missing = serde_json::to_value(&receipt).unwrap();
        missing["settlement"]
            .as_object_mut()
            .unwrap()
            .remove("scope_settled");
        assert!(serde_json::from_value::<ShellExecutionEvidence>(missing).is_err());
    }

    fn contract() -> WorkspaceConfinementContract {
        WorkspaceConfinementContract {
            profile_id: WORKSPACE_CONFINEMENT_PROFILE.into(),
            toolchain_manifest: ToolchainManifest {
                schema_version: 1,
                inputs: ["/usr/lib", "/usr/bin"]
                    .into_iter()
                    .map(|path| ToolchainInput {
                        guest_mount_path: path.into(),
                        content_digest: format!("sha256:{}", "a".repeat(64)),
                    })
                    .collect(),
                launcher_digest: format!("sha256:{}", "b".repeat(64)),
                supervisor_digest: format!("sha256:{}", "c".repeat(64)),
            },
        }
    }

    #[test]
    fn normalized_identity_and_strict_wire_validation() {
        let original = contract();
        let mut reordered = original.clone();
        reordered.toolchain_manifest.inputs.reverse();
        assert_eq!(original.fingerprint(), reordered.fingerprint());
        let decoded: WorkspaceConfinementContract =
            serde_json::from_value(serde_json::to_value(&original).unwrap()).unwrap();
        assert_eq!(decoded, original.clone().normalized().unwrap());
        for path in [
            "/",
            "/etc",
            "/sys",
            "/home/private",
            "/workspace/nested",
            "/tmp",
            "/opt",
            "usr/bin",
            "/usr//bin",
            "/usr/./bin",
            "/usr/../bin",
            "/usr/bin/",
            "/usr/bin",
            "/usr/lib/child",
            "/usr",
        ] {
            let mut invalid = original.clone();
            invalid.toolchain_manifest.inputs.push(ToolchainInput {
                guest_mount_path: path.into(),
                content_digest: format!("sha256:{}", "a".repeat(64)),
            });
            assert!(invalid.validate().is_err(), "{path}");
            assert!(
                serde_json::from_value::<WorkspaceConfinementContract>(
                    serde_json::to_value(invalid).unwrap()
                )
                .is_err()
            );
        }
        for changed_identity in 0..3 {
            let mut changed = original.clone();
            let digest = format!("sha256:{}", "d".repeat(64));
            match changed_identity {
                0 => changed.toolchain_manifest.inputs[0].content_digest = digest,
                1 => changed.toolchain_manifest.launcher_digest = digest,
                _ => changed.toolchain_manifest.supervisor_digest = digest,
            }
            assert_ne!(
                original.fingerprint().unwrap(),
                changed.fingerprint().unwrap()
            );
        }
        let value = serde_json::to_value(original).unwrap();
        for required in ["profile_id", "toolchain_manifest"] {
            let mut missing = value.clone();
            missing.as_object_mut().unwrap().remove(required);
            assert!(serde_json::from_value::<WorkspaceConfinementContract>(missing).is_err());
        }
        for required in [
            "inputs",
            "launcher_digest",
            "supervisor_digest",
            "schema_version",
        ] {
            let mut missing = value.clone();
            missing["toolchain_manifest"]
                .as_object_mut()
                .unwrap()
                .remove(required);
            assert!(serde_json::from_value::<WorkspaceConfinementContract>(missing).is_err());
        }
        for (pointer, replacement) in [
            ("/profile_id", serde_json::json!("unknown")),
            ("/toolchain_manifest/schema_version", serde_json::json!(2)),
            ("/toolchain_manifest/inputs", serde_json::json!([])),
            (
                "/toolchain_manifest/launcher_digest",
                serde_json::json!("sha256:abc"),
            ),
            (
                "/toolchain_manifest/supervisor_digest",
                serde_json::json!(format!("sha256:{}", "A".repeat(64))),
            ),
        ] {
            let mut invalid = value.clone();
            *invalid.pointer_mut(pointer).unwrap() = replacement;
            assert!(serde_json::from_value::<WorkspaceConfinementContract>(invalid).is_err());
        }
        let mut invalid = value;
        invalid["host_source_path"] = serde_json::json!("/private/toolchain");
        assert!(serde_json::from_value::<WorkspaceConfinementContract>(invalid).is_err());
    }
}
