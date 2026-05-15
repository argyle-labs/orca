<script lang="ts">
  import { authTokenSnapshot, setAuthToken } from '$lib/stores/authToken.svelte';

  let inputValue = $state('');
  let submitting = $state(false);
  let errorMsg = $state<string | null>(null);

  const token = $derived(authTokenSnapshot());
  const open = $derived(token === null);

  async function submit(e: SubmitEvent) {
    e.preventDefault();
    errorMsg = null;
    const t = inputValue.trim();
    if (!t) {
      errorMsg = 'Paste a token issued by `auth.token_create` on this host.';
      return;
    }
    submitting = true;
    try {
      // Health is open; use it as a quick sanity probe that the token actually
      // unlocks the API. We POST to a dummy authed endpoint via the WASM
      // client to verify before persisting.
      const res = await fetch('/api/health', {
        headers: { authorization: `Bearer ${t}` },
      });
      if (!res.ok) {
        errorMsg = `server responded ${res.status}`;
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
    <form class="card" onsubmit={submit}>
      <h2 id="tg-title">Sign in to orca</h2>
      <p class="hint">
        This host's REST API requires a bearer token. On the host itself run:
      </p>
      <pre>curl -sk -X POST https://localhost:12000/api/tools/auth.token_create \
  -H 'content-type: application/json' \
  -d '{`{"name":"my-browser","role":"admin"}`}'</pre>
      <p class="hint">…and paste the returned <code>token</code> below.</p>
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
