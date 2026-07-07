<script lang="ts">
  import Modal from './primitives/Modal.svelte';
  import { podList, podJoin } from '$lib/client/sdk.gen';
  import { unwrap } from '$lib/stores/runTool';
  import type { PodDiscoveryRowDto } from '$lib/client/types.gen';

  type Mode = 'invite' | 'accept';

  interface Props {
    open: boolean;
    /** Pre-select a mode when opening. Defaults to 'accept' (the most common
     *  user flow: someone hands you a code). */
    initialMode?: Mode;
    /** Pre-fill the code field (e.g. opened from an inbound-offer notification). */
    initialCode?: string;
    onclose: () => void;
    onpaired?: () => void;
  }
  let { open, initialMode = 'accept', initialCode = '', onclose, onpaired }: Props = $props();

  type AcceptResult = {
    action: string;
    pod_id?: string;
    inviter_peer_id?: string;
    inviter_hostname?: string;
    inviter_addr?: string;
    inviter_port?: number;
  };

  type InviteResult = {
    action: string;
    pairing_code?: string;
    joiner_hostname?: string;
    joiner_addr?: string;
    joiner_port?: number;
    joiner_pubkey_fp?: string;
    expires_at?: number;
  };


  let mode = $state<Mode>('accept');
  let code = $state('');

  // accept-mode state
  let acceptPending = $state(false);
  let acceptError = $state<string | null>(null);
  let acceptSuccess = $state<AcceptResult | null>(null);

  // invite-mode state
  let discovery = $state<PodDiscoveryRowDto[]>([]);
  let discoveryLoading = $state(false);
  let manualAddr = $state('');
  let invitePending = $state(false);
  let inviteError = $state<string | null>(null);
  let inviteResult = $state<InviteResult | null>(null);
  let waitingForJoiner = $state(false);
  let pollHandle: ReturnType<typeof setInterval> | null = null;

  // Re-sync local state whenever the parent reopens the modal with new props.
  $effect(() => {
    if (open) {
      mode = initialMode;
      if (initialCode) code = initialCode;
    } else {
      stopPolling();
    }
  });

  // Load discovery whenever the modal opens — discovery is always shown at the
  // top, independent of which tab is active. One modal, one pathway.
  $effect(() => {
    if (open) {
      void loadDiscovery();
    }
  });

  function reset() {
    code = '';
    acceptPending = false;
    acceptError = null;
    acceptSuccess = null;
    manualAddr = '';
    invitePending = false;
    inviteError = null;
    inviteResult = null;
    waitingForJoiner = false;
    stopPolling();
  }

  function handleClose() {
    reset();
    onclose();
  }

  async function loadDiscovery() {
    discoveryLoading = true;
    try {
      const list = await unwrap(podList({ body: {} }));
      const rows = (list?.members ?? [])
        .filter((m) => m.state === 'discovered');
      discovery = rows.filter((r) => r.can_invite);
    } catch (e) {
      inviteError = e instanceof Error ? e.message : String(e);
    } finally {
      discoveryLoading = false;
    }
  }

  async function submitAccept() {
    const trimmed = code.trim().toUpperCase();
    if (!/^[A-Z0-9]{6}$/.test(trimmed)) {
      acceptError = 'Enter the 6-character pairing code from the inviter.';
      return;
    }
    acceptPending = true;
    acceptError = null;
    try {
      const data = await unwrap(podJoin({ body: {
        action: 'accept',
        code: trimmed,
      } }));
      if (data.action !== 'accept') throw new Error(`unexpected action: ${data.action}`);
      acceptSuccess = data;
      onpaired?.();
    } catch (e) {
      acceptError = e instanceof Error ? e.message : String(e);
    } finally {
      acceptPending = false;
    }
  }

  async function invite(target: { addr: string; port?: number; fp?: string }) {
    invitePending = true;
    inviteError = null;
    try {
      const body: { action: string; addr: string; port?: number } = { action: 'invite', addr: target.addr };
      if (target.port) body.port = target.port;
      const data = await unwrap(podJoin({ body }));
      if (data.action !== 'invite') throw new Error(`unexpected action: ${data.action}`);
      inviteResult = data;
      waitingForJoiner = true;
      startPolling(target.fp ?? data.joiner_pubkey_fp ?? null);
    } catch (e) {
      inviteError = e instanceof Error ? e.message : String(e);
    } finally {
      invitePending = false;
    }
  }

  async function submitManualInvite() {
    if (!manualAddr.trim()) {
      inviteError = 'Enter a host or host:port to invite.';
      return;
    }
    await invite({ addr: manualAddr.trim() });
  }

  function startPolling(fp: string | null) {
    stopPolling();
    if (!fp) return;
    pollHandle = setInterval(async () => {
      try {
        const list = await unwrap(podList({ body: {} }));
        const peers = (list?.members ?? [])
          .filter((m) => m.state === 'joined')
          .map((m) => ({
            peer_id: m.peer_id,
            hostname: m.hostname,
            pubkey_fp: m.pubkey_fp,
          }));
        // Peer service builds `peer_id` from the joiner's CN (machine_id_short).
        // We match by hostname here because pubkey_fp isn't on PodPeerDto;
        // the inviter row uses the joiner's reported hostname.
        const joinerHostname = inviteResult?.joiner_hostname;
        const match = (peers ?? []).find(
          (p) => joinerHostname && p.hostname === joinerHostname,
        );
        if (match) {
          waitingForJoiner = false;
          stopPolling();
          onpaired?.();
        }
      } catch {
        // ignore polling errors — keep waiting
      }
    }, 2000);
  }

  function stopPolling() {
    if (pollHandle) {
      clearInterval(pollHandle);
      pollHandle = null;
    }
  }

  async function copyCode() {
    if (!inviteResult?.pairing_code) return;
    try {
      await navigator.clipboard.writeText(inviteResult.pairing_code);
    } catch {
      // ignore — user can read it off screen
    }
  }
