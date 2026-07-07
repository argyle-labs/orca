<script lang="ts">
  import { goto } from '$app/navigation';
  import { onMount, tick } from 'svelte';
  import { runTool, allToolNames, type ToolName } from '$lib/stores/runTool';
  import { NAV_SECTIONS } from '$lib/nav';
  import {
    isCommandPaletteOpen,
    closeCommandPalette,
  } from '$lib/stores/commandPalette.svelte';

  type Entry =
    | { kind: 'nav'; label: string; href: string; section: string; enabled: boolean }
    | { kind: 'tool'; label: string; tool: ToolName; group: string };

  const open = $derived(isCommandPaletteOpen());

  let query = $state('');
  let activeIdx = $state(0);
  let inputEl: HTMLInputElement | null = $state(null);
  let toolNames = $state<ToolName[]>([]);

  // Enumerate tool functions from the generated SDK module. Source of truth
  // is whatever the live OpenAPI spec produced — every operationId becomes a
  // callable name here automatically.
  // allToolNames() is async — it triggers the lazy SDK chunk import on
  // first call so the SDK stays off the first-paint critical path. The
  // palette populates its tool list as soon as the chunk resolves.
  onMount(() => {
    void allToolNames().then((names) => {
      toolNames = names;
    });
  });

  const navEntries = $derived<Entry[]>(
    NAV_SECTIONS.flatMap((s) =>
      s.items.map((i) => ({
        kind: 'nav' as const,
        label: i.label,
        href: i.href,
        section: s.label,
        enabled: i.enabled !== false,
      })),
    ),
  );

  const toolEntries = $derived<Entry[]>(
    toolNames.map((t) => ({
      kind: 'tool' as const,
      label: humanize(String(t)),
      tool: t,
      group: groupOf(t),
    })),
  );

  const filtered = $derived(filterAndRank([...navEntries, ...toolEntries], query));

  $effect(() => {
    if (open) {
      query = '';
      activeIdx = 0;
      tick().then(() => inputEl?.focus());
    }
  });

  $effect(() => {
    // reset selection when filter changes
    void query;
    activeIdx = 0;
  });

  // operationIds are camelCase (e.g. `hostInfo`, `agentBackendKeyStatus`).
  // Split on capitals for a readable label and use the first word as the group.
  function humanize(camel: string): string {
    return camel
      .replace(/([A-Z])/g, ' $1')
      .trim()
      .toLowerCase();
  }

  function groupOf(method: ToolName): string {
    const s = String(method);
    const match = s.match(/^[a-z]+/);
    return match ? match[0] : 'misc';
  }

  function filterAndRank(entries: Entry[], q: string): Entry[] {
    const term = q.trim().toLowerCase();
    if (!term) return entries.slice(0, 60);
    const scored = entries
      .map((e) => ({ e, score: score(e.label.toLowerCase(), term) }))
      .filter((s) => s.score > 0)
      .sort((a, b) => b.score - a.score);
    return scored.slice(0, 60).map((s) => s.e);
  }

  /** Simple substring + prefix bonus + acronym match. Good enough for v1. */
  function score(hay: string, needle: string): number {
    if (hay === needle) return 1000;
    if (hay.startsWith(needle)) return 500 + needle.length;
    const idx = hay.indexOf(needle);
    if (idx >= 0) return 200 - idx;
    // acronym: first letters of each token
    const initials = hay
      .split(/[\s._-]+/)
      .map((w) => w[0])
      .join('');
    if (initials.startsWith(needle)) return 150;
    // fuzzy: chars appear in order
    let hi = 0;
    for (const c of needle) {
      hi = hay.indexOf(c, hi);
      if (hi === -1) return 0;
      hi++;
    }
    return 50;
  }

  async function execute(entry: Entry) {
    closeCommandPalette();
    if (entry.kind === 'nav') {
      if (!entry.enabled) return;
      await goto(entry.href);
      return;
    }
    // Palette is a generic dispatcher: tools picked from here are invoked
    // with empty args. Tools whose Args type has required fields surface a
    // typed validation error through the toast pipeline. Domain pages should
    // call the typed SDK function directly instead.
    await runTool(entry.tool, {}, { successMessage: `${entry.label} ✓` });
  }

  function handleKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      closeCommandPalette();
    } else if (e.key === 'ArrowDown') {
      e.preventDefault();
      activeIdx = Math.min(activeIdx + 1, filtered.length - 1);
      scrollActiveIntoView();
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      activeIdx = Math.max(activeIdx - 1, 0);
      scrollActiveIntoView();
    } else if (e.key === 'Enter') {
      e.preventDefault();
      const entry = filtered[activeIdx];
      if (entry) execute(entry);
    }
  }

  function scrollActiveIntoView() {
    tick().then(() => {
      const el = document.querySelector('.cmd-result.active');
      el?.scrollIntoView({ block: 'nearest' });
    });
  }
