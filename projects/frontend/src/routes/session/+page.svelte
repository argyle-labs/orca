<script lang="ts">
  import Button from '$lib/components/Button.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import Spinner from '$lib/components/Spinner.svelte';
  import { notifications } from '$lib/stores/notifications';
  import { runMcpTool } from '$lib/api/client';
  import { onMount } from 'svelte';

  const MCP_SERVER = 'orca-local';

  async function callTool(name: string, args: Record<string, unknown> = {}): Promise<string> {
    const data = await runMcpTool({ body: { server: MCP_SERVER, name, arguments: args } });
    return (data as any).content?.[0]?.text ?? JSON.stringify(data);
  }

  interface Run { id: number; agent: string; prompt: string; output: string; error: string | null; ms: number; }

  let agents: string[] = $state([]);
  let agent = $state('wolf');
  let prompt = $state('');
  let runs: Run[] = $state([]);
  let loading = $state(false);
  let expandedId: number | null = $state(null);
  let nextId = 0;
  let bottomEl: HTMLDivElement | undefined;

  onMount(async () => {
    try {
      const raw = await callTool('orca_agents');
      agents = raw.split('\n').filter((l: string) => l.startsWith('@'))
        .map((l: string) => l.slice(1, l.indexOf(':'))).filter(Boolean);
    } catch (e) { notifications.error(`Failed to load agents: ${e}`); }
  });

  async function submit() {
    if (!prompt.trim() || loading) return;
    loading = true;
    const id = nextId++;
    const p = prompt;
    const a = agent;
    prompt = '';
    const start = Date.now();
    try {
      const output = await callTool('orca_run', { agent: a, prompt: p });
      runs = [...runs, { id, agent: a, prompt: p, output, error: null, ms: Date.now() - start }];
    } catch (e) {
      runs = [...runs, { id, agent: a, prompt: p, output: '', error: String(e), ms: Date.now() - start }];
    } finally {
      loading = false;
      setTimeout(() => bottomEl?.scrollIntoView({ behavior: 'smooth' }), 50);
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.key === 'Enter' && (e.metaKey || e.ctrlKey)) submit();
  }
</script>

<svelte:head><title>Session — orca</title></svelte:head>

<div class="page">
  <h1>Agent Session</h1>

  <div class="controls">
    <select bind:value={agent} class="agent-select">
      {#each agents as a}<option value={a}>{a}</option>{/each}
      {#if !agents.length}<option value="wolf">wolf</option>{/if}
    </select>
    <textarea
      bind:value={prompt}
      onkeydown={onKeydown}
      placeholder="Prompt… (⌘+Enter to send)"
      rows={3}
      class="prompt-input"
    ></textarea>
    <Button variant="primary" onclick={submit} disabled={loading || !prompt.trim()}>
      {#if loading}<Spinner size={14} />{/if}
      Run
    </Button>
  </div>

  <div class="runs">
    {#each runs as run (run.id)}
      <div class="run" class:error={run.error}>
        <div class="run-header" role="button" tabindex="0"
             onclick={() => expandedId = expandedId === run.id ? null : run.id}
             onkeydown={(e) => e.key === 'Enter' && (expandedId = expandedId === run.id ? null : run.id)}>
          <Badge color={run.error ? 'red' : 'purple'}>{run.agent}</Badge>
          <span class="run-prompt">{run.prompt.slice(0, 80)}{run.prompt.length > 80 ? '…' : ''}</span>
          <span class="run-ms">{run.ms}ms</span>
        </div>
        {#if expandedId === run.id}
          <pre class="run-output">{run.error ?? run.output}</pre>
        {/if}
      </div>
    {/each}
    <div bind:this={bottomEl}></div>
  </div>
</div>

<style>
  .controls { display: flex; flex-direction: column; gap: var(--space-3); margin-bottom: var(--space-6); }
  .agent-select {
    width: fit-content; background: var(--color-surface); border: 1px solid var(--color-border);
    color: var(--color-text); padding: var(--space-2) var(--space-3); border-radius: var(--radius-sm);
    font-size: var(--text-sm);
  }
  .prompt-input {
    background: var(--color-surface); border: 1px solid var(--color-border);
    color: var(--color-text); padding: var(--space-3); border-radius: var(--radius-sm);
    font-family: inherit; font-size: var(--text-sm); resize: vertical;
  }
  .prompt-input:focus { outline: none; border-color: var(--color-accent); }
  .runs { display: flex; flex-direction: column; gap: var(--space-2); }
  .run { background: var(--color-surface); border: 1px solid var(--color-border); border-radius: var(--radius-md); overflow: hidden; }
  .run.error { border-color: var(--color-error); }
  .run-header { display: flex; align-items: center; gap: var(--space-3); padding: var(--space-3) var(--space-4); cursor: pointer; }
  .run-header:hover { background: var(--color-surface-2); }
  .run-prompt { flex: 1; font-size: var(--text-sm); color: var(--color-text-dim); font-family: var(--font-mono); }
  .run-ms { font-size: var(--text-xs); color: var(--color-text-faint); }
  .run-output { margin: 0; padding: var(--space-4); border-top: 1px solid var(--color-border); font-size: var(--text-sm); white-space: pre-wrap; word-break: break-word; }
</style>
