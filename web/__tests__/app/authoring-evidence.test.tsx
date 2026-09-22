import { act, fireEvent, render, screen } from '@testing-library/react';
import { AuthoringPage } from '@/components/app/authoring-page';
import { createAuthoringIntent } from '@/lib/api/harnesses';
import { runPreparedEvaluation } from '@/lib/api/evaluations';
import type { AuthoringIntentRecord } from '@/lib/api/types';

vi.mock('next/navigation', () => ({ useSearchParams: () => new URLSearchParams('sessionId=session') }));
vi.mock('@/lib/api/harnesses', () => ({ createAuthoringIntent: vi.fn() }));
vi.mock('@/lib/api/evaluations', () => ({ runPreparedEvaluation: vi.fn() }));

const record = {
  harness_run: { harness_run_id: 'frozen', input_json: { source_packets: [
    { source_id: 'task', event_type: 'user_query', title: 'Original task', content: 'Return a JSON verdict.' },
    { source_id: 'guidance', event_type: 'user_message', title: 'Guidance', content: 'Please hurry.' },
  ] }, output_json: {} },
  skill_drafts: [{ skill_draft_id: 'draft', candidate_name: 'Review', description: 'Review carefully',
    content_markdown: '# Review\nPreserve examples.', rules: [{ skill_rule_id: 'rule', statement: 'Return JSON',
      rationale: 'The user requested a structured verdict.', citations: [{ citation_id: 'quote', source_id: 'task',
        source_locator_json: { validation: 'exact_source_match', start_byte: 0, end_byte: 22 },
        source_metadata_json: { evidence_kind: 'user_statement' }, evidence_text_preview: 'Return a JSON verdict.',
      }] }] }],
  evaluation: { status: 'unavailable', reason: 'No case' },
  inference: { providers: [], usage_status: 'unavailable', estimated_cost_usd: null },
} as unknown as AuthoringIntentRecord;

beforeEach(() => vi.resetAllMocks());

async function submit() {
  render(<AuthoringPage />);
  fireEvent.change(screen.getByLabelText('Authoring goal'), { target: { value: 'Create a review skill' } });
  fireEvent.click(screen.getByRole('button', { name: '生成结果' }));
  await screen.findByText('Review carefully');
}

it('shows candidate and source evidence while evaluation is still running', async () => {
  vi.mocked(createAuthoringIntent).mockResolvedValue({ ...record,
    evaluation_plan: { trials: [] } as unknown as NonNullable<AuthoringIntentRecord['evaluation_plan']> });
  let reject!: (reason: Error) => void;
  vi.mocked(runPreparedEvaluation).mockReturnValue(new Promise((_, fail) => { reject = fail; }));
  await submit();
  expect(screen.getByText('正在验证：候选与依据已可查看')).toBeInTheDocument();
  expect(screen.getByText('已匹配冻结原文')).toBeInTheDocument();
  expect(screen.getByText(/用户陈述或偏好/)).toBeInTheDocument();
  expect(screen.getByText('查看原文')).toBeInTheDocument();
  expect(screen.getByRole('link', { name: '审阅并发布此 Skill' })).toHaveAttribute('href', '/harnesses?runId=frozen&draftId=draft');
  await act(async () => reject(new Error('Trial unavailable')));
  expect(screen.getByText(/Trial unavailable/)).toBeInTheDocument();
  expect(screen.getByText('Review carefully')).toBeInTheDocument();
});

it('reuses the candidate when selecting an original task and explicit expected result', async () => {
  vi.mocked(createAuthoringIntent).mockResolvedValue(record);
  await submit();
  expect(screen.queryByRole('option', { name: 'Please hurry.' })).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText('验证任务'), { target: { value: 'task' } });
  fireEvent.change(screen.getByLabelText('预期 JSON 结果'), { target: { value: '{"ok":true}' } });
  await act(async () => fireEvent.click(screen.getByRole('button', { name: '用这个任务验证' })));
  const requests = vi.mocked(createAuthoringIntent).mock.calls;
  expect(requests).toHaveLength(2);
  expect(requests[1][0]).toEqual({ ...requests[0][0], validation_task: { source_id: 'task', expected_result: { ok: true } } });
});
