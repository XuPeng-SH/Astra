use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const LOCAL_MODELS_FILE_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalInferenceProtocol {
    OpenaiCompatible,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum LocalCredentialRef {
    Environment { name: String },
    ProtectedFile { secret_id: String },
    SystemKeychain { service: String, account: String },
    None,
}

impl std::fmt::Debug for LocalCredentialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match self {
            Self::Environment { .. } => "environment",
            Self::ProtectedFile { .. } => "protected_file",
            Self::SystemKeychain { .. } => "system_keychain",
            Self::None => "none",
        };
        f.debug_struct("LocalCredentialRef")
            .field("kind", &kind)
            .finish()
    }
}

impl LocalCredentialRef {
    pub fn validate(&self) -> Result<(), LocalModelConfigError> {
        match self {
            Self::Environment { name } => validate_environment_name(name),
            Self::ProtectedFile { secret_id } => {
                validate_component("protected secret id", secret_id)
            }
            Self::SystemKeychain { service, account } => {
                validate_component("keychain service", service)?;
                validate_component("keychain account", account)
            }
            Self::None => Ok(()),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModelDefinition {
    pub protocol: LocalInferenceProtocol,
    pub base_url: String,
    pub model: String,
    pub credential: LocalCredentialRef,
}

impl std::fmt::Debug for LocalModelDefinition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalModelDefinition")
            .field("protocol", &self.protocol)
            .field("model_present", &!self.model.is_empty())
            .field("credential", &self.credential)
            .finish()
    }
}

impl LocalModelDefinition {
    pub fn validate(&self) -> Result<(), LocalModelConfigError> {
        validate_component("model", &self.model)?;
        validate_base_url(&self.base_url)?;
        self.credential.validate()
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalModelConfig {
    pub version: u32,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub models: BTreeMap<String, LocalModelDefinition>,
}

impl std::fmt::Debug for LocalModelConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalModelConfig")
            .field("version", &self.version)
            .field("revision", &self.revision)
            .field("model_count", &self.models.len())
            .finish()
    }
}

impl Default for LocalModelConfig {
    fn default() -> Self {
        Self {
            version: LOCAL_MODELS_FILE_VERSION,
            revision: 0,
            models: BTreeMap::new(),
        }
    }
}

impl LocalModelConfig {
    pub fn validate(&self) -> Result<(), LocalModelConfigError> {
        if self.version != LOCAL_MODELS_FILE_VERSION {
            return Err(LocalModelConfigError::UnsupportedVersion {
                actual: self.version,
                expected: LOCAL_MODELS_FILE_VERSION,
            });
        }
        for (name, model) in &self.models {
            validate_component("local model name", name)?;
            model
                .validate()
                .map_err(|source| LocalModelConfigError::Model {
                    name: name.clone(),
                    source: Box::new(source),
                })?;
        }
        Ok(())
    }
}

/// Provider authorization resolved for one local client attachment.
///
/// This value deliberately implements neither `Serialize` nor `Clone` and its
/// debug representation never reveals the credential. Callers should resolve
/// environment-backed values in the attaching process and pass the value over
/// authenticated local IPC, rather than letting a shared host inspect its own
/// startup environment.
pub struct ResolvedLocalCredential(String);

impl ResolvedLocalCredential {
    pub fn from_environment(
        reference: &LocalCredentialRef,
        mut read: impl FnMut(&str) -> Option<String>,
    ) -> Result<Option<Self>, LocalModelConfigError> {
        reference.validate()?;
        match reference {
            LocalCredentialRef::Environment { name } => {
                let value = read(name).ok_or_else(|| {
                    LocalModelConfigError::CredentialUnavailable(format!(
                        "environment variable {name} is not set in this terminal"
                    ))
                })?;
                if value.is_empty() {
                    return Err(LocalModelConfigError::CredentialUnavailable(format!(
                        "environment variable {name} is empty in this terminal"
                    )));
                }
                Ok(Some(Self(value)))
            }
            LocalCredentialRef::None => Ok(None),
            LocalCredentialRef::ProtectedFile { .. }
            | LocalCredentialRef::SystemKeychain { .. } => {
                Err(LocalModelConfigError::CredentialUnavailable(
                    "credential must be resolved by its configured local backend".to_string(),
                ))
            }
        }
    }

