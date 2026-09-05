use std::path::{Path, PathBuf};
use std::process::Stdio;

/// CLI-owned Runner process. `kill_on_drop` makes every early-return path
/// bounded; astra-edge retains its own durable inference journal before exit.
pub(crate) struct ManagedLocalRunner {
    child: tokio::process::Child,
}

impl ManagedLocalRunner {
    pub(crate) async fn stop(mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), self.child.wait()).await;
    }
}

fn runner_binary(current_exe: &Path) -> Result<PathBuf, String> {
    if let Some(explicit) = std::env::var_os("ASTRA_EDGE_BIN") {
        return Ok(PathBuf::from(explicit));
    }
    let filename = if cfg!(windows) {
        "astra-edge.exe"
    } else {
        "astra-edge"
    };
    current_exe
        .parent()
        .map(|parent| parent.join(filename))
        .ok_or_else(|| "cannot locate the Astra installation directory".to_string())
}

/// Start the inference-capable User Runner beside the CLI. The child resolves
/// environment credentials from this exact terminal and reads stored secrets
/// only through its owner-protected backend; neither reaches Server.
pub(crate) fn start(
    api_origin: &str,
    profile: Option<&str>,
    workspace: &Path,
) -> Result<ManagedLocalRunner, String> {
    let executable = runner_binary(
        &std::env::current_exe().map_err(|error| format!("locate Astra executable: {error}"))?,
    )?;
    if !executable.is_file() {
        return Err(format!(
            "User Runner executable is missing at {}; reinstall Astra or set ASTRA_EDGE_BIN",
            executable.display()
        ));
    }
    let mut command = tokio::process::Command::new(executable);
    command
        .arg("--server-url")
        .arg(api_origin)
        .arg("--workspace-dir")
        .arg(workspace)
        .arg("--reconnect=true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(profile) = profile {
        command.arg("--profile").arg(profile);
    }
    let child = command
        .spawn()
        .map_err(|error| format!("start User Runner: {error}"))?;
    Ok(ManagedLocalRunner { child })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_binary_path_is_platform_specific_and_not_shell_interpreted() {
        let path = runner_binary(Path::new("/opt/astra/bin/astra")).unwrap();
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(if cfg!(windows) {
                "astra-edge.exe"
            } else {
                "astra-edge"
            })
        );
    }
}
