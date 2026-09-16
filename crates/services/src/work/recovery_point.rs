//! Durable storage for immutable Work recovery points.
//!
//! This repository owns the recovery-point record only. Edge/User Runner code
//! captures files and uploads content; this layer validates the declared
//! manifest and stores one owner-scoped, idempotent `preparing` record.
//! A canonical verifier can advance it to `captured` after proving the
//! immutable Work/Session basis and the current execution identity. `captured`
//! is a logical progress boundary, not a promise that code, artifacts, or an
//! unfinished Run can be restored. A future `ready` state requires a separate
//! verifier for those durable payloads and effect receipts.

use astra_core::SharedPool;
use astra_turn_types::{
    RecoveryPointBindingStateV1, RecoveryPointExecutionBindingV1, RecoveryPointExecutorKindV1,
    RecoveryPointManifestV1, SessionKeyV1,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::{MySql, QueryBuilder, Row, query};

use crate::session_context_coordinator::{
    SessionContextCoordinatorError, SessionExecutionBindingStateV1, SessionExecutionBindingV1,
    lock_recovery_context_in_transaction,
};

use super::plan_context_repository::load_recovery_basis_in_transaction;
use super::repository::{DatabaseWorkRepository, WorkConflictResource, WorkRepositoryError};
use super::{WorkBranchId, WorkChangeRef, WorkContentHash, WorkId, WorkOwnerId};

pub const WORK_RECOVERY_POINT_SCHEMA_VERSION: u32 = 1;
const RECOVERY_POINT_ID_MAX_BYTES: usize = 128;
const RECOVERY_POINT_PAGE_MAX_ITEMS: u16 = 256;
const REQUEST_HASH_SCHEMA_VERSION: u16 = 1;
const RECOVERY_POINT_SELECT_SQL: &str =
    "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
            request_hash, status, manifest_json, manifest_hash, failure_reason,
            DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
            DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
            DATE_FORMAT(ready_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS ready_at
     FROM work_recovery_points";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkRecoveryPointStatus {
    Preparing,
    Captured,
    Ready,
    Failed,
    Aborted,
}

impl WorkRecoveryPointStatus {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "preparing" => Some(Self::Preparing),
            "captured" => Some(Self::Captured),
            "ready" => Some(Self::Ready),
            "failed" => Some(Self::Failed),
            "aborted" => Some(Self::Aborted),
            _ => None,
        }
    }
}

