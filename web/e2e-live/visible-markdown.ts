import { unified } from 'unified';
import remarkGfm from 'remark-gfm';
import remarkMath from 'remark-math';
import remarkParse from 'remark-parse';

type MarkdownNode = {
  type: string;
  value?: string;
  children?: MarkdownNode[];
};

const BLOCK_NODES = new Set([
  'blockquote',
  'list',
  'listItem',
  'root',
  'table',
  'tableRow',
]);

function visibleText(node: MarkdownNode): string {
  if (
    node.type === 'text' ||
    node.type === 'inlineCode' ||
    node.type === 'code' ||
    node.type === 'inlineMath' ||
    node.type === 'math' ||
    node.type === 'html'
  ) {
    return node.value ?? '';
  }
  if (node.type === 'break') return '\n';
  if (!node.children?.length) return '';
  const separator = BLOCK_NODES.has(node.type) ? '\n' : '';
  return node.children.map(visibleText).join(separator);
}

/**
 * Return the text a rendered Markdown transcript exposes to a user.
 *
 * The live browser journey uses this as an oracle, so it must follow the same
 * Markdown grammar as the product renderer. Parsing the document with remark
 * preserves inline and fenced code exactly, including multi-backtick spans and
 * tilde fences, while omitting presentation-only markers.
 */
export function visibleMarkdownSnippet(markdown: string): string {
  try {
    const tree = unified()
      .use(remarkParse)
      .use(remarkGfm)
      .use(remarkMath)
      .parse(markdown) as unknown as MarkdownNode;
    return visibleText(tree).replace(/\s+/gu, ' ').trim().slice(0, 120);
  } catch {
    // A malformed provider response should still produce a bounded diagnostic
    // rather than crash the browser oracle. The normal renderer will expose
    // the raw text in this case, so keep the fallback equally conservative.
    return markdown.replace(/\s+/gu, ' ').trim().slice(0, 120);
  }
}
