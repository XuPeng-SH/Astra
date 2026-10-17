'use client';

import { useSearchParams } from 'next/navigation';
import { useRef, useState } from 'react';

type Reference = { runId: string | null; attemptId: string | null; draftId: string | null };

/** External navigation replaces the view; our own durable-reference updates keep in-flight work mounted. */
export function useAuthoringReference(scopeKey: string) {
  const params = useSearchParams();
  const reference: Reference = { runId: params.get('runId'), attemptId: params.get('attemptId'), draftId: params.get('draftId') };
  const urlKey = JSON.stringify(reference);
  const [view, setView] = useState({ scopeKey, urlKey, reference, generation: 0 });
  const internal = useRef<string[]>([]);
  if (view.scopeKey !== scopeKey || view.urlKey !== urlKey) {
    const index = internal.current.indexOf(urlKey);
    const ownUpdate = view.scopeKey === scopeKey && index >= 0;
    setView(ownUpdate
      ? { ...view, urlKey }
      : { scopeKey, urlKey, reference, generation: view.generation + 1 });
    if (ownUpdate) internal.current.splice(0, index + 1);
    else internal.current = [];
  }
  function replaceReference(next: Partial<Reference>) {
    const reference = { runId: next.runId ?? null, attemptId: next.attemptId ?? null, draftId: next.draftId ?? null };
    internal.current.push(JSON.stringify(reference));
    const url = new URL(window.location.href);
    for (const [key, value] of Object.entries(reference)) {
      if (value === null) url.searchParams.delete(key);
      else url.searchParams.set(key, value);
    }
    window.history.replaceState(null, '', url);
  }
  return { reference: view.reference, key: JSON.stringify([scopeKey, view.generation]), replaceReference };
}
