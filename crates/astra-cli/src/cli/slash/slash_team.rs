use crate::cli::{
    cli_config::cli_args::{TeamArgs, TeamSubcommand},
    cli_config::cli_utils::truncate_str,
    session::session_state::SessionState,
    theme,
};
use astra_services::team_persistence::{CreateTeam, TeamPersistenceService, UpdateTeam};
use crossterm::style::Stylize;
use std::collections::HashMap;

/// Typed input for the existing ordinary Chat command. This is only a
/// configuration carrier; admission, execution, cancellation, and recovery
/// remain owned by the normal root-turn path.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TeamChatRequest {
    pub message: String,
    pub selection: astra_services::runs::AgentProfileSelection,
}

pub(crate) async fn resolve_team_run_chat_request(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    team_name: &str,
    lead_agent_id: Option<&str>,
    task: &str,
) -> Result<TeamChatRequest, String> {
    if task.trim().is_empty() {
        return Err("Team run task cannot be empty".into());
    }
    let store = crate::cli::http_team_store::HttpTeamStore::new(api, profile);
    let team = TeamPersistenceService::load_team(&store, "", team_name)
        .await
        .map_err(|error| format!("failed to load team '{team_name}': {error}"))?
        .ok_or_else(|| format!("Team '{team_name}' not found"))?;
    let lead = resolve_team_lead_profile(&team, lead_agent_id)?;
    Ok(TeamChatRequest {
        message: task.trim().to_string(),
        selection: astra_services::runs::AgentProfileSelection {
            team_id: team.team_id,
            lead_agent_id: Some(lead.agent_id),
        },
    })
}

/// Resolve configuration intent only. Root admission owns current authority.
pub(crate) fn resolve_team_lead_profile(
    team: &Team,
    lead_agent_id: Option<&str>,
) -> Result<astra_services::coordination::AgentProfile, String> {
    if team.members.is_empty() {
        return Err(format!(
            "Team '{}' has no members. Add a member before starting a lead turn.",
            team.name
        ));
    }
    let mut profiles = team
        .members
        .iter()
        .map(|member| astra_services::team_persistence::resolve_member_to_profile(member, &team));
    match lead_agent_id {
        Some(id) => profiles
            .find(|profile| profile.agent_id == id.trim())
            .ok_or_else(|| {
                format!(
                    "lead '{}' is not a member of Team '{}'; use an Agent ID shown by `team info`",
                    id, team.name
                )
            }),
        None => {
            let mut coordinators = profiles.filter(|profile| profile.can_delegate);
            let lead = coordinators.next().ok_or_else(|| {
                format!("Team '{}' has no delegation-capable coordinator; select a member with --lead-agent-id", team.name)
            })?;
            if coordinators.next().is_some() {
                return Err(format!(
                    "Team '{}' has multiple delegation-capable coordinators; select one with --lead-agent-id",
                    team.name
                ));
            }
            Ok(lead)
        }
    }
}

/// Reuse the persistence owner's canonical configuration; the CLI has no
/// second lossy Team/TeamMember schema or configuration registry.
pub(crate) type Team = astra_services::team_persistence::TeamDefinition;
pub(crate) type TeamMember = astra_services::team_persistence::TeamMemberDef;

/// Get current git HEAD commit SHA (best-effort).
fn git_head_sha() -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn team_member_description(member: &TeamMember) -> String {
    member
        .system_prompt
        .clone()
        .unwrap_or_else(|| format!("{} agent", member.role))
}

/// Configuration observations, not execution or completion evidence.
pub(crate) async fn load_team_configurations(
    store: &dyn TeamPersistenceService,
    user_id: &str,
    name: Option<&str>,
) -> Result<Vec<Team>, String> {
    match name {
        Some(name) => Ok(vec![load_named_team(store, user_id, name).await?]),
        None => store.list_teams(user_id).await,
    }
}

