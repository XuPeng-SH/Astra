//! Typed batched judgments shared by auxiliary inference callers.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgmentRequest {
    pub schema_version: u32,
    pub state: Value,
    pub questions: BTreeMap<String, JudgmentQuestion>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum JudgmentQuestion {
    Noul {
        instructions: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum JudgmentAnswer {
    Noul { noul: f64 },
}
impl JudgmentAnswer {
    pub fn probability(&self) -> f64 {
        match self {
            Self::Noul { noul } => *noul,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgmentResponse {
    pub schema_version: u32,
    pub model: String,
    pub answers: BTreeMap<String, JudgmentAnswer>,
}
impl JudgmentRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("unsupported judgment version");
        }
        if self.state.is_null() || self.questions.is_empty() {
            return Err("judgment requires evidence and questions");
        }
        for (id, question) in &self.questions {
            if id.trim().is_empty() {
                return Err("empty judgment question identity");
            }
            match question {
                JudgmentQuestion::Noul { instructions, .. } if instructions.trim().is_empty() => {
                    return Err("empty judgment instructions");
                }
                _ => {}
            }
        }
        Ok(())
    }
}
impl JudgmentResponse {
    pub fn validate_for(&self, request: &JudgmentRequest) -> Result<(), &'static str> {
        request.validate()?;
        if self.schema_version != 1
            || self.model.trim().is_empty()
            || !self.answers.keys().eq(request.questions.keys())
        {
            return Err("judgment answer identity mismatch");
        }
        if self.answers.values().any(|answer| {
            !answer.probability().is_finite() || !(0.0..=1.0).contains(&answer.probability())
        }) {
            return Err("invalid judgment probability");
        }
        Ok(())
    }
}
