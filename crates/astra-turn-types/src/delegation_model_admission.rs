//! Trusted, invocation-local model requirements for delegated execution.
//! Tool arguments cannot construct this admission; the runtime attaches it
//! after interpreting an authoritative user intent and before durable dispatch.

use serde::{Deserialize, Serialize};

use crate::ModelSelection;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationModelInstructionSource {
    pub user_id: String,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub owner_generation: u64,
    pub control_epoch: usize,
    pub applied_intent_id: Option<String>,
    pub session_turn: u32,
    pub user_intent_digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationReasoningEffort {
    Low,
    Medium,
    High,
    Max,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationReasoningRequirement {
    ModelDefault,
    Off,
    Effort { effort: DelegationReasoningEffort },
    Budget { tokens: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationModelSlotConstraint {
    /// Zero-based index in the admitted canonical delegation batch.
    pub slot_index: u32,
    pub model_selection: Option<ModelSelection>,
    pub model_strength: Option<DelegationRequirementStrength>,
    pub reasoning: Option<DelegationReasoningRequirement>,
    pub reasoning_strength: Option<DelegationRequirementStrength>,
    /// Bounded source evidence for explaining task binding; never authority
    /// independent of the runtime-created source and exact slot identity.
    pub task_scope_quote: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationModelAdmissionOutcome {
    ExplicitlyUnconstrained {
        slot_count: u32,
    },
    Constrained {
        slots: Vec<DelegationModelSlotConstraint>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationModelAdmission {
    pub source: DelegationModelInstructionSource,
    pub invocation_id: String,
    /// Digest of exact canonical logical arguments before runtime correlation.
    pub arguments_digest: String,
    pub outcome: DelegationModelAdmissionOutcome,
    /// Trusted child-state projection for every canonical slot. It is frozen
    /// with the invocation rather than reconstructed from a later parent
    /// context snapshot or child prompt.
    pub child_requirements: Vec<DelegationIntentRequirements>,
}

/// Human instruction provenance survives child creation. It is deliberately
/// separate from the current run's dispatch epoch and invocation identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationUserRequirementSource {
    pub user_id: String,
    pub session_id: String,
    pub session_turn: u32,
    pub applied_intent_id: Option<String>,
    pub user_intent_digest: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRequirementPropagation {
    DirectChildren,
    Descendants,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegationRequirementStrength {
    Default,
    Hard,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DelegationIntentRequirement {
    pub requirement_id: String,
    pub model_selection: Option<ModelSelection>,
    pub reasoning: Option<DelegationReasoningRequirement>,
    /// A task-specific requirement still needs admitted applicability at each
    /// invocation; this quote alone never authorizes a slot assignment.
    pub task_scope_quote: Option<String>,
    pub propagation: DelegationRequirementPropagation,
    pub strength: DelegationRequirementStrength,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum DelegationIntentRequirements {
    #[default]
    Unassessed,
    Unconstrained {
        source: DelegationUserRequirementSource,
    },
    Unresolved {
        source: DelegationUserRequirementSource,
        reason: String,
    },
    Unavailable {
        source: DelegationUserRequirementSource,
        reason: String,
        attempts: u8,
    },
    Requirements {
        source: DelegationUserRequirementSource,
        requirements: Vec<DelegationIntentRequirement>,
    },
}

impl DelegationIntentRequirements {
    pub fn validate(&self) -> Result<(), &'static str> {
        let source = match self {
            Self::Unassessed => return Ok(()),
            Self::Unconstrained { source }
            | Self::Unresolved { source, .. }
            | Self::Unavailable { source, .. }
            | Self::Requirements { source, .. } => source,
        };
        if source.user_id.trim().is_empty()
            || source.session_id.trim().is_empty()
            || source.user_intent_digest.trim().is_empty()
        {
            return Err("delegation requirement source is incomplete");
        }
        match self {
            Self::Unresolved { reason, .. } if reason.trim().is_empty() => {
                Err("unresolved delegation requirement has no reason")
            }
            Self::Unavailable {
                reason, attempts, ..
            } if reason.trim().is_empty() || !(1..=2).contains(attempts) => {
                Err("unavailable delegation assessment has invalid retry state")
            }
            Self::Requirements { requirements, .. } => {
                if requirements.is_empty() || requirements.len() > 16 {
                    return Err("delegation requirement set has invalid size");
                }
                let mut ids = std::collections::BTreeSet::new();
                for item in requirements {
                    if item.requirement_id.trim().is_empty()
                        || !ids.insert(item.requirement_id.as_str())
                        || (item.model_selection.is_none() && item.reasoning.is_none())
                        || item
                            .model_selection
                            .as_ref()
                            .is_some_and(|model| model.offering_id.trim().is_empty())
                        || item
                            .task_scope_quote
                            .as_ref()
                            .is_some_and(|scope| scope.trim().is_empty())
                        || matches!(
                            item.reasoning,
                            Some(DelegationReasoningRequirement::Budget { tokens: 0 })
                        )
                    {
                        return Err("delegation requirement is incomplete or duplicated");
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Project only restrictions explicitly admitted to apply beyond this
    /// run's direct children. An empty projection is explicit, not a missing
    /// inheritance record.
    pub fn for_child_descendants(&self) -> Result<Self, &'static str> {
        self.validate()?;
        Ok(match self {
            Self::Requirements {
                source,
                requirements,
            } => {
                let inherited = requirements
                    .iter()
                    .filter(|item| {
                        item.propagation == DelegationRequirementPropagation::Descendants
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if inherited.is_empty() {
                    Self::Unconstrained {
                        source: source.clone(),
                    }
                } else {
                    Self::Requirements {
                        source: source.clone(),
                        requirements: inherited,
                    }
                }
            }
            other => other.clone(),
        })
    }
}

impl DelegationModelAdmission {
    pub fn validate_identity(
        &self,
        invocation_id: &str,
        arguments_digest: &str,
        control_epoch: usize,
        expected_slots: usize,
    ) -> Result<(), &'static str> {
        if self.invocation_id != invocation_id
            || self.arguments_digest != arguments_digest
            || self.source.control_epoch != control_epoch
        {
            return Err("delegation model admission belongs to another invocation or intent");
        }
        if self.child_requirements.len() != expected_slots {
            return Err("delegation child requirement projection has wrong slot count");
        }
        for child in &self.child_requirements {
            child.validate()?;
            let origin = match child {
                DelegationIntentRequirements::Unassessed => None,
                DelegationIntentRequirements::Unconstrained { source }
                | DelegationIntentRequirements::Unresolved { source, .. }
                | DelegationIntentRequirements::Unavailable { source, .. }
                | DelegationIntentRequirements::Requirements { source, .. } => Some(source),
            };
            if origin.is_some_and(|origin| {
                origin.user_id != self.source.user_id || origin.session_id != self.source.session_id
            }) {
                return Err("delegation child requirement source changed owner");
            }
        }
        match &self.outcome {
            DelegationModelAdmissionOutcome::ExplicitlyUnconstrained { slot_count } => {
                if *slot_count == 0 || *slot_count > 16 || *slot_count as usize != expected_slots {
                    return Err("delegation model admission has invalid slot count");
                }
            }
            DelegationModelAdmissionOutcome::Constrained { slots } => {
                if slots.is_empty() || slots.len() > 16 || slots.len() != expected_slots {
                    return Err("delegation model admission has invalid slots");
                }
                for (index, slot) in slots.iter().enumerate() {
                    if slot.slot_index != index as u32 {
                        return Err("delegation model admission has missing or reordered slots");
                    }
                    if slot.model_selection.is_some() != slot.model_strength.is_some()
                        || slot.reasoning.is_some() != slot.reasoning_strength.is_some()
                    {
                        return Err("delegation model requirement strength is missing or orphaned");
                    }
                }
                if slots
                    .iter()
                    .all(|slot| slot.model_selection.is_none() && slot.reasoning.is_none())
                {
                    return Err("constrained delegation has no model requirements");
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_rejects_wrong_identity_and_swapped_slots() {
        let source = DelegationModelInstructionSource {
            user_id: "u".into(),
            session_id: "s".into(),
            run_id: "r".into(),
            turn_chain_id: "t".into(),
            owner_generation: 1,
            control_epoch: 2,
            applied_intent_id: None,
            session_turn: 3,
            user_intent_digest: "digest".into(),
        };
        let mut admission = DelegationModelAdmission {
            source,
            invocation_id: "call".into(),
            arguments_digest: "args".into(),
            child_requirements: vec![Default::default(); 2],
            outcome: DelegationModelAdmissionOutcome::Constrained {
                slots: vec![
                    DelegationModelSlotConstraint {
                        slot_index: 0,
                        model_selection: Some(ModelSelection {
                            offering_id: "offer-b".into(),
                        }),
                        model_strength: Some(DelegationRequirementStrength::Hard),
                        reasoning: None,
                        reasoning_strength: None,
                        task_scope_quote: Some("review".into()),
                    },
                    DelegationModelSlotConstraint {
                        slot_index: 1,
                        model_selection: None,
                        model_strength: None,
                        reasoning: None,
                        reasoning_strength: None,
                        task_scope_quote: None,
                    },
                ],
            },
        };
        assert!(admission.validate_identity("call", "args", 2, 2).is_ok());
        assert!(admission.validate_identity("call", "args", 2, 1).is_err());
        assert!(admission.validate_identity("other", "args", 2, 2).is_err());
        assert!(
            admission
                .validate_identity("call", "changed", 2, 2)
                .is_err()
        );
        assert!(admission.validate_identity("call", "args", 3, 2).is_err());
        if let DelegationModelAdmissionOutcome::Constrained { slots } = &mut admission.outcome {
            slots.swap(0, 1);
        }
        assert!(admission.validate_identity("call", "args", 2, 2).is_err());
    }

    #[test]
    fn child_projection_consumes_direct_only_requirement_without_losing_source() {
        let source = DelegationUserRequirementSource {
            user_id: "user".into(),
            session_id: "session".into(),
            session_turn: 1,
            applied_intent_id: None,
            user_intent_digest: "digest".into(),
        };
        let requirement = |id: &str, propagation| DelegationIntentRequirement {
            requirement_id: id.into(),
            model_selection: Some(ModelSelection {
                offering_id: "offer".into(),
            }),
            reasoning: None,
            task_scope_quote: None,
            propagation,
            strength: DelegationRequirementStrength::Hard,
        };
        let direct = DelegationIntentRequirements::Requirements {
            source: source.clone(),
            requirements: vec![requirement(
                "direct",
                DelegationRequirementPropagation::DirectChildren,
            )],
        };
        assert_eq!(
            direct.for_child_descendants().unwrap(),
            DelegationIntentRequirements::Unconstrained {
                source: source.clone()
            }
        );
        let mixed = DelegationIntentRequirements::Requirements {
            source: source.clone(),
            requirements: vec![
                requirement("direct", DelegationRequirementPropagation::DirectChildren),
                requirement("subtree", DelegationRequirementPropagation::Descendants),
            ],
        };
        assert_eq!(
            mixed.for_child_descendants().unwrap(),
            DelegationIntentRequirements::Requirements {
                source,
                requirements: vec![requirement(
                    "subtree",
                    DelegationRequirementPropagation::Descendants
                )]
            }
        );
        let malformed = DelegationIntentRequirements::Requirements {
            source: DelegationUserRequirementSource {
                user_id: "user".into(),
                session_id: "session".into(),
                session_turn: 1,
                applied_intent_id: None,
                user_intent_digest: "digest".into(),
            },
            requirements: Vec::new(),
        };
        assert!(malformed.for_child_descendants().is_err());
    }
}
