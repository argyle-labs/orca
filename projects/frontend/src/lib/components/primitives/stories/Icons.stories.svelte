<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import CheckIcon from '../icons/CheckIcon.svelte';
  import ChevronDownIcon from '../icons/ChevronDownIcon.svelte';
  import MoonIcon from '../icons/MoonIcon.svelte';
  import SearchIcon from '../icons/SearchIcon.svelte';
  import SidebarToggleIcon from '../icons/SidebarToggleIcon.svelte';
  import SunIcon from '../icons/SunIcon.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Icons',
    tags: ['autodocs'],
  });

  const ICONS = [
    { name: 'CheckIcon', component: CheckIcon },
    { name: 'ChevronDownIcon', component: ChevronDownIcon },
    { name: 'MoonIcon', component: MoonIcon },
    { name: 'SearchIcon', component: SearchIcon },
    { name: 'SidebarToggleIcon', component: SidebarToggleIcon },
    { name: 'SunIcon', component: SunIcon },
  ];
</script>

<Story name="Gallery">
  {#snippet template()}
    <div
      style="display:grid; grid-template-columns:repeat(auto-fill, minmax(140px, 1fr)); gap:1rem;"
    >
      {#each ICONS as { name, component: C }}
        <div
          style="display:flex; flex-direction:column; align-items:center; gap:0.5rem; padding:1rem; border:1px solid var(--color-border); border-radius:6px; color:var(--color-text);"
        >
          <C size={24} />
          <code style="font-size:0.75rem; color:var(--color-text-dim);">{name}</code>
        </div>
      {/each}
    </div>
  {/snippet}
</Story>

<Story name="Sizes">
  {#snippet template()}
    <div style="display:flex; gap:1.5rem; align-items:center; color:var(--color-text);">
      <SearchIcon size={12} />
      <SearchIcon size={16} />
      <SearchIcon size={20} />
      <SearchIcon size={24} />
      <SearchIcon size={32} />
      <SearchIcon size={48} />
    </div>
  {/snippet}
</Story>

<Story name="In context (with currentColor)">
  {#snippet template()}
    <div style="display:flex; gap:1rem; align-items:center;">
      <span style="display:inline-flex; gap:0.4rem; align-items:center; color:var(--color-accent);">
        <CheckIcon size={14} /> success accent
      </span>
      <span style="display:inline-flex; gap:0.4rem; align-items:center; color:var(--color-error);">
        <CheckIcon size={14} /> error red
      </span>
      <span style="display:inline-flex; gap:0.4rem; align-items:center; color:var(--color-text-dim);">
        <ChevronDownIcon size={14} /> dim
      </span>
    </div>
  {/snippet}
</Story>
