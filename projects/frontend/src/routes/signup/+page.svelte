<script lang="ts">
  import { onMount } from 'svelte';
  import { goto } from '$app/navigation';
  import { signUp, signupStatus } from '$lib/stores/session.svelte';

  let username = $state('');
  let password = $state('');
  let submitting = $state(false);
  let errorMsg = $state<string | null>(null);
  let allowed = $state<boolean | null>(null);
  let reason = $state<string>('');

  onMount(async () => {
    const status = await signupStatus();
    allowed = status?.allowed ?? false;
    reason = status?.reason ?? '';
    if (!allowed) {
      errorMsg =
        reason === 'closed'
          ? 'Public sign-up is closed. Ask an admin to create your account.'
          : 'Sign-up unavailable.';
    }
  });

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    if (!allowed) return;
    if (password.length < 8) {
      errorMsg = 'Password must be at least 8 characters.';
      return;
    }
    submitting = true;
    errorMsg = null;
    try {
      await signUp(username, password);
      await goto('/');
    } catch (err) {
      errorMsg = err instanceof Error ? err.message : 'sign-up failed';
    } finally {
      submitting = false;
    }
  }
</script>

<div class="auth-wrap">
  <form onsubmit={submit} class="auth-form">
    <h1>
      {reason === 'first_user' ? 'Create the admin account' : 'Create an account'}
    </h1>
    {#if reason === 'first_user'}
      <p class="hint">
        This host has no users yet — the first account becomes an admin.
      </p>
    {/if}
    <label>
      Username
      <input
        type="text"
        bind:value={username}
        required
        autocomplete="username"
        disabled={!allowed}
      />
    </label>
    <label>
      Password
      <input
        type="password"
        bind:value={password}
        required
        minlength={8}
        autocomplete="new-password"
        disabled={!allowed}
      />
    </label>
    {#if errorMsg}
      <p class="error">{errorMsg}</p>
    {/if}
    <button type="submit" disabled={submitting || !allowed}>
      {submitting ? 'Creating…' : 'Create account'}
    </button>
    {#if reason !== 'first_user'}
      <p class="hint">
        Already have an account? <a href="/signin">Sign in</a>.
      </p>
    {/if}
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
