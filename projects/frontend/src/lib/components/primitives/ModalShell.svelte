<script lang="ts">
  import type { Snippet } from 'svelte';

  let {
    open,
    onclose,
    align = 'center',
    children,
  }: {
    open: boolean;
    onclose: () => void;
    align?: 'center' | 'top';
    children: Snippet;
  } = $props();

  let dialog = $state<HTMLDialogElement>();

  $effect(() => {
    if (!dialog) return;
    if (open) dialog.showModal();
    else { try { dialog.close(); } catch {} }
  });
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<dialog
  class="shell-{align}"
  bind:this={dialog}
  onclose={onclose}
  onclick={(e) => { if (e.target === dialog) onclose(); }}
>
  {@render children()}
</dialog>

<style>
  dialog {
    background: transparent;
    border: none;
    padding: 0;
    max-width: 100vw;
    max-height: 100vh;
    overflow: visible;
  }
  dialog::backdrop { background: rgba(0, 0, 0, 0.65); }
  .shell-center { margin: auto; }
  .shell-top    { margin: 80px auto auto; }
</style>
