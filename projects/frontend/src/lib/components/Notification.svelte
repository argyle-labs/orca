<script lang="ts">
  import { notifications } from '$lib/stores/notifications';

  const colors = { success: 'var(--color-success)', error: 'var(--color-error)', info: 'var(--color-info)' };
</script>

<div class="toast-container">
  {#each $notifications as n (n.id)}
    <div class="toast" style="--toast-color:{colors[n.type]}">
      <span class="toast-msg">{n.message}</span>
      <button class="toast-dismiss" onclick={() => notifications.remove(n.id)}>✕</button>
    </div>
  {/each}
</div>

<style>
  .toast-container {
    position: fixed;
    bottom: var(--space-6);
    right: var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    z-index: 1000;
  }
  .toast {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-3) var(--space-4);
    background: var(--color-surface-2);
    border: 1px solid var(--toast-color);
    border-left: 3px solid var(--toast-color);
    border-radius: var(--radius-md);
    min-width: 280px;
    max-width: 420px;
    font-size: var(--text-sm);
    animation: slide-in 0.15s ease;
  }
  .toast-msg { flex: 1; }
  .toast-dismiss {
    background: none; border: none;
    color: var(--color-text-dim); cursor: pointer;
    font-size: var(--text-xs); padding: 0;
  }
  @keyframes slide-in { from { transform: translateX(20px); opacity: 0; } }
</style>