</script>

<Modal {open} title="Add a system" onclose={handleClose} size="sm">
  {#if !inviteResult && !acceptSuccess}
    <section class="discovery-section">
      <div class="section-label">Discovered on LAN</div>
      {#if discoveryLoading && discovery.length === 0}
        <p class="dim sm">Loading mDNS discovery…</p>
      {:else if discovery.length === 0}
        <p class="dim sm">None seen yet. Add by address or paste a code below.</p>
      {:else}
        <ul class="discovery">
          {#each discovery as d (d.pubkey_fp)}
            <li>
              <div class="d-id">
                <strong>{d.hostname}</strong>
                <span class="dim">{d.addr}:{d.port}</span>
              </div>
              <button
                class="btn primary sm"
                disabled={invitePending}
                onclick={() => invite({ addr: d.addr, port: d.port, fp: d.pubkey_fp })}
              >+ Add</button>
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/if}

  <div class="mode-tabs" role="tablist">
    <button
      class="tab"
      class:active={mode === 'accept'}
      role="tab"
      aria-selected={mode === 'accept'}
      onclick={() => (mode = 'accept')}
    >Accept code</button>
    <button
      class="tab"
      class:active={mode === 'invite'}
      role="tab"
      aria-selected={mode === 'invite'}
      onclick={() => (mode = 'invite')}
    >Invite host</button>
  </div>

  {#if mode === 'accept'}
    {#if acceptSuccess}
      <p class="ok">
        ✓ Joined pod <code>{acceptSuccess.pod_id ?? '?'}</code> via
        <strong>{acceptSuccess.inviter_hostname ?? acceptSuccess.inviter_peer_id ?? 'inviter'}</strong>.
      </p>
      {#if acceptSuccess.inviter_addr}
        <p class="dim">{acceptSuccess.inviter_addr}:{acceptSuccess.inviter_port}</p>
      {/if}
      <div class="actions">
        <button class="btn" onclick={handleClose}>Done</button>
      </div>
    {:else}
      <p class="hint">
        Paste the 6-character pairing code the inviter showed you.
      </p>
      <input
        type="text"
        class="code-input"
        placeholder="ABC123"
        maxlength="6"
        autocapitalize="characters"
        autocomplete="off"
        spellcheck="false"
        bind:value={code}
        onkeydown={(e) => e.key === 'Enter' && submitAccept()}
        disabled={acceptPending}
      />
      {#if acceptError}
        <p class="err">{acceptError}</p>
      {/if}
      <div class="actions">
        <button class="btn ghost" onclick={handleClose} disabled={acceptPending}>Cancel</button>
        <button class="btn primary" onclick={submitAccept} disabled={acceptPending || code.trim().length !== 6}>
          {acceptPending ? 'Pairing…' : 'Pair'}
        </button>
      </div>
    {/if}
  {:else if inviteResult}
    <p class="hint">
      Invitation sent to <strong>{inviteResult.joiner_hostname}</strong>
      ({inviteResult.joiner_addr}:{inviteResult.joiner_port}). Have them paste
      this code into <em>Pair with code</em>, or run
      <code>orca pod accept</code>:
    </p>
    <div class="code-display">
      <span class="code-value">{inviteResult.pairing_code}</span>
      <button class="copy-btn" onclick={copyCode} title="Copy">⧉</button>
    </div>
    {#if waitingForJoiner}
      <p class="dim waiting">Waiting for {inviteResult.joiner_hostname} to accept…</p>
    {:else}
      <p class="ok">✓ {inviteResult.joiner_hostname} has accepted.</p>
    {/if}
    <div class="actions">
      <button class="btn" onclick={handleClose}>{waitingForJoiner ? 'Close (keep pairing in background)' : 'Done'}</button>
    </div>
  {:else}
    <p class="hint">Enter the host's address manually.</p>
    <div class="manual-row">
      <input
        type="text"
        class="addr-input"
        placeholder="host or host:port"
        bind:value={manualAddr}
        onkeydown={(e) => e.key === 'Enter' && submitManualInvite()}
        disabled={invitePending}
      />
      <button class="btn primary" onclick={submitManualInvite} disabled={invitePending || !manualAddr.trim()}>
        {invitePending ? 'Inviting…' : 'Invite'}
      </button>
    </div>
    {#if inviteError}
      <p class="err">{inviteError}</p>
    {/if}

    <div class="actions">
      <button class="btn ghost" onclick={handleClose}>Cancel</button>
    </div>
  {/if}
</Modal>

<style>
  .discovery-section {
    margin-bottom: var(--space-3);
    padding-bottom: var(--space-3);
    border-bottom: 1px solid var(--color-border);
  }
  .section-label {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-dim);
    margin-bottom: var(--space-2);
  }
  .dim.sm { font-size: var(--text-xs); margin: 0; }
  .mode-tabs { display: flex; gap: var(--space-1); margin-bottom: var(--space-3); border-bottom: 1px solid var(--color-border); }
  .tab {
    background: none;
    border: none;
    color: var(--color-text-dim);
    padding: var(--space-2) var(--space-3);
    font-size: var(--text-sm);
    cursor: pointer;
    border-bottom: 2px solid transparent;
  }
  .tab.active { color: var(--color-text); border-bottom-color: var(--color-accent); }

  .hint { color: var(--color-text-dim); margin: 0 0 var(--space-3); font-size: var(--text-sm); }
  .hint code, .hint em { font-family: var(--font-mono); font-style: normal; }

  .code-input, .addr-input {
    width: 100%;
    font-size: var(--text-base);
    padding: var(--space-3);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    background: var(--color-bg);
    color: var(--color-text);
  }
  .code-input {
    font-family: var(--font-mono);
    font-size: var(--text-xl);
    letter-spacing: 0.25em;
    text-align: center;
    text-transform: uppercase;
  }
  .code-input:focus, .addr-input:focus { outline: 2px solid var(--color-accent); outline-offset: 1px; }

  .code-display {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: var(--space-3);
    background: var(--color-bg);
    border: 1px dashed var(--color-border);
    border-radius: var(--radius-md);
    margin-bottom: var(--space-2);
  }
  .code-value { flex: 1; font-family: var(--font-mono); font-size: var(--text-xl); letter-spacing: 0.25em; text-align: center; }
  .copy-btn { background: none; border: 1px solid var(--color-border); border-radius: var(--radius-sm); padding: var(--space-1) var(--space-2); color: var(--color-text-dim); cursor: pointer; }

  .waiting { font-style: italic; }

  .discovery { list-style: none; margin: 0 0 var(--space-3); padding: 0; max-height: 200px; overflow-y: auto; }
  .discovery li {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: var(--space-2);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    margin-bottom: var(--space-1);
  }
  .d-id { display: flex; flex-direction: column; gap: 2px; }

  .manual-row { display: flex; gap: var(--space-2); align-items: stretch; margin-bottom: var(--space-2); }
  .manual-row .addr-input { flex: 1; }

  .err { color: var(--color-danger, #e11d48); font-size: var(--text-sm); margin: var(--space-3) 0 0; }
  .ok { color: var(--color-text); margin: 0 0 var(--space-2); }
  .ok code, .ok strong { font-family: var(--font-mono); }
  .dim { color: var(--color-text-dim); font-size: var(--text-sm); margin: 0 0 var(--space-3); }
  .actions { display: flex; justify-content: flex-end; gap: var(--space-2); margin-top: var(--space-4); }
  .btn {
    padding: var(--space-2) var(--space-4);
    border-radius: var(--radius-md);
    border: 1px solid var(--color-border);
    background: var(--color-surface);
    color: var(--color-text);
    cursor: pointer;
    font-size: var(--text-sm);
  }
  .btn.sm { padding: var(--space-1) var(--space-3); font-size: var(--text-xs); }
  .btn:disabled { opacity: 0.5; cursor: not-allowed; }
  .btn.primary { background: var(--color-accent); border-color: var(--color-accent); color: var(--color-on-accent, #fff); }
  .btn.ghost { background: transparent; }
</style>
