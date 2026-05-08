<script lang="ts">
  import ModalShell from './ModalShell.svelte';

  let { open, onclose }: { open: boolean; onclose: () => void } = $props();

  interface CheckResult { label: string; tool: string; output: string; ok: boolean; }
  interface HealthData  { timestamp: string; checks: CheckResult[]; }

  let data     = $state<HealthData | null>(null);
  let loading  = $state(false);
  let error    = $state<string | null>(null);
  let expanded = $state<Record<string, boolean>>({});

  async function runSweep() {
    loading = true; error = null;
    try {
      const res = await fetch('/api/rebuy/health/local');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      data = await res.json();
    } catch (e: any) { error = e.message ?? 'fetch failed'; }
    finally { loading = false; }
  }

  $effect(() => { if (open) runSweep(); });

  const ts = $derived(data ? new Date(data.timestamp).toLocaleTimeString() : null);
</script>

<ModalShell {open} {onclose}>
  <div class="modal">
    <div class="hd-header">
      <span class="hd-title">Local Health</span>
      {#if ts}<span class="hd-ts">Last run: {ts}</span>{/if}
      <button class="hd-action" onclick={runSweep} disabled={loading}>{loading ? '…' : 'Refresh'}</button>
      <button class="hd-close" onclick={onclose}>✕</button>
    </div>
    <div class="hd-body">
      {#if loading}
        <div class="hd-status">Running checks — this may take 10–30 seconds…</div>
      {:else if error}
        <div class="hd-status error">{error}</div>
      {:else if data}
        {#each data.checks as check (check.tool)}
          <div class="check">
            <button class="check-header" onclick={() => expanded[check.tool] = !expanded[check.tool]}>
              <span class="dot {check.ok ? 'ok' : 'fail'}">●</span>
              <span class="check-label">{check.label}</span>
              <span class="check-tool">{check.tool}</span>
              <span class="toggle">{expanded[check.tool] ? '▴ collapse' : '▾ expand'}</span>
            </button>
            {#if expanded[check.tool]}
              <div class="check-output">
                <pre class="svc-output">{check.output}</pre>
              </div>
            {/if}
          </div>
        {/each}
      {/if}
    </div>
  </div>
</ModalShell>

<style>
  .modal { width:min(720px,94vw); max-height:85vh; background:var(--color-surface); border:1px solid var(--color-border); border-radius:var(--radius-lg); box-shadow:var(--shadow-lg); display:flex; flex-direction:column; overflow:hidden; }
  .hd-header { display:flex; align-items:center; gap:var(--space-3); padding:var(--space-3) var(--space-4); border-bottom:1px solid var(--color-border); flex-shrink:0; }
  .hd-title { font-weight:var(--weight-semibold); font-size:var(--text-base); flex:1; }
  .hd-ts { font-size:var(--text-xs); color:var(--color-text-dim); }
  .hd-action { background:var(--color-surface-2); border:1px solid var(--color-border); border-radius:var(--radius-md); color:var(--color-text-muted); font-size:var(--text-xs); padding:3px 10px; cursor:pointer; transition:color var(--transition-fast); }
  .hd-action:hover:not(:disabled) { color:var(--color-text); }
  .hd-action:disabled { opacity:var(--opacity-disabled); cursor:not-allowed; }
  .hd-close { background:none; border:none; cursor:pointer; color:var(--color-text-dim); font-size:var(--text-sm); padding:2px 4px; border-radius:var(--radius-sm); }
  .hd-close:hover { color:var(--color-text); }
  .hd-body { flex:1; overflow-y:auto; }
  .hd-status { padding:var(--space-6); text-align:center; color:var(--color-text-dim); font-size:var(--text-sm); }
  .hd-status.error { color:var(--color-error); }
  .check { border-bottom:1px solid var(--color-border); }
  .check:last-child { border-bottom:none; }
  .check-header { display:flex; align-items:center; gap:var(--space-2); width:100%; background:none; border:none; padding:var(--space-3) var(--space-4); cursor:pointer; text-align:left; transition:background var(--transition-fast); }
  .check-header:hover { background:var(--color-surface-2); }
  .dot { font-size:0.6rem; flex-shrink:0; }
  .ok   { color:var(--color-success); }
  .fail { color:var(--color-error); }
  .check-label { flex:1; font-size:var(--text-sm); color:var(--color-text); font-weight:var(--weight-medium); }
  .check-tool { font-size:var(--text-xs); font-family:var(--font-mono); color:var(--color-text-dim); }
  .toggle { font-size:var(--text-xs); color:var(--color-text-faint); margin-left:auto; }
  .check-output { padding:0 var(--space-4) var(--space-3); }
</style>
