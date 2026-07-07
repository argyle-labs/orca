<script lang="ts">
  import { goto } from '$app/navigation';
  import { signIn } from '$lib/stores/session.svelte';

  let username = $state('');
  let password = $state('');
  let submitting = $state(false);
  let errorMsg = $state<string | null>(null);

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    submitting = true;
    errorMsg = null;
    try {
      await signIn(username, password);
      await goto('/');
    } catch (err) {
      errorMsg = err instanceof Error ? err.message : 'sign-in failed';
    } finally {
      submitting = false;
    }
  }
</script>

<div class="auth-wrap">
  <form onsubmit={submit} class="auth-form">
    <h1>Sign in to orca</h1>
    <label>
      Username
      <input type="text" bind:value={username} required autocomplete="username" />
    </label>
    <label>
      Password
      <input type="password" bind:value={password} required autocomplete="current-password" />
    </label>
    {#if errorMsg}
      <p class="error">{errorMsg}</p>
    {/if}
    <button type="submit" disabled={submitting}>
      {submitting ? 'Signing in…' : 'Sign in'}
    </button>
    <p class="hint">
      No account? <a href="/signup">Create one</a>.
    </p>
  </form>
</div>

<style>
  .auth-wrap {
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 12vh;
  }
  .auth-form {
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
    min-width: 320px;
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
  button {
    background: var(--color-accent, var(--color-text));
    color: var(--color-bg);
    border: none;
    border-radius: 4px;
    padding: 8px;
    font-size: var(--text-sm);
    cursor: pointer;
  }
  button:disabled { opacity: 0.6; cursor: not-allowed; }
  .error { color: var(--color-error); font-size: var(--text-xs); margin: 0; }
  .hint { font-size: var(--text-xs); color: var(--color-text-muted); margin: 0; }
</style>
