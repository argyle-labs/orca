<script lang="ts">
  import type { PropertyRow } from '../parseDoc';
  let { rows }: { rows: PropertyRow[] } = $props();
</script>

{#if rows.length > 0}
  <section class="properties-panel" aria-label="Properties">
    <h2 class="properties-panel-title">Properties</h2>
    <div class="properties-panel-grid">
      {#each rows as row (row.key)}
        <div class="properties-panel-row">
          <div class="properties-panel-key">
            <span class="properties-panel-icon">
              <svg aria-hidden="true" width="14" height="14" viewBox="0 0 24 24" fill="none"
                   stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">
                <line x1="4" y1="6"  x2="20" y2="6"  />
                <line x1="4" y1="12" x2="20" y2="12" />
                <line x1="4" y1="18" x2="14" y2="18" />
              </svg>
            </span>
            <span class="properties-panel-key-text">{row.key}</span>
          </div>
          <div class="properties-panel-value">
            {#if Array.isArray(row.value)}
              <div class="properties-panel-tags">
                {#each row.value as v, i (i)}
                  <span class="properties-panel-tag">{v}</span>
                {/each}
              </div>
            {:else}
              <span>{row.value}</span>
            {/if}
          </div>
        </div>
      {/each}
    </div>
  </section>
{/if}

<style>
  .properties-panel { margin-top: var(--space-6); border-top: 1px solid var(--color-border); padding-top: var(--space-4); }
  .properties-panel-title { font-size: var(--text-xs); font-weight: var(--weight-semibold); color: var(--color-text-dim); text-transform: uppercase; letter-spacing: 0.05em; margin-bottom: var(--space-3); }
  .properties-panel-grid { display: flex; flex-direction: column; gap: var(--space-2); }
  .properties-panel-row { display: grid; grid-template-columns: 140px 1fr; gap: var(--space-2); font-size: var(--text-sm); }
  .properties-panel-key { display: flex; align-items: center; gap: var(--space-1); color: var(--color-text-dim); }
  .properties-panel-icon { color: var(--color-text-faint); flex-shrink: 0; }
  .properties-panel-key-text { font-weight: var(--weight-medium); }
  .properties-panel-value { color: var(--color-text-muted); }
  .properties-panel-tags { display: flex; flex-wrap: wrap; gap: var(--space-1); }
  .properties-panel-tag { background: var(--color-surface-2); border: 1px solid var(--color-border); border-radius: var(--radius-sm); padding: 1px 6px; font-size: var(--text-xs); color: var(--color-text-dim); }
</style>
