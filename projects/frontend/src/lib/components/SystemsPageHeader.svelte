<script lang="ts">
  import Badge from '$lib/components/primitives/Badge.svelte';
  import { peers } from '$lib/stores/peers.svelte';

  interface Props {
    onAddSystem: () => void;
  }
  let { onAddSystem }: Props = $props();
  let count = $derived(peers.candidates.length);
</script>

<header>
  <div class="title-row">
    <h1>Systems</h1>
    <div class="actions">
      <button type="button" class="add" onclick={onAddSystem} aria-label="Add a system">
        <span>+ System</span>
        {#if count > 0}<Badge color="accent">{count}</Badge>{/if}
      </button>
    </div>
  </div>
  <p class="lede">Connected orca instances.</p>
</header>

<style>
  .title-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }
  h1 {
    margin: 0 0 var(--space-1);
    font-size: var(--text-xl);
    letter-spacing: 0.02em;
  }
  .lede {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }
  .actions {
    margin-left: auto;
    display: flex;
    gap: var(--space-2);
  }
  .add {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    color: var(--color-text);
    padding: var(--space-2) var(--space-3);
    cursor: pointer;
    font: inherit;
    font-size: var(--text-sm);
  }
  .add:hover { background: var(--color-surface-2); }
</style>
