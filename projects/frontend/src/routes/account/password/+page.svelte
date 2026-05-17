<script lang="ts">
  import { goto } from '$app/navigation';
  import { changePassword } from '$lib/stores/session.svelte';
  import { notifications } from '$lib/stores/notifications';

  let current = $state('');
  let next = $state('');
  let confirm = $state('');
  let submitting = $state(false);
  let errorMsg = $state<string | null>(null);

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    errorMsg = null;
    if (next.length < 8) {
      errorMsg = 'new password must be at least 8 characters';
      return;
    }
    if (next !== confirm) {
      errorMsg = 'new passwords do not match';
      return;
    }
    submitting = true;
    try {
      await changePassword(current, next);
      notifications.success('password changed');
      await goto('/');
    } catch (err) {
      errorMsg = err instanceof Error ? err.message : 'change failed';
    } finally {
      submitting = false;
    }
  }
</script>

<div class="wrap">
  <form onsubmit={submit} class="form">
    <h1>Change password</h1>
    <label>
      Current password
      <input
        type="password"
        bind:value={current}
        required
        autocomplete="current-password"
      />
    </label>
    <label>
      New password
      <input
        type="password"
        bind:value={next}
        required
        minlength={8}
        autocomplete="new-password"
      />
    </label>
    <label>
      Confirm new password
      <input
        type="password"
        bind:value={confirm}
        required
        autocomplete="new-password"
      />
    </label>
    {#if errorMsg}
      <p class="error">{errorMsg}</p>
    {/if}
    <div class="row">
      <button type="button" class="cancel" onclick={() => goto('/')}>
        Cancel
      </button>
      <button type="submit" disabled={submitting}>
        {submitting ? 'Saving…' : 'Save'}
      </button>
    </div>
  </form>
</div>

<style>
  .wrap {
    display: flex;
    justify-content: center;
    padding-top: 6vh;
  }
  .form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-width: 360px;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 6px;
    padding: var(--space-4);
  }
  h1 {
    font-size: var(--text-lg);
    margin: 0 0 var(--space-2);
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 4px;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
  }
  input {
    background: var(--color-bg);
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    padding: 6px 8px;
    font-size: var(--text-sm);
  }
  .row {
    display: flex;
    gap: var(--space-2);
    justify-content: flex-end;
  }
  button {
    background: var(--color-accent, var(--color-text));
    color: var(--color-bg);
    border: none;
    border-radius: 4px;
    padding: 8px 14px;
    font-size: var(--text-sm);
    cursor: pointer;
  }
  button.cancel {
    background: transparent;
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
  }
  button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .error {
    color: var(--color-error);
    font-size: var(--text-xs);
    margin: 0;
  }
</style>
