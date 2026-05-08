<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  let { data } = $props();

  let container: HTMLDivElement;
  let instance: { unmount: () => void } | null = null;

  onMount(async () => {
    const { mountSchemaApp } = await import('$schema/mount');
    if (data.schema) {
      instance = mountSchemaApp(container, data.schema, data.db);
    } else {
      container.innerHTML =
        '<div style="display:flex;align-items:center;justify-content:center;height:100%;color:var(--color-text-dim)">No schema data. Configure a database in System → Schema.</div>';
    }
  });

  onDestroy(() => {
    instance?.unmount();
  });
</script>

<svelte:head><title>Schema — orca</title></svelte:head>

<div class="schema-host" bind:this={container}></div>

<style>
  .schema-host {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    overflow: hidden;
  }
</style>
