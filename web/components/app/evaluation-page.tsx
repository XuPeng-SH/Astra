'use client';

import { AlertTriangle, CheckCircle2, Play, RefreshCw, Scale } from 'lucide-react';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { EmptyState } from '@/components/ui/empty-state';
import { Input } from '@/components/ui/input';
import { PageHeader } from '@/components/ui/page-header';
import { Textarea } from '@/components/ui/textarea';
import {
  getEvaluationExperiment,
  listEvaluationModels,
  listPersonalSkillSources,
  listPersonalSkillVersions,
  prepareEvaluation,
  runPreparedEvaluation,
  type EvaluationModel,
  type EvaluationProjection,
  type EvaluationReport,
  type PersonalSkillSource,
  type PersonalSkillVersion,
} from '@/lib/api/evaluations';
import { listModels } from '@/lib/api/models';

type PrimaryModel = Awaited<ReturnType<typeof listModels>>['items'][number];
const defaultExpected = '{\n  "ok": true\n}';

function statusTone(value: string) {
  if (value === 'observed' || value === 'recorded' || value === 'pass') {
    return 'border-success/30 bg-success/10 text-success';
  }
  if (value === 'unavailable' || value === 'fail' || value === 'failed') {
    return 'border-danger/30 bg-danger/10 text-danger';
  }
  return 'border-border bg-surface-muted text-text-secondary';
}