/// A declared capture request.  Recording it is durable progress for an
/// upload, not proof that the capture can be restored.  The repository keeps
/// the status `preparing` until a canonical publication verifier exists.
#[derive(Debug, Clone)]
pub struct NewWorkRecoveryPoint {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub request_id: WorkChangeRef,
    pub manifest: RecoveryPointManifestV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkRecoveryPointRecord {
    pub schema_version: u32,
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: WorkBranchId,
    pub recovery_point_id: String,
    pub request_id: WorkChangeRef,
    pub request_hash: WorkContentHash,
    pub status: WorkRecoveryPointStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RecoveryPointManifestV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_hash: Option<WorkContentHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkRecoveryPointQuery {
    pub owner_id: WorkOwnerId,
    pub work_id: WorkId,
    pub branch_id: Option<WorkBranchId>,
    pub limit: u16,
}

impl WorkRecoveryPointQuery {
    pub fn new(owner_id: WorkOwnerId, work_id: WorkId) -> Self {
        Self {
            owner_id,
            work_id,
            branch_id: None,
            limit: 32,
        }
    }

    pub fn branch(mut self, branch_id: WorkBranchId) -> Self {
        self.branch_id = Some(branch_id);
        self
    }

    pub fn limit(mut self, limit: u16) -> Self {
        self.limit = limit;
        self
    }
}

#[derive(Clone, Debug)]
pub struct DatabaseWorkRecoveryPointRepository {
    pool: SharedPool,
}

impl DatabaseWorkRecoveryPointRepository {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Record a declared capture idempotently.  The Work and branch identity
    /// are checked in the same transaction as the insert, while the immutable
    /// manifest hash prevents a request id from being reused for a different
    /// capture.  This deliberately returns `preparing`; it must not expose a
    /// caller-supplied manifest as a restorable `ready` point.
    pub async fn record_preparing(
        &self,
        request: NewWorkRecoveryPoint,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        validate_request(&request)?;
        let manifest_hash = manifest_hash(&request.manifest)?;
        let request_hash = request_hash(&request, &manifest_hash)?;

        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin Work recovery point capture", source)
        })?;

        let branch_row = query(
            "SELECT b.session_id
             FROM works w
             INNER JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.branch_id = ?
             WHERE w.owner_id = ? AND w.work_id = ?
             LIMIT 1
             FOR UPDATE",
        )
        .bind(request.branch_id.as_str())
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("lock Work recovery point owner", source)
        })?;
        let Some(branch_row) = branch_row else {
            return Err(WorkRepositoryError::NotFound);
        };
        let branch_session_id = branch_row
            .try_get::<String, _>("session_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
        if branch_session_id != request.manifest.session_key.session_id {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }

        if let Some(row) = query(
            "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
                    request_hash, status, manifest_json, manifest_hash, failure_reason,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                    DATE_FORMAT(ready_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS ready_at
             FROM work_recovery_points
             WHERE owner_id = ? AND work_id = ? AND request_id = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.request_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load Work recovery point request", source)
        })? {
            let stored_hash = row
                .try_get::<String, _>("request_hash")
                .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
            if stored_hash != request_hash.as_str() {
                return Err(WorkRepositoryError::Conflict {
                    resource: WorkConflictResource::RecoveryPointRequest,
                });
            }
            let record = decode_record(row)?;
            transaction.commit().await.map_err(|source| {
                WorkRepositoryError::persistence("commit idempotent Work recovery point", source)
            })?;
            return Ok(record);
        }

        query(
            "INSERT INTO work_recovery_points
             (owner_id, work_id, branch_id, recovery_point_id, request_id,
              request_hash, status, manifest_json, manifest_hash, failure_reason,
              created_at, updated_at, ready_at)
             VALUES (?, ?, ?, ?, ?, ?, 'preparing', ?, ?, NULL, NOW(6), NOW(6), NULL)",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.branch_id.as_str())
        .bind(request.manifest.recovery_point_id.as_str())
        .bind(request.request_id.as_str())
        .bind(request_hash.as_str())
        .bind(serde_json::to_string(&request.manifest).map_err(|source| {
            WorkRepositoryError::ManifestEncoding {
                entity: "Work recovery point manifest",
                source,
            }
        })?)
        .bind(manifest_hash.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::insert(
                "record Work recovery point capture",
                WorkConflictResource::RecoveryPointIdentity,
                source,
            )
        })?;

        let row = query(
            "SELECT owner_id, work_id, branch_id, recovery_point_id, request_id,
                    request_hash, status, manifest_json, manifest_hash, failure_reason,
                    DATE_FORMAT(created_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS created_at,
                    DATE_FORMAT(updated_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS updated_at,
                    DATE_FORMAT(ready_at, '%Y-%m-%dT%H:%i:%s.%fZ') AS ready_at
             FROM work_recovery_points
             WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ?
             LIMIT 1",
        )
        .bind(request.owner_id.as_str())
        .bind(request.work_id.as_str())
        .bind(request.manifest.recovery_point_id.as_str())
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load recorded Work recovery point", source)
        })?;
        let record = decode_record(row)?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit Work recovery point capture", source)
        })?;
        Ok(record)
    }

    /// Canonically capture the logical Work boundary represented by a
    /// preparing row. The caller supplies only the row identity; all
    /// authoritative revisions, context-head facts, and execution-binding
    /// identity are read while the Work, branch, recovery row, and Session
    /// context are locked in that order. No filesystem, Artifact, or active
    /// Run claim is made here, so the result is intentionally `captured` and
    /// carries no restore/continue capability by itself.
    pub async fn mark_captured(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        branch_id: &WorkBranchId,
        recovery_point_id: &str,
    ) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
        if recovery_point_id.is_empty()
            || recovery_point_id.len() > RECOVERY_POINT_ID_MAX_BYTES
            || recovery_point_id
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery point",
                std::io::Error::other("invalid recovery point identity"),
            ));
        }

        let mut transaction = self.pool.get().begin().await.map_err(|source| {
            WorkRepositoryError::persistence("begin Work recovery point capture", source)
        })?;

        // Keep this lock order identical to capture admission and branch
        // deletion: Work/branch first, then the recovery row. Reversing it
        // would allow a publisher and deletion executor to deadlock.
        let branch_row = query(
            "SELECT b.session_id, b.branch_revision, b.goal_revision_ref,
                    b.criteria_set_revision_ref, b.current_graph_revision,
                    b.deletion_operation_id,
                    CASE WHEN b.archived_at IS NULL THEN 0 ELSE 1 END AS branch_archived,
                    CASE WHEN w.archived_at IS NULL THEN 0 ELSE 1 END AS work_archived
             FROM works w
             INNER JOIN work_branches b
               ON b.owner_id = w.owner_id AND b.work_id = w.work_id
              AND b.branch_id = ?
             WHERE w.owner_id = ? AND w.work_id = ?
             LIMIT 1
             FOR UPDATE",
        )
        .bind(branch_id.as_str())
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("lock Work recovery capture basis", source)
        })?
        .ok_or(WorkRepositoryError::NotFound)?;
        let branch_session_id =
            branch_row
                .try_get::<String, _>("session_id")
                .map_err(|source| {
                    WorkRepositoryError::corrupt("Work recovery capture basis", source)
                })?;
        let branch_archived =
            branch_row
                .try_get::<i64, _>("branch_archived")
                .map_err(|source| {
                    WorkRepositoryError::corrupt("Work recovery capture basis", source)
                })?
                != 0;
        let work_archived = branch_row
            .try_get::<i64, _>("work_archived")
            .map_err(|source| {
                WorkRepositoryError::corrupt("Work recovery capture basis", source)
            })?
            != 0;
        if work_archived || branch_archived {
            return Err(WorkRepositoryError::Archived);
        }
        if branch_row
            .try_get::<Option<String>, _>("deletion_operation_id")
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery capture basis", source))?
            .is_some()
        {
            return Err(WorkRepositoryError::BranchDeleting);
        }

        let recovery_row = query(&format!(
            "{RECOVERY_POINT_SELECT_SQL}
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ?
             LIMIT 1 FOR UPDATE"
        ))
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(recovery_point_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|source| WorkRepositoryError::persistence("lock Work recovery point", source))?
        .ok_or(WorkRepositoryError::NotFound)?;
        let record = decode_record(recovery_row)?;
        if record.status == WorkRecoveryPointStatus::Captured {
            transaction.commit().await.map_err(|source| {
                WorkRepositoryError::persistence("commit idempotent Work recovery capture", source)
            })?;
            return Ok(record);
        }
        if record.status != WorkRecoveryPointStatus::Preparing {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }
        let manifest = record.manifest.as_ref().ok_or_else(|| {
            WorkRepositoryError::corrupt(
                "Work recovery point",
                std::io::Error::other("preparing recovery point has no manifest"),
            )
        })?;
        if manifest.session_key.session_id != branch_session_id {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }

        let basis =
            load_recovery_basis_in_transaction(&mut transaction, owner_id, work_id, branch_id)
                .await?;
        if !revisions_match_manifest(&basis, manifest) {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: "the Work or branch revisions changed since capture began",
            });
        }
        if manifest.run.is_some() {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: "Run frontier verification is not available yet",
            });
        }

        let context = lock_recovery_context_in_transaction(&mut transaction, &manifest.session_key)
            .await
            .map_err(map_context_verification_error)?;
        if context.head != manifest.context_head || context.head.cursor != manifest.session_cursor {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: "the Session context head changed since capture began",
            });
        }
        if context.has_active_reservation {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: "the Session still has an active turn reservation",
            });
        }
        if context.has_unresolved_invocations {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: "the Session has an unresolved tool invocation",
            });
        }
        let current_execution =
            canonical_execution_binding(&manifest.session_key, context.execution_binding.as_ref())?;
        if current_execution != manifest.execution {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: "the Session execution binding changed since capture began",
            });
        }

        let updated = query(
            "UPDATE work_recovery_points
             SET status = 'captured', updated_at = NOW(6), failure_reason = NULL, ready_at = NULL
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ? AND status = 'preparing'",
        )
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(recovery_point_id)
        .execute(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("publish captured Work recovery point", source)
        })?;
        if updated.rows_affected() != 1 {
            return Err(WorkRepositoryError::Conflict {
                resource: WorkConflictResource::RecoveryPointIdentity,
            });
        }
        let captured_row = query(&format!(
            "{RECOVERY_POINT_SELECT_SQL}
             WHERE owner_id = ? AND work_id = ? AND branch_id = ?
               AND recovery_point_id = ?
             LIMIT 1"
        ))
        .bind(owner_id.as_str())
        .bind(work_id.as_str())
        .bind(branch_id.as_str())
        .bind(recovery_point_id)
        .fetch_one(&mut *transaction)
        .await
        .map_err(|source| {
            WorkRepositoryError::persistence("load captured Work recovery point", source)
        })?;
        let captured = decode_record(captured_row)?;
        transaction.commit().await.map_err(|source| {
            WorkRepositoryError::persistence("commit captured Work recovery point", source)
        })?;
        Ok(captured)
    }

    pub async fn load(
        &self,
        owner_id: &WorkOwnerId,
        work_id: &WorkId,
        recovery_point_id: &str,
    ) -> Result<Option<WorkRecoveryPointRecord>, WorkRepositoryError> {
        let sql = format!(
            "{RECOVERY_POINT_SELECT_SQL} WHERE owner_id = ? AND work_id = ? AND recovery_point_id = ? LIMIT 1"
        );
        let row = query(&sql)
            .bind(owner_id.as_str())
            .bind(work_id.as_str())
            .bind(recovery_point_id)
            .fetch_optional(self.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("load Work recovery point", source)
            })?;
        row.map(decode_record).transpose()
    }

    pub async fn list(
        &self,
        query_value: WorkRecoveryPointQuery,
    ) -> Result<Vec<WorkRecoveryPointRecord>, WorkRepositoryError> {
        if query_value.limit == 0 || query_value.limit > RECOVERY_POINT_PAGE_MAX_ITEMS {
            return Err(WorkRepositoryError::corrupt(
                "Work recovery point query",
                std::io::Error::other(format!(
                    "limit must be between 1 and {RECOVERY_POINT_PAGE_MAX_ITEMS}"
                )),
            ));
        }
        let mut builder = QueryBuilder::<MySql>::new(RECOVERY_POINT_SELECT_SQL);
        builder
            .push(" WHERE owner_id = ")
            .push_bind(query_value.owner_id.as_str())
            .push(" AND work_id = ")
            .push_bind(query_value.work_id.as_str());
        if let Some(branch_id) = &query_value.branch_id {
            builder
                .push(" AND branch_id = ")
                .push_bind(branch_id.as_str());
        }
        builder
            .push(" ORDER BY created_at DESC, recovery_point_id DESC LIMIT ")
            .push_bind(i64::from(query_value.limit));
        let rows = builder
            .build()
            .fetch_all(self.pool.get())
            .await
            .map_err(|source| {
                WorkRepositoryError::persistence("list Work recovery points", source)
            })?;
        rows.into_iter().map(decode_record).collect()
    }
}

