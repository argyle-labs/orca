<script lang="ts">
  import { onMount, onDestroy, untrack } from 'svelte';
  import { page } from '$app/stores';
  import { goto } from '$app/navigation';
  import { act } from '$lib/stores/notifications';
  import { runMcpTool } from '$lib/api/client';
  import { marked } from 'marked';

  let { data } = $props();

  // ── Problem list ──────────────────────────────────────────────────────────────

  type Problem = { num: number; title: string; difficulty: 'Easy' | 'Medium' | 'Hard'; solved: boolean };

  function parseProblems(text: string): Problem[] {
    return text.split('\n')
      .map(line => {
        const m = line.match(/^\s*(\d+)\.\s+\[(\w+)\s*\]\s+(✓?)\s*(.+)$/);
        if (!m) return null;
        return { num: parseInt(m[1], 10), difficulty: m[2].trim() as Problem['difficulty'], solved: m[3].trim() === '✓', title: m[4].trim() };
      })
      .filter((p): p is Problem => p !== null);
  }

  let allProblems = $state<Problem[]>(untrack(() => parseProblems(data.problemsText ?? '')));
  let progressText = $state<string>(untrack(() => data.progressText ?? ''));

  let diffFilter  = $state<string>('All');
  let langFilter  = $state<string>('ts');
  let showSolved  = $state<boolean>(true);
  let search      = $state<string>('');
  let listVisible = $state<boolean>(true);

  let filteredProblems = $derived.by(() => {
    const q = search.trim().toLowerCase();
    return allProblems
      .filter(p => diffFilter === 'All' || p.difficulty === diffFilter)
      .filter(p => showSolved || !p.solved)
      .filter(p => {
        if (!q) return true;
        if (/^\d+$/.test(q)) return String(p.num).startsWith(q);
        return p.title.toLowerCase().includes(q) || String(p.num) === q;
      });
  });

  // ── Editor state ──────────────────────────────────────────────────────────────

  type PanelMode = 'description' | 'editor';

  let selectedNum    = $state<number | null>(null);
  let selectedTitle  = $state<string>('');
  let description    = $state<string>('');
  let descriptionHtml = $derived(description ? marked.parse(description) as string : '');
  let loadingDesc    = $state(false);
  let panel          = $state<PanelMode>('description');

  // CodeMirror (lazily loaded on first editor open)
  let editorEl  = $state<HTMLDivElement | null>(null);
  let editorView: any = null;
  let loadingCode = $state(false);

  // Output
  let output     = $state<string>('');
  let running    = $state<boolean>(false);
  let lastResult = $state<'pass' | 'fail' | null>(null);
  let outputOpen = $state<boolean>(true);

  const LANGS = ['ts', 'kt', 'java', 'go', 'rs', 'php'] as const;

  function diffColor(d: string) {
    return d === 'Easy' ? 'easy' : d === 'Medium' ? 'medium' : 'hard';
  }

  // ── Editor setup ──────────────────────────────────────────────────────────────

  async function createEditor(content: string) {
    if (editorView) { editorView.destroy(); editorView = null; }
    if (!editorEl) return;

    const [
      { EditorView, keymap, lineNumbers, highlightActiveLine },
      { EditorState },
      { javascript },
      { oneDark },
      { defaultKeymap, history, historyKeymap, indentWithTab },
      { indentOnInput, syntaxHighlighting, defaultHighlightStyle, bracketMatching },
      { closeBrackets },
    ] = await Promise.all([
      import('@codemirror/view'),
      import('@codemirror/state'),
      import('@codemirror/lang-javascript'),
      import('@codemirror/theme-one-dark'),
      import('@codemirror/commands'),
      import('@codemirror/language'),
      import('@codemirror/autocomplete'),
    ]);

    const state = EditorState.create({
      doc: content,
      extensions: [
        lineNumbers(),
        highlightActiveLine(),
        history(),
        indentOnInput(),
        bracketMatching(),
        closeBrackets(),
        syntaxHighlighting(defaultHighlightStyle),
        javascript({ typescript: true }),
        oneDark,
        keymap.of([...defaultKeymap, ...historyKeymap, indentWithTab]),
        EditorView.theme({
          '&': { height: '100%', fontSize: '13px' },
          '.cm-scroller': { overflow: 'auto', fontFamily: 'var(--font-mono)' },
          '.cm-content': { caretColor: '#abb2bf' },
        }),
        EditorView.lineWrapping,
      ],
    });
    editorView = new EditorView({ state, parent: editorEl });
  }

  $effect(() => {
    if (panel === 'editor' && editorEl && !editorView) {
      // editorEl mounted — create editor with current code if we have it
    }
  });

  // ── Problem selection ─────────────────────────────────────────────────────────

  async function selectProblem(num: number, title: string) {
    if (selectedNum === num && panel === 'description') return;
    selectedNum   = num;
    selectedTitle = title;
    output        = '';
    lastResult    = null;
    panel         = 'description';
    listVisible   = false;

    loadingDesc = true;
    const res = await act(() => runMcpTool({
      body: { server: 'leetcode', name: 'leetcode_get_problem', arguments: { number: num } },
    }));
    description = res?.content?.[0]?.text ?? '(no description)';
    loadingDesc = false;
    pushUrl();
  }

  async function openEditor() {
    if (selectedNum === null) return;
    panel = 'editor';
    loadingCode = true;
    pushUrl();

    const res = await runMcpTool({
      body: { server: 'leetcode', name: 'leetcode_get_code', arguments: { number: selectedNum, lang: langFilter } },
    });
    const code = res?.content?.[0]?.text ?? '';
    loadingCode = false;

    // Wait for editorEl to be in the DOM (panel switch triggers re-render)
    await new Promise(r => setTimeout(r, 30));
    createEditor(code);
  }

  function backToDescription() {
    panel = 'description';
    if (editorView) { editorView.destroy(); editorView = null; }
    pushUrl();
  }

  async function saveAndRun() {
    if (selectedNum === null || !editorView) return;
    const code = editorView.state.doc.toString();

    running    = true;
    output     = '';
    lastResult = null;

    // Save first
    await runMcpTool({
      body: { server: 'leetcode', name: 'leetcode_save_code', arguments: { number: selectedNum, code, lang: langFilter } },
    });

    // Run
    const res = await act(() => runMcpTool({
      body: { server: 'leetcode', name: 'leetcode_run_problem', arguments: { number: selectedNum, lang: langFilter } },
    }), { success: `Ran problem ${selectedNum}` });

    output = res?.content?.[0]?.text ?? '';
    const passed = /All.*passed|✓/.test(output) && !/✗/.test(output);
    lastResult = passed ? 'pass' : 'fail';
    running    = false;
    outputOpen = true;

    await refreshData();
  }

  async function runCurrentFile() {
    if (selectedNum === null) return;
    running    = true;
    output     = '';
    lastResult = null;

    const res = await act(() => runMcpTool({
      body: { server: 'leetcode', name: 'leetcode_run_problem', arguments: { number: selectedNum, lang: langFilter } },
    }), { success: `Ran problem ${selectedNum}` });

    output = res?.content?.[0]?.text ?? '';
    const passed = /All.*passed|✓/.test(output) && !/✗/.test(output);
    lastResult = passed ? 'pass' : 'fail';
    running    = false;
    outputOpen = true;

    await refreshData();
  }

  async function refreshData() {
    const [listRes, progressRes] = await Promise.allSettled([
      runMcpTool({ body: { server: 'leetcode', name: 'leetcode_list_problems', arguments: { limit: 800, lang: langFilter } } }),
      runMcpTool({ body: { server: 'leetcode', name: 'leetcode_get_progress', arguments: { lang: langFilter } } }),
    ]);
    if (listRes.status === 'fulfilled') allProblems = parseProblems(listRes.value?.content?.[0]?.text ?? '');
    if (progressRes.status === 'fulfilled') progressText = progressRes.value?.content?.[0]?.text ?? '';
  }

  async function pickRandom() {
    const res = await act(() => runMcpTool({
      body: { server: 'leetcode', name: 'leetcode_pick_problem', arguments: { difficulty: diffFilter === 'All' ? undefined : diffFilter, lang: langFilter } },
    }));
    const text = res?.content?.[0]?.text ?? '';
    const m = text.match(/Picked:\s*(\d+)/);
    if (m) {
      const num = parseInt(m[1], 10);
      const prob = allProblems.find(p => p.num === num);
      if (prob) await selectProblem(num, prob.title);
    }
  }

  function pushUrl() {
    if (selectedNum === null) return;
    goto(`/leetcode?num=${selectedNum}&lang=${langFilter}&panel=${panel}`, { replaceState: true, noScroll: true, keepFocus: true });
  }

  onMount(async () => {
    const params = $page.url.searchParams;
    const num  = params.get('num');
    const lang = params.get('lang');
    const pnl  = params.get('panel');

    if (lang && ['ts','kt','java','go','rs','php'].includes(lang)) langFilter = lang;

    if (num) {
      const n = parseInt(num, 10);
      const prob = allProblems.find(p => p.num === n);
      if (prob) {
        await selectProblem(n, prob.title);
        if (pnl === 'editor') await openEditor();
      }
    }
  });

  onDestroy(() => { if (editorView) editorView.destroy(); });
