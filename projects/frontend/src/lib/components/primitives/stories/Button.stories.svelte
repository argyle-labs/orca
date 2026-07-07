<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import Button from '../Button.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/Button',
    component: Button,
    tags: ['autodocs'],
    argTypes: {
      variant: { control: 'select', options: ['primary', 'secondary', 'ghost', 'danger'] },
      size: { control: 'select', options: ['xs', 'sm', 'md'] },
      disabled: { control: 'boolean' },
    },
    args: { variant: 'secondary', size: 'md', disabled: false },
  });
</script>

<Story name="Primary" args={{ variant: 'primary' }}>
  {#snippet template({ children: _children, ...args })}
    <Button {...args}>Save changes</Button>
  {/snippet}
</Story>

<Story name="Secondary">
  {#snippet template({ children: _children, ...args })}
    <Button {...args}>Cancel</Button>
  {/snippet}
</Story>

<Story name="Ghost" args={{ variant: 'ghost' }}>
  {#snippet template({ children: _children, ...args })}
    <Button {...args}>Dismiss</Button>
  {/snippet}
</Story>

<Story name="Danger" args={{ variant: 'danger' }}>
  {#snippet template({ children: _children, ...args })}
    <Button {...args}>Delete</Button>
  {/snippet}
</Story>

<Story name="Sizes">
  {#snippet template()}
    <div style="display:flex; gap:0.5rem; align-items:center;">
      <Button size="xs">xs</Button>
      <Button size="sm">sm</Button>
      <Button size="md">md</Button>
    </div>
  {/snippet}
</Story>

<Story name="Disabled" args={{ disabled: true }}>
  {#snippet template({ children: _children, ...args })}
    <Button {...args}>Unavailable</Button>
  {/snippet}
</Story>