async fn load_named_team(
    store: &dyn TeamPersistenceService,
    user_id: &str,
    name: &str,
) -> Result<Team, String> {
    store
        .load_team(user_id, name)
        .await?
        .ok_or_else(|| format!("Team '{name}' not found"))
}

pub(crate) fn team_configuration_lines<'a>(
    teams: impl IntoIterator<Item = &'a Team>,
) -> Vec<String> {
    let mut teams = teams.into_iter().peekable();
    let mut lines = vec!["Team definitions · not live execution status".into()];
    if teams.peek().is_none() {
        lines.push("No teams defined. Create one with astra team create <name>.".into());
    }
    for team in teams {
        lines.push(String::new());
        lines.push(format!("{} · {}", team.name, team.description));
        lines.push(format!("Revision {}", team.revision));
        if team.members.is_empty() {
            lines.push("Draft · add members before starting a task.".into());
        }
        for member in &team.members {
            let profile = astra_services::team_persistence::resolve_member_to_profile(member, team);
            lines.push(format!(
                "{} · {}",
                member.role,
                team_member_description(member)
            ));
            lines.push(format!("  Profile ID: {}", profile.agent_id));
            lines.push(format!(
                "  Can delegate: {} · read-only: {} · max depth: {}",
                member.can_delegate, member.read_only, profile.max_delegation_depth
            ));
            lines.push(format!(
                "  Configured model: {}",
                member
                    .model_selection
                    .as_ref()
                    .map(|model| model.offering_id.as_str())
                    .unwrap_or("inherit parent")
            ));
            if let Some(tools) = &member.allow_tools {
                lines.push(format!(
                    "  Allowed tools: {}",
                    if tools.is_empty() {
                        "none".into()
                    } else {
                        tools.join(", ")
                    }
                ));
            }
            if !member.skills.is_empty() {
                lines.push(format!("  Skills: {}", member.skills.join(", ")));
            }
            if !member.mcp_servers.is_empty() {
                lines.push(format!(
                    "  MCP selection: {} (unsupported at run admission)",
                    member.mcp_servers.join(", ")
                ));
            }
        }
        let mut context: Vec<_> = team.context.iter().collect();
        context.sort_by(|left, right| left.0.cmp(right.0));
        for (key, value) in context {
            lines.push(format!("  {key} = {}", truncate_str(value, 60)));
        }
        let name = shell_words::quote(&team.name);
        lines.push(if team.members.is_empty() {
            format!("Next: astra team add-member {name} <role>")
        } else {
            format!("Start: /team run {name} <task>")
        });
    }
    lines
}

// ── Slash Command Handler ───────────────────────────────────────────────

