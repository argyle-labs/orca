<script lang="ts">
  import Button from '$lib/components/Button.svelte';
  import RetentionPicker from '$lib/components/RetentionPicker.svelte';

  interface Props {
    onInvite: () => void;
    onAccept: () => void;
  }
  let { onInvite, onAccept }: Props = $props();
</script>

<header>
  <div class="title-row">
    <h1>Systems</h1>
    <div class="actions">
      <Button onclick={onInvite}>+ Invite host</Button>
      <Button onclick={onAccept}>+ Pair with code</Button>
    </div>
    <RetentionPicker />
  </div>
  <p class="lede">Connected orca instances.</p>
</header>

<style>
  .title-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-4);
  }
  h1 {
    margin: 0 0 var(--space-1);
    font-size: var(--text-xl);
    letter-spacing: 0.02em;
  }
  .lede {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--text-sm);
  }
  .actions {
    margin-left: auto;
    display: flex;
    gap: var(--space-2);
  }
</style>
