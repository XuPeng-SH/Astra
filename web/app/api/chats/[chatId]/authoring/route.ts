import { NextRequest, NextResponse } from 'next/server';
import { RuntimeClientError, requireRuntimeClient, runtimeErrorDetail } from '@/lib/runtime-client';

export const dynamic = 'force-dynamic';

export async function POST(
  request: NextRequest,
  context: { params: Promise<{ chatId: string }> },
) {
  try {
    const runtime = await requireRuntimeClient({
      auth: 'required',
      operation: 'create authoring intent',
    });
    const { chatId } = await context.params;
    const body = await request.json();
    return NextResponse.json(
      await runtime.post(`/harnesses/authoring/${encodeURIComponent(chatId)}`, body),
      { status: 201 },
    );
  } catch (error) {
    return NextResponse.json(
      { error: runtimeErrorDetail(error, 'Failed to start authoring.') },
      { status: error instanceof RuntimeClientError ? (error.status ?? 502) : 502 },
    );
  }
}