impl DatabaseWorkRepository {
    pub fn recovery_points(&self) -> DatabaseWorkRecoveryPointRepository {
        DatabaseWorkRecoveryPointRepository::new(self.pool.clone())
    }
}

fn revisions_match_manifest(
    basis: &super::WorkPlanBasis,
    manifest: &RecoveryPointManifestV1,
) -> bool {
    u64::try_from(basis.work_revision.get()).ok() == Some(manifest.work_revision)
        && u64::try_from(basis.branch_revision.get()).ok() == Some(manifest.branch_revision)
        && u64::try_from(basis.graph_revision.get()).ok() == Some(manifest.graph_revision)
        && u64::try_from(basis.goal_revision.get()).ok() == Some(manifest.goal_revision)
        && u64::try_from(basis.criteria_set_revision.get()).ok()
            == Some(manifest.criteria_set_revision)
        && basis.branch_goal_revision == basis.goal_revision
        && basis.branch_criteria_set_revision == basis.criteria_set_revision
}

fn canonical_execution_binding(
    key: &SessionKeyV1,
    current: Option<&SessionExecutionBindingV1>,
) -> Result<RecoveryPointExecutionBindingV1, WorkRepositoryError> {
    let logical_workspace_id = format!("session:{}:branch:{}", key.session_id, key.branch_id);
    let fallback = SessionExecutionBindingV1::server_work_default(logical_workspace_id);
    let current = current.unwrap_or(&fallback);
    let binding_state = match current.state {
        SessionExecutionBindingStateV1::Ready => RecoveryPointBindingStateV1::Ready,
        SessionExecutionBindingStateV1::Switching => RecoveryPointBindingStateV1::Switching,
        SessionExecutionBindingStateV1::NeedsAttention => {
            RecoveryPointBindingStateV1::NeedsAttention
        }
    };
    let (executor_kind, executor_id) = match current.executor.kind {
        crate::runs::ExecutorBindingRequestKind::ServerLocal => {
            (RecoveryPointExecutorKindV1::Server, "server".to_owned())
        }
        crate::runs::ExecutorBindingRequestKind::EdgeAgent => (
            RecoveryPointExecutorKindV1::Edge,
            current.executor.executor_id.clone().ok_or(
                WorkRepositoryError::RecoveryPointNotCapturable {
                    reason: "the current Edge executor has no canonical identity",
                },
            )?,
        ),
        _ => {
            return Err(WorkRepositoryError::RecoveryPointNotCapturable {
                reason: "the current executor kind is not recoverable",
            });
        }
    };
    let mut binding = RecoveryPointExecutionBindingV1 {
        binding_generation: current.generation,
        binding_state,
        logical_workspace_id: current.logical_workspace_id.clone(),
        executor_kind,
        executor_id,
        binding_hash: String::new(),
        physical_workspace_id: current.physical_workspace_id.clone(),
    };
    binding.binding_hash = binding.content_hash();
    Ok(binding)
}

