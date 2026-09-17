import { render } from '@testing-library/react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import remarkMath from 'remark-math';
import { visibleMarkdownSnippet } from '@/e2e-live/visible-markdown';

describe('visibleMarkdownSnippet', () => {
  it('keeps inline code identifiers and operators visible', () => {
    const snippet = visibleMarkdownSnippet('Use `run_id` to find `a*b != c` next');
    expect(snippet).toContain('run_id');
    expect(snippet).toContain('a*b != c');
  });

  it('keeps fenced code contents while dropping the fence', () => {
    expect(visibleMarkdownSnippet('```python\n__init__\n```')).toContain('__init__');
  });

  it('matches Markdown code delimiter rules used by the Web renderer', () => {
    expect(visibleMarkdownSnippet('Use ```__init__``` now')).toBe('Use __init__ now');
    expect(visibleMarkdownSnippet('Use ``a`b`` now')).toBe('Use a`b now');
    expect(visibleMarkdownSnippet('~~~python\n__init__\n~~~')).toBe('__init__');
  });

  it('is a substring of the real transcript renderer for code forms', () => {
    for (const markdown of [
      'Use ```__init__``` now',
      'Use ``a`b`` now',
      '~~~python\n__init__\n~~~',
      'The **result** is **ready**.',
      'Call `foo()`.',
    ]) {
      const view = render(
        <ReactMarkdown remarkPlugins={[remarkGfm, remarkMath]}>{markdown}</ReactMarkdown>,
      );
      const rendered = view.container.textContent?.replace(/\s+/gu, ' ').trim() ?? '';
      expect(rendered).toContain(visibleMarkdownSnippet(markdown));
      view.unmount();
    }
  });

  it('removes presentation markers from ordinary Markdown', () => {
    expect(visibleMarkdownSnippet('## **Result**\n\n- *ready*')).toBe('Result ready');
    expect(visibleMarkdownSnippet('The **result** is **ready**.')).toBe(
      'The result is ready.',
    );
    expect(visibleMarkdownSnippet('Call `foo()`.')).toBe('Call foo().');
  });
});
