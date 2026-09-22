'use client';

import { AlertTriangle, CheckCircle2, Loader2, Sparkles } from 'lucide-react';
import { useSearchParams } from 'next/navigation';
import { useEffect, useRef, useState } from 'react';
import { createAuthoringIntent, listAuthoringTargets, loadAuthoringResult, decideSkillDraft, publishSkillDraft } from '@/lib/api/harnesses';
import { runPreparedEvaluation, getEvaluationExperiment, type EvaluationProjection, type EvaluationReport } from '@/lib/api/evaluations';
import type { AuthoringIntentRecord, AuthoringIntentRequest } from '@/lib/api/types';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { PageHeader } from '@/components/ui/page-header';
import { SkillEvidence, SkillContentComparison, SkillSourceCoverage, frozenSkillSources } from '@/components/app/skill-evidence';
import { SkillUseAction } from '@/components/app/skill-use-action';
import { Textarea } from '@/components/ui/textarea';

function countDraftCitations(draft: AuthoringIntentRecord['skill_drafts'][number]) {
  return draft.rules.reduce((count, rule) => count + rule.citations.length, 0);
}

function reportHasSupportedResult(report: EvaluationReport) {
  return (
    !report.manifest.coverage.evidence_incomplete &&
    ['paired_support', 'mechanism_supported'].includes(report.report.causal_strength)
  );
}

type AuthoringResult = AuthoringIntentRecord & {
  evaluation_report?: EvaluationReport;
  evaluation_error?: string;
};

type AuthoringScope = { ownerId: string; runtimeKey: string };
export function AuthoringPage(scope: AuthoringScope) {
  const params = useSearchParams();
  const sessionId = params.get('sessionId');
  const runId = params.get('runId');
  return <AuthoringSession key={JSON.stringify([scope.ownerId, scope.runtimeKey, sessionId, runId])} {...scope} sessionId={sessionId} runId={runId} />;
}

