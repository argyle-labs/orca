<script lang="ts">
  import AddSystemMenu from '$lib/components/AddSystemMenu.svelte';

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
      <AddSystemMenu {onInvite} {onAccept} />
    </div>
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
