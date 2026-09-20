//! Process-local allocation authority. A restart deliberately cannot adopt old paths.
use super::*;
use astra_runtime_env::WorkspaceConfinementContract;

#[derive(Debug)]
pub struct Allocations {
    pub provider: Provider,
    owner: Option<String>,
    entries: HashMap<PathBuf, Allocation>,
}

struct Allocation {
    directory: std::fs::File,
    run_id: String,
    session_id: String,
    executor: Arc<astra_tools::executor::DefaultToolExecutor>,
    source_commit: String,
    // A failed or interrupted execution cannot authorize later capture or release.
    unsettled: bool,
}

impl std::fmt::Debug for Allocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Allocation")
            .field("run_id", &self.run_id)
            .field("session_id", &self.session_id)
            .field("unsettled", &self.unsettled)
            .finish_non_exhaustive()
    }
}

impl Allocations {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            owner: None,
            entries: HashMap::new(),
        }
    }

    pub fn bind_owner(&mut self, owner: &str) -> Result<(), String> {
        if self
            .owner
            .as_deref()
            .is_some_and(|previous| previous != owner)
        {
            return Err("dedicated provider authenticated owner changed".into());
        }
        self.owner = Some(owner.into());
        Ok(())
    }

    pub fn prepare(
        &mut self,
        base: &Path,
        materialization: &str,
        request: astra_server_types::edge_ws_protocol::EdgeWorkspacePreparationRequest<'_>,
    ) -> Result<WorkspaceSourceIdentity, String> {
        let key = request.workspace_key;
        let commit = request.source_commit;
        let session_id = request.session_id;
        if session_id.trim().is_empty() {
            return Err("allocation requires a Session identity".into());
        }
        self.provider.revalidate()?;
        if request.confinement != self.provider.contract() {
            return Err("frozen confinement contract differs from the dedicated provider".into());
        }
        if self.owner.is_none() {
            return Err("allocation requires authenticated owner".into());
        }
        let run_id = key
            .strip_prefix("trial-")
            .filter(|value| !value.is_empty())
            .ok_or("evaluation workspace_key must be trial-{run_id}")?;
        let path = evaluation_workspace_path(base, materialization, key)?;
        if let Some(existing) = self.entries.get(&path) {
            if existing.session_id != session_id {
                return Err("allocation Session identity mismatch".into());
            }
            self.validate(&path, Some(commit))?;
            return verify_evaluation_workspace(&path, commit);
        }
        match std::fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            _ => {
                return Err(
                    "unknown existing allocation is quarantined; restart cannot adopt it".into(),
                );
            }
        }
        let source = prepare_evaluation_workspace(base, materialization, key, commit)?;
        let directory = self.provider.retain_allocation(&path)?;
        let executor = Arc::new(
            astra_tools::executor::DefaultToolExecutor::for_workspace(
                &path,
                self.owner.as_ref().unwrap().clone(),
                session_id,
                "astra-edge/0.1",
                Duration::from_secs(30),
            )
            .with_shell_process_boundary(self.provider.boundary(&path), vec![path.join(".git")]),
        );
        self.entries.insert(
            path,
            Allocation {
                directory,
                run_id: run_id.into(),
                session_id: session_id.into(),
                executor,
                source_commit: commit.into(),
                unsettled: false,
            },
        );
        Ok(source)
    }

    pub fn validate(&self, path: &Path, commit: Option<&str>) -> Result<(), String> {
        self.provider.revalidate()?;
        let allocation = self
            .entries
            .get(path)
            .ok_or("workspace was not allocated by this process")?;
        if allocation.unsettled {
            return Err("allocation settlement is unproven; preserving evidence".into());
        }
        if commit.is_some_and(|commit| commit != allocation.source_commit) {
            return Err("allocation source commit mismatch".into());
        }
        same_directory(&allocation.directory, path)
    }

    pub fn executor(
        &self,
        path: &Path,
        identity: &astra_turn_types::ToolInvocationIdentity,
    ) -> Result<Arc<astra_tools::executor::DefaultToolExecutor>, String> {
        self.validate(path, None)?;
        let entry = &self.entries[path];
        if !allocation_identity_matches(
            self.owner.as_deref(),
            &entry.run_id,
            &entry.session_id,
            identity,
        ) {
            return Err("allocation owner or Run identity mismatch".into());
        }
        Ok(Arc::clone(&entry.executor))
    }

    pub fn mark_settled(&mut self, path: &Path) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.unsettled = false;
        }
    }

    pub fn mark_unsettled(&mut self, path: &Path) {
        if let Some(entry) = self.entries.get_mut(path) {
            entry.unsettled = true;
        }
    }

    pub fn release(&mut self, base: &Path, path: &Path, commit: &str) -> Result<(), String> {
        self.validate(path, Some(commit))?;
        release_evaluation_workspace(base, path, commit)?;
        self.entries.remove(path);
        Ok(())
    }
}

