import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import type { Components } from 'react-markdown';

function slugify(text: string): string {
  return text
    .toLowerCase()
    .trim()
    .replace(/[^\w\s-]/g, '')
    .replace(/[\s_]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

function extractText(node: React.ReactNode): string {
  if (typeof node === 'string') return node;
  if (typeof node === 'number') return String(node);
  if (Array.isArray(node)) return node.map(extractText).join('');
  if (node && typeof node === 'object' && 'props' in node) {
    return extractText((node as React.ReactElement<{ children?: React.ReactNode }>).props.children);
  }
  return '';
}

function makeHeading(level: 1 | 2 | 3 | 4 | 5 | 6) {
  return function Heading({ children, ...rest }: React.HTMLAttributes<HTMLHeadingElement>) {
    const Tag = `h${level}` as 'h1' | 'h2' | 'h3' | 'h4' | 'h5' | 'h6';
    const id = slugify(extractText(children));
    return <Tag id={id} {...rest}>{children}</Tag>;
  };
}

function Anchor({ href, children, ...rest }: React.AnchorHTMLAttributes<HTMLAnchorElement>) {
  function handleClick(e: React.MouseEvent<HTMLAnchorElement>) {
    if (href?.startsWith('#')) {
      e.preventDefault();
      const id = href.slice(1);
      const el = document.getElementById(id);
      if (el) el.scrollIntoView({ behavior: 'smooth', block: 'start' });
    }
  }
  return (
    <a href={href} onClick={handleClick} {...rest}>
      {children}
    </a>
  );
}

const COMPONENTS: Components = {
  h1: makeHeading(1),
  h2: makeHeading(2),
  h3: makeHeading(3),
  h4: makeHeading(4),
  h5: makeHeading(5),
  h6: makeHeading(6),
  a: Anchor,
};

export function MarkdownRenderer({ content }: { content: string }) {
  return (
    <article className="markdown">
      <ReactMarkdown remarkPlugins={[remarkGfm]} components={COMPONENTS}>
        {content}
      </ReactMarkdown>
    </article>
  );
}