</script>

<svelte:head><title>{selectedNum ? `#${selectedNum} — LeetCode` : 'LeetCode — orca'}</title></svelte:head>

<div class="workspace">
  <!-- ── Problem list (collapsible left panel) ───────────────────────────── -->
  {#if listVisible}
    <div class="list-panel">
      <div class="list-header">
        <input class="search" type="search" placeholder="Search…" bind:value={search} />
        <button class="icon-btn" title="Hide list" onclick={() => listVisible = false}>✕</button>
      </div>
      <div class="list-filters">
        <select bind:value={diffFilter} onchange={refreshData}>
          <option value="All">All</option>
          <option value="Easy">Easy</option>
          <option value="Medium">Medium</option>
          <option value="Hard">Hard</option>
        </select>
        <select bind:value={langFilter} onchange={refreshData}>
          {#each LANGS as l (l)}<option value={l}>{l}</option>{/each}
        </select>
        <label class="toggle"><input type="checkbox" bind:checked={showSolved} /> Solved</label>
        <button class="btn-sm" onclick={pickRandom}>Random</button>
      </div>
      {#if progressText}
        <pre class="progress-mini">{progressText}</pre>
      {/if}
      <div class="problem-list">
        {#each filteredProblems as p (p.num)}
          <button
            class="problem-row {selectedNum === p.num ? 'active' : ''} {p.solved ? 'solved' : ''}"
            onclick={() => selectProblem(p.num, p.title)}
          >
            <span class="num">{p.num}</span>
            <span class="dot {diffColor(p.difficulty)}"></span>
            <span class="title">{p.title}</span>
            {#if p.solved}<span class="check">✓</span>{/if}
          </button>
        {/each}
        {#if filteredProblems.length === 0}
          <div class="empty">No problems match.</div>
        {/if}
      </div>
    </div>
  {/if}

  <!-- ── Main work area ─────────────────────────────────────────────────── -->
  <div class="main">
    <!-- Toolbar -->
    <div class="toolbar">
      <div class="toolbar-left">
        {#if !listVisible}
          <button class="icon-btn" title="Show list" onclick={() => listVisible = true}>☰</button>
        {/if}
        {#if selectedNum !== null}
          <span class="problem-id">#{selectedNum}</span>
          <span class="problem-title-sm">{selectedTitle}</span>
        {:else}
          <span class="placeholder-text">Select a problem</span>
        {/if}
      </div>
      <div class="toolbar-right">
        {#if selectedNum !== null}
          {#if panel === 'editor'}
            <button class="btn-sm" onclick={backToDescription}>← Description</button>
          {:else}
            <button class="btn-sm" onclick={openEditor}>Solve ✎</button>
          {/if}
          <select bind:value={langFilter} class="lang-select" onchange={pushUrl}>
            {#each LANGS as l (l)}<option value={l}>{l}</option>{/each}
          </select>
          {#if panel === 'editor'}
            <button class="btn-run" onclick={saveAndRun} disabled={running}>
              {running ? 'Running…' : 'Run'}
            </button>
          {:else}
            <button class="btn-run" onclick={runCurrentFile} disabled={running}>
              {running ? 'Running…' : 'Run'}
            </button>
          {/if}
        {/if}
      </div>
    </div>

    <!-- Content area -->
    <div class="content" class:editor-mode={panel === 'editor'}>
      {#if selectedNum === null}
        <div class="splash">
          <p>Select a problem from the list to begin.</p>
        </div>
      {:else if panel === 'description'}
        <div class="desc-panel">
          {#if loadingDesc}
            <div class="loading">Loading…</div>
          {:else}
            <div class="description markdown">{@html descriptionHtml}</div>
          {/if}
        </div>
      {:else}
        <!-- Editor mode: code editor + output -->
        <div class="editor-area">
          <div class="editor-wrap" bind:this={editorEl}>
            {#if loadingCode}<div class="loading-overlay">Loading…</div>{/if}
          </div>
          <div class="output-panel" class:open={outputOpen}>
            <button class="output-toggle" onclick={() => outputOpen = !outputOpen}>
              <span class="output-label {lastResult ?? ''}">
                {lastResult === 'pass' ? '✓ All tests passed' : lastResult === 'fail' ? '✗ Tests failed' : 'Output'}
              </span>
              <span>{outputOpen ? '▾' : '▸'}</span>
            </button>
            {#if outputOpen}
              <pre class="output">{output || 'Run to see output…'}</pre>
            {/if}
          </div>
        </div>
      {/if}
    </div>
  </div>
</div>

<style>
  .workspace {
    display: flex;
    height: 100%;
    overflow: hidden;
    background: var(--color-bg);
  }

  /* ── List panel ── */
  .list-panel {
    width: 280px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--color-border);
    background: var(--color-surface);
    overflow: hidden;
  }
  .list-header {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding: var(--space-2);
    border-bottom: 1px solid var(--color-border);
    flex-shrink: 0;
  }
  .search {
    flex: 1;
    padding: 4px 8px;
    font-size: var(--text-xs);
    background: var(--color-surface-2);
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    outline: none;
  }
  .search:focus { border-color: var(--color-accent); }
  .list-filters {
    display: flex;
    align-items: center;
    gap: var(--space-1);
    padding: var(--space-1) var(--space-2);
    border-bottom: 1px solid var(--color-border);
    flex-shrink: 0;
    flex-wrap: wrap;
  }
  select {
    padding: 2px 6px;
    font-size: var(--text-xs);
    background: var(--color-surface-2);
    color: var(--color-text);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
  }
  .toggle { display: flex; align-items: center; gap: 3px; font-size: var(--text-xs); color: var(--color-text-dim); cursor: pointer; }
  .btn-sm {
    padding: 2px 8px;
    font-size: var(--text-xs);
    background: var(--color-surface-2);
    color: var(--color-text-dim);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    cursor: pointer;
    white-space: nowrap;
  }
  .btn-sm:hover { color: var(--color-text); background: var(--color-surface-3, var(--color-surface-2)); }
  .progress-mini {
    font-size: 10px;
    color: var(--color-text-dim);
    background: var(--color-surface-2);
    padding: var(--space-2);
    margin: 0;
    border-bottom: 1px solid var(--color-border);
    white-space: pre;
    flex-shrink: 0;
  }
  .problem-list { overflow-y: auto; flex: 1; }
  .problem-row {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    padding: 5px var(--space-2);
    background: none;
    border: none;
    border-bottom: 1px solid var(--color-border);
    cursor: pointer;
    text-align: left;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    transition: background var(--transition-fast), color var(--transition-fast);
  }
  .problem-row:last-child { border-bottom: none; }
  .problem-row:hover { background: var(--color-surface-2); color: var(--color-text); }
  .problem-row.active { background: rgba(124,106,247,0.1); color: var(--color-accent); }
  .problem-row.solved .title { color: var(--color-text-muted); }
  .num { font-size: 10px; color: var(--color-text-dim); width: 28px; flex-shrink: 0; text-align: right; }
  .dot { width: 6px; height: 6px; border-radius: 50%; flex-shrink: 0; }
  .dot.easy   { background: #22c55e; }
  .dot.medium { background: #f59e0b; }
  .dot.hard   { background: #ef4444; }
  .title { flex: 1; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .check { font-size: 9px; color: #22c55e; flex-shrink: 0; }
  .empty { padding: var(--space-4); text-align: center; color: var(--color-text-dim); font-size: var(--text-xs); }

  /* ── Main area ── */
  .main {
    flex: 1;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    min-width: 0;
  }

  .toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 0 var(--space-3);
    height: 40px;
    border-bottom: 1px solid var(--color-border);
    background: var(--color-surface);
    flex-shrink: 0;
    gap: var(--space-2);
  }
  .toolbar-left  { display: flex; align-items: center; gap: var(--space-2); min-width: 0; }
  .toolbar-right { display: flex; align-items: center; gap: var(--space-2); flex-shrink: 0; }
  .icon-btn {
    display: flex; align-items: center; justify-content: center;
    width: 24px; height: 24px;
    background: none; border: none; cursor: pointer;
    color: var(--color-text-dim); border-radius: var(--radius-sm);
    font-size: var(--text-xs);
    transition: color var(--transition-fast), background var(--transition-fast);
  }
  .icon-btn:hover { color: var(--color-text); background: var(--color-surface-2); }
  .problem-id { font-size: var(--text-sm); font-weight: var(--weight-semibold); color: var(--color-text-dim); flex-shrink: 0; }
  .problem-title-sm { font-size: var(--text-sm); color: var(--color-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .placeholder-text { font-size: var(--text-sm); color: var(--color-text-dim); }
  .lang-select { padding: 3px 6px; font-size: var(--text-xs); background: var(--color-surface-2); color: var(--color-text); border: 1px solid var(--color-border); border-radius: var(--radius-sm); }
  .btn-run {
    padding: 5px 18px;
    font-size: var(--text-sm);
    font-weight: var(--weight-medium);
    background: var(--color-accent);
    color: #fff;
    border: none;
    border-radius: var(--radius-sm);
    cursor: pointer;
    transition: opacity var(--transition-fast);
  }
  .btn-run:disabled { opacity: 0.5; cursor: not-allowed; }
  .btn-run:not(:disabled):hover { opacity: 0.85; }

  /* ── Content ── */
  .content {
    flex: 1;
    overflow: hidden;
    display: flex;
    flex-direction: column;
  }
  .splash {
    display: flex;
    align-items: center;
    justify-content: center;
    height: 100%;
    color: var(--color-text-dim);
    font-size: var(--text-sm);
  }

  /* Description panel */
  .desc-panel {
    flex: 1;
    overflow-y: auto;
    padding: var(--space-4);
  }
  .loading { padding: var(--space-4); color: var(--color-text-dim); font-size: var(--text-sm); }
  .description {
    font-size: var(--text-sm);
    line-height: 1.7;
    color: var(--color-text);
    max-width: 720px;
  }
  .description.markdown :global(h1),
  .description.markdown :global(h2) { font-size: var(--text-base); font-weight: var(--weight-semibold); margin: 0 0 var(--space-2); }
  .description.markdown :global(p) { margin: 0 0 var(--space-3); }
  .description.markdown :global(p:last-child) { margin-bottom: 0; }
  .description.markdown :global(ul), .description.markdown :global(ol) { margin: 0 0 var(--space-3); padding-left: 1.4em; }
  .description.markdown :global(li) { margin-bottom: 3px; }
  .description.markdown :global(code) { font-family: var(--font-mono); background: var(--color-surface-2); padding: 1px 5px; border-radius: 3px; font-size: 0.9em; }
  .description.markdown :global(pre) { background: var(--color-surface-2); padding: var(--space-3); border-radius: var(--radius-sm); overflow-x: auto; margin: 0 0 var(--space-3); }
  .description.markdown :global(pre code) { background: none; padding: 0; }
  .description.markdown :global(strong) { font-weight: var(--weight-semibold); }

  /* Editor area */
  .editor-area {
    flex: 1;
    display: flex;
    flex-direction: column;
    overflow: hidden;
    position: relative;
  }
  .editor-wrap {
    flex: 1;
    overflow: hidden;
    position: relative;
    background: #282c34; /* oneDark bg */
  }
  .editor-wrap :global(.cm-editor) {
    height: 100%;
  }
  .loading-overlay {
    position: absolute;
    inset: 0;
    display: flex;
    align-items: center;
    justify-content: center;
    color: var(--color-text-dim);
    font-size: var(--text-sm);
    background: #282c34;
  }

  /* Output panel */
  .output-panel {
    flex-shrink: 0;
    border-top: 1px solid var(--color-border);
    background: var(--color-surface);
    max-height: 40%;
    display: flex;
    flex-direction: column;
  }
  .output-toggle {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 6px var(--space-3);
    background: none;
    border: none;
    cursor: pointer;
    width: 100%;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    flex-shrink: 0;
  }
  .output-toggle:hover { background: var(--color-surface-2); }
  .output-label { font-weight: var(--weight-semibold); }
  .output-label.pass { color: #22c55e; }
  .output-label.fail { color: #ef4444; }
  .output {
    font-family: var(--font-mono);
    font-size: var(--text-xs);
    white-space: pre-wrap;
    padding: var(--space-2) var(--space-3);
    margin: 0;
    overflow-y: auto;
    flex: 1;
    color: var(--color-text);
    background: var(--color-surface);
  }
</style>