    pub fn expose_to_local_transport(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ResolvedLocalCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResolvedLocalCredential")
            .field("present", &!self.0.is_empty())
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum LocalModelConfigError {
    #[error("local model configuration I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("local model configuration JSON at {path}: {diagnostic}")]
    Json {
        path: PathBuf,
        diagnostic: JsonDecodeDiagnostic,
    },
    #[error("unsupported local model configuration version {actual}; expected {expected}")]
    UnsupportedVersion { actual: u32, expected: u32 },
    #[error("invalid {field}: {reason}")]
    Invalid { field: &'static str, reason: String },
    #[error("invalid local model {name}: {source}")]
    Model {
        name: String,
        source: Box<LocalModelConfigError>,
    },
    #[error("local model configuration revision changed; expected {expected}, actual {actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("local credential unavailable: {0}")]
    CredentialUnavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JsonDecodeDiagnostic {
    category: &'static str,
    line: usize,
    column: usize,
}

impl std::fmt::Display for JsonDecodeDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} at line {}, column {}",
            self.category, self.line, self.column
        )
    }
}

impl From<&serde_json::Error> for JsonDecodeDiagnostic {
    fn from(error: &serde_json::Error) -> Self {
        Self {
            category: match error.classify() {
                serde_json::error::Category::Io => "I/O error",
                serde_json::error::Category::Syntax => "syntax error",
                serde_json::error::Category::Data => "invalid data",
                serde_json::error::Category::Eof => "unexpected EOF",
            },
            line: error.line(),
            column: error.column(),
        }
    }
}

pub struct LocalModelConfigStore {
    path: PathBuf,
}

impl LocalModelConfigStore {
    pub fn with_path(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<LocalModelConfig, LocalModelConfigError> {
        if !self.path.exists() {
            return Ok(LocalModelConfig::default());
        }
        let lock = open_lock(&self.lock_path())?;
        lock.lock_shared().map_err(|source| self.io(source))?;
        self.load_unlocked()
    }

    /// Atomically replace the desired configuration under a revision CAS.
    /// Invalid candidates never alter the previously working file.
    pub fn replace(
        &self,
        expected_revision: u64,
        mut candidate: LocalModelConfig,
    ) -> Result<LocalModelConfig, LocalModelConfigError> {
        candidate.validate()?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|source| LocalModelConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let lock_path = self.lock_path();
        let lock = open_lock(&lock_path)?;
        lock.lock_exclusive()
            .map_err(|source| LocalModelConfigError::Io {
                path: lock_path,
                source,
            })?;
        let current = self.load_unlocked()?;
        if current.revision != expected_revision {
            return Err(LocalModelConfigError::RevisionConflict {
                expected: expected_revision,
                actual: current.revision,
            });
        }
        candidate.revision =
            expected_revision
                .checked_add(1)
                .ok_or_else(|| LocalModelConfigError::Invalid {
                    field: "revision",
                    reason: "revision is exhausted".to_string(),
                })?;
        let body = serde_json::to_vec_pretty(&candidate).map_err(|source| {
            LocalModelConfigError::Json {
                path: self.path.clone(),
                diagnostic: JsonDecodeDiagnostic::from(&source),
            }
        })?;
        write_atomic_private(&self.path, &body)?;
        Ok(candidate)
    }

    fn load_unlocked(&self) -> Result<LocalModelConfig, LocalModelConfigError> {
        if !self.path.exists() {
            return Ok(LocalModelConfig::default());
        }
        let bytes = fs::read(&self.path).map_err(|source| self.io(source))?;
        let config: LocalModelConfig =
            serde_json::from_slice(&bytes).map_err(|source| LocalModelConfigError::Json {
                path: self.path.clone(),
                diagnostic: JsonDecodeDiagnostic::from(&source),
            })?;
        config.validate()?;
        Ok(config)
    }

    fn lock_path(&self) -> PathBuf {
        self.path.with_extension("json.lock")
    }

    fn io(&self, source: std::io::Error) -> LocalModelConfigError {
        LocalModelConfigError::Io {
            path: self.path.clone(),
            source,
        }
    }
}

fn validate_component(field: &'static str, value: &str) -> Result<(), LocalModelConfigError> {
    if value.trim().is_empty() {
        return Err(LocalModelConfigError::Invalid {
            field,
            reason: "must not be empty".to_string(),
        });
    }
    if value.len() > 512 || value.chars().any(char::is_control) {
        return Err(LocalModelConfigError::Invalid {
            field,
            reason: "must be at most 512 bytes and contain no control characters".to_string(),
        });
    }
    Ok(())
}

fn validate_environment_name(name: &str) -> Result<(), LocalModelConfigError> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if !valid || name.len() > 128 {
        return Err(LocalModelConfigError::Invalid {
            field: "environment credential name",
            reason: "must be a portable environment variable name".to_string(),
        });
    }
    Ok(())
}

fn validate_base_url(value: &str) -> Result<(), LocalModelConfigError> {
    validate_component("base URL", value)?;
    let parsed = url::Url::parse(value).map_err(|_| LocalModelConfigError::Invalid {
        field: "base URL",
        reason: "must be an absolute HTTP(S) URL".to_string(),
    })?;
    let loopback = parsed.host().is_some_and(|host| match host {
        url::Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    });
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(LocalModelConfigError::Invalid {
            field: "base URL",
            reason: "must use HTTPS, except for an explicit loopback endpoint".to_string(),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() || parsed.fragment().is_some() {
        return Err(LocalModelConfigError::Invalid {
            field: "base URL",
            reason: "must not contain userinfo or a fragment".to_string(),
        });
    }
    for (name, _) in parsed.query_pairs() {
        let name = name.to_ascii_lowercase();
        if [
            "key",
            "token",
            "auth",
            "signature",
            "password",
            "credential",
        ]
        .iter()
        .any(|sensitive| name.contains(sensitive))
        {
            return Err(LocalModelConfigError::Invalid {
                field: "base URL",
                reason: "credential-like query parameters must use the credential backend"
                    .to_string(),
            });
        }
    }
    Ok(())
}

fn open_lock(path: &Path) -> Result<File, LocalModelConfigError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| LocalModelConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
}

fn write_atomic_private(path: &Path, body: &[u8]) -> Result<(), LocalModelConfigError> {
    let parent = path
        .parent()
        .ok_or_else(|| LocalModelConfigError::Invalid {
            field: "models path",
            reason: "must have a parent directory".to_string(),
        })?;
    // A unique create-new file prevents a pre-planted `.tmp` symlink or
    // permissive inode from redirecting or weakening secret-adjacent config.
    let mut temporary = tempfile::Builder::new()
        .prefix(".astra-models-")
        .tempfile_in(parent)
        .map_err(|source| LocalModelConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(body)
        .and_then(|()| temporary.as_file().sync_all())
        .map_err(|source| LocalModelConfigError::Io {
            path: temporary.path().to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| LocalModelConfigError::Io {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|source| LocalModelConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(credential: LocalCredentialRef) -> LocalModelDefinition {
        LocalModelDefinition {
            protocol: LocalInferenceProtocol::OpenaiCompatible,
            base_url: "https://provider.example/v1".to_string(),
            model: "coding-model".to_string(),
            credential,
        }
    }

    #[test]
    fn configuration_serializes_only_credential_reference() {
        let mut config = LocalModelConfig::default();
        config.models.insert(
            "work".to_string(),
            model(LocalCredentialRef::Environment {
                name: "WORK_LLM_API_KEY".to_string(),
            }),
        );
        let resolved =
            ResolvedLocalCredential::from_environment(&config.models["work"].credential, |_| {
                Some("provider-secret-canary".to_string())
            })
            .expect("resolve current attachment")
            .expect("secret is present");
        assert_eq!(
            resolved.expose_to_local_transport(),
            "provider-secret-canary"
        );
        let json = serde_json::to_string(&config).expect("serialize config");
        assert!(json.contains("WORK_LLM_API_KEY"));
        assert!(!json.contains("provider-secret-canary"));
        assert!(serde_json::from_str::<LocalModelConfig>(&json).is_ok());
        let inline = r#"{"version":1,"revision":0,"models":{"work":{"protocol":"openai_compatible","base_url":"https://provider.example/v1","model":"coding-model","credential":{"kind":"environment","name":"WORK_LLM_API_KEY","value":"provider-secret-canary"}}}}"#;
        assert!(serde_json::from_str::<LocalModelConfig>(inline).is_err());
    }

    #[test]
    fn environment_credentials_are_attachment_scoped_and_secret_safe() {
        let reference = LocalCredentialRef::Environment {
            name: "WORK_LLM_API_KEY".to_string(),
        };
        let first = ResolvedLocalCredential::from_environment(&reference, |name| {
            (name == "WORK_LLM_API_KEY").then(|| "first-terminal-secret".to_string())
        })
        .expect("first terminal resolves")
        .expect("credential present");
        let second = ResolvedLocalCredential::from_environment(&reference, |_| {
            Some("second-terminal-secret".to_string())
        })
        .expect("second terminal resolves")
        .expect("credential present");
        assert_ne!(
            first.expose_to_local_transport(),
            second.expose_to_local_transport()
        );
        for debug in [format!("{first:?}"), format!("{second:?}")] {
            assert!(!debug.contains("terminal-secret"));
            assert!(debug.contains("present: true"));
        }
    }

    #[test]
    fn invalid_candidate_and_stale_revision_preserve_previous_file() {
        let root = tempfile::tempdir().expect("tempdir");
        let store = LocalModelConfigStore::with_path(root.path().join("models.json"));
        let mut first = LocalModelConfig::default();
        first
            .models
            .insert("work".to_string(), model(LocalCredentialRef::None));
        let applied = store.replace(0, first).expect("apply first revision");
        assert_eq!(applied.revision, 1);

        let mut invalid = applied.clone();
        invalid.models.get_mut("work").unwrap().base_url = "http://provider.example/v1".to_string();
        assert!(store.replace(1, invalid).is_err());
        assert_eq!(store.load().expect("load after invalid"), applied);

        let mut stale = applied.clone();
        stale.models.get_mut("work").unwrap().model = "other".to_string();
        assert!(matches!(
            store.replace(0, stale),
            Err(LocalModelConfigError::RevisionConflict {
                expected: 0,
                actual: 1
            })
        ));
        assert_eq!(store.load().expect("load after conflict"), applied);
    }

    #[test]
    fn config_rejects_unknown_fields_and_unsafe_sources() {
        let unknown = r#"{"version":1,"revision":0,"models":{},"secret":"leak"}"#;
        assert!(serde_json::from_str::<LocalModelConfig>(unknown).is_err());
        assert!(
            model(LocalCredentialRef::Environment {
                name: "BAD-NAME".to_string()
            })
            .validate()
            .is_err()
        );
        let mut unsafe_model = model(LocalCredentialRef::None);
        unsafe_model.base_url = "https://user:secret@provider.example/v1".to_string();
        assert!(unsafe_model.validate().is_err());
    }

    #[test]
    fn invalid_environment_reference_never_reaches_the_environment_reader() {
        let reference = LocalCredentialRef::Environment {
            name: "INVALID-NAME".to_string(),
        };
        let mut reads = 0;
        assert!(
            ResolvedLocalCredential::from_environment(&reference, |_| {
                reads += 1;
                Some("must-not-be-read".to_string())
            })
            .is_err()
        );
        assert_eq!(reads, 0);
    }

    #[test]
    fn private_configuration_debug_omits_endpoint_and_reference_details() {
        let definition = LocalModelDefinition {
            base_url: "https://private.example/v1?account=secret-account".to_string(),
            credential: LocalCredentialRef::SystemKeychain {
                service: "secret-service".to_string(),
                account: "secret-account".to_string(),
            },
            ..model(LocalCredentialRef::None)
        };
        let mut config = LocalModelConfig::default();
        config
            .models
            .insert("secret-alias".to_string(), definition.clone());
        for debug in [
            format!("{definition:?}"),
            format!("{:?}", definition.credential),
            format!("{config:?}"),
        ] {
            for secret in [
                "private.example",
                "secret-service",
                "secret-account",
                "secret-alias",
            ] {
                assert!(!debug.contains(secret));
            }
        }
    }

    #[test]
    fn malformed_private_json_error_is_content_free() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("models.json");
        let canary = "private-provider-token-canary";
        fs::write(
            &path,
            format!(
                r#"{{"version":1,"revision":0,"models":{{"work":{{"protocol":"{canary}","base_url":"https://provider.example/v1","model":"m","credential":{{"kind":"none"}}}}}}}}"#
            ),
        )
        .unwrap();
        let raw = serde_json::from_str::<LocalModelConfig>(&fs::read_to_string(&path).unwrap())
            .unwrap_err();
        assert!(
            raw.to_string().contains(canary),
            "fixture must prove raw leak"
        );
        let error = LocalModelConfigStore::with_path(path).load().unwrap_err();
        assert!(!error.to_string().contains(canary));
        assert!(!format!("{error:?}").contains(canary));
    }

    #[test]
    fn credential_like_url_queries_are_rejected() {
        let mut definition = model(LocalCredentialRef::None);
        definition.base_url = "https://provider.example/v1?api-version=2026-01-01".to_string();
        definition.validate().expect("non-secret provider query");
        definition.base_url = "https://provider.example/v1?api_key=secret".to_string();
        assert!(definition.validate().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_replace_never_follows_a_preplanted_legacy_temp_symlink() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("tempdir");
        let victim = root.path().join("victim");
        fs::write(&victim, "do-not-touch").expect("write victim");
        symlink(&victim, root.path().join("models.json.tmp")).expect("plant legacy temp link");
        let store = LocalModelConfigStore::with_path(root.path().join("models.json"));
        store
            .replace(0, LocalModelConfig::default())
            .expect("unique temporary file ignores planted link");
        assert_eq!(fs::read_to_string(victim).unwrap(), "do-not-touch");
    }

    #[cfg(unix)]
    #[test]
    fn models_file_is_private_despite_a_permissive_legacy_temp_file() {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let root = tempfile::tempdir().expect("tempdir");
        let legacy = root.path().join("models.json.tmp");
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o666)
            .open(&legacy)
            .expect("create permissive legacy temp");
        fs::set_permissions(&legacy, fs::Permissions::from_mode(0o666)).unwrap();
        let path = root.path().join("models.json");
        LocalModelConfigStore::with_path(path.clone())
            .replace(0, LocalModelConfig::default())
            .expect("write private config");
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(legacy).unwrap().permissions().mode() & 0o777,
            0o666
        );
    }
}
