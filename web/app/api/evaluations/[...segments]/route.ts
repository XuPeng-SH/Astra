import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

type RouteContext = { params: Promise<{ segments: string[] }> };
type HttpMethod = 'GET' | 'POST';

function encodedSegments(segments: string[]) {
  return segments.map((segment) => encodeURIComponent(segment)).join('/');
}

function runtimePath(segments: string[], method: HttpMethod) {
  if (segments.length === 1 && segments[0] === 'models' && method === 'GET') {
    return '/models?purpose=typed_judgment&limit=200';
  }
  if (segments.length === 1 && segments[0] === 'skills' && method === 'GET') {
    return '/skills/user';
  }
  if (
    segments.length === 3 &&
    segments[0] === 'skills' &&
    segments[2] === 'versions' &&
    method === 'GET'
  ) {
    return `/skills/user/${encodeURIComponent(segments[1])}/versions`;
  }
  if (segments[0] !== 'experiments') {
    return null;
  }
  if (segments.length === 2 && segments[1] === 'prepare' && method === 'POST') {
    return '/evaluation/experiments/prepare';
  }
  if (segments.length === 2 && method === 'GET') {
    return `/evaluation/experiments/${encodedSegments([segments[1]])}`;
  }
  if (segments.length === 3 && segments[2] === 'report' && method === 'GET') {
    return `/evaluation/experiments/${encodedSegments([segments[1]])}/report`;
  }
  if (
    segments.length === 5 &&
    segments[2] === 'trials' &&
    (segments[4] === 'start' || segments[4] === 'assess') &&
    method === 'POST'
  ) {
    return `/evaluation/experiments/${encodedSegments([segments[1]])}/trials/${encodedSegments([segments[3]])}/${segments[4]}`;
  }
  return null;
}

async function handle(request: NextRequest, method: HttpMethod, context: RouteContext) {
  const { segments } = await context.params;
  const path = runtimePath(segments, method);
  if (!path) {
    return NextResponse.json({ error: 'evaluation route not found' }, { status: 404 });
  }

  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: `${method} evaluation control plane`,
    });
    const hasBody = !(segments.length === 5 && segments[4] === 'assess');
    const json = method === 'POST' && hasBody ? await request.json() : undefined;
    return NextResponse.json(
      await runtime.request(path, {
        method,
        auth: 'required',
        operation: `${method} ${path}`,
        ...(json === undefined ? {} : { json }),
      }),
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Evaluation control plane is unavailable.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}

export async function GET(request: NextRequest, context: RouteContext) {
  return handle(request, 'GET', context);
}

export async function POST(request: NextRequest, context: RouteContext) {
  return handle(request, 'POST', context);
}
