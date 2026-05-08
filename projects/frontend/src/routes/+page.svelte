<script lang="ts">
  import { getRecent, timeAgo } from '$lib/stores/navHistory';
  import { browser } from '$app/environment';

  const recent = browser ? getRecent(12).filter(e => e.path !== '/') : [];

  const QUICK_LINKS = [
    { href: '/plugins',    label: 'Plugins' },
    { href: '/schema',     label: 'Schema Explorer' },
    { href: '/api-docs',   label: 'API Docs' },
    { href: '/session',    label: 'Agent Session' },
    { href: '/system',     label: 'System Status' },
    { href: '/graphql',    label: 'GraphQL IDE' },
    { href: '/ctx7',       label: 'Library Docs' },
  ];
</script>

<svelte:head><title>orca</title></svelte:head>

<div class="home">
  <h1 class="title">orca</h1>
  <p class="sub">Local dev tool — docs, services, schema, MCP proxy</p>

  {#if recent.length > 0}
    <section>
      <h2 class="section-label">Recently accessed</h2>
      <div class="grid">
        {#each recent as entry}
          <a href={entry.path} class="card">
            <span class="card-path">{entry.path}</span>
            <span class="card-time">{timeAgo(entry.ts)}</span>
          </a>
        {/each}
      </div>
    </section>
  {:else}
    <section>
      <h2 class="section-label">Quick links</h2>
      <div class="grid">
        {#each QUICK_LINKS as link}
          <a href={link.href} class="card"><span class="card-path">{link.label}</span></a>
        {/each}
      </div>
    </section>
  {/if}
</div>

<style>
  .home { padding: var(--space-8); max-width: 800px; margin: 0 auto; }
  .title { font-size: 2.5rem; font-weight: 700; margin-bottom: var(--space-2); }
  .sub { color: var(--color-text-dim); margin-bottom: var(--space-8); }
  .section-label { font-size: var(--text-sm); color: var(--color-text-dim); font-weight: 500;
                   text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: var(--space-3); }
  .grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(180px, 1fr)); gap: var(--space-3); }
  .card {
    display: flex; flex-direction: column; gap: var(--space-1);
    padding: var(--space-3) var(--space-4);
    background: var(--color-surface); border: 1px solid var(--color-border);
    border-radius: var(--radius-md); text-decoration: none; color: var(--color-text);
  }
  .card:hover { border-color: var(--color-accent); }
  .card-path { font-size: var(--text-sm); font-family: var(--font-mono); }
  .card-time { font-size: var(--text-xs); color: var(--color-text-dim); }
</style>
