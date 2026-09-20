//! Portable identity of an actually supported workspace confinement boundary.
//!
//! Providers must enumerate every readable immutable toolchain mount (including
//! its entire subtree) and retain those inputs for the execution lifetime. This
//! contract carries no host source paths and is not itself proof of isolation.

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

pub const WORKSPACE_CONFINEMENT_PROFILE: &str = "linux_restricted_root_x86_64_v1";

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