</script>

{#if open}
  <div
    class="cmd-scrim"
    onclick={(e) => {
      if (e.target === e.currentTarget) closeCommandPalette();
    }}
    role="presentation"
  >
    <div class="cmd-panel" role="dialog" aria-modal="true" aria-label="Command palette">
      <input
        bind:this={inputEl}
        bind:value={query}
        onkeydown={handleKey}
        type="text"
        class="cmd-input"
        placeholder="Search pages and tools…"
        autocomplete="off"
        spellcheck="false"
      />

      <div class="cmd-results">
        {#if filtered.length === 0}
          <div class="cmd-empty">No matches</div>
        {/if}
        {#each filtered as entry, i (`${entry.kind}-${entry.kind === 'nav' ? entry.href : entry.tool}`)}
          <button
            class="cmd-result {i === activeIdx ? 'active' : ''}"
            class:disabled={entry.kind === 'nav' && !entry.enabled}
            onmouseenter={() => (activeIdx = i)}
            onclick={() => execute(entry)}
            type="button"
          >
            <span class="cmd-kind">{entry.kind === 'nav' ? '→' : '⌁'}</span>
            <span class="cmd-label">{entry.label}</span>
            <span class="cmd-group">
              {entry.kind === 'nav' ? entry.section : entry.group}
            </span>
          </button>
        {/each}
      </div>

      <div class="cmd-footer">
        <span><kbd>↑</kbd> <kbd>↓</kbd> navigate</span>
        <span><kbd>↵</kbd> select</span>
        <span><kbd>esc</kbd> close</span>
      </div>
    </div>
  </div>
{/if}

<style>
  .cmd-scrim {
    position: fixed;
    inset: 0;
    background: rgba(0, 0, 0, 0.5);
    display: flex;
    align-items: flex-start;
    justify-content: center;
    padding-top: 12vh;
    z-index: var(--z-modal);
  }
  .cmd-panel {
    width: min(640px, 90vw);
    max-height: 70vh;
    display: flex;
    flex-direction: column;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-lg);
    overflow: hidden;
  }
  .cmd-input {
    width: 100%;
    background: transparent;
    color: var(--color-text);
    border: none;
    border-bottom: 1px solid var(--color-border);
    padding: var(--space-4);
    font-size: var(--text-base);
    outline: none;
  }
  .cmd-input::placeholder { color: var(--color-text-dim); }

  .cmd-results {
    overflow-y: auto;
    padding: var(--space-2) 0;
    min-height: 60px;
  }
  .cmd-empty {
    padding: var(--space-4);
    text-align: center;
    color: var(--color-text-dim);
    font-size: var(--text-sm);
  }

  .cmd-result {
    width: 100%;
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: 8px var(--space-4);
    background: transparent;
    border: none;
    text-align: left;
    color: var(--color-text-muted);
    cursor: pointer;
    font-size: var(--text-sm);
  }
  .cmd-result.active {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .cmd-result.disabled { opacity: var(--opacity-disabled); cursor: not-allowed; }

  .cmd-kind {
    width: 14px;
    color: var(--color-accent);
    text-align: center;
    font-size: 11px;
  }
  .cmd-label { flex: 1; }
  .cmd-group {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.06em;
    color: var(--color-text-dim);
  }

  .cmd-footer {
    display: flex;
    gap: var(--space-4);
    padding: 6px var(--space-4);
    border-top: 1px solid var(--color-border);
    font-size: 10px;
    color: var(--color-text-dim);
  }
  kbd {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 0 4px;
    font-family: var(--font-mono);
    font-size: 10px;
  }
</style>
