<script lang="ts">
  import { marked } from 'marked';
  let { data } = $props();

  function stripAppPrefix(md: string): string {
    return md.replace(/^(# .+?) — (.+)$/m, '# $2');
  }

  const html = $derived(data.content ? marked(stripAppPrefix(data.content)) : '');
</script>

<svelte:head><title>{data.path || 'Docs'} — orca</title></svelte:head>

<div class="page">
  {#if html}
    <article class="doc">{@html html}</article>
  {:else}
    <p style="color:var(--color-text-dim)">No document found at /{data.root}/{data.path}</p>
  {/if}
</div>

<style>
  .doc {
    max-width: 800px;
    line-height: 1.75;
    overflow-wrap: break-word;
    word-break: break-word;
    min-width: 0;
  }
  .doc :global(h1), .doc :global(h2), .doc :global(h3) { margin-top: var(--space-8); border-bottom: 1px solid var(--color-border); padding-bottom: var(--space-2); }
  .doc :global(code) { background: var(--color-surface-2); padding: 1px 5px; border-radius: 3px; font-family: var(--font-mono); overflow-wrap: break-word; word-break: break-all; }
  .doc :global(pre) { overflow-x: auto; white-space: pre; }
  .doc :global(pre code) { word-break: normal; overflow-wrap: normal; }
  .doc :global(blockquote) { border-left: 3px solid var(--color-accent); padding-left: var(--space-4); color: var(--color-text-dim); margin: 0; }
  .doc :global(table) { border-collapse: collapse; width: 100%; display: block; overflow-x: auto; }
  .doc :global(td), .doc :global(th) { border: 1px solid var(--color-border); padding: var(--space-2) var(--space-3); }
  .doc :global(img) { max-width: 100%; height: auto; }
  .doc :global(a) { overflow-wrap: break-word; word-break: break-all; }
</style>