fn map_context_verification_error(error: SessionContextCoordinatorError) -> WorkRepositoryError {
    match error {
        SessionContextCoordinatorError::Database { operation, source } => {
            WorkRepositoryError::persistence(operation, source)
        }
        SessionContextCoordinatorError::DatabaseJson { entity, source } => {
            WorkRepositoryError::ManifestEncoding { entity, source }
        }
        _ => WorkRepositoryError::RecoveryPointNotCapturable {
            reason: "the canonical Session context is missing or requires repair",
        },
    }
}

#[derive(Serialize)]
struct RequestHashInput<'a> {
    schema_version: u16,
    owner_id: &'a WorkOwnerId,
    work_id: &'a WorkId,
    branch_id: &'a WorkBranchId,
    request_id: &'a WorkChangeRef,
    manifest_hash: &'a WorkContentHash,
}

fn validate_request(request: &NewWorkRecoveryPoint) -> Result<(), WorkRepositoryError> {
    request
        .manifest
        .validate()
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point manifest", source))?;
    if request.manifest.owner_id != request.owner_id.as_str()
        || request.manifest.work_id != request.work_id.as_str()
        || request.manifest.branch_id != request.branch_id.as_str()
    {
        return Err(WorkRepositoryError::Conflict {
            resource: WorkConflictResource::RecoveryPointIdentity,
        });
    }
    if request.manifest.recovery_point_id.len() > RECOVERY_POINT_ID_MAX_BYTES {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery point manifest",
            std::io::Error::other("recovery_point_id exceeds storage width"),
        ));
    }
    Ok(())
}

