<script lang="ts">
  import { onMount, onDestroy } from 'svelte';
  import { callTool } from '$lib/stores/runTool';
  import StatusDot from '$lib/components/StatusDot.svelte';

  /**
   * v1: shows the local instance only — derived from the orca backend this
   * UI is talking to. Federation/remote-orca discovery isn't wired yet; this
   * page is the natural home for it once pods land. Each instance row
   * polls `api.health` on its own cadence.
   */
  interface Instance {
    id: string;
    label: string;
    origin: string;
    role: 'local';
    version: string | null;
    target: string | null;
    frontend: string | null;
    health: 'up' | 'down' | 'unknown';
    error: string | null;
    lastChecked: number | null;
  }

  let instances = $state<Instance[]>([]);
  let pollHandle: ReturnType<typeof setInterval> | null = null;
  const POLL_MS = 5000;

  function originForLocal(): string {
    if (typeof window === 'undefined') return '';
    return window.location.origin;
  }

  async function refresh(inst: Instance) {
    try {
      const [health, spec] = await Promise.all([
        callTool('health', {}),
        callTool('system_runtime_spec', {}),
      ]);
      inst.health = (health as { ok: boolean }).ok ? 'up' : 'down';
      const s = spec as { version: string; target: string; frontend: string };
      inst.version = s.version;
      inst.target = s.target;
      inst.frontend = s.frontend;
      inst.error = null;
    } catch (e) {
      inst.health = 'down';
      inst.error = e instanceof Error ? e.message : String(e);
    } finally {
      inst.lastChecked = Date.now();
      // Trigger reactivity — replace the array reference.
      instances = [...instances];
    }
  }

  onMount(() => {
    instances = [
      {
        id: 'local',
        label: 'Local',
        origin: originForLocal(),
        role: 'local',
        version: null,
        target: null,
        frontend: null,
        health: 'unknown',
        error: null,
        lastChecked: null,
      },
    ];
    instances.forEach(refresh);
    pollHandle = setInterval(() => instances.forEach(refresh), POLL_MS);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });

  function relTime(ts: number | null): string {
    if (!ts) return '—';
    const sec = Math.round((Date.now() - ts) / 1000);
    if (sec < 5) return 'just now';
    if (sec < 60) return `${sec}s ago`;
    if (sec < 3600) return `${Math.round(sec / 60)}m ago`;
    return `${Math.round(sec / 3600)}h ago`;
  }
</script>

<section class="page">
  <header>
    <h1>Overview</h1>
    <p class="lede">Connected orca instances.</p>
  </header>

  <div class="instances">
    {#each instances as inst (inst.id)}
      <article class="instance" class:down={inst.health === 'down'}>
        <div class="row">
          <div class="ident">
            <StatusDot ok={inst.health === 'up' ? true : inst.health === 'down' ? false : null} />
            <span class="label">{inst.label}</span>
            <span class="role">{inst.role}</span>
          </div>
          <button class="refresh" onclick={() => refresh(inst)} title="Refresh">↻</button>
        </div>

        <dl class="meta">
          <dt>origin</dt><dd><code>{inst.origin || '—'}</code></dd>
          <dt>version</dt><dd><code>{inst.version ?? '—'}</code></dd>
          <dt>target</dt><dd><code>{inst.target ?? '—'}</code></dd>
          <dt>frontend</dt><dd><code>{inst.frontend ?? '—'}</code></dd>
          <dt>checked</dt><dd>{relTime(inst.lastChecked)}</dd>
        </dl>

        {#if inst.error}
          <div class="err">{inst.error}</div>
        {/if}
      </article>
    {/each}
  </div>

  <p class="hint">
    Remote orca instances (pods) aren't discovered yet. When federation lands
    they'll appear here automatically.
  </p>
</section>

<style>
  .page {
    max-width: var(--content-max);
    margin: 0 auto;
    padding: var(--space-6) var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
  }
  header h1 { margin: 0 0 var(--space-1); font-size: var(--text-xl); letter-spacing: 0.02em; }
  .lede { margin: 0; color: var(--color-text-muted); font-size: var(--text-sm); }

  .instances {
    display: grid;
    grid-template-columns: repeat(auto-fill, minmax(320px, 1fr));
    gap: var(--space-4);
  }
  .instance {
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md);
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-3);
  }
  .instance.down { border-color: var(--color-error); }

  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  .ident {
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  .label { font-weight: var(--weight-semibold); }
  .role {
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--color-text-dim);
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
  }
  .refresh {
    background: transparent;
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    width: 24px; height: 22px;
    cursor: pointer;
  }
  .refresh:hover { background: var(--color-surface-2); color: var(--color-text); }

  dl.meta {
    margin: 0;
    display: grid;
    grid-template-columns: 80px 1fr;
    row-gap: 4px;
    column-gap: var(--space-3);
    font-size: var(--text-xs);
  }
  dt { color: var(--color-text-dim); text-transform: uppercase; letter-spacing: 0.06em; font-size: 10px; }
  dd { margin: 0; color: var(--color-text); }
  code {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
    font-size: var(--text-xs);
  }

  .err { color: var(--color-error); font-size: var(--text-xs); font-family: var(--font-mono); }

  .hint {
    color: var(--color-text-dim);
    font-size: var(--text-xs);
    margin: 0;
  }
</style>