pub(crate) async fn handle_team_command(
    args: TeamArgs,
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    state: &mut SessionState,
) -> Result<(), String> {
    if matches!(args.command, Some(TeamSubcommand::Run(_))) {
        return Err("Team execution must use the ordinary Chat entrypoint".into());
    }
    if matches!(args.command, Some(TeamSubcommand::Leave)) {
        return Err("Use /team leave in the interactive workbench; this one-shot command cannot change a conversation's next-turn selection.".into());
    }
    // HTTP persistence binds its authenticated owner; there is no local
    // collection hydration or second mutable configuration registry.
    let user_id = state.ingestion_user_id.as_deref().unwrap_or("");

    match args.command {
        None | Some(TeamSubcommand::List) => {
            let teams = load_team_configurations(state.team_store.as_ref(), user_id, None).await?;
            eprintln!("{}", team_configuration_lines(&teams).join("\n"));
            eprintln!("{}", team_subcommands_hint());
        }

        Some(TeamSubcommand::Create(command)) => {
            let name = command.name.as_str();
            let description = command.description.join(" ");
            let rest = description.as_str();
            if name.is_empty() {
                return Err("Usage: /team create <name> [description]".into());
            }
            let description = if rest.is_empty() {
                format!("Custom team: {name}")
            } else {
                rest.to_string()
            };
            let definition = CreateTeam {
                team_id: uuid::Uuid::new_v4().to_string(),
                name: name.to_string(),
                description,
                members: Vec::new(),
                context: HashMap::new(),
            };
            state
                .team_store
                .create_team(user_id, &definition)
                .await
                .map_err(|error| {
                    format!(
                        "failed to persist team '{name}' (ID {}): {error}",
                        definition.team_id
                    )
                })?;
            eprintln!(
                "  {} Team '{}' created. Add members with /team add-member {} <role> <description>",
                theme::icon_ok(),
                name.magenta(),
                name
            );
        }

        Some(TeamSubcommand::AddMember(member_args)) => {
            let team = member_args.team.as_str();
            let role = member_args.role.as_str();
            let desc = member_args.description.join(" ");
            let model_selection = if let Some(model) = member_args.model.as_deref() {
                let token = crate::cli::session::session_runtime::fresh_access_token(api, profile)
                    .await
                    .ok_or_else(|| "Not logged in".to_string())?;
                let selected =
                    crate::cli::session::session_runtime::resolve_server_model_selection(
                        api,
                        &token,
                        model,
                        astra_core::model_wire::purpose::ModelCatalogPurpose::Chat,
                    )
                    .await?;
                Some(astra_turn_types::ModelSelection {
                    offering_id: selected.offering_id,
                })
            } else {
                None
            };
            let member = TeamMember {
                role: role.to_string(),
                agent_id: uuid::Uuid::new_v4().to_string(),
                system_prompt: Some(if desc.is_empty() {
                    format!("{role} agent")
                } else {
                    desc
                }),
                skills: Vec::new(),
                model_selection,
                mcp_servers: Vec::new(),
                can_delegate: member_args.can_delegate,
                max_delegation_depth: member_args.max_delegation_depth.unwrap_or(0),
                ..Default::default()
            };
            let mut definition = load_named_team(state.team_store.as_ref(), user_id, team).await?;
            if definition
                .members
                .iter()
                .any(|existing| existing.role == role)
            {
                return Err(format!("Role '{}' already exists in team '{team}'", role));
            }
            definition.members.push(member);
            state
                .team_store
                .update_team(user_id, &definition.team_id, &UpdateTeam::from(&definition))
                .await
                .map_err(|error| format!("failed to persist team '{team}': {error}"))?;
            eprintln!(
                "  {} Added role '{}' to team '{}'",
                theme::icon_ok(),
                role.green(),
                team.magenta()
            );
        }

        Some(TeamSubcommand::Info(command)) => {
            let teams =
                load_team_configurations(state.team_store.as_ref(), user_id, Some(&command.name))
                    .await?;
            eprintln!("{}", team_configuration_lines(&teams).join("\n"));
        }

        Some(TeamSubcommand::Delete(command)) => {
            let name = command.name.as_str();
            if name.is_empty() {
                return Err("Usage: /team delete <name>".into());
            }
            let team = load_named_team(state.team_store.as_ref(), user_id, name).await?;
            let deleted = state
                .team_store
                .delete_team(user_id, &team.team_id)
                .await
                .map_err(|error| format!("failed to delete team '{name}': {error}"))?;
            if !deleted {
                return Err(format!("Team '{name}' was not found in persistence store"));
            }
            eprintln!("  {} Team '{}' deleted", theme::icon_ok(), name);
        }

        Some(TeamSubcommand::Context(command)) => {
            let team = command.team.as_str();
            let key = command.key.as_str();
            let context_value = command.value.join(" ");
            let value = context_value.as_str();
            if team.is_empty() || key.is_empty() {
                return Err("Usage: /team context <team> <key> <value>".into());
            }
            let mut candidate = load_named_team(state.team_store.as_ref(), user_id, team).await?;
            candidate.context.insert(key.to_string(), value.to_string());
            state
                .team_store
                .update_team(user_id, &candidate.team_id, &UpdateTeam::from(&candidate))
                .await
                .map_err(|error| format!("failed to persist team '{team}': {error}"))?;
            eprintln!(
                "  {} Set context '{}'='{}' on team '{}'",
                theme::icon_ok(),
                key,
                truncate_str(value, 40),
                team.magenta()
            );
        }

        Some(TeamSubcommand::Snapshot(command)) => {
            let name = command.team.as_str();
            let snapshot_label = command.label.join(" ");
            let label = snapshot_label.as_str();
            if name.is_empty() {
                return Err("Usage: /team snapshot <team> [label]".into());
            }
            let team_definition = load_named_team(state.team_store.as_ref(), user_id, name).await?;

            let snapshot_id = format!("team-{}-{}", name, chrono::Utc::now().timestamp());
            let git_sha = git_head_sha();
            let session_id = state.session_id.clone();
            let now = chrono::Utc::now().to_rfc3339();

            let snap_label = if label.is_empty() {
                format!("team {} snapshot", name)
            } else {
                label.to_string()
            };

            // Persist the complete canonical definition, not a display-only
            // projection that would discard member capabilities or context.
            let team_def_json = Some(
                serde_json::to_string(&team_definition)
                    .map_err(|error| format!("failed to encode team snapshot: {error}"))?,
            );
            let snap_record = astra_services::team_persistence::TeamSnapshotRecord {
                snapshot_id: snapshot_id.clone(),
                team_id: team_definition.team_id.clone(),
                team_name: name.to_string(),
                user_id: team_definition.user_id.clone(),
                label: snap_label.clone(),
                git_commit: git_sha.clone(),
                session_id: session_id.clone(),
                team_definition_json: team_def_json,
                created_at: now.clone(),
            };
            let accepted = state
                .team_store
                .save_snapshot(&snap_record)
                .await
                .map_err(|error| format!("failed to persist snapshot: {error}"))?;
            eprintln!(
                "\n  {} Snapshot '{}' created for team '{}'",
                theme::icon_ok(),
                accepted.snapshot_id.as_str().dim(),
                name.magenta()
            );
            if let Some(ref sha) = accepted.git_commit {
                eprintln!("    {} Git: {}", "🔖".dim(), sha.get(..12).unwrap_or(sha),);
            }
            eprintln!(
                "    {} Use '/team restore {} {}' to restore.",
                "💡".dim(),
                name,
                accepted.snapshot_id,
            );
            eprintln!();
        }

        Some(TeamSubcommand::Restore(command)) => {
            let name = command.team.as_str();
            let snapshot_id = command.snapshot_id.as_str();
            if name.is_empty() || snapshot_id.is_empty() {
                return Err("Usage: /team restore <team> <snapshot-id>".into());
            }
            let current = load_named_team(state.team_store.as_ref(), user_id, name).await?;
            let snap = state
                .team_store
                .find_snapshot(snapshot_id, &current.user_id)
                .await
                .map_err(|error| format!("failed to load snapshot '{snapshot_id}': {error}"))?
                .ok_or_else(|| format!("Snapshot '{snapshot_id}' not found"))?;
            if snap.snapshot_id != snapshot_id {
                return Err("Snapshot response does not match the requested identity".into());
            }
            if snap.team_id != current.team_id {
                return Err(format!(
                    "Snapshot '{}' belongs to team '{}', not '{}'",
                    snap.snapshot_id, snap.team_name, name
                ));
            }
            let definition_json = snap
                .team_definition_json
                .as_deref()
                .ok_or_else(|| "Snapshot has no Team configuration".to_string())?;
            let definition: Team = serde_json::from_str(definition_json)
                .map_err(|error| format!("invalid snapshot configuration: {error}"))?;
            if definition.team_id != current.team_id || definition.user_id != current.user_id {
                return Err("Snapshot configuration belongs to another Team or owner".into());
            }
            astra_services::team_persistence::validate_team(&definition)
                .map_err(|errors| format!("invalid snapshot configuration: {errors:?}"))?;
            // Restore configuration, never historical identity or Git state.
            let mut update = UpdateTeam::from(&definition);
            update.expected_revision = current.revision;
            state
                .team_store
                .update_team(user_id, &current.team_id, &update)
                .await
                .map_err(|error| format!("failed to restore Team configuration: {error}"))?;
            eprintln!(
                "  {} Team configuration restored; Git and running tasks unchanged.\n",
                theme::icon_ok()
            );
        }

        Some(TeamSubcommand::Run(_) | TeamSubcommand::Leave) => {
            return Err("Team execution must use the ordinary Chat entrypoint".into());
        }
    }
    Ok(())
}

