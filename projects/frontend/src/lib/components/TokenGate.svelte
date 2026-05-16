<script lang="ts">
  import { onMount } from 'svelte';
  import { authTokenSnapshot, setAuthToken } from '$lib/stores/authToken.svelte';

  let inputValue = $state('');
  let submitting = $state(false);
  let errorMsg = $state<string | null>(null);
  // null = probing, true = first-time setup on loopback, false = paste flow
  let bootstrap = $state<boolean | null>(null);

  const token = $derived(authTokenSnapshot());
  const open = $derived(token === null);

  onMount(async () => {
    try {
      const res = await fetch('/api/auth/bootstrap');
      bootstrap = res.ok ? !!(await res.json()).available : false;
    } catch {
      bootstrap = false;
    }
  });

  function defaultTokenName(): string {
    const platform =
      (navigator as Navigator & { userAgentData?: { platform?: string } }).userAgentData
        ?.platform ??
      navigator.platform ??
      'browser';
    const stamp = new Date().toISOString().slice(0, 16).replace(/[T:]/g, '-');
    return `browser-${platform.toLowerCase()}-${stamp}`;
  }

  async function bootstrapMint() {
    submitting = true;
    errorMsg = null;
    try {
      const res = await fetch('/api/tools/auth.token_create', {
        method: 'POST',
        headers: { 'content-type': 'application/json' },
        body: JSON.stringify({ name: defaultTokenName(), role: 'admin' }),
      });
      if (!res.ok) {
        errorMsg = `server responded ${res.status} — bootstrap may already be closed`;
        bootstrap = false;
        return;
      }
      const data = (await res.json()) as { token: string };
      if (!data.token) {
        errorMsg = 'response missing token';
        return;
      }
      setAuthToken(data.token);
    } catch (e) {
      errorMsg = e instanceof Error ? e.message : String(e);
    } finally {
      submitting = false;
    }
  }

  async function submitPaste(e: SubmitEvent) {
    e.preventDefault();
    errorMsg = null;
    const t = inputValue.trim();
    if (!t) {
      errorMsg = 'Paste a token issued by `auth.token_create` on this host.';
      return;
    }
    submitting = true;
    try {
      // Verify against a real gated endpoint so a wrong token surfaces a 401.
      const res = await fetch('/api/tools/auth.token_list', {
        method: 'POST',
        headers: {
          'content-type': 'application/json',
          authorization: `Bearer ${t}`,
        },
        body: '{}',
      });
      if (!res.ok) {
        errorMsg =
          res.status === 401 ? 'token rejected by server' : `server responded ${res.status}`;
        return;
      }
      setAuthToken(t);
      inputValue = '';
    } catch (err) {
      errorMsg = err instanceof Error ? err.message : String(err);
    } finally {
      submitting = false;
    }
  }
</script>

{#if open}
  <div class="overlay" role="dialog" aria-modal="true" aria-labelledby="tg-title">
    <div class="card">
      {#if bootstrap === null}
        <h2 id="tg-title">orca</h2>
        <p class="hint">Checking authentication state…</p>
      {:else if bootstrap}
        <h2 id="tg-title">Welcome to orca</h2>
        <p class="hint">
          This host has no API tokens yet. Click below to mint an admin token for this
          browser — the token is stored locally and used to authenticate every request.
        </p>
        <button onclick={bootstrapMint} disabled={submitting}>
          {submitting ? 'Creating…' : 'Create admin token for this browser'}
        </button>
        {#if errorMsg}
          <p class="error">{errorMsg}</p>
        {/if}
      {:else}
        <h2 id="tg-title">Sign in to orca</h2>
        <p class="hint">
          This host's REST API requires a bearer token. On the host itself run:
        </p>
        <pre>orca auth token_create my-browser admin</pre>
        <p class="hint">…and paste the returned <code>token</code> below.</p>
        <form onsubmit={submitPaste}>
          <label>
            <span class="sr-only">Bearer token</span>
            <input
              type="password"
              autocomplete="off"
              spellcheck="false"
              placeholder="orca_…"
              bind:value={inputValue}
              disabled={submitting}
            />
          </label>
          {#if errorMsg}
            <p class="error">{errorMsg}</p>
          {/if}
          <button type="submit" disabled={submitting || inputValue.length === 0}>
            {submitting ? 'Verifying…' : 'Save token'}
          </button>
        </form>
      {/if}
    </div>
  </div>
{/if}

<style>
  .overlay {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.55);
    display: flex;
    align-items: center;
    justify-content: center;
    z-index: 1000;
    padding: var(--space-4);
  }
  .card {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 8px;
    padding: var(--space-4);
    width: min(520px, 100%);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  h2 {
    margin: 0;
    font-size: var(--text-md);
    font-weight: var(--weight-semibold);
  }
  .hint {
    margin: 0;
    color: var(--color-text-muted);
    font-size: var(--text-xs);
  }
  pre {
    margin: 0;
    padding: var(--space-2);
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    font-size: var(--text-xs);
    overflow-x: auto;
    font-family: var(--font-mono);
    white-space: pre-wrap;
    word-break: break-all;
  }
  input {
    width: 100%;
    padding: 8px 10px;
    background: var(--color-bg);
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    font-family: var(--font-mono);
    font-size: var(--text-sm);
  }
  form {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  button {
    padding: 8px 14px;
    background: var(--color-accent, #4a90e2);
    color: white;
    border: none;
    border-radius: 4px;
    cursor: pointer;
    font-weight: var(--weight-semibold);
  }
  button:disabled {
    opacity: 0.6;
    cursor: not-allowed;
  }
  .error {
    margin: 0;
    color: var(--color-error, #f87171);
    font-size: var(--text-xs);
  }
  .sr-only {
    position: absolute;
    width: 1px;
    height: 1px;
    overflow: hidden;
    clip: rect(0 0 0 0);
  }
</style>
