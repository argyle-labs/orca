<script lang="ts">
  import type { Snippet } from 'svelte';
  interface Props {
    title: string;
    trailing?: Snippet;
  }
  let { title, trailing }: Props = $props();
</script>

<div class="section-head">
  <span>{title}</span>
  {#if trailing}{@render trailing()}{/if}
</div>

<style>
  .section-head {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-dim);
    border-bottom: 1px solid var(--color-border);
    padding-bottom: var(--space-1);
  }
</style>
