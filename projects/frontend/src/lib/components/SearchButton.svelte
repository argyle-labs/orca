<script lang="ts">
  import SearchIcon from '$lib/components/primitives/icons/SearchIcon.svelte';

  interface Props {
    onclick: () => void;
    label?: string;
    shortcut?: string;
  }
  let { onclick, label = 'Search', shortcut = '⌘K' }: Props = $props();
</script>

<button type="button" class="search-btn" {onclick} title="Search & commands ({shortcut})">
  <SearchIcon />
  <span class="search-label">{label}</span>
  <kbd>{shortcut}</kbd>
</button>

<style>
  .search-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 26px;
    padding: 0 10px;
    background: var(--color-bg);
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    cursor: pointer;
    font-size: var(--text-xs);
  }
  .search-btn:hover {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .search-label {
    min-width: 160px;
    text-align: left;
  }
  kbd {
    font-family: var(--font-mono);
    font-size: 10px;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 0 4px;
    color: var(--color-text-dim);
  }
  @media (max-width: 768px) {
    .search-label {
      display: none;
      min-width: 0;
    }
  }
</style>
