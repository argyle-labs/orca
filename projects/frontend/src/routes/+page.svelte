<script lang="ts">
  import { onMount } from 'svelte';
  import { orca } from '$lib/orcaClient';

  let version = $state<string | null>(null);
  let target = $state<string | null>(null);
  let frontend = $state<string | null>(null);
  let error = $state<string | null>(null);

  onMount(async () => {
    try {
      const client = await orca();
      const spec = (await client.system_runtime_spec({})) as {
        version: string;
        frontend: string;
        target: string;
      };
      version = spec.version;
      frontend = spec.frontend;
      target = spec.target;
    } catch (e) {
      error = String(e);
    }
  });
</script>

<section class="landing">
  <h1>orca</h1>
  <p class="lede">No workspace mounted yet.</p>

  <div class="meta">
    {#if error}
      <span class="err">{error}</span>
    {:else}
      <span>version: <code>{version ?? '…'}</code></span>
      <span>target: <code>{target ?? '…'}</code></span>
      <span>frontend: <code>{frontend ?? '…'}</code></span>
    {/if}
  </div>
</section>

<style>
  .landing {
    max-width: 640px;
    margin: 0 auto;
    padding: var(--space-8) var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  h1 {
    font-size: 2rem;
    letter-spacing: 0.02em;
    margin: 0;
  }
  .lede {
    color: var(--color-text-muted);
    margin: 0;
  }
  .meta {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-4);
    font-size: var(--text-xs);
    color: var(--color-text-muted);
    margin-top: var(--space-4);
  }
  code {
    color: var(--color-text);
    background: var(--color-surface-2);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
  }
  .err {
    color: var(--color-error);
  }
</style>
