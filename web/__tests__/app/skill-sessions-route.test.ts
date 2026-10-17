// @vitest-environment node
import { NextRequest } from 'next/server';
import { GET, POST } from '@/app/api/sessions/route';
import { POST as deactivate } from '@/app/api/skills/user/[skillName]/deactivate/route';
import { requireRuntimeClient, RuntimeClientError } from '@/lib/runtime-client';

vi.mock('@/lib/runtime-client', async (original) => ({ ...await original<typeof import('@/lib/runtime-client')>(), requireRuntimeClient: vi.fn() }));

beforeEach(() => vi.resetAllMocks());

it('lists only through the authenticated session API and forwards the complete cursor', async () => {
  const listRuntimeSessions = vi.fn().mockResolvedValue({ sessions: [], next_cursor: null });
  vi.mocked(requireRuntimeClient).mockResolvedValue({ sdk: { listRuntimeSessions } } as never);
  const response = await GET(new NextRequest('http://web.test/api/sessions?after_updated_at=stamp&after_session_id=session'));
  expect(response.status).toBe(200);
  expect(requireRuntimeClient).toHaveBeenCalledWith(expect.objectContaining({ auth: 'required' }));
  expect(listRuntimeSessions).toHaveBeenCalledWith({ limit: 50, cursor: { updated_at: 'stamp', session_id: 'session' } });
  expect((await GET(new NextRequest('http://web.test/api/sessions?after_session_id=session'))).status).toBe(400);
  expect(listRuntimeSessions).toHaveBeenCalledTimes(1);
});

it('rejects unauthenticated creation without falling back to a local session', async () => {
  vi.mocked(requireRuntimeClient).mockRejectedValue(new RuntimeClientError({ operation: 'create session', path: '/sessions', status: 401, detail: 'Login required' }));
  const response = await POST(new NextRequest('http://web.test/api/sessions', { method: 'POST', body: JSON.stringify({ title: 'Skill task' }) }));
  expect(response.status).toBe(401);
});

it('forwards deactivation only through the authenticated runtime with its explicit version expectation', async () => {
  const post = vi.fn().mockResolvedValue({ active_version_id: null });
  vi.mocked(requireRuntimeClient).mockResolvedValue({ post } as never);
  const body = { session_id: 'session', expected_active_version_id: 'first' };
  const response = await deactivate(new NextRequest('http://web.test/api/skills/user/review/deactivate', {
    method: 'POST', body: JSON.stringify(body),
  }), { params: Promise.resolve({ skillName: 'review/changes' }) });
  expect(response.status).toBe(200);
  expect(requireRuntimeClient).toHaveBeenCalledWith({ auth: 'required', operation: 'deactivate personal Skill' });
  expect(post).toHaveBeenCalledWith('/skills/user/review%2Fchanges/deactivate', body);
  vi.mocked(requireRuntimeClient).mockRejectedValue(new RuntimeClientError({ operation: 'deactivate', path: '/skills/user/review/deactivate', status: 409, detail: 'Changed version' }));
  const conflict = await deactivate(new NextRequest('http://web.test/api/skills/user/review/deactivate', {
    method: 'POST', body: JSON.stringify(body),
  }), { params: Promise.resolve({ skillName: 'review' }) });
  expect(conflict.status).toBe(409);
  expect(post).toHaveBeenCalledTimes(1);
});
