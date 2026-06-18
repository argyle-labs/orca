<script lang="ts">
  import ModalShell from './ModalShell.svelte';

  interface Props {
    open: boolean;
    title?: string;
    size?: 'sm' | 'md' | 'lg' | 'xl' | 'full';
    onclose: () => void;
    children: import('svelte').Snippet;
  }
  let { open, title, size = 'md', onclose, children }: Props = $props();

  const maxWidths: Record<string, string> = { sm: '400px', md: '600px', lg: '800px', xl: '1000px', full: '1000px' };
  const isFull = $derived(size === 'full');
</script>

<ModalShell {open} {onclose}>
  <div class="modal-inner" class:modal-full={isFull} style="--_mw:{maxWidths[size] ?? '600px'}">
    {#if title}
      <div class="modal-header">
        <h3>{title}</h3>
        <button class="modal-close" onclick={onclose} aria-label="Close">✕</button>
      </div>
    {/if}
    <div class="modal-body" class:modal-body-full={isFull}>{@render children()}</div>
  </div>
</ModalShell>

<style>
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
