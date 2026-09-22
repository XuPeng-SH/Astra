'use client';

import { AlertTriangle, CheckCircle2, Loader2, Sparkles } from 'lucide-react';
import { useSearchParams } from 'next/navigation';
import { useState } from 'react';
import { createAuthoringIntent } from '@/lib/api/harnesses';
import { runPreparedEvaluation, type EvaluationReport } from '@/lib/api/evaluations';
import type { AuthoringIntentRecord } from '@/lib/api/types';
import { Button } from '@/components/ui/button';
import { Card } from '@/components/ui/card';
import { PageHeader } from '@/components/ui/page-header';
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

export function AuthoringPage() {
  const searchParams = useSearchParams();
  const sessionId = searchParams.get('sessionId');
  const [goal, setGoal] = useState('');
  const [result, setResult] = useState<AuthoringResult | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

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
      const created = await createAuthoringIntent({ goal: trimmed, idempotency_key: crypto.randomUUID() }, sessionId ?? undefined);
      if (created.skill_drafts.length === 0) {
        throw new Error("本次生成没有产生 Skill 候选，请调整目标后重试。");
      }
      if (!created.evaluation_plan) {
        setResult(created);
        return;
      }
      try {
        const report = await runPreparedEvaluation(created.evaluation_plan, {
          waitSecs: Math.max(300, created.evaluation_plan.trials.length * 300 + 60),
        });
        setResult({ ...created, evaluation_report: report });
      } catch (reason) {
        setResult({
          ...created,
          evaluation_error: reason instanceof Error ? reason.message : 'Evaluation did not settle.',
        });
      }
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
          <div className="mt-4 flex items-center justify-between gap-3">
            <p className="text-xs text-text-muted">
              不需要选择 Harness、模型、验证器或上下文来源。
            </p>
            <Button
              type="button"
              onClick={() => void submit()}
              disabled={busy || !goal.trim()}
              leadingIcon={busy ? Loader2 : Sparkles}
            >
              {busy ? '正在生成' : '生成结果'}
            </Button>
          </div>
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
                  <h2 className="text-base font-semibold text-text">评估结果</h2>
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
                          ? '候选已生成，但当前上下文没有可复放的真实案例。'
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
                  <details className="mt-3">
                    <summary className="cursor-pointer text-xs text-text-muted">
                      查看证据与成本详情
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
                <pre className="mt-4 max-h-[520px] overflow-auto whitespace-pre-wrap rounded-control border border-border bg-surface-muted p-4 text-xs leading-5 text-text-secondary">
                  {draft.content_markdown}
                </pre>
                <p className="mt-3 text-xs text-text-muted">
                  基于 {draft.rules.length} 条证据规则 · {countDraftCitations(draft)} 条引用
                </p>
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
