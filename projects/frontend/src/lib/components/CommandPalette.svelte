<script lang="ts">
  import ModalShell from './ModalShell.svelte';

  let { open, onclose }: { open: boolean; onclose: () => void } = $props();

  interface McpTool {
    server: string; name: string; description: string;
    inputSchema: { type?: string; properties?: Record<string, { type: string; description?: string; enum?: string[] }>; required?: string[]; };
  }

  let tools    = $state<McpTool[]>([]);
  let query    = $state('');
  let selected = $state<McpTool | null>(null);
  let form     = $state<Record<string, string | boolean>>({});
  let output   = $state<string | null>(null);
  let running  = $state(false);
  let error    = $state<string | null>(null);
  let inputEl: HTMLInputElement | null = $state(null);

  $effect(() => {
    if (open && tools.length === 0) {
      fetch('/api/mcp/tools').then((r) => r.json()).then((d) => tools = d).catch(() => {});
    }
    if (open) setTimeout(() => inputEl?.focus(), 10);
  });

  function formatName(tool: McpTool): string {
    const prefix = tool.server.replace(/-/g, '_') + '_';
    const stripped = tool.name.startsWith(prefix) ? tool.name.slice(prefix.length) : tool.name;
    const parts = stripped.split('_');
    if (parts.length === 1) return parts[0].charAt(0).toUpperCase() + parts[0].slice(1);
    const [group, ...rest] = parts;
    return `${group.charAt(0).toUpperCase() + group.slice(1)}: ${rest.map((p) => p.charAt(0).toUpperCase() + p.slice(1)).join(' ')}`;
  }

  let filtered = $derived(
    query
      ? tools.filter((t) => t.name.toLowerCase().includes(query.toLowerCase()) || t.description.toLowerCase().includes(query.toLowerCase()))
      : tools
  );

  let grouped = $derived(() => {
    const g: Record<string, McpTool[]> = {};
    for (const t of filtered) { if (!g[t.server]) g[t.server] = []; g[t.server].push(t); }
    return Object.entries(g).sort(([a], [b]) => a.localeCompare(b));
  });

  function select(tool: McpTool) { selected = tool; form = {}; output = null; error = null; }
  function back()                 { selected = null; output = null; error = null; }

  async function run() {
    if (!selected) return;
    running = true; output = null; error = null;
    try {
      const res = await fetch('/api/mcp/run', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ server: selected.server, name: selected.name, arguments: form }),
      });
      const d = await res.json();
      if (!res.ok) { error = d.error ?? 'Request failed'; return; }
      output = d.content?.[0]?.text ?? JSON.stringify(d);
    } catch (e) { error = String(e); }
    finally { running = false; }
  }

  function handleClose() {
    if (selected) { selected = null; }
    else { onclose(); }
  }
</script>