fn team_subcommands_hint() -> &'static str {
    "Subcommands: /team list · info · create · add-member · context · run · snapshot · restore · delete · help"
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{Team, TeamMember, git_head_sha, team_subcommands_hint};
    use crate::cli::cli_config::cli_utils::{
        CredentialsFile, Profile, TestCliProfileIdentityGuard,
        install_cli_profile_identity_for_test, save_credentials,
    };
    use crate::cli::session::session_state::SessionState;
    use astra_services::team_persistence::{CreateTeam, UpdateTeam};
    use std::collections::HashMap;
    use wiremock::matchers::{body_json, body_partial_json, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    async fn handle_team_command(
        command: &str,
        api: &astra_thin_client::ThinClient,
        profile: Option<&str>,
        state: &mut SessionState,
    ) -> Result<(), String> {
        let crate::cli::cli_config::cli_args::Command::Team(args) =
            crate::cli::command_router::parse_team_bridge_command(command)?
        else {
            unreachable!("the Team parser only returns Team commands")
        };
        super::handle_team_command(args, api, profile, state).await
    }

    fn http_session(
        server: &MockServer,
    ) -> (
        astra_thin_client::ThinClient,
        SessionState,
        TestCliProfileIdentityGuard,
    ) {
        let mut creds = CredentialsFile::default();
        creds.profiles.insert(
            "default".into(),
            Profile {
                account_id: Some("u".into()),
                access_token: Some("test-token".into()),
                ..Default::default()
            },
        );
        save_credentials(&creds).unwrap();
        let identity = install_cli_profile_identity_for_test("default", Some("u")).unwrap();
        let api = astra_thin_client::ThinClient::new(&server.uri(), None).unwrap();
        let mut state = SessionState::default();
        state.ingestion_user_id = Some("u".into());
        state.team_store =
            std::sync::Arc::new(crate::cli::http_team_store::HttpTeamStore::new(&api, None));
        (api, state, identity)
    }

    async fn mock_named_team(server: &MockServer, team: &Team) {
        Mock::given(method("GET"))
            .and(path(format!(
                "/teams/name/{}",
                urlencoding::encode(&team.name)
            )))
            .respond_with(ResponseTemplate::new(200).set_body_json(team))
            .expect(1)
            .mount(server)
            .await;
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn team_http_commands_preserve_literals_and_fail_without_retry() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _env_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let server = MockServer::start().await;
        let (api, mut state, _identity) = http_session(&server);
        let mut current = make_team(&["first"]);
        current.team_id = "stable-team-id".into();
        current.revision = 7;
        for (command, verb, endpoint, reads) in [
            ("create fresh", "POST", "/teams", 0),
            ("add-member test second", "PUT", "/teams/stable-team-id", 1),
            ("context test key value", "PUT", "/teams/stable-team-id", 1),
            ("list", "GET", "/teams", 0),
        ] {
            if reads != 0 {
                mock_named_team(&server, &current).await;
            }
            Mock::given(method(verb))
                .and(path(endpoint))
                .respond_with(ResponseTemplate::new(503))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                handle_team_command(command, &api, None, &mut state)
                    .await
                    .is_err()
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), reads + 1);
            let request = requests.last().unwrap();
            assert_eq!(request.method.as_str(), verb);
            if verb == "PUT" {
                let input: UpdateTeam = request.body_json().unwrap();
                assert_eq!(input.expected_revision, current.revision);
                assert_eq!(input.members[0], current.members[0]);
            }
            server.verify().await;
            server.reset().await;
        }

        current.members.clear();
        current.name = "fresh team's".into();
        current.description = "Keep user's exact words".into();
        for (index, (command, expected)) in [
            ("create \"fresh team's\" \"Keep user's exact words\"", serde_json::json!({
                "name": "fresh team's", "description": "Keep user's exact words"
            })),
            ("context \"fresh team's\" \"acceptance criteria\" \"Don't change --flags\"", serde_json::json!({
                "context": {"acceptance criteria": "Don't change --flags"}
            })),
            ("add-member \"fresh team's\" \"delivery lead\" --can-delegate --max-delegation-depth 3 -- --can-delegate \"literal description\"", serde_json::json!({
                "members": [{"role": "delivery lead", "system_prompt": "--can-delegate literal description",
                    "skills": [], "mcp_servers": [], "can_delegate": true, "max_delegation_depth": 3}]
            })),
        ].into_iter().enumerate() {
            let updating = index != 0;
            if updating { mock_named_team(&server, &current).await; }
            let immutable_id = current.team_id.clone();
            Mock::given(method(if updating { "PUT" } else { "POST" }))
                .and(path(if updating { format!("/teams/{immutable_id}") } else { "/teams".into() }))
                .and(body_partial_json(expected))
                .respond_with(move |request: &Request| {
                    let mut accepted: serde_json::Value = request.body_json().unwrap();
                    if updating {
                        accepted["team_id"] = serde_json::json!(immutable_id);
                        accepted["revision"] = serde_json::json!(accepted["expected_revision"].as_u64().unwrap() + 1);
                        accepted.as_object_mut().unwrap().remove("expected_revision");
                    } else {
                        accepted["revision"] = serde_json::json!(1);
                    }
                    accepted["user_id"] = serde_json::json!("u");
                    ResponseTemplate::new(200).set_body_json(accepted)
                }).expect(1).mount(&server).await;
            handle_team_command(command, &api, None, &mut state).await.unwrap();
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), if updating { 2 } else { 1 });
            if updating {
                let input: UpdateTeam = requests[1].body_json().unwrap();
                assert_eq!(input.expected_revision, current.revision);
                assert_eq!(input.name, current.name);
                assert_eq!(input.description, current.description);
                if index == 1 { assert_eq!(input.members, current.members); }
                else {
                    assert_eq!(input.context, current.context);
                    assert_eq!(input.members.len(), 1);
                    assert!(uuid::Uuid::parse_str(&input.members[0].agent_id).is_ok());
                }
                current.context = input.context;
                current.members = input.members;
                current.revision += 1;
            } else {
                let input: CreateTeam = requests[0].body_json().unwrap();
                assert!(uuid::Uuid::parse_str(&input.team_id).is_ok());
                assert!(input.members.is_empty());
                assert!(input.context.is_empty());
                current.team_id = input.team_id;
                current.revision = 1;
            }
            server.verify().await;
            server.reset().await;
        }
        for command in [
            "add-member test lead --max-delegation-depth 0",
            "add-member test lead --max-delegation-depth 3",
        ] {
            assert!(crate::cli::command_router::parse_team_bridge_command(command).is_err());
        }
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[serial_test::serial]
    #[tokio::test]
    async fn snapshot_and_cold_restore_preserve_owner_identity_and_current_revision() {
        let _creds_guard = crate::tests::isolate_credentials();
        let _env_guard = crate::test_utils::ProcessEnvGuard::remove("ASTRA_ACCESS_TOKEN");
        let server = MockServer::start().await;
        let (api, mut state, _identity) = http_session(&server);
        let mut saved = make_team(&["first"]);
        saved.context.insert("contract".into(), "before".into());
        saved.members[0].mcp_servers = vec!["fixture-mcp".into()];
        saved.members[0].can_delegate = true;
        saved.members[0].max_delegation_depth = 2;
        let snapshot = serde_json::json!({
            "snapshot_id": "snap-server-identity", "team_id": saved.team_id, "team_name": saved.name,
            "label": "accepted-label", "git_commit": "not-a-checkout-target",
            "session_id": null, "team_definition_json": serde_json::to_string(&saved).unwrap(),
            "created_at": "2026-10-03T00:00:00Z"
        });
        let snapshot_path = format!("/teams/{}/snapshots", saved.team_id);
        mock_named_team(&server, &saved).await;
        Mock::given(method("POST"))
            .and(path(&snapshot_path))
            .and(body_partial_json(
                serde_json::json!({"label": "local-label"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(&snapshot))
            .expect(1)
            .mount(&server)
            .await;
        let head_before = git_head_sha();
        handle_team_command("snapshot test local-label", &api, None, &mut state)
            .await
            .unwrap();
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        server.verify().await;
        server.reset().await;

        let mut current = saved.clone();
        current.name = "renamed".into();
        current.revision = 9;
        current.members = make_team(&["second"]).members;
        current.context.insert("contract".into(), "after".into());
        let mut update = UpdateTeam::from(&saved);
        update.expected_revision = current.revision;
        let mut accepted = saved.clone();
        accepted.revision = current.revision + 1;
        // A cold session reads current configuration once; historical revision is not the CAS.
        let (_, mut cold_state, _cold_identity) = http_session(&server);
        for status in [200, 409, 503] {
            mock_named_team(&server, &current).await;
            Mock::given(method("GET"))
                .and(path("/teams/snapshots/snap-server-identity"))
                .respond_with(ResponseTemplate::new(200).set_body_json(&snapshot))
                .expect(1)
                .mount(&server)
                .await;
            Mock::given(method("PUT"))
                .and(path(format!("/teams/{}", current.team_id)))
                .and(body_json(serde_json::to_value(&update).unwrap()))
                .respond_with(ResponseTemplate::new(status).set_body_json(&accepted))
                .expect(1)
                .mount(&server)
                .await;
            let result = handle_team_command(
                "restore renamed snap-server-identity",
                &api,
                None,
                &mut cold_state,
            )
            .await;
            assert_eq!(result.is_ok(), status == 200, "status={status}");
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                3,
                "no retry or readback"
            );
            assert_eq!(
                git_head_sha(),
                head_before,
                "restore must not check out snapshot Git state"
            );
            server.verify().await;
            server.reset().await;
        }

        // Same-name recreation / foreign snapshots / malformed saved definitions never write.
        let mut foreign = saved.clone();
        foreign.user_id = "another-owner".into();
        let mut recreated = saved.clone();
        recreated.team_id = "deleted-team-id".into();
        for (returned_id, envelope_id, definition) in [
            (
                "different-snapshot",
                current.team_id.as_str(),
                Some(serde_json::to_string(&saved).unwrap()),
            ),
            ("snap-server-identity", current.team_id.as_str(), None),
            (
                "snap-server-identity",
                current.team_id.as_str(),
                Some("{".into()),
            ),
            (
                "snap-server-identity",
                current.team_id.as_str(),
                Some(serde_json::to_string(&foreign).unwrap()),
            ),
            (
                "snap-server-identity",
                current.team_id.as_str(),
                Some(serde_json::to_string(&recreated).unwrap()),
            ),
            (
                "snap-server-identity",
                "deleted-team-id",
                Some(serde_json::to_string(&saved).unwrap()),
            ),
        ] {
            let mut invalid = snapshot.clone();
            invalid["snapshot_id"] = serde_json::json!(returned_id);
            invalid["team_id"] = serde_json::json!(envelope_id);
            invalid["team_definition_json"] = serde_json::json!(definition);
            mock_named_team(&server, &current).await;
            Mock::given(method("GET"))
                .and(path("/teams/snapshots/snap-server-identity"))
                .respond_with(ResponseTemplate::new(200).set_body_json(invalid))
                .expect(1)
                .mount(&server)
                .await;
            assert!(
                handle_team_command(
                    "restore renamed snap-server-identity",
                    &api,
                    None,
                    &mut cold_state
                )
                .await
                .is_err()
            );
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), 2);
            assert!(
                requests
                    .iter()
                    .all(|request| request.method.as_str() == "GET")
            );
            server.verify().await;
            server.reset().await;
        }

        mock_named_team(&server, &current).await;
        Mock::given(method("POST"))
            .and(path(&snapshot_path))
            .respond_with(ResponseTemplate::new(503))
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            handle_team_command("snapshot renamed failed", &api, None, &mut cold_state)
                .await
                .is_err()
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
        server.verify().await;
    }

    // ── Coordination tests ──────────────────────────────────────────

    fn make_team(roles: &[&str]) -> Team {
        Team {
            team_id: uuid::Uuid::new_v4().to_string(),
            user_id: "u".into(),
            name: "test".into(),
            description: "test team".into(),
            members: roles
                .iter()
                .map(|r| TeamMember {
                    role: r.to_string(),
                    agent_id: format!("member-{r}"),
                    system_prompt: Some(format!("{r} agent")),
                    skills: vec![],
                    model_selection: None,
                    mcp_servers: vec![],
                    can_delegate: false,
                    max_delegation_depth: 0,
                    ..Default::default()
                })
                .collect(),
            context: HashMap::new(),
            revision: 1,
        }
    }

    // ── Snapshot tests ─────────────────────────────────

    #[test]
    fn git_head_sha_returns_some_in_git_repo() {
        // This test runs inside a git repo, so should return Some
        let sha = git_head_sha();
        assert!(sha.is_some(), "Expected Some(sha) in a git repo");
        let sha = sha.unwrap();
        assert!(sha.len() >= 7, "SHA too short: {}", sha);
    }

    #[serial_test::serial]
    #[test]
    fn team_subcommands_hint_mentions_run_and_restore() {
        let hint = team_subcommands_hint();
        assert!(hint.contains("run"));
        assert!(hint.contains("restore"));
        assert!(hint.contains("help"));
    }

    // ── New feature tests ───────────────────────────────────────

    #[serial_test::serial]
    #[tokio::test]
    async fn native_team_lead_selection_uses_permissions_and_one_configuration_read() {
        let _creds_guard = crate::tests::isolate_credentials();
        let server = wiremock::MockServer::start().await;
        let (api, _state, _identity) = http_session(&server);
        for (permissions, requested, expected) in [
            ([false, true], None, Some("member-1")),
            ([false, false], None, None),
            ([true, true], None, None),
            ([false, true], Some("member-0"), Some("member-0")),
            ([true, true], Some("member-1"), Some("member-1")),
            ([false, true], Some("missing"), None),
            ([false, true], Some(""), None),
        ] {
            let mut team = make_team(&["coordinator-looking", "ordinary-looking"]);
            for (index, member) in team.members.iter_mut().enumerate() {
                member.agent_id = format!("member-{index}");
                member.can_delegate = permissions[index];
                member.max_delegation_depth = u32::from(permissions[index]);
            }
            wiremock::Mock::given(wiremock::matchers::method("GET"))
                .and(wiremock::matchers::path("/teams/name/test"))
                .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(&team))
                .expect(1)
                .mount(&server)
                .await;
            let result =
                super::resolve_team_run_chat_request(&api, None, "test", requested, " task ").await;
            match expected {
                Some(id) => {
                    let request = result.unwrap();
                    assert_eq!(request.selection.team_id, team.team_id);
                    assert_eq!(request.selection.lead_agent_id.as_deref(), Some(id));
                    assert_eq!(request.message, "task");
                }
                None => assert!(
                    result.is_err(),
                    "ambiguous or invalid selection must not launch"
                ),
            }
            assert_eq!(server.received_requests().await.unwrap().len(), 1);
            server.reset().await;
        }
    }
}
