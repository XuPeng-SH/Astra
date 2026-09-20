import { requestJson } from '@/lib/api/request';

export type EvaluationModel = {
  offering_id: string;
  name: string;
  provider: string;
  is_active: boolean;
  access_label?: string;
};

export type EvaluationModelPage = {
  items: EvaluationModel[];
  total: number;
  next_cursor: {
    provider: string;
    model_name: string;
    model_id: string;
  } | null;
};

export type PersonalSkillSource = {
  source_id: string;
  skill_name: string;
  status: string;
  visibility: string;
};

export type PersonalSkillVersion = {
  version_id: string;
  skill_name: string;
  version: string;
  content_hash: string;
  status: string;
  token_estimate: number;
  created_at: string;
};

export type EvaluationPrepareResponse = {
  experiment: {
    experiment_id: string;
    spec_fingerprint: string;
  };
  trials: EvaluationTrialBinding[];
  adapter_profile_version: string;
};

export type EvaluationTrialBinding = {
  trial_id: string;
  binding_status: string;
  session_id: string | null;
  run_id: string | null;
  trial: {
    sequence: number;
    case_id: string;
    arm: 'baseline' | 'candidate' | string;
    repetition: number;
  };
};

export type EvaluationProjection = {
  experiment: EvaluationPrepareResponse['experiment'];
  trials: Array<{
    binding: EvaluationTrialBinding;
    run_status: string | null;
    lifecycle: 'planned' | 'running' | 'waiting' | 'paused' | 'terminal_awaiting_observation' | 'observed' | 'unavailable' | string;
    task_assessment: {
      outcome: {
        status: 'pass' | 'fail' | 'unavailable' | string;
        reason?: string;
      };
    } | null;
  }>;
  observed_trial_count: number;
  missing_observation_trial_ids: string[];
  unavailable_trial_ids: string[];
};

export type EvaluationReport = {
  manifest: {
    coverage: {
      planned_trial_count: number;
      observed_trial_count: number;
      missing_trial_ids: string[];
      unavailable_trial_ids: string[];
      evidence_incomplete: boolean;
      metric_gaps: Array<{
        trial_id: string;
        dimension: string;
        metric: string;
        expected_unit: string;
        reason: string;
      }>;
    };
    report_content_hash: string;
    artifact_fingerprint: string;
  };
  report: {
    conclusion: string;
    causal_strength: string;
    observations: Array<{ trial_id: string; arm: string; status: string; measurements: unknown[] }>;
  };
  markdown: string;
};

export function listEvaluationModels() {
  return requestJson<EvaluationModelPage>('/api/evaluations/models');
}

export function listPersonalSkillSources() {
  return requestJson<PersonalSkillSource[]>('/api/evaluations/skills');
}

export function listPersonalSkillVersions(skillName: string) {
  return requestJson<PersonalSkillVersion[]>(
    `/api/evaluations/skills/${encodeURIComponent(skillName)}/versions`,
  );
}

export function prepareEvaluation(payload: Record<string, unknown>) {
  return requestJson<EvaluationPrepareResponse>('/api/evaluations/experiments/prepare', {
    method: 'POST',
    body: JSON.stringify(payload),
  });
}

export function getEvaluationExperiment(experimentId: string) {
  return requestJson<EvaluationProjection>(
    `/api/evaluations/experiments/${encodeURIComponent(experimentId)}`,
  );
}

export function startEvaluationTrial(experimentId: string, trialId: string) {
  return requestJson<{ run_id: string; session_id: string; status: string }>(
    `/api/evaluations/experiments/${encodeURIComponent(experimentId)}/trials/${encodeURIComponent(trialId)}/start`,
    { method: 'POST', body: '{}' },
  );
}

export function assessEvaluationTrial(experimentId: string, trialId: string) {
  return requestJson<{ status: 'pending' | 'recorded'; assessment?: { outcome: string } }>(
    `/api/evaluations/experiments/${encodeURIComponent(experimentId)}/trials/${encodeURIComponent(trialId)}/assess`,
    { method: 'POST' },
  );
}

export function getEvaluationReport(experimentId: string) {
  return requestJson<EvaluationReport>(
    `/api/evaluations/experiments/${encodeURIComponent(experimentId)}/report`,
  );
}

export async function runPreparedEvaluation(
  prepared: EvaluationPrepareResponse,
  options: {
    pollMs?: number;
    waitSecs: number;
    onProjection?: (projection: EvaluationProjection) => void;
  },
) {
  const experimentId = prepared.experiment.experiment_id;
  const deadline = Date.now() + options.waitSecs * 1000;
  const trials = [...prepared.trials].sort((left, right) => left.trial.sequence - right.trial.sequence);
  for (const trial of trials) {
    if (trial.binding_status === 'planned') {
      await startEvaluationTrial(experimentId, trial.trial_id);
    } else if (trial.binding_status !== 'bound') {
      throw new Error(`Trial ${trial.trial_id} has unsupported binding status ${trial.binding_status}.`);
    }
    while (true) {
      const projection = await getEvaluationExperiment(experimentId);
      options.onProjection?.(projection);
      const current = projection.trials.find((entry) => entry.binding.trial_id === trial.trial_id);
      if (!current) {
        throw new Error(`Evaluation projection omitted trial ${trial.trial_id}.`);
      }
      if (current.lifecycle === 'observed' || current.lifecycle === 'terminal_awaiting_observation') {
        break;
      }
      if (current.lifecycle === 'unavailable' || current.lifecycle === 'planned') {
        throw new Error(`Trial ${trial.trial_id} became ${current.lifecycle}.`);
      }
      if (Date.now() >= deadline) {
        throw new Error(`Evaluation wait deadline exceeded at trial ${trial.trial_id}.`);
      }
      await new Promise((resolve) => window.setTimeout(resolve, options.pollMs ?? 500));
    }
    while (true) {
      const assessment = await assessEvaluationTrial(experimentId, trial.trial_id);
      if (assessment.status === 'recorded') {
        break;
      }
      if (Date.now() >= deadline) {
        throw new Error(`Assessment wait deadline exceeded at trial ${trial.trial_id}.`);
      }
      await new Promise((resolve) => window.setTimeout(resolve, options.pollMs ?? 500));
    }
  }
  return getEvaluationReport(experimentId);
}
