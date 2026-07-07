<script lang="ts">
  import Button from '$lib/components/primitives/Button.svelte';
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
    <h1>Topology</h1>
    <div class="actions">
      <Button onclick={onAddSystem} title="Add a system">
        + System{#if count > 0}&nbsp;<Badge color="accent">{count}</Badge>{/if}
      </Button>
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
</style>