function AuthoringSession({ ownerId, runtimeKey, sessionId, runId }: AuthoringScope & { sessionId: string | null; runId: string | null }) {
  const storageKey = `astra:authoring:v1:${encodeURIComponent(ownerId)}:${encodeURIComponent(runtimeKey)}:${encodeURIComponent(sessionId ?? '')}:${encodeURIComponent(runId ?? '')}`;
  const evaluationAbort = useRef<AbortController | null>(null);
  const mounted = useRef(true);
  const [recovering, setRecovering] = useState(true);
  const [savedRunId, setSavedRunId] = useState<string | null>(null);
  const [projection, setProjection] = useState<EvaluationProjection | null>(null);
  const [targets, setTargets] = useState<Array<NonNullable<AuthoringIntentRequest['target_skill']>>>([]);
  const [targetVersion, setTargetVersion] = useState('');
  const [targetsLoading, setTargetsLoading] = useState(Boolean(sessionId));
  const [targetsError, setTargetsError] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    setTargets([]); setTargetVersion(''); setTargetsError(null);
    setTargetsLoading(Boolean(sessionId));
    if (sessionId) listAuthoringTargets(sessionId).then((items) => {
      if (!cancelled) setTargets(items);
    }).catch((reason) => {
      if (!cancelled) setTargetsError(reason instanceof Error ? reason.message : '无法读取当前 Skill');
    }).finally(() => { if (!cancelled) setTargetsLoading(false); });
    return () => { cancelled = true; };
  }, [sessionId]);
  const [goal, setGoal] = useState('');
  const [result, setResult] = useState<AuthoringResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [submittedRequest, setSubmittedRequest] = useState<AuthoringIntentRequest | null>(null);
  const [evaluating, setEvaluating] = useState(false);
  const [createNew, setCreateNew] = useState(false);
  const [taskSourceId, setTaskSourceId] = useState('');
  const [expectedResult, setExpectedResult] = useState('');
  const sources = result ? frozenSkillSources(result.harness_run) : [];
  const tasks = sources.filter((source) => source.event_type === 'user_query');
  const baseline = result?.harness_run.output_json?.authoring as { baseline?: { content_markdown?: string; version_id?: string } } | undefined;

  useEffect(() => {
    mounted.current = true;
    let active = true;
    async function recover() {
      try {
        const raw = window.localStorage.getItem(storageKey);
        const saved = raw ? JSON.parse(raw) as { runId?: string } : {};
        const reference = runId ?? saved.runId;
        if (typeof reference !== 'string') return;
        setSavedRunId(reference);
        const loaded = await loadAuthoringResult(reference);
        if (!active) return;
        setResult(loaded); setGoal(loaded.goal);
        setSubmittedRequest((loaded.harness_run.output_json.authoring as { request?: AuthoringIntentRequest } | undefined)?.request ?? null);
      } catch (reason) { if (active) setError(reason instanceof Error ? reason.message : '无法恢复已有结果'); }
      finally { if (active) setRecovering(false); }
    }
    void recover();
    return () => { active = false; mounted.current = false; evaluationAbort.current?.abort(); };
  }, [storageKey, runId]);

  function remember(created: AuthoringIntentRecord) {
    setSavedRunId(created.harness_run.harness_run_id);
    try { window.localStorage.setItem(storageKey, JSON.stringify({ runId: created.harness_run.harness_run_id })); }
    catch { setError('浏览器无法保存恢复引用；请保留候选的审核链接。'); }
  }

  async function showAndEvaluate(created: AuthoringIntentRecord) {
    if (!mounted.current) return;
    setResult(created); setProjection(null);
    setSubmittedRequest((created.harness_run.output_json.authoring as { request?: AuthoringIntentRequest } | undefined)?.request ?? null);
    if (!created.evaluation_plan) return;
    setEvaluating(true);
    const controller = new AbortController();
    evaluationAbort.current = controller;
    try {
      // Persisted preparation bindings are a snapshot; recover live binding status before starting anything.
      const current = await getEvaluationExperiment(created.evaluation_plan.experiment.experiment_id, { signal: controller.signal });
      setProjection(current);
      const report = await runPreparedEvaluation({ ...created.evaluation_plan,
        experiment: current.experiment, trials: current.trials.map((trial) => trial.binding),
      }, {
        waitSecs: Math.max(300, current.trials.length * 300 + 60), signal: controller.signal, onProjection: setProjection,
      });
      setResult({ ...created, evaluation_report: report });
      setProjection(await getEvaluationExperiment(current.experiment.experiment_id, { signal: controller.signal }));
    } catch (reason) {
      if (!controller.signal.aborted) setResult((current) => ({ ...(current ?? created), evaluation_error: reason instanceof Error ? reason.message : 'Evaluation did not settle.' }));
    } finally { setEvaluating(false); }
  }

  async function resume() {
    if (!savedRunId) return;
    setBusy(true); setError(null);
    try { await showAndEvaluate(await loadAuthoringResult(savedRunId)); }
    catch (reason) { setError(reason instanceof Error ? reason.message : '无法恢复已有结果'); }
    finally { setBusy(false); }
  }

  async function approveAndPublish(draftId: string) {
    const draft = result?.skill_drafts.find((entry) => entry.skill_draft_id === draftId);
    if (!result || !draft) return;
    setBusy(true); setError(null);
    try {
      const runId = result.harness_run.harness_run_id;
      await decideSkillDraft(runId, draftId, { expected_revision: draft.revision, decision: 'approve', reason: 'User reviewed the exact candidate and its source evidence.' });
      if (!mounted.current) return;
      await publishSkillDraft(runId, draftId, { expected_revision: draft.revision, visibility: 'private' });
      const loaded = await loadAuthoringResult(runId);
      setResult((current) => current && loaded.skill_drafts.find((entry) => entry.skill_draft_id === draftId)?.revision === draft.revision
        ? { ...current, ...loaded } : loaded);
    } catch (reason) { setError(reason instanceof Error ? reason.message : '无法保存已审核的 Skill'); }
    finally { setBusy(false); }
  }

  async function validateTask() {
    if (!submittedRequest || !taskSourceId) return;
    setBusy(true);
    setError(null);
    try {
      const expected = JSON.parse(expectedResult);
      const created = await createAuthoringIntent({ ...submittedRequest,
        validation_task: { source_id: taskSourceId, expected_result: expected },
      }, (result?.harness_run.input_json.session_ids as string[] | undefined)?.[0]);
      if (!mounted.current) return;
      remember(created);
      await showAndEvaluate(created);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : '无法准备验证任务');
    } finally { setBusy(false); }
  }

  async function submit() {
    const trimmed = goal.trim();
    if (!trimmed) {
      setError('Describe the outcome you want to create or improve.');
      return;
    }
    setBusy(true);
    setError(null);
    setResult(null);
    try {
      const request: AuthoringIntentRequest = { goal: trimmed, create_new: createNew, idempotency_key: crypto.randomUUID(),
        ...(!createNew && targetVersion ? { target_skill: targets.find((target) => target.version_id === targetVersion) } : {}),
      };
      setSubmittedRequest(request);
      const created = await createAuthoringIntent(request, sessionId ?? undefined);
      if (!mounted.current) return;
      if (created.skill_drafts.length === 0) throw new Error('本次生成没有产生 Skill 候选，请调整目标后重试。');
      remember(created);
      await showAndEvaluate(created);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : 'Failed to start authoring.');
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="h-full overflow-y-auto">
      <div className="mx-auto w-full max-w-4xl px-5 py-6 sm:px-8 lg:px-10">
        <PageHeader
          title="Describe the capability you want"
          description="Astra resolves the internal workflow, creates a candidate, and reports what the available evidence can prove."
        />
        {sessionId ? (
          <p className="mt-2 text-xs text-text-muted">
            The current conversation will be used as context automatically.
          </p>
        ) : null}

        <Card className="mt-6 p-5 sm:p-6">
          <Textarea
            value={goal}
            onChange={(event) => setGoal(event.target.value)}
            placeholder="For example: 帮我把刚才反复做的流程变成一个可复用的 Skill"
            rows={5}
            disabled={busy}
            aria-label="Authoring goal"
          />
          {sessionId ? <label className="mt-3 flex gap-2 text-xs">
            <input type="checkbox" checked={createNew} disabled={busy} onChange={(event) => setCreateNew(event.target.checked)} />
            创建新 Skill；不修改当前会话使用的 Skill
          </label> : null}
          {targetsError ? <p className="mt-2 text-sm text-danger">{targetsError}</p> : null}
          {!createNew && targets.length > 1 ? <label className="mt-3 block text-sm">
            要优化哪个 Skill？
            <select aria-label="要优化的 Skill" value={targetVersion} disabled={busy}
              onChange={(event) => setTargetVersion(event.target.value)} className="mt-2 w-full border p-2">
              <option value="">请选择</option>
              {targets.map((target) => <option key={target.version_id} value={target.version_id}>{target.skill_name}</option>)}
            </select>
          </label> : null}
          <div className="mt-4 flex items-center justify-between gap-3">
            <p className="text-xs text-text-muted">
              不需要选择 Harness、模型、验证器或上下文来源。
            </p>
            <Button
              type="button"
              onClick={() => void submit()}
              disabled={busy || recovering || !goal.trim() || (!createNew && (targetsLoading || !!targetsError || (targets.length > 1 && !targetVersion)))}
              leadingIcon={busy ? Loader2 : Sparkles}
            >
              {evaluating ? '正在评估' : busy ? '正在生成' : result ? '另生成一个候选' : '生成结果'}
            </Button>
          </div>
          {recovering ? <p className="mt-3 text-sm">正在恢复已保存的结果…</p> : null}
          {savedRunId && !evaluating ? <Button variant="ghost" className="mt-3" disabled={busy || recovering} onClick={() => void resume()}>恢复已有结果与评估</Button> : null}
          {error ? <p className="mt-3 text-sm text-danger">{error}</p> : null}
        </Card>

        {result ? (
          <section className="mt-6 space-y-4" aria-live="polite">
            <Card className="p-5 sm:p-6">
              <div className="flex items-start gap-3">
                {result.evaluation_report && reportHasSupportedResult(result.evaluation_report) ? (
                  <CheckCircle2 className="mt-0.5 size-5 text-success" />
                ) : (
                  <AlertTriangle className="mt-0.5 size-5 text-warning" />
                )}
                <div>
                  <h2 className="text-base font-semibold text-text">{evaluating ? '正在验证：候选与依据已可查看' : '评估结果'}</h2>
                  {result.evaluation_report ? (
                    <>
                      <p className="mt-1 text-sm leading-6 text-text-secondary">
                        {result.evaluation_report.report.conclusion}
                      </p>
                      <p className="mt-2 text-xs leading-5 text-text-muted">
                        {result.evaluation_report.manifest.coverage.evidence_incomplete
                          ? '评估已执行，但证据覆盖不完整，不能据此宣称改进。'
                          : '评估已执行，下面的报告保留了真实运行证据。'}
                      </p>
                      <details className="mt-3">
                        <summary className="cursor-pointer text-xs text-text-muted">
                          查看完整评估报告
                        </summary>
                        <pre className="mt-2 max-h-[420px] overflow-auto whitespace-pre-wrap rounded-control border border-border bg-surface-muted p-3 text-xs leading-5 text-text-secondary">
                          {result.evaluation_report.markdown}
                        </pre>
                      </details>
                    </>
                  ) : (
                    <p className="mt-1 text-sm leading-6 text-text-secondary">
                      {result.evaluation_error
                        ? '候选已生成，但真实评估尚未完成。'
                        : result.evaluation.status === 'unavailable'
                          ? '候选已生成，评估暂不可用；具体缺失条件见下方详情。'
                          : result.evaluation.status === 'prepared'
                            ? '候选已生成，真实评估已准备并等待运行。'
                            : '候选已生成，当前证据还不足以证明真实任务改进。'}
                    </p>
                  )}
                  {result.evaluation_error ? (
                    <p className="mt-2 text-xs leading-5 text-warning">
                      评估未完成：{result.evaluation_error}
                    </p>
                  ) : null}
                  {projection ? <div className="mt-3 text-sm">
                    {projection.trials.map((trial) => <p key={trial.binding.trial_id}>
                      {trial.binding.trial.arm === 'baseline' ? '原版本 / 无此 Skill' : '候选版本'}：
                      {trial.task_assessment?.outcome.status === 'pass' ? '通过此任务判定' : trial.task_assessment?.outcome.status === 'fail' ? '未通过此任务判定' : trial.lifecycle}
                    </p>)}
                    <p className="mt-2 text-xs text-text-muted">仅描述这些任务的实际结果，不证明普遍改进。</p>
                  </div> : null}
                  <details className="mt-3">
                    <summary className="cursor-pointer text-xs text-text-muted">
                      查看生成证据与生成成本（不含评估运行）
                    </summary>
                    <p className="mt-2 text-xs leading-5 text-text-muted">{result.evaluation.reason}</p>
                    <p className="mt-2 text-xs leading-5 text-text-muted">
                      {result.inference.providers.length
                        ? `提供方：${result.inference.providers.join(', ')}`
                        : '提供方证据不可用'}
                      {' · '}
                      {result.inference.usage_status}
                      {' · '}
                      {result.inference.estimated_cost_usd === null
                        ? '成本不可用'
                        : `估算成本 $${result.inference.estimated_cost_usd.toFixed(6)}`}
                    </p>
                  </details>
                </div>
              </div>
            </Card>

            {!result.evaluation_plan ? <Card className="p-5">
              <h3 className="font-medium">用已有任务验证</h3>
              <p className="my-2 text-xs text-text-muted">生成依据不等于效果证明。选择原始任务，并明确预期结果；目前此入口支持完整 JSON 结果的精确比较。无法给出判定条件时，不会宣称验证通过。</p>
              {tasks.length ? <>
                <select aria-label="验证任务" value={taskSourceId} disabled={busy} onChange={(event) => setTaskSourceId(event.target.value)} className="w-full border p-2">
                  <option value="">选择原始任务</option>
                  {tasks.map((task) => <option key={task.source_id} value={task.source_id}>{task.content.slice(0, 120)}</option>)}
                </select>
                {taskSourceId ? <pre className="my-2 max-h-48 overflow-auto whitespace-pre-wrap text-xs">{tasks.find((task) => task.source_id === taskSourceId)?.content}</pre> : null}
                <Textarea aria-label="预期 JSON 结果" placeholder='预期 JSON 结果，例如 {"ok": true}' value={expectedResult} disabled={busy} onChange={(event) => setExpectedResult(event.target.value)} />
                <Button className="mt-3" disabled={busy || !submittedRequest || !taskSourceId || !expectedResult.trim()} onClick={() => void validateTask()}>用这个任务验证</Button>
              </> : <p className="text-sm">缺少可复用的原始用户任务。请在包含实际任务的会话中生成或优化 Skill；当前候选仍可审阅。</p>}
            </Card> : null}

            {result.skill_drafts.map((draft) => (
              <Card key={draft.skill_draft_id} className="p-5 sm:p-6">
                <div className="flex items-start justify-between gap-4">
                  <div>
                    <p className="text-xs font-semibold uppercase tracking-[0.1em] text-text-muted">
                      Skill 候选
                    </p>
                    <h2 className="mt-1 text-lg font-semibold text-text">{draft.candidate_name}</h2>
                    <p className="mt-1 text-sm text-text-secondary">{draft.description}</p>
                  </div>
                  <span className="rounded-full border border-border bg-surface-muted px-2.5 py-1 text-xs text-text-muted">
                    私有候选
                  </span>
                </div>
                {baseline?.baseline?.content_markdown ? <SkillContentComparison before={baseline.baseline.content_markdown} after={draft.content_markdown} /> : null}
                <SkillSourceCoverage run={result.harness_run} />
                <SkillEvidence rules={draft.rules} sources={sources} />
                <pre className="mt-4 max-h-[520px] overflow-auto whitespace-pre-wrap rounded-control border border-border bg-surface-muted p-4 text-xs leading-5 text-text-secondary">
                  {draft.content_markdown}
                </pre>
                <p className="mt-3 text-xs text-text-muted">
                  基于 {draft.rules.length} 条证据规则 · {countDraftCitations(draft)} 条引用
                </p>
                {draft.published_version_id ? <SkillUseAction key={draft.published_version_id} run={result.harness_run} skillName={draft.candidate_name} versionId={draft.published_version_id} /> :
                  <Button className="mt-4" disabled={busy} onClick={() => void approveAndPublish(draft.skill_draft_id)}>我已审核，保存为私有 Skill</Button>}
                <Button
                  className="mt-4"
                  href={`/harnesses?runId=${encodeURIComponent(result.harness_run.harness_run_id)}&draftId=${encodeURIComponent(draft.skill_draft_id)}`}
                >
                  审阅并发布此 Skill
                </Button>
              </Card>
            ))}
          </section>
        ) : null}
      </div>
    </div>
  );
}
