<script lang="ts">
  import Button from './primitives/Button.svelte';

  interface Offer {
    offer_id: string;
    peer_hostname: string;
    peer_addr: string;
    peer_port: number;
  }
  interface Props {
    offers: Offer[];
    onaccept: () => void;
  }
  let { offers, onaccept }: Props = $props();
</script>

{#if offers.length > 0}
  <div class="inbound-banner" role="status">
    {#each offers as o (o.offer_id)}
      <div class="inbound-row">
        <div>
          <strong>{o.peer_hostname}</strong> wants to add this host to a pod.
          <span class="dim">({o.peer_addr}:{o.peer_port})</span>
        </div>
        <Button variant="primary" size="sm" onclick={onaccept}>Accept</Button>
      </div>
    {/each}
  </div>
{/if}

<style>
  .inbound-banner {
    margin: 0 0 var(--space-4);
    padding: var(--space-3);
    background: var(--color-accent-subtle, color-mix(in srgb, var(--color-accent) 12%, transparent));
    border: 1px solid var(--color-accent);
    border-radius: var(--radius-md);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .inbound-row {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: var(--space-3);
    font-size: var(--text-sm);
  }
  .dim {
    color: var(--color-text-dim);
    font-family: var(--font-mono);
    margin-left: var(--space-1);
  }
</style>
