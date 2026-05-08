import { describe, it, expect } from 'vitest';
import { render, act } from '@testing-library/react';
import { Toaster, toast } from './Toaster';
import { MarkdownRenderer } from './MarkdownRenderer';

describe('Toaster snapshots', () => {
  it('renders nothing when no toasts', () => {
    const { container } = render(<Toaster />);
    expect(container.firstChild).toBeNull();
  });

  it('renders a single error toast', async () => {
    const { container } = render(<Toaster />);
    await act(async () => {
      toast('error', 'Something went wrong');
    });
    expect(container).toMatchSnapshot();
  });

  it('renders a success toast', async () => {
    const { container } = render(<Toaster />);
    await act(async () => {
      toast('success', 'Saved successfully');
    });
    expect(container).toMatchSnapshot();
  });

  it('renders an info toast', async () => {
    const { container } = render(<Toaster />);
    await act(async () => {
      toast('info', 'Just so you know');
    });
    expect(container).toMatchSnapshot();
  });
});

describe('MarkdownRenderer snapshots', () => {
  it('renders headings with slugified ids', () => {
    const { container } = render(<MarkdownRenderer content="# Hello World\n\n## Sub Section" />);
    expect(container).toMatchSnapshot();
  });

  it('renders inline code and code blocks', () => {
    const { container } = render(
      <MarkdownRenderer content={'Use `const x = 1` inline.\n\n```ts\nconst y = 2;\n```'} />
    );
    expect(container).toMatchSnapshot();
  });

  it('renders GFM table', () => {
    const { container } = render(
      <MarkdownRenderer content={'| Col A | Col B |\n|-------|-------|\n| a     | b     |'} />
    );
    expect(container).toMatchSnapshot();
  });

  it('renders anchor with hash href', () => {
    const { container } = render(
      <MarkdownRenderer content={'[Jump](#section)'} />
    );
    expect(container).toMatchSnapshot();
  });
});