export function EvaluationPage() {
  const [sources, setSources] = useState<PersonalSkillSource[]>([]);
  const [versions, setVersions] = useState<PersonalSkillVersion[]>([]);
  const [primaryModels, setPrimaryModels] = useState<PrimaryModel[]>([]);
  const [judgmentModels, setJudgmentModels] = useState<EvaluationModel[]>([]);
  const [skillName, setSkillName] = useState('');
  const [versionId, setVersionId] = useState('');
  const [primaryOfferingId, setPrimaryOfferingId] = useState('');
  const [judgmentOfferingId, setJudgmentOfferingId] = useState('');
  const [caseId, setCaseId] = useState('routing-case');
  const [message, setMessage] = useState('Apply the pinned Skill and return exactly the expected JSON.');
  const [expectedJson, setExpectedJson] = useState(defaultExpected);
  const [wallTimeSecs, setWallTimeSecs] = useState('120');
  const [projection, setProjection] = useState<EvaluationProjection | null>(null);
  const [report, setReport] = useState<EvaluationReport | null>(null);
  const [experimentId, setExperimentId] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState('Choose a published Skill revision to begin.');

  const publishedVersions = useMemo(
    () => versions.filter((version) => version.status === 'published'),
    [versions],
  );

  const loadCatalog = useCallback(async () => {
    setLoading(true);
    setError(null);
    try {
      const [skillPayload, primaryPayload, judgmentPayload] = await Promise.all([
        listPersonalSkillSources(),
        listModels(),
        listEvaluationModels(),
      ]);
      const availableSources = skillPayload.filter((source) => source.status !== 'deleted');
      setSources(availableSources);
      setPrimaryModels(primaryPayload.items);
      setJudgmentModels(judgmentPayload.items.filter((model) => model.is_active));
      setSkillName((current) => current || availableSources[0]?.skill_name || '');
      setPrimaryOfferingId((current) => current || primaryPayload.items[0]?.id || '');
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : 'Failed to load Evaluation catalog.');
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => {
    void loadCatalog();
  }, [loadCatalog]);

  useEffect(() => {
    if (!skillName) {
      setVersions([]);
      setVersionId('');
      return;
    }
    let active = true;
    void listPersonalSkillVersions(skillName)
      .then((payload) => {
        if (!active) return;
        setVersions(payload);
        setVersionId((current) => current || payload.find((version) => version.status === 'published')?.version_id || '');
      })
      .catch((reason: unknown) => {
        if (active) setError(reason instanceof Error ? reason.message : 'Failed to load Skill revisions.');
      });
    return () => {
      active = false;
    };
  }, [skillName]);

  const resetResult = useCallback(() => {
    setExperimentId(null);
    setProjection(null);
    setReport(null);
    setError(null);
    setStatus('Choose a published Skill revision to begin.');
  }, []);

  const runComparison = useCallback(async () => {
    setError(null);
    setReport(null);
    if (!skillName || !versionId || !primaryOfferingId) {
      setError('Select a Skill revision and a primary Offering first.');
      return;
    }
    let expected: unknown;
    try {
      expected = JSON.parse(expectedJson);
    } catch (reason) {
      setError(`Expected JSON is invalid: ${reason instanceof Error ? reason.message : String(reason)}`);
      return;
    }
    const wall = Number(wallTimeSecs);
    if (!Number.isSafeInteger(wall) || wall < 1) {
      setError('Wall time must be a positive whole number of seconds.');
      return;
    }
    setBusy(true);
    resetResult();
    let currentExperimentId: string | null = null;
    try {
      setStatus('Freezing Skill, model, judgment policy, and task criterion…');
      const prepared = await prepareEvaluation({
        submission_idempotency_key: `web-skill-routing-${crypto.randomUUID()}`,
        target: {
          kind: 'skill_routing_judgment',
          skill_name: skillName,
          baseline: { revision_id: versionId },
          candidate: { revision_id: versionId },
        },
        case: {
          case_id: caseId.trim() || 'routing-case',
          message: message.trim(),
          verifier_config: { expected },
        },
        model_offering_id: primaryOfferingId,
        ...(judgmentOfferingId ? { judgment_model_offering_id: judgmentOfferingId } : {}),
        max_concurrency: 1,
        max_wall_time_secs: wall,
      });
      currentExperimentId = prepared.experiment.experiment_id;
      setExperimentId(currentExperimentId);
      setStatus(`Prepared ${prepared.trials.length} trials; running the baseline arm first…`);
      const finalReport = await runPreparedEvaluation(prepared, {
        waitSecs: wall * prepared.trials.length + 60,
        onProjection: (next) => {
          setProjection(next);
          const current = next.trials.find((trial) => trial.lifecycle !== 'observed');
          setStatus(current ? `${current.binding.trial_id}: ${current.lifecycle}` : 'All trials observed; assessing report…');
        },
      });
      setReport(finalReport);
      setProjection(await getEvaluationExperiment(currentExperimentId));
      setStatus('Evaluation report is ready.');
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : 'Evaluation failed.');
      if (currentExperimentId) setStatus(`Experiment ${currentExperimentId} remains available for review.`);
    } finally {
      setBusy(false);
    }
  }, [caseId, expectedJson, judgmentOfferingId, message, primaryOfferingId, resetResult, skillName, versionId, wallTimeSecs]);

  if (loading) {
    return <div className="flex h-full items-center justify-center text-sm text-text-secondary">Loading Evaluation catalog…</div>;
  }

  if (sources.length === 0) {
    return (
      <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
        <div className="mx-auto max-w-5xl">
          <PageHeader title="Evaluation" description="Compare a pinned Skill with and without the frozen judgment decision point." />
          <div className="mt-8">
            <EmptyState
              icon={Scale}
              title="No personal Skills available"
              description="Publish an instruction-only Skill in Harnesses first, then return here to evaluate its behavior."
            />
          </div>
        </div>
      </div>
    );
  }

  const selectedVersion = publishedVersions.find((version) => version.version_id === versionId);
  const coverage = report?.manifest.coverage;

  return (
    <div className="h-full overflow-y-auto overscroll-contain px-8 py-8">
      <div className="mx-auto max-w-6xl">
        <PageHeader
          title="Evaluation"
          description="A controlled A/A comparison: both arms use the same published Skill revision; the candidate adds the frozen Skill routing judgment."
          action={<Button variant="ghost" leadingIcon={RefreshCw} onClick={loadCatalog} disabled={busy}>Refresh catalog</Button>}
        />

        <div className="mt-8 grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(320px,0.7fr)]">
          <Card>
            <div className="flex items-start gap-3">
              <span className="flex size-9 shrink-0 items-center justify-center rounded-control bg-accent/10 text-accent"><Scale className="size-4" /></span>
              <div>
                <h2 className="text-base font-semibold">Skill routing comparison</h2>
                <p className="mt-1 text-sm leading-6 text-text-secondary">The server freezes the exact Skill, primary model, judgment Offering, verifier, and runtime conditions before either trial starts.</p>
              </div>
            </div>

            <div className="mt-6 grid gap-4 sm:grid-cols-2">
              <SelectField label="Skill" value={skillName} onChange={(value) => { setSkillName(value); setVersionId(''); }} disabled={busy}>
                {sources.map((source) => <option key={source.skill_name} value={source.skill_name}>{source.skill_name}</option>)}
              </SelectField>
              <SelectField label="Pinned published revision" value={versionId} onChange={setVersionId} disabled={busy || publishedVersions.length === 0}>
                {publishedVersions.map((version) => <option key={version.version_id} value={version.version_id}>{version.version} · {version.version_id}</option>)}
              </SelectField>
              <SelectField label="Primary Offering" value={primaryOfferingId} onChange={setPrimaryOfferingId} disabled={busy}>
                {primaryModels.map((model) => <option key={model.id} value={model.id}>{model.name} · {model.id}</option>)}
              </SelectField>
              <SelectField label={<>Judgment Offering <span className="font-normal text-text-muted">optional</span></>} value={judgmentOfferingId} onChange={setJudgmentOfferingId} disabled={busy}>
                <option value="">Use configured default</option>
                {judgmentModels.map((model) => <option key={model.offering_id} value={model.offering_id}>{model.name} · {model.provider}</option>)}
              </SelectField>
            </div>

            <div className="mt-4 grid gap-4 sm:grid-cols-[minmax(0,1fr)_150px]">
              <label className="text-sm font-medium">Case message
                <Textarea value={message} onChange={(event) => setMessage(event.target.value)} disabled={busy} className="mt-1.5 min-h-24" />
              </label>
              <label className="text-sm font-medium">Case ID
                <Input value={caseId} onChange={(event) => setCaseId(event.target.value)} disabled={busy} className="mt-1.5" />
                <span className="mt-2 block text-xs font-normal text-text-muted">The expected value stays in the verifier, outside the prompt.</span>
              </label>
            </div>

            <label className="mt-4 block text-sm font-medium">Expected JSON
              <Textarea value={expectedJson} onChange={(event) => setExpectedJson(event.target.value)} disabled={busy} className="mt-1.5 min-h-32 font-mono text-xs" />
            </label>

            <div className="mt-4 flex flex-wrap items-end justify-between gap-4">
              <label className="text-sm font-medium">Max wall time per trial
                <Input type="number" min={1} value={wallTimeSecs} onChange={(event) => setWallTimeSecs(event.target.value)} disabled={busy} className="mt-1.5 w-40" />
              </label>
              <Button leadingIcon={Play} onClick={runComparison} disabled={busy || !selectedVersion || !message.trim()}>{busy ? 'Running…' : 'Run comparison'}</Button>
            </div>
            <p className="mt-4 text-xs leading-5 text-text-muted">Jev is an enhancement when selected or configured. If it is unavailable, the candidate keeps the basic Skill path and the report cannot establish a Jev benefit.</p>
          </Card>

          <div className="space-y-5">
            <Card>
              <div className="flex items-center gap-2 text-sm font-semibold"><span className="size-2 rounded-full bg-accent" />Run status</div>
              <p className="mt-3 text-sm leading-6 text-text-secondary">{status}</p>
              {experimentId ? <p className="mt-3 break-all font-mono text-xs text-text-muted">{experimentId}</p> : null}
              {error ? <div className="mt-4 flex gap-2 rounded-control border border-danger/30 bg-danger/10 p-3 text-sm text-danger"><AlertTriangle className="mt-0.5 size-4 shrink-0" /><span>{error}</span></div> : null}
            </Card>

            {projection ? <TrialEvidence projection={projection} /> : null}

            {report && coverage ? (
              <Card>
                <div className="flex items-center gap-2 text-sm font-semibold"><CheckCircle2 className="size-4 text-success" />Report coverage</div>
                <div className="mt-4 grid grid-cols-2 gap-3 text-sm">
                  <Metric label="Observed" value={`${coverage.observed_trial_count}/${coverage.planned_trial_count}`} />
                  <Metric label="Metric gaps" value={`${coverage.metric_gaps.length}`} />
                </div>
                <p className="mt-4 text-sm leading-6 text-text-secondary">{report.report.conclusion}</p>
                {coverage.evidence_incomplete ? <p className="mt-3 text-xs text-warning">Evidence is incomplete; this comparison does not establish an improvement.</p> : null}
                <details className="mt-4">
                  <summary className="cursor-pointer text-xs font-medium text-text-secondary">View Markdown report</summary>
                  <pre className="mt-3 max-h-96 overflow-auto whitespace-pre-wrap rounded-control bg-surface-muted p-3 text-xs leading-5 text-text-secondary">{report.markdown}</pre>
                </details>
              </Card>
            ) : null}
          </div>
        </div>
      </div>
    </div>
  );
}

