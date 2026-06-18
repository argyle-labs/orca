<script lang="ts">
  import type { Snippet } from 'svelte';

  interface Props {
    open: boolean;
    title?: string;
    size?: 'sm' | 'md' | 'lg' | 'xl' | 'full';
    align?: 'center' | 'top';
    onclose: () => void;
    children: Snippet;
  }
  let { open, title, size = 'md', align = 'center', onclose, children }: Props = $props();

  const maxWidths: Record<string, string> = { sm: '400px', md: '600px', lg: '800px', xl: '1000px', full: '1000px' };
  const isFull = $derived(size === 'full');

  let dialog = $state<HTMLDialogElement>();

  $effect(() => {
    if (!dialog) return;
    if (open) dialog.showModal();
    else { try { dialog.close(); } catch {} }
  });
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<dialog
  class="modal modal-{align}"
  bind:this={dialog}
  onclose={onclose}
  onclick={(e) => { if (e.target === dialog) onclose(); }}
>
  <div class="modal-inner" class:modal-full={isFull} style="--_mw:{maxWidths[size] ?? '600px'}">
    {#if title}
      <div class="modal-header">
        <h3>{title}</h3>
        <button class="modal-close" onclick={onclose} aria-label="Close">✕</button>
      </div>
    {/if}
    <div class="modal-body" class:modal-body-full={isFull}>{@render children()}</div>
  </div>
</dialog>

<style>
  .modal {
    background: transparent;
    border: none;
    padding: 0;
    max-width: 100vw;
    max-height: 100vh;
    overflow: visible;
  }
  .modal::backdrop { background: rgba(0, 0, 0, 0.65); }
  .modal-center { margin: auto; }
  .modal-top    { margin: 80px auto auto; }
  .modal-inner {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    color: var(--color-text);
    padding: var(--space-6);
    width: min(var(--_mw, 600px), 92vw);
    box-shadow: var(--shadow-lg);
  }
  .modal-full {
    max-height: 88vh;
    display: flex;
    flex-direction: column;
  }
  .modal-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    margin-bottom: var(--space-4);
    flex-shrink: 0;
  }
  .modal-header h3 { margin: 0; font-size: var(--text-lg); }
  .modal-close {
    background: none;
    border: none;
    color: var(--color-text-dim);
    cursor: pointer;
    font-size: var(--text-base);
    padding: var(--space-1);
  }
  .modal-body-full {
    flex: 1;
    min-height: 0;
    display: flex;
    flex-direction: column;
    position: relative;
  }
</style>
