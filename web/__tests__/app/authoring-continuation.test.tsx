import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { HarnessesPage } from '@/components/app/harnesses-page';
import { listChats } from '@/lib/api/chats';
import * as harness from '@/lib/api/harnesses';

const navigation = vi.hoisted(() => ({ query: 'runId=existing&draftId=chosen' }));
vi.mock('next/navigation', () => ({ useSearchParams: () => new URLSearchParams(navigation.query) }));
vi.mock('@/lib/api/chats', () => ({ listChats: vi.fn().mockResolvedValue({ items: [] }) }));
vi.mock('@/lib/api/harnesses', () => ({
  listHarnessTemplates: vi.fn().mockResolvedValue([]),
  listHarnessNodeCatalog: vi.fn().mockResolvedValue([]),
  getHarnessRun: vi.fn().mockResolvedValue({ harness_run_id: 'existing', status: 'waiting_for_review', output_json: {} }),
  listSkillDrafts: vi.fn().mockResolvedValue([{
    skill_draft_id: 'chosen', candidate_name: 'Persisted candidate', description: 'Existing work',
    content_markdown: 'Keep conclusions concise.', status: 'proposed', rules: [],
  }]),
  createSkillifyRun: vi.fn(), decideSkillDraft: vi.fn(), decideSkillRule: vi.fn(), publishSkillDraft: vi.fn(),
}));

beforeEach(() => {
  vi.restoreAllMocks(); vi.clearAllMocks(); window.localStorage.clear();
  navigation.query = 'runId=existing&draftId=chosen';
  window.history.replaceState(null, '', '/harnesses?runId=existing&draftId=chosen');
  const replaceState = window.history.replaceState.bind(window.history);
  vi.spyOn(window.history, 'replaceState').mockImplementation((data, unused, url) => {
    replaceState(data, unused, url); navigation.query = window.location.search.slice(1);
  });
});

it('opens the persisted authoring candidate without creating or publishing another run', async () => {
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  expect((await screen.findAllByText('Persisted candidate')).length).toBeGreaterThan(0);
  expect(harness.getHarnessRun).toHaveBeenCalledWith('existing');
  expect(harness.listSkillDrafts).toHaveBeenCalledWith('existing');
  expect(harness.createSkillifyRun).not.toHaveBeenCalled();
  expect(harness.publishSkillDraft).not.toHaveBeenCalled();
});


it('retains the advanced Harness request before dispatch and reuses it after reload', async () => {
  window.localStorage.clear(); vi.clearAllMocks(); navigation.query = '';
  vi.mocked(harness.listHarnessTemplates).mockResolvedValue([{ template_id: 'skillify.v1', name: 'Skillify' }] as Awaited<ReturnType<typeof harness.listHarnessTemplates>>);
  vi.mocked(listChats).mockResolvedValue({ items: [{ id: 'source-session', title: 'Source conversation' }] } as Awaited<ReturnType<typeof listChats>>);
  vi.mocked(harness.createSkillifyRun).mockImplementationOnce(async (request) => {
    const saved = JSON.parse(window.localStorage.getItem(`astra:harness:v1:owner:runtime:${request.idempotency_key}`)!);
    expect(saved.pending).toEqual(request);
    expect(request.idempotency_key).toBeTruthy();
    throw new Error('Generation response lost');
  }).mockResolvedValueOnce({ harness_run_id: 'recovered', status: 'running', output_json: {} } as Awaited<ReturnType<typeof harness.createSkillifyRun>>)
    .mockResolvedValue({ harness_run_id: 'recovered', status: 'waiting_for_review', output_json: {} } as Awaited<ReturnType<typeof harness.createSkillifyRun>>);
  const view = render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
  fireEvent.click(await screen.findByText('Source conversation'));
  fireEvent.click(screen.getByRole('button', { name: 'Run Skillify' }));
  await screen.findByText('Generation response lost');
  const request = vi.mocked(harness.createSkillifyRun).mock.calls[0][0];
  view.unmount();
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: '恢复原生成任务' }));
  await screen.findByText('原生成任务仍在执行；稍后恢复同一任务以读取结果。');
  expect(JSON.parse(window.localStorage.getItem(`astra:harness:v1:owner:runtime:${request.idempotency_key}`)!).pending).toEqual(request);
  expect(screen.getByRole('button', { name: 'Run Skillify' })).toBeDisabled();
  fireEvent.click(screen.getByRole('button', { name: '恢复原生成任务' }));
  await waitFor(() => expect(harness.listSkillDrafts).toHaveBeenCalledWith('recovered'));
  expect(vi.mocked(harness.createSkillifyRun).mock.calls.map(([body]) => body)).toEqual([request, request, request]);
  expect(JSON.parse(window.localStorage.getItem(`astra:harness:v1:owner:runtime:${request.idempotency_key}`)!)).toEqual({ runId: 'recovered' });
});


