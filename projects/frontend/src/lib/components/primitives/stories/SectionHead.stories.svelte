<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import SectionHead from '../SectionHead.svelte';
  import Button from '../Button.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/SectionHead',
    component: SectionHead,
    tags: ['autodocs'],
    args: { title: 'Section' },
  });
</script>

<Story name="Plain">
  {#snippet template({ trailing: _t, ...args })}
    <div style="width:300px;">
      <SectionHead {...args} />
    </div>
  {/snippet}
</Story>

<Story name="With trailing action" args={{ title: 'Update' }}>
  {#snippet template({ trailing: _t, ...args })}
    <div style="width:300px;">
      <SectionHead {...args}>
        {#snippet trailing()}
          <Button size="xs">Refresh</Button>
        {/snippet}
      </SectionHead>
    </div>
  {/snippet}
</Story>