<ModalShell open={open} onclose={handleClose} align="top">
  <div class="modal">
    {#if selected}
      <div class="header">
        <button class="back" onclick={back}>← back</button>
        <div class="tool-meta">
          <span class="tool-name">{formatName(selected)}</span>
          <span class="tool-path">{selected.server} / {selected.name}</span>
        </div>
      </div>
      <div class="body">
        <div class="detail">
          <p class="tool-desc">{selected.description}</p>
          {#each Object.entries(selected.inputSchema.properties ?? []) as [key, prop] (key)}
            {@const required = (selected.inputSchema.required ?? []).includes(key)}
            {#if prop.type === 'boolean'}
              <label class="check-row">
                <input type="checkbox" checked={!!form[key]} onchange={(e) => form[key] = (e.target as HTMLInputElement).checked} />
                {key}{required ? ' *' : ''}{prop.description ? ` — ${prop.description}` : ''}
              </label>
            {:else if prop.enum && prop.enum.length > 0}
              <div class="field">
                <label for="f-{key}">{key}{required ? ' *' : ''}{prop.description ? ` — ${prop.description}` : ''}</label>
                <select id="f-{key}" value={(form[key] as string) ?? ''} onchange={(e) => form[key] = (e.target as HTMLSelectElement).value}>
                  <option value="">— choose —</option>
                  {#each prop.enum as v (v)}<option value={v}>{v}</option>{/each}
                </select>
              </div>
            {:else}
              <div class="field">
                <label for="f-{key}">{key}{required ? ' *' : ''}{prop.description ? ` — ${prop.description}` : ''}</label>
                <input id="f-{key}" type={prop.type === 'number' || prop.type === 'integer' ? 'number' : 'text'}
                       value={(form[key] as string) ?? ''} oninput={(e) => form[key] = (e.target as HTMLInputElement).value} />
              </div>
            {/if}
          {/each}
          <button class="run-btn" onclick={run} disabled={running}>{running ? 'Running…' : 'Run'}</button>
          {#if error}<div class="error">{error}</div>{/if}
          {#if output !== null}<pre class="output">{output}</pre>{/if}
        </div>
      </div>
    {:else}
      <div class="header">
        <!-- svelte-ignore a11y_autofocus -->
        <input bind:this={inputEl} class="search" placeholder="Search MCP tools…" bind:value={query} />
      </div>
      <div class="body">
        {#each grouped() as [server, serverTools] (server)}
          <div class="group-label">{server}</div>
          {#each serverTools as tool (`${tool.server}/${tool.name}`)}
            <button class="tool-row" onclick={() => select(tool)}>
              <div class="tool-row-name">
                <span class="tool-name-text">{formatName(tool)}</span>
                <span class="tool-name-raw">{tool.name}</span>
              </div>
              <span class="tool-desc-text">{tool.description}</span>
            </button>
          {/each}
        {/each}
        {#if filtered.length === 0}
          <div class="empty">No commands match.</div>
        {/if}
      </div>
    {/if}
  </div>
</ModalShell>

<style>
  .modal { width:min(640px,94vw); max-height:80vh; background:var(--color-surface); border:1px solid var(--color-border); border-radius:var(--radius-lg); box-shadow:var(--shadow-lg); display:flex; flex-direction:column; overflow:hidden; }
  .header { padding:var(--space-3) var(--space-4); border-bottom:1px solid var(--color-border); flex-shrink:0; display:flex; align-items:center; gap:var(--space-3); }
  .search { flex:1; background:none; border:none; outline:none; font-size:var(--text-base); color:var(--color-text); }
  .search::placeholder { color:var(--color-text-faint); }
  .back { background:none; border:none; cursor:pointer; color:var(--color-accent); font-size:var(--text-sm); padding:0; }
  .tool-meta { display:flex; flex-direction:column; gap:2px; }
  .tool-name { font-size:var(--text-base); font-weight:var(--weight-semibold); color:var(--color-text); }
  .tool-path { font-size:var(--text-xs); font-family:var(--font-mono); color:var(--color-text-dim); }
  .body { flex:1; overflow-y:auto; padding:var(--space-1); }
  .group-label { font-size:var(--text-xs); font-weight:var(--weight-semibold); color:var(--color-text-dim); text-transform:uppercase; letter-spacing:0.05em; padding:var(--space-2) var(--space-3) var(--space-1); }
  .tool-row { width:100%; background:none; border:none; padding:var(--space-2) var(--space-3); border-radius:var(--radius-md); cursor:pointer; text-align:left; transition:background var(--transition-fast); }
  .tool-row:hover { background:var(--color-surface-2); }
  .tool-row-name { display:flex; align-items:baseline; gap:var(--space-2); margin-bottom:2px; }
  .tool-name-text { font-size:var(--text-sm); font-weight:var(--weight-medium); color:var(--color-text); }
  .tool-name-raw { font-size:var(--text-xs); font-family:var(--font-mono); color:var(--color-text-faint); }
  .tool-desc-text { font-size:var(--text-xs); color:var(--color-text-dim); }
  .empty { padding:var(--space-4); color:var(--color-text-dim); font-size:var(--text-sm); }
  .detail { display:flex; flex-direction:column; gap:var(--space-3); padding:var(--space-3); }
  .tool-desc { font-size:var(--text-sm); color:var(--color-text-muted); margin:0; }
  .field { display:flex; flex-direction:column; gap:var(--space-1); }
  .field label { font-size:var(--text-xs); color:var(--color-text-dim); }
  .field input, .field select { background:var(--color-bg); border:1px solid var(--color-border); border-radius:var(--radius-md); color:var(--color-text); font-size:var(--text-sm); padding:var(--space-1) var(--space-2); outline:none; }
  .field input:focus, .field select:focus { border-color:var(--color-accent); }
  .check-row { display:flex; align-items:center; gap:var(--space-2); font-size:var(--text-sm); color:var(--color-text-muted); cursor:pointer; }
  .run-btn { background:var(--color-accent); color:#fff; border:none; border-radius:var(--radius-md); padding:var(--space-2) var(--space-4); font-size:var(--text-sm); font-weight:var(--weight-semibold); cursor:pointer; align-self:flex-start; transition:background var(--transition-fast); }
  .run-btn:hover:not(:disabled) { background:#6a5ae0; }
  .run-btn:disabled { opacity:var(--opacity-disabled); cursor:not-allowed; }
  .error { color:var(--color-error); font-size:var(--text-sm); }
  .output { font-family:var(--font-mono); font-size:var(--text-xs); white-space:pre-wrap; word-break:break-all; background:var(--color-bg); border:1px solid var(--color-border); border-radius:var(--radius-md); padding:var(--space-3); margin:0; }
</style>