it('updates the canonical Harness result link when generation started from an older run', async () => {
  window.localStorage.clear(); vi.clearAllMocks();
  window.history.replaceState(null, '', '/harnesses?attemptId=retained-key');
  const request = { session_ids: ['session'], idempotency_key: 'retained-key' };
  window.localStorage.setItem('astra:harness:v1:owner:runtime:retained-key', JSON.stringify({ pending: request }));
  vi.mocked(harness.createSkillifyRun).mockResolvedValue({ harness_run_id: 'new', status: 'waiting_for_review', output_json: {} } as Awaited<ReturnType<typeof harness.createSkillifyRun>>);
  const view = render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: '恢复原生成任务' }));
  await waitFor(() => expect(harness.listSkillDrafts).toHaveBeenCalledWith('new'));
  expect(window.location.search).toBe('?runId=new');
  view.unmount(); navigation.query = window.location.search.slice(1);
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  await waitFor(() => expect(harness.getHarnessRun).toHaveBeenLastCalledWith('new'));
  expect(harness.createSkillifyRun).toHaveBeenCalledTimes(1);
});

it('keeps two advanced Harness attempts and their recovery routes independent', async () => {
  vi.mocked(harness.listHarnessTemplates).mockResolvedValue([{ template_id: 'skillify.v1', name: 'Skillify' }] as Awaited<ReturnType<typeof harness.listHarnessTemplates>>);
  vi.mocked(listChats).mockResolvedValue({ items: [{ id: 'session', title: 'Source' }] } as Awaited<ReturnType<typeof listChats>>);
  vi.mocked(harness.createSkillifyRun).mockRejectedValue(new Error('Response lost'));
  const urls: string[] = [];
  for (const name of ['A', 'B']) {
    window.history.replaceState(null, '', '/harnesses');
    const tab = render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
    fireEvent.click(await screen.findByRole('button', { name: 'Open' }));
    fireEvent.click(await screen.findByText('Source'));
    fireEvent.change(screen.getByPlaceholderText('my-workflow-style'), { target: { value: name } });
    fireEvent.click(screen.getByRole('button', { name: 'Run Skillify' }));
    await screen.findByText('Response lost');
    urls.push(window.location.search);
    tab.unmount();
  }
  const [a, b] = vi.mocked(harness.createSkillifyRun).mock.calls.map(([request]) => request);
  expect(a.idempotency_key).not.toBe(b.idempotency_key);
  expect(window.localStorage.length).toBe(2);
  window.history.replaceState(null, '', `/harnesses${urls[0]}`);
  const recovered = render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  vi.mocked(harness.createSkillifyRun).mockResolvedValue({ harness_run_id: 'A', status: 'waiting_for_review', output_json: {} } as Awaited<ReturnType<typeof harness.createSkillifyRun>>);
  fireEvent.click(await screen.findByRole('button', { name: '恢复原生成任务' }));
  await waitFor(() => expect(harness.listSkillDrafts).toHaveBeenCalledWith('A'));
  expect(vi.mocked(harness.createSkillifyRun).mock.calls[2][0]).toEqual(a);
  expect(JSON.parse(window.localStorage.getItem(`astra:harness:v1:owner:runtime:${b.idempotency_key}`)!)).toEqual({ pending: b });
  recovered.unmount();
  window.history.replaceState(null, '', `/harnesses${urls[1]}`);
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: '恢复原生成任务' }));
  await waitFor(() => expect(vi.mocked(harness.createSkillifyRun).mock.calls[3][0]).toEqual(b));
});

