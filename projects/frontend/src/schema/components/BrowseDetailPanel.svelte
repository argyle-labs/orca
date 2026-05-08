<script lang="ts">
  interface Props {
    selectedId: string | null;
    tables: Table[];
    fks: FK[];
    domains: Domain[];
    onclose: () => void;
    ongoto: (id: string) => void;
  }
  let { selectedId, tables, fks, domains, onclose, ongoto }: Props = $props();

  const table = $derived(selectedId ? tables.find((t) => t.name === selectedId) ?? null : null);
  const domain = $derived.by(() => {
    if (!selectedId) return null;
    for (const d of domains) if (d.tables.includes(selectedId)) return d;
    return null;
  });
  const fkOut = $derived(fks.filter((f) => f.from === selectedId));
  const fkIn = $derived(fks.filter((f) => f.to === selectedId));
</script>

{#if table}
  <div class="browse-detail">
    <div class="browse-detail-header">
      <div class="browse-detail-title">
        {#if domain}<span class="browse-detail-dot" style="background: {domain.color}"></span>{/if}
        <span>{table.name}</span>
        {#if domain}<span class="browse-detail-domain">{domain.group ?? domain.label}</span>{/if}
      </div>
      <button class="browse-detail-close" onclick={onclose}>✕</button>
    </div>

    <div class="browse-detail-body">
      <div class="browse-detail-section-label">Columns ({table.columns.length})</div>
      {#each table.columns as col (col.name)}
        <div class={`browse-col${col.fk ? ' browse-col-fk' : ''}`}>
          <span class="browse-col-badges">
            {#if col.pk}<span class="col-badge pk">PK</span>{/if}
            {#if col.fk}<span class="col-badge fk">FK</span>{/if}
            {#if !col.pk && !col.fk}<span class="col-badge none">  </span>{/if}
          </span>
          <span class="browse-col-name">{col.name}</span>
          <span class="browse-col-type">{col.type}</span>
          {#if col.fkTarget}
            <button class="browse-col-target" onclick={() => ongoto(col.fkTarget!)}>
              → {col.fkTarget}
            </button>
          {/if}
        </div>
      {/each}

      {#if fkIn.length > 0}
        <div class="browse-detail-section-label" style="margin-top: 1rem">Referenced by ({fkIn.length})</div>
        {#each fkIn as f (`${f.from}.${f.fromCol}`)}
          <button class="browse-rel-item" onclick={() => ongoto(f.from)}>
            ← {f.from} <span class="browse-rel-via">via {f.fromCol}</span>
          </button>
        {/each}
      {/if}

      {#if fkOut.length > 0}
        <div class="browse-detail-section-label" style="margin-top: 1rem">References ({fkOut.length})</div>
        {#each fkOut as f (`${f.from}.${f.fromCol}`)}
          <button class="browse-rel-item" onclick={() => ongoto(f.to)}>
            → {f.to} <span class="browse-rel-via">via {f.fromCol}</span>
          </button>
        {/each}
      {/if}
    </div>
  </div>
{/if}
