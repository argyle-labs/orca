<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import SegmentedControl from '../SegmentedControl.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/SegmentedControl',
    component: SegmentedControl,
    tags: ['autodocs'],
  });
</script>

<script lang="ts">
  let channel = $state('stable');
  let view = $state<'tree' | 'list'>('tree');
</script>

<Story name="Channel selector">
  {#snippet template()}
    <SegmentedControl
      ariaLabel="Channel"
      value={channel}
      onchange={(v) => (channel = v as string)}
      items={[
        { label: 'stable', value: 'stable' },
        { label: 'rc', value: 'rc' },
        { label: 'dev', value: 'dev' },
      ]}
    />
    <p style="margin-top:0.5rem; font-size:0.8rem; color:var(--color-text-dim);">
      Selected: {channel}
    </p>
  {/snippet}
</Story>

<Story name="View mode (binary)">
  {#snippet template()}
    <SegmentedControl
      ariaLabel="View"
      value={view}
      onchange={(v) => (view = v as 'tree' | 'list')}
      items={[
        { label: 'Tree', value: 'tree' },
        { label: 'List', value: 'list' },
      ]}
    />
  {/snippet}
</Story>

<Story name="Disabled">
  {#snippet template()}
    <SegmentedControl
      ariaLabel="Disabled"
      value="b"
      onchange={() => {}}
      disabled
      items={[
        { label: 'A', value: 'a' },
        { label: 'B', value: 'b' },
        { label: 'C', value: 'c' },
      ]}
    />
  {/snippet}
</Story>