it('allocates a new recovery record when an old attempt URL already resolved to a result', async () => {
  window.history.replaceState(null, '', '/harnesses?attemptId=completed');
  window.localStorage.setItem('astra:harness:v1:owner:runtime:completed', JSON.stringify({ runId: 'old' }));
  vi.mocked(listChats).mockResolvedValue({ items: [{ id: 'session', title: 'Source' }] } as Awaited<ReturnType<typeof listChats>>);
  vi.mocked(harness.createSkillifyRun).mockRejectedValue(new Error('New response lost'));
  const view = render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByText('Source'));
  fireEvent.click(screen.getByRole('button', { name: 'Run Skillify' }));
  await screen.findByText('New response lost');
  const request = vi.mocked(harness.createSkillifyRun).mock.calls[0][0];
  expect(new URL(window.location.href).searchParams.get('attemptId')).toBe(request.idempotency_key);
  expect(JSON.parse(window.localStorage.getItem('astra:harness:v1:owner:runtime:completed')!)).toEqual({ runId: 'old' });
  view.unmount();
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  fireEvent.click(await screen.findByRole('button', { name: '恢复原生成任务' }));
  await waitFor(() => expect(vi.mocked(harness.createSkillifyRun).mock.calls[1][0]).toEqual(request));
});

it.each(['rule', 'draft'])('keeps the opening revision on a concurrent %s buffer', async (second) => {
  let draft = {
    skill_draft_id: 'chosen', revision: 1, candidate_name: 'Review', description: 'Review rules',
    content_markdown: 'A:old / B:old', status: 'proposed',
    rules: ['A', 'B'].map((id) => ({ skill_rule_id: id, statement: `${id}:old`, rationale: '', status: 'proposed', rule_type: 'instruction', source_count: 0, citations: [] })),
  } as unknown as Awaited<ReturnType<typeof harness.listSkillDrafts>>[number];
  vi.mocked(harness.listSkillDrafts).mockImplementation(async () => [draft]);
  vi.mocked(harness.decideSkillRule).mockImplementation(async (_run, _draft, _rule, request) => {
    if (request.expected_revision !== draft.revision) throw new Error('Revision conflict; reopen the editor');
    draft = { ...draft, revision: draft.revision + 1, content_markdown: (request.after_json as { content_markdown: string }).content_markdown };
    return draft;
  });
  vi.mocked(harness.decideSkillDraft).mockImplementation(async (_run, _draft, request) => {
    if (request.expected_revision !== draft.revision) throw new Error('Revision conflict; reopen the editor');
    throw new Error('Stale content must never be accepted');
  });
  render(<HarnessesPage ownerId="owner" runtimeKey="runtime" />);
  await screen.findByText('Review rules');
  const editors = screen.getAllByRole('button', { name: 'Edit' });
  fireEvent.click(editors[1]);
  fireEvent.click(editors[second === 'rule' ? 2 : 0]);
  const revised = screen.getAllByLabelText('Revised skill content');
  fireEvent.change(revised[0], { target: { value: 'A:new / B:old' } });
  if (second === 'rule') fireEvent.change(revised[1], { target: { value: 'A:old / B:new' } });
  // The whole-draft Save precedes the rule Save in document order.
  fireEvent.click(screen.getAllByRole('button', { name: 'Save' })[second === 'rule' ? 0 : 1]);
  await waitFor(() => expect(draft.revision).toBe(2));
  await waitFor(() => expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled());
  fireEvent.click(screen.getByRole('button', { name: 'Save' }));
  await screen.findByText('Revision conflict; reopen the editor');
  expect(draft.content_markdown).toBe('A:new / B:old');
  const call = second === 'rule' ? vi.mocked(harness.decideSkillRule).mock.calls[1][3] : vi.mocked(harness.decideSkillDraft).mock.calls[0][2];
  expect(call.expected_revision).toBe(1);
});
