<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Badge from '../Badge.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Badge',
    component: Badge,
    tags: ['autodocs'],
    argTypes: {
      color: {
        control: 'select',
        options: ['gray', 'green', 'yellow', 'red', 'blue', 'purple', 'accent'],
      },
    },
    args: { color: 'gray' },
  });
</script>

<Story name="Default">
  {#snippet template({ children: _children, ...args })}
    <Badge {...args}>label</Badge>
  {/snippet}
</Story>

<Story name="All colors">
  {#snippet template()}
    <div style="display:flex; gap:0.5rem; flex-wrap:wrap;">
      <Badge color="gray">gray</Badge>
      <Badge color="green">up</Badge>
      <Badge color="yellow">degraded</Badge>
      <Badge color="red">down</Badge>
      <Badge color="blue">info</Badge>
      <Badge color="purple">lxc</Badge>
      <Badge color="accent">system</Badge>
    </div>
  {/snippet}
</Story>
