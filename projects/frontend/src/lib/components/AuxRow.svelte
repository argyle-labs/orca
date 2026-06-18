<script lang="ts">
  import StatusDot from './primitives/StatusDot.svelte';
  import Button from './primitives/Button.svelte';

  interface Props {
    name: string;
    sub?: string;
    tag?: string;
    statusOk: boolean | null;
    actionLabel: string;
    busyLabel?: string;
    busy?: boolean;
    actionVariant?: 'primary' | 'danger';
    onaction: () => void;
  }
  let {
    name,
    sub,
    tag,
    statusOk,
    actionLabel,
    busyLabel,
    busy = false,
    actionVariant = 'primary',
    onaction,
  }: Props = $props();
</script>

<div class="aux-row">
  <div class="aux-ident">
    <StatusDot ok={statusOk} />
    <span class="aux-name">{name}</span>
    {#if sub}<span class="dim">{sub}</span>{/if}
    {#if tag}<span class="aux-tag">{tag}</span>{/if}
  </div>
  <Button variant={actionVariant} size="sm" disabled={busy} onclick={onaction}>
    {busy && busyLabel ? busyLabel : actionLabel}
  </Button>
</div>

<style>
  .aux-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-2) var(--space-3);
    border-bottom: 1px solid var(--color-border);
  }
  .aux-row:last-child { border-bottom: none; }
  .aux-ident {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-sm);
  }
  .aux-name { font-weight: var(--weight-semibold); }
  .dim {
    color: var(--color-text-dim);
    font-family: var(--font-mono);
    font-size: var(--text-xs);
  }
  .aux-tag {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-text-dim);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
  }
</style>
