'use client';

import { useState } from 'react';
import { Button } from '@/components/ui/button';
import { activatePersonalSkill, listAuthoringTargets } from '@/lib/api/harnesses';
import type { HarnessRun } from '@/lib/api/types';

export function SkillUseAction({ run, skillName, versionId }: { run: HarnessRun; skillName: string; versionId: string }) {
  const sessions = (run.input_json.session_ids ?? []) as string[];
  const baseline = (run.output_json.authoring as { baseline?: { skill_name: string; version_id: string } } | undefined)?.baseline;
  const oldVersion = baseline?.skill_name === skillName ? baseline.version_id : null;
  const [sessionId, setSessionId] = useState(sessions.length === 1 ? sessions[0] : '');
  const [expectedVersion, setExpectedVersion] = useState<string | null>(oldVersion);
  const [activeVersion, setActiveVersion] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [notice, setNotice] = useState('发布不会自动更改当前会话。');

  async function activateVersion(target: string) {
    setBusy(true); setError(null);
    try {
      const adopted = await activatePersonalSkill(skillName, sessionId, target, expectedVersion);
      setExpectedVersion(adopted.version_id); setActiveVersion(adopted.version_id);
      setNotice(`已启用 ${skillName} · ${adopted.version_id}。下一次请求使用此版本，正在执行的请求不变。`);
    } catch (reason) { setError(reason instanceof Error ? reason.message : '无法启用此版本'); }
    finally { setBusy(false); }
  }

  async function refreshVersion() {
    setBusy(true); setError(null);
    try {
      const current = (await listAuthoringTargets(sessionId)).find((target) => target.skill_name === skillName)?.version_id ?? null;
      setExpectedVersion(current); setActiveVersion(current);
      setNotice(current === versionId ? `已确认 ${skillName} · ${versionId} 已启用。` : `当前版本：${current ?? '未启用'}。再次点击使用会明确替换这个版本。`);
    } catch (reason) { setError(reason instanceof Error ? reason.message : '无法读取当前版本'); }
    finally { setBusy(false); }
  }

  if (!sessions.length) return <p className="mt-3 text-xs">Skill 已保存。此草稿没有来源会话，尚未在任何会话中启用。</p>;
  return <div className="mt-3 space-y-2 text-sm">
    {sessions.length > 1 ? <label>选择使用此 Skill 的来源会话
      <select aria-label="启用 Skill 的会话" value={sessionId} disabled={busy} onChange={(event) => {
        setSessionId(event.target.value); setExpectedVersion(oldVersion); setActiveVersion(null); setError(null);
      }}><option value="">请选择</option>{sessions.map((id) => <option key={id} value={id}>{id}</option>)}</select>
    </label> : null}
    <p className="text-xs text-text-muted">{notice}</p>
    {error ? <p role="alert" className="text-danger">{error} <button disabled={busy} onClick={() => void refreshVersion()}>读取当前版本后重选</button></p> : null}
    <div className="flex flex-wrap gap-2">
      <Button disabled={busy || !sessionId || activeVersion === versionId} onClick={() => void activateVersion(versionId)}>在此会话使用此版本</Button>
      {oldVersion && activeVersion === versionId ? <Button variant="ghost" disabled={busy} onClick={() => void activateVersion(oldVersion)}>切回原版本</Button> : null}
      {activeVersion && sessionId ? <Button variant="ghost" href={`/chats/${encodeURIComponent(sessionId)}`}>返回会话</Button> : null}
    </div>
  </div>;
}
