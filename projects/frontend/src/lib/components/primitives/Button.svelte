<script lang="ts">
  interface Props {
    variant?: 'primary' | 'secondary' | 'ghost' | 'danger';
    size?: 'xs' | 'sm' | 'md';
    disabled?: boolean;
    type?: 'button' | 'submit';
    title?: string;
    onclick?: () => void;
    children: import('svelte').Snippet;
  }
  let { variant = 'secondary', size = 'md', disabled = false, type = 'button', title, onclick, children }: Props = $props();
</script>

<button {type} {disabled} {title} class="btn btn--{variant} btn--{size}" {onclick}>
  {@render children()}
</button>

<style>
  .btn {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font-family: inherit;
    font-size: var(--text-sm);
    transition: background 0.1s, border-color 0.1s;
    white-space: nowrap;
  }
  .btn:disabled { opacity: 0.5; cursor: not-allowed; }

  .btn--md { padding: var(--space-2) var(--space-3); }
  .btn--sm { padding: var(--space-1) var(--space-2); font-size: var(--text-xs); }
  .btn--xs { padding: 2px 8px; font-size: 11px; border-radius: 4px; }

  .btn--primary {
    background: var(--color-accent);
    border-color: var(--color-accent);
    color: white;
  }
  .btn--primary:hover:not(:disabled) { background: var(--color-accent-dim); }

  .btn--secondary {
    background: var(--color-surface);
    color: var(--color-text);
  }
  .btn--secondary:hover:not(:disabled) { background: var(--color-surface-2); }

  .btn--ghost {
    background: transparent;
    border-color: transparent;
    color: var(--color-text-dim);
  }
  .btn--ghost:hover:not(:disabled) { background: var(--color-surface); color: var(--color-text); }

  .btn--danger {
    background: transparent;
    border-color: var(--color-error);
    color: var(--color-error);
  }
  .btn--danger:hover:not(:disabled) { background: var(--color-error); color: white; }
</style>
