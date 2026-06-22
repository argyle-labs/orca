<script lang="ts">
  interface Address {
    kind: string;
    kind_label: string;
    value: string;
  }
  interface Props {
    addresses: Address[];
  }
  let { addresses }: Props = $props();
</script>

{#if addresses.length > 0}
  <dl class="addr-grid">
    {#each addresses as a (a.kind + ':' + a.value)}
      <dt>{a.kind_label}</dt>
      <dd><code>{a.value}</code></dd>
    {/each}
  </dl>
{/if}

<style>
  .addr-grid {
    margin: 0;
    display: grid;
    grid-template-columns: 110px 1fr;
    row-gap: 6px;
    column-gap: var(--space-3);
    font-size: var(--text-xs);
  }
  dt {
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: 10px;
    padding-top: 2px;
  }
  dd {
    margin: 0;
    color: var(--color-text);
    word-break: break-all;
  }
  code {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
    font-size: var(--text-xs);
  }
</style>
