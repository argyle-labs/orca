<script lang="ts">
  import { serverHealth } from '$lib/stores/serverHealth';
</script>

{#if $serverHealth === 'down'}
  <div class="banner">
    Orca backend unreachable —
    <button onclick={() => serverHealth.retry()}>retry</button>
  </div>
{/if}

<style>
  .banner {
    background: rgba(248, 113, 113, 0.12);
    border-bottom: 1px solid rgba(248, 113, 113, 0.3);
    color: var(--color-error);
    font-size: var(--text-xs);
    padding: 5px var(--space-4);
    text-align: center;
  }
  .banner button {
    background: none;
    border: none;
    cursor: pointer;
    color: var(--color-error);
    font-size: var(--text-xs);
    text-decoration: underline;
    padding: 0;
  }
</style>
