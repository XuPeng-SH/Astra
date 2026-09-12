//! Derive pending graph application from immutable admission and accepted
//! proposal facts inside the same snapshot transaction used by assignment.

use super::{
    InternalSessionId, WorkEstablishmentMutationGroup, WorkItemDeliveryStatus, WorkOwnerId,
    WorkRepositoryError, WorkTaskExecutionSnapshot, compile_work_establishment_plan,
    decode_work_establishment_payload,
};
use sqlx::{MySql, QueryBuilder, Row, Transaction};
use std::collections::BTreeSet;

pub(super) async fn load_graph_mutation_barrier(
    tx: &mut Transaction<'_, MySql>,
    owner: &WorkOwnerId,
    session: &InternalSessionId,
    snapshot: &WorkTaskExecutionSnapshot,
) -> Result<(Vec<WorkEstablishmentMutationGroup>, bool), WorkRepositoryError> {
    // The existing owner/session recovery index selects this one genesis.
    // Completed establishment is still the immutable owner of future changes.
    let rows = sqlx::query(
        "SELECT operation_id, payload_json FROM work_establishment_operations
         WHERE owner_id = ? AND session_id = ? AND work_id = ? AND branch_id = ?
           AND operation_state IN ('pending', 'complete')
         ORDER BY operation_id LIMIT 2",
    )
    .bind(owner.as_str())
    .bind(session.as_str())
    .bind(snapshot.basis().work_id.as_str())
    .bind(snapshot.basis().branch_id.as_str())
    .fetch_all(&mut **tx)
    .await
    .map_err(|e| WorkRepositoryError::persistence("load Work mutation schedules", e))?;
    let corrupt = |message: String| {
        WorkRepositoryError::corrupt("Work mutation schedule", std::io::Error::other(message))
    };
    if rows.len() > 1 {
        return Err(corrupt(
            "too many establishment schedules for one Work branch".into(),
        ));
    }
    let mut pending = Vec::new();
    let mut has_unapplied = false;
    for row in rows {
        let operation_id: String = row
            .try_get("operation_id")
            .map_err(|e| WorkRepositoryError::corrupt("Work mutation schedule identity", e))?;
        let payload: String = row
            .try_get("payload_json")
            .map_err(|e| WorkRepositoryError::corrupt("Work mutation schedule payload", e))?;
        let (_, decision) = decode_work_establishment_payload(&payload).map_err(&corrupt)?;
        let Some(decision) = decision else {
            continue;
        };
        if decision.deferred_graph_mutations().is_empty() {
            continue;
        }
        let (_, tasks) = decision
            .initial_work_plan()
            .ok_or_else(|| corrupt("scheduled mutations require an initial Work graph".into()))?;
        let plan = compile_work_establishment_plan(
            &operation_id,
            tasks,
            decision.deferred_graph_mutations(),
        )
        .map_err(&corrupt)?;
        // Initial graph materialization has its own establishment recovery
        // phase; a mutation must never run against a partial genesis.
        let initial_graph_present = plan.initial_items.iter().all(|initial| {
            snapshot
                .items()
                .iter()
                .any(|item| item.item_id.as_str() == initial.item_id)
        });
        let proposal_ids = plan
            .mutation_groups
            .iter()
            .map(|group| {
                group
                    .proposal_id(
                        owner.as_str(),
                        session.as_str(),
                        snapshot.basis().work_id.as_str(),
                        snapshot.basis().branch_id.as_str(),
                    )
                    .map_err(&corrupt)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut query =
            QueryBuilder::<MySql>::new("SELECT proposal_id FROM work_proposals WHERE owner_id = ");
        query
            .push_bind(owner.as_str())
            .push(" AND work_id = ")
            .push_bind(snapshot.basis().work_id.as_str())
            .push(" AND branch_id = ")
            .push_bind(snapshot.basis().branch_id.as_str())
            .push(" AND proposal_kind = 'plan_patch' AND status = 'accepted' AND proposal_id IN (");
        let mut ids = query.separated(", ");
        for id in &proposal_ids {
            ids.push_bind(id.as_str());
        }
        ids.push_unseparated(")");
        let accepted = query
            .build_query_scalar::<String>()
            .fetch_all(&mut **tx)
            .await
            .map_err(|e| WorkRepositoryError::persistence("load applied Work mutation markers", e))?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let trigger_items = plan
            .mutation_groups
            .iter()
            .zip(&proposal_ids)
            .filter(|(_, id)| !accepted.contains(id.as_str()))
            .flat_map(|(group, _)| group.trigger_items.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        // Trigger delivery belongs to the immutable initial revision. A later
        // retirement must not erase a receipt already satisfying another group.
        let (_, deliveries) = super::plan_context_repository::load_item_executions(
            tx,
            owner,
            &snapshot.basis().work_id,
            &snapshot.basis().branch_id,
            &trigger_items,
        )
        .await?;
        for (group, proposal_id) in plan.mutation_groups.into_iter().zip(proposal_ids) {
            if accepted.contains(proposal_id.as_str()) {
                continue;
            }
            has_unapplied = true;
            if !initial_graph_present {
                continue;
            }
            let all_delivered = group.trigger_items.iter().all(|trigger| {
                deliveries
                    .get(trigger)
                    .is_some_and(|delivery| delivery.status == WorkItemDeliveryStatus::Delivered)
            });
            if all_delivered {
                pending.push(group);
            }
        }
    }
    Ok((pending, has_unapplied))
}