fn manifest_hash(
    manifest: &RecoveryPointManifestV1,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let value = manifest
        .content_hash()
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point manifest", source))?;
    WorkContentHash::parse(value).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work recovery point manifest hash",
            std::io::Error::other(source),
        )
    })
}

fn request_hash(
    request: &NewWorkRecoveryPoint,
    manifest_hash: &WorkContentHash,
) -> Result<WorkContentHash, WorkRepositoryError> {
    let payload = serde_json::to_vec(&RequestHashInput {
        schema_version: REQUEST_HASH_SCHEMA_VERSION,
        owner_id: &request.owner_id,
        work_id: &request.work_id,
        branch_id: &request.branch_id,
        request_id: &request.request_id,
        manifest_hash,
    })
    .map_err(|source| WorkRepositoryError::ManifestEncoding {
        entity: "Work recovery point request",
        source,
    })?;
    WorkContentHash::parse(format!("sha256:{:x}", Sha256::digest(payload))).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work recovery point request hash",
            std::io::Error::other(source),
        )
    })
}

fn decode_record(
    row: sqlx::mysql::MySqlRow,
) -> Result<WorkRecoveryPointRecord, WorkRepositoryError> {
    let text = |field: &'static str| {
        row.try_get::<String, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))
    };
    let optional_text = |field: &'static str| {
        row.try_get::<Option<String>, _>(field)
            .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))
    };
    let owner_id = WorkOwnerId::parse(text("owner_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let work_id = WorkId::parse(text("work_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let branch_id = WorkBranchId::parse(text("branch_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let request_id = WorkChangeRef::parse(text("request_id")?)
        .map_err(|source| WorkRepositoryError::corrupt("Work recovery point", source))?;
    let status_name = text("status")?;
    let status = WorkRecoveryPointStatus::parse(&status_name).ok_or_else(|| {
        WorkRepositoryError::corrupt(
            "Work recovery point",
            std::io::Error::other("unknown recovery point status"),
        )
    })?;
    let request_hash = WorkContentHash::parse(text("request_hash")?).map_err(|source| {
        WorkRepositoryError::corrupt(
            "Work recovery point request hash",
            std::io::Error::other(source),
        )
    })?;
    let manifest_hash = optional_text("manifest_hash")?
        .map(|value| {
            WorkContentHash::parse(value).map_err(|source| {
                WorkRepositoryError::corrupt(
                    "Work recovery point manifest hash",
                    std::io::Error::other(source),
                )
            })
        })
        .transpose()?;
    let recovery_point_id = text("recovery_point_id")?;
    if recovery_point_id.len() > RECOVERY_POINT_ID_MAX_BYTES
        || recovery_point_id.is_empty()
        || recovery_point_id
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery point",
            std::io::Error::other("invalid recovery point identity"),
        ));
    }
    let manifest = optional_text("manifest_json")?
        .map(|value| {
            let manifest: RecoveryPointManifestV1 =
                serde_json::from_str(&value).map_err(|source| {
                    WorkRepositoryError::corrupt("Work recovery point manifest", source)
                })?;
            manifest.validate().map_err(|source| {
                WorkRepositoryError::corrupt("Work recovery point manifest", source)
            })?;
            if manifest.owner_id != owner_id.as_str()
                || manifest.work_id != work_id.as_str()
                || manifest.branch_id != branch_id.as_str()
                || manifest.recovery_point_id != recovery_point_id
            {
                return Err(WorkRepositoryError::corrupt(
                    "Work recovery point manifest",
                    std::io::Error::other("manifest identity does not match storage owner"),
                ));
            }
            if manifest_hash.as_ref().is_none_or(|expected| {
                manifest
                    .content_hash()
                    .ok()
                    .and_then(|hash| WorkContentHash::parse(hash).ok())
                    .as_ref()
                    != Some(expected)
            }) {
                return Err(WorkRepositoryError::corrupt(
                    "Work recovery point manifest",
                    std::io::Error::other("manifest hash does not match manifest content"),
                ));
            }
            Ok(manifest)
        })
        .transpose()?;
    if status == WorkRecoveryPointStatus::Ready && (manifest.is_none() || manifest_hash.is_none()) {
        return Err(WorkRepositoryError::corrupt(
            "Work recovery point",
            std::io::Error::other("ready recovery point has no complete manifest"),
        ));
    }
    let created_at = super::repository::decode_timestamp(
        "Work recovery point",
        "created_at",
        text("created_at")?,
    )?;
    let updated_at = super::repository::decode_timestamp(
        "Work recovery point",
        "updated_at",
        text("updated_at")?,
    )?;
    let ready_at = optional_text("ready_at")?
        .map(|value| super::repository::decode_timestamp("Work recovery point", "ready_at", value))
        .transpose()?;
    Ok(WorkRecoveryPointRecord {
        schema_version: WORK_RECOVERY_POINT_SCHEMA_VERSION,
        owner_id,
        work_id,
        branch_id,
        recovery_point_id,
        request_id,
        request_hash,
        status,
        manifest,
        manifest_hash,
        failure_reason: optional_text("failure_reason")?,
        created_at,
        updated_at,
        ready_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_turn_types::{
        RECOVERY_POINT_MANIFEST_SCHEMA_VERSION, RecoveryPointBindingStateV1,
        RecoveryPointExecutionBindingV1, RecoveryPointExecutorKindV1, RecoveryPointReasonV1,
        SessionCursorV1, SessionKeyV1,
    };

    #[test]
    fn query_defaults_to_a_small_user_facing_page() {
        let query = WorkRecoveryPointQuery::new(
            WorkOwnerId::parse("owner").unwrap(),
            WorkId::parse("work").unwrap(),
        );
        assert_eq!(query.limit, 32);
        assert!(query.branch_id.is_none());
    }

    #[test]
    fn request_hash_changes_when_manifest_changes() {
        let owner = WorkOwnerId::parse("owner").unwrap();
        let work = WorkId::parse("work").unwrap();
        let branch = WorkBranchId::parse("branch").unwrap();
        let session_key = SessionKeyV1::owner_session("tenant", "owner", "session", "main");
        let mut manifest = RecoveryPointManifestV1 {
            schema_version: RECOVERY_POINT_MANIFEST_SCHEMA_VERSION,
            recovery_point_id: "rp".into(),
            owner_id: "owner".into(),
            work_id: "work".into(),
            branch_id: "branch".into(),
            work_revision: 1,
            branch_revision: 1,
            graph_revision: 1,
            goal_revision: 1,
            criteria_set_revision: 1,
            session_key: session_key.clone(),
            session_cursor: SessionCursorV1 {
                schema_version: 1,
                owner_id: "owner".into(),
                session_id: "session".into(),
                branch_id: "main".into(),
                completed_turn: 1,
                journal_event_seq: 1,
                conversation_seq: 1,
                canonical_root_hash: "a".repeat(64),
                projection_schema: 1,
                compaction_generation: 0,
                config_version_id: None,
            },
            context_head: astra_turn_types::SessionContextHeadV1 {
                schema_version: 1,
                key: session_key.clone(),
                cursor: SessionCursorV1 {
                    schema_version: 1,
                    owner_id: "owner".into(),
                    session_id: "session".into(),
                    branch_id: "main".into(),
                    completed_turn: 1,
                    journal_event_seq: 1,
                    conversation_seq: 1,
                    canonical_root_hash: "a".repeat(64),
                    projection_schema: 1,
                    compaction_generation: 0,
                    config_version_id: None,
                },
                latest_manifest_root: "a".repeat(64),
                total_canonical_bytes: 1,
                total_message_count: 1,
                writer_epoch: 1,
            },
            run: None,
            execution: RecoveryPointExecutionBindingV1 {
                binding_generation: 1,
                binding_state: RecoveryPointBindingStateV1::Ready,
                logical_workspace_id: "workspace".into(),
                executor_kind: RecoveryPointExecutorKindV1::Server,
                executor_id: "server".into(),
                binding_hash: String::new(),
                physical_workspace_id: None,
            },
            workspace: None,
            artifacts: vec![],
            environment: Default::default(),
            reason: RecoveryPointReasonV1::RunSettled,
            created_at: "2026-09-16T00:00:00Z".into(),
        };
        manifest.execution.binding_hash = manifest.execution.content_hash();
        let first = NewWorkRecoveryPoint {
            owner_id: owner.clone(),
            work_id: work.clone(),
            branch_id: branch.clone(),
            request_id: WorkChangeRef::parse("request").unwrap(),
            manifest: manifest.clone(),
        };
        validate_request(&first).unwrap();
        let first_manifest_hash = manifest_hash(&manifest).unwrap();
        let first_hash = request_hash(&first, &first_manifest_hash).unwrap();
        manifest.created_at = "2026-09-16T00:00:01Z".into();
        let second = NewWorkRecoveryPoint {
            manifest,
            ..first.clone()
        };
        let second_manifest_hash = manifest_hash(&second.manifest).unwrap();
        let second_hash = request_hash(&second, &second_manifest_hash).unwrap();
        assert_ne!(first_hash, second_hash);
    }

    #[test]
    fn captured_status_is_decoded_as_a_logical_boundary() {
        assert_eq!(
            WorkRecoveryPointStatus::parse("captured"),
            Some(WorkRecoveryPointStatus::Captured)
        );
        assert_ne!(
            WorkRecoveryPointStatus::Captured,
            WorkRecoveryPointStatus::Ready
        );
    }

    #[test]
    fn missing_execution_binding_uses_the_canonical_server_default() {
        let key = SessionKeyV1::owner_session("tenant", "owner", "session", "branch");
        let binding = canonical_execution_binding(&key, None).expect("server default binding");
        assert_eq!(binding.executor_kind, RecoveryPointExecutorKindV1::Server);
        assert_eq!(binding.executor_id, "server");
        assert_eq!(
            binding.logical_workspace_id,
            "session:session:branch:branch"
        );
        assert_eq!(binding.binding_state, RecoveryPointBindingStateV1::Ready);
        assert_eq!(binding.binding_hash, binding.content_hash());
    }
}
