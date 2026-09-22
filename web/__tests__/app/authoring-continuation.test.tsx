import { render, screen } from '@testing-library/react';
import { HarnessesPage } from '@/components/app/harnesses-page';
import * as harness from '@/lib/api/harnesses';

vi.mock('next/navigation', () => ({ useSearchParams: () => new URLSearchParams('runId=existing&draftId=chosen') }));
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

it('opens the persisted authoring candidate without creating or publishing another run', async () => {
  render(<HarnessesPage />);
  expect((await screen.findAllByText('Persisted candidate')).length).toBeGreaterThan(0);
  expect(harness.getHarnessRun).toHaveBeenCalledWith('existing');
  expect(harness.listSkillDrafts).toHaveBeenCalledWith('existing');
  expect(harness.createSkillifyRun).not.toHaveBeenCalled();
  expect(harness.publishSkillDraft).not.toHaveBeenCalled();
});
