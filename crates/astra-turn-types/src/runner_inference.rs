//! Shared public identity and control protocol for the Runner inference facet.
//!
//! These messages contain public model facts and opaque identities only. Local
//! URLs, credentials, headers, and file references never belong in this module.
//! A protocol declaration is not execution authority: the current Server rejects
//! binding publication until durable inference dispatch and custody are available.

use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Deserializer, Serialize};

pub const RUNNER_INFERENCE_PROTOCOL_VERSION: u16 = 1;
const MAX_INFERENCE_ID_BYTES: usize = 64;
const MAX_MODEL_NAME_BYTES: usize = 255;

/// Opaque public reference, deliberately excluding URL/path syntax. Reconnect
/// generations are transport facts and do not appear in the binding identity.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct RunnerInferenceId(String);

impl RunnerInferenceId {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_INFERENCE_ID_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(
                "inference identity must be 1-64 ASCII letters, digits, hyphens or underscores",
            );
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunnerInferenceId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// Provider model identifiers are public request content, not a secret lookup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RunnerInferenceModelName(String);

impl RunnerInferenceModelName {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_MODEL_NAME_BYTES
            || value.trim() != value
            || value.chars().any(char::is_control)
            || value.contains("://")
        {
            return Err("inference model name must be bounded public model text, not a URL");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunnerInferenceModelName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingIdentity {
    pub runner_id: RunnerInferenceId,
    pub journal_id: RunnerInferenceId,
    pub binding_id: RunnerInferenceId,
    pub binding_revision: NonZeroU64,
    pub profile_revision: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerInferenceProtocol {
    #[serde(rename = "openai_chat_completions")]
    OpenAiChatCompletions,
}

/// Complete public definition for one revision. Principal/workspace/session
/// ownership is derived by Server, never accepted from this payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingDefinition {
    pub identity: RunnerInferenceBindingIdentity,
    pub model_name: RunnerInferenceModelName,
    pub protocol: RunnerInferenceProtocol,
    pub context_window: NonZeroU32,
    pub max_output_tokens: NonZeroU32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerInferenceBindingChange {
    Publish {
        definition: RunnerInferenceBindingDefinition,
    },
    Disable {
        identity: RunnerInferenceBindingIdentity,
    },
}

impl RunnerInferenceBindingChange {
    pub fn identity(&self) -> &RunnerInferenceBindingIdentity {
        match self {
            Self::Publish { definition } => &definition.identity,
            Self::Disable { identity } => identity,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingPublication {
    pub protocol_version: u16,
    pub operation_id: RunnerInferenceId,
    pub expected_publication_revision: u64,
    pub change: RunnerInferenceBindingChange,
}

/// Durable receipt for an exact publication operation. Replaying a receipt does
/// not make that historical revision current again.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingReceipt {
    pub operation_id: RunnerInferenceId,
    pub publication_revision: NonZeroU64,
    pub identity: RunnerInferenceBindingIdentity,
}

/// Owner-scoped immutable Astra artifact, never an arbitrary fetch URL. Content
/// hashes establish integrity; they do not authorize cross-owner access.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceArtifactReference {
    pub artifact_id: RunnerInferenceId,
    pub sha256: RunnerInferenceDigest,
    pub byte_len: NonZeroU64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct RunnerInferenceDigest(String);

impl RunnerInferenceDigest {
    pub fn new(value: impl Into<String>) -> Result<Self, &'static str> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("inference digest must be 64 lowercase hexadecimal characters");
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for RunnerInferenceDigest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceAttemptIdentity {
    pub user_id: String,
    pub scope: crate::InferenceInvocationScope,
    pub invocation_id: RunnerInferenceId,
    pub attempt_id: RunnerInferenceId,
    pub binding: RunnerInferenceBindingIdentity,
    pub request: RunnerInferenceArtifactReference,
}

/// One persisted grant, replayed verbatim. Socket delivery generations are not
/// durable start authority and cannot extend either deadline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceDispatchGrant {
    pub attempt: RunnerInferenceAttemptIdentity,
    pub grant_id: RunnerInferenceId,
    pub process_boot_nonce: RunnerInferenceId,
    pub start_before_unix_ms: u64,
    pub deadline_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceTerminalAck {
    pub attempt: RunnerInferenceAttemptIdentity,
    pub terminal_sha256: RunnerInferenceDigest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerInferenceStartEvidence {
    FenceCommitted,
    ProviderStarted,
    ExpiredWithoutFence,
    CancelledWithoutFence,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerInferenceRejection {
    InferenceUnsupported,
    ProtocolVersionUnsupported,
    ConnectionSuperseded,
    BindingIdentityMismatch,
}

/// There is intentionally no accepted/selectable variant before a concrete
/// executor and durable dispatch implement that promise.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunnerInferenceNegotiation {
    Unavailable { reason: RunnerInferenceRejection },
}

impl RunnerInferenceNegotiation {
    pub fn for_protocol_version(version: u16) -> Self {
        Self::Unavailable {
            reason: if version == RUNNER_INFERENCE_PROTOCOL_VERSION {
                RunnerInferenceRejection::InferenceUnsupported
            } else {
                RunnerInferenceRejection::ProtocolVersionUnsupported
            },
        }
    }
}

/// A rejection is a control response, not a durable publication receipt. It
/// does not advance the publication revision or create an effective Offering.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerInferenceBindingRejection {
    pub operation_id: RunnerInferenceId,
    pub reason: RunnerInferenceRejection,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn publication_json() -> serde_json::Value {
        json!({
            "protocol_version": 1,
            "operation_id": "operation-1",
            "expected_publication_revision": 0,
            "change": {
                "action": "publish",
                "definition": {
                    "identity": {"runner_id": "runner-1", "journal_id": "journal-1", "binding_id": "binding-1", "binding_revision": 1, "profile_revision": 1},
                    "model_name": "public-model", "protocol": "openai_chat_completions",
                    "context_window": 8192, "max_output_tokens": 1024
                }
            }
        })
    }

    #[test]
    fn public_binding_roundtrip_carries_only_public_definition() {
        let value = publication_json();
        let publication: RunnerInferenceBindingPublication =
            serde_json::from_value(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(publication).unwrap(), value);
    }

    #[test]
    fn private_material_and_caller_claimed_authority_are_rejected_at_every_boundary() {
        for path in ["", "/change/definition", "/change/definition/identity"] {
            for forbidden in [
                "api_key",
                "endpoint_url",
                "headers",
                "credential_ref",
                "user_id",
                "workspace_id",
                "session_id",
                "connection_generation",
            ] {
                let mut value = publication_json();
                value
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(forbidden.into(), json!("canary-secret"));
                assert!(
                    serde_json::from_value::<RunnerInferenceBindingPublication>(value).is_err(),
                    "accepted {forbidden} at {path}"
                );
            }
        }
    }

    #[test]
    fn binding_identifiers_and_revisions_are_bounded_and_exact() {
        for invalid in [
            "",
            " runner",
            "runner/path",
            "https://private.invalid",
            "runner\n",
        ] {
            assert!(RunnerInferenceId::new(invalid).is_err());
        }
        assert!(RunnerInferenceId::new("r".repeat(65)).is_err());
        assert!(RunnerInferenceId::new("r".repeat(64)).is_ok());
        for field in ["binding_revision", "profile_revision"] {
            let mut value = publication_json();
            value["change"]["definition"]["identity"][field] = json!(0);
            assert!(serde_json::from_value::<RunnerInferenceBindingPublication>(value).is_err());
        }
        assert!(RunnerInferenceModelName::new("m".repeat(256)).is_err());
        assert!(RunnerInferenceModelName::new("https://private.invalid/v1").is_err());
    }

    #[test]
    fn version_negotiation_never_claims_an_unimplemented_executor() {
        assert_eq!(
            RunnerInferenceNegotiation::for_protocol_version(1),
            RunnerInferenceNegotiation::Unavailable {
                reason: RunnerInferenceRejection::InferenceUnsupported
            }
        );
        for version in [0, 2, u16::MAX] {
            assert_eq!(
                RunnerInferenceNegotiation::for_protocol_version(version),
                RunnerInferenceNegotiation::Unavailable {
                    reason: RunnerInferenceRejection::ProtocolVersionUnsupported
                }
            );
        }
    }
}
