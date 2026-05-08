<script lang="ts">
  import { onMount } from 'svelte';
  import type { Component } from 'svelte';

  let { data } = $props();

  let SchemaApp = $state<Component<{ data: SchemaData; initialTabName?: string }> | null>(null);
  let normalized = $state<SchemaData | null>(null);

  onMount(async () => {
    if (!data.schema) return;
    const mod = await import('$schema/App.svelte');
    normalized = mod.normalizeSchema(data.schema);
    if (normalized.tabs.length === 0) {
      normalized = null;
      return;
    }
    SchemaApp = mod.default;
  });
</script>

<svelte:head><title>Schema — orca</title></svelte:head>

<div class="schema-host">
  {#if !data.schema}
    <div class="empty">No schema data. Configure a database in System → Schema.</div>
  {:else if SchemaApp && normalized}
    <SchemaApp data={normalized} initialTabName={data.db} />
  {:else if normalized && normalized.tabs.length === 0}
    <div class="empty">No schema data. Configure a database in System → Schema.</div>
  {/if}
</div>

<style>
  .schema-host {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
  .empty {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--color-text-dim);
  }
</style>
