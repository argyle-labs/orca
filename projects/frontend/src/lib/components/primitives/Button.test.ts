import { render, screen } from '@testing-library/svelte';
import { describe, it, expect } from 'vitest';
import { createRawSnippet } from 'svelte';
import Button from './Button.svelte';

function textSnippet(text: string) {
  return createRawSnippet(() => ({ render: () => `<span>${text}</span>` }));
}

describe('Button', () => {
  it('renders children text', () => {
    render(Button, { props: { children: textSnippet('Click me') } });
    expect(screen.getByRole('button')).toHaveTextContent('Click me');
  });

  it('is disabled when disabled prop is true', () => {
    render(Button, { props: { disabled: true, children: textSnippet('X') } });
    expect(screen.getByRole('button')).toBeDisabled();
  });

  it('applies the variant class', () => {
    render(Button, { props: { variant: 'danger', children: textSnippet('Delete') } });
    expect(screen.getByRole('button').className).toContain('btn--danger');
  });
});