fn allocation_identity_matches(
    owner: Option<&str>,
    run_id: &str,
    session_id: &str,
    identity: &astra_turn_types::ToolInvocationIdentity,
) -> bool {
    owner == Some(identity.user_id.as_str())
        && run_id == identity.run_id
        && session_id == identity.session_id
}

fn same_directory(retained: &std::fs::File, path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let old = retained.metadata().map_err(|error| error.to_string())?;
        let current = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
        if current.is_dir() && old.dev() == current.dev() && old.ino() == current.ino() {
            return Ok(());
        }
    }
    Err("retained allocation directory identity changed".into())
}

/// Adapts startup authority to allocation operations; unsupported hosts fail startup.
pub struct Provider(evaluation_provider::EvaluationProviderAuthority);
impl std::fmt::Debug for Provider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("DedicatedProvider")
    }
}
impl Provider {
    pub async fn start(path: &Path, source: &Path) -> Result<Self, String> {
        let authority = evaluation_provider::EvaluationProviderAuthority::start(path, source)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(
            deployment_id = authority.deployment_id(),
            contract = authority.contract_fingerprint(),
            "Dedicated provider preflight passed"
        );
        Ok(Self(authority))
    }
    pub fn revalidate(&self) -> Result<(), String> {
        self.0.revalidate().map_err(|e| e.to_string())
    }
    pub fn contract(&self) -> &WorkspaceConfinementContract {
        self.0.contract()
    }
    pub fn retain_allocation(&self, path: &Path) -> Result<std::fs::File, String> {
        if path.parent() != Some(self.0.allocation_root()) {
            return Err("allocation must be a direct child of retained allocation root".into());
        }
        #[cfg(unix)]
        {
            astra_sandbox::open_directory_beneath(
                self.0.allocation_directory(),
                Path::new(path.file_name().ok_or("allocation has no filename")?),
            )
            .map_err(|e| e.to_string())
        }
        #[cfg(not(unix))]
        {
            Err("unsupported evaluation host".into())
        }
    }
    pub fn boundary(&self, path: &Path) -> astra_sandbox::ShellProcessBoundary {
        astra_sandbox::ShellProcessBoundary {
            workspace: path.into(),
            home: "/home/sandbox".into(),
            temp: "/tmp".into(),
            read_only_paths: self.0.read_only_paths(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn allocation_identity_requires_authenticated_owner_and_exact_run() {
        let identity = astra_turn_types::ToolInvocationIdentity::new(
            "owner",
            "session",
            "run",
            "chain",
            "invocation",
        )
        .unwrap();
        assert!(allocation_identity_matches(
            Some("owner"),
            "run",
            "session",
            &identity
        ));
        assert!(!allocation_identity_matches(
            None, "run", "session", &identity
        ));
        assert!(!allocation_identity_matches(
            Some("owner"),
            "run",
            "other-session",
            &identity
        ));
        assert!(!allocation_identity_matches(
            Some("other"),
            "run",
            "session",
            &identity
        ));
        assert!(!allocation_identity_matches(
            Some("owner"),
            "other",
            "session",
            &identity
        ));
    }

    #[test]
    fn retained_directory_rejects_replacement_and_symlink() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("allocation");
        std::fs::create_dir(&path).unwrap();
        let retained = std::fs::File::open(&path).unwrap();
        assert!(same_directory(&retained, &path).is_ok());
        std::fs::rename(&path, root.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(same_directory(&retained, &path).is_err());
        #[cfg(unix)]
        {
            std::fs::remove_dir(&path).unwrap();
            std::os::unix::fs::symlink(root.path().join("old"), &path).unwrap();
            assert!(same_directory(&retained, &path).is_err());
        }
    }
}