function SelectField({ label, value, onChange, disabled, children }: { label: React.ReactNode; value: string; onChange: (value: string) => void; disabled?: boolean; children: React.ReactNode }) {
  return (
    <label className="text-sm font-medium">{label}
      <select value={value} onChange={(event) => onChange(event.target.value)} disabled={disabled} className="mt-1.5 h-10 w-full rounded-control border border-border bg-surface px-3 text-sm font-normal text-text outline-none focus:border-accent">
        {children}
      </select>
    </label>
  );
}

function TrialEvidence({ projection }: { projection: EvaluationProjection }) {
  return (
    <Card>
      <h2 className="text-sm font-semibold">Trial evidence</h2>
      <div className="mt-3 space-y-2">
        {projection.trials.map((trial) => (
          <div key={trial.binding.trial_id} className="flex items-center justify-between gap-3 rounded-control border border-border bg-surface-muted px-3 py-2 text-xs">
          <div className="min-w-0"><span className="font-medium">{trial.binding.trial.arm}</span><span className="ml-2 truncate text-text-muted">{trial.binding.trial_id}</span></div>
            <span className={`shrink-0 rounded-full border px-2 py-0.5 ${statusTone(trial.task_assessment?.outcome.status ?? trial.lifecycle)}`}>{trial.task_assessment?.outcome.status ?? trial.lifecycle}</span>
          </div>
        ))}
      </div>
    </Card>
  );
}

function Metric({ label, value }: { label: string; value: string }) {
  return <div className="rounded-control border border-border bg-surface-muted px-3 py-2"><div className="text-xs text-text-muted">{label}</div><div className="mt-1 font-semibold">{value}</div></div>;
}
