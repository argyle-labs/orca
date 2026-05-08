<script lang="ts">
  interface Props {
    drift: DriftReport;
    ongoto: (id: string) => void;
  }
  let { drift, ongoto }: Props = $props();

  let open = $state(false);

  function goTo(id: string) {
    ongoto(id);
    open = false;
  }

  const cd = $derived(drift.constraintDrift);
</script>

{#if drift.totalIssues === 0}
  <button id="drift-btn" class="clean" title="No config drift">&#x2713;</button>
{:else}
  <button id="drift-btn" class="has-issues" onclick={() => (open = !open)} title="Config drift detected">
    &#x26A0; {drift.totalIssues}
  </button>

  <div id="drift-panel" class={open ? 'open' : ''}>
    <div id="drift-header">
      <h2>Config Drift</h2>
      <button id="drift-close" onclick={() => (open = false)}>&times;</button>
    </div>

    {#if drift.unassignedTables.length > 0}
      <div class="drift-section">
        <h3 class="drift-section-title amber">Unassigned Tables ({drift.unassignedTables.length})</h3>
        <p class="drift-section-desc">In database but not in any domain</p>
        {#each drift.unassignedTables as name (name)}
          <div class="drift-item clickable" onclick={() => goTo(name)} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Enter') goTo(name); }}>
            {name}
          </div>
        {/each}
      </div>
    {/if}

    {#if drift.ghostTables.length > 0}
      <div class="drift-section">
        <h3 class="drift-section-title red">Ghost Tables ({drift.ghostTables.length})</h3>
        <p class="drift-section-desc">In config but not in database</p>
        {#each drift.ghostTables as name (name)}
          <div class="drift-item">{name}</div>
        {/each}
      </div>
    {/if}

    {#if drift.unmappedFkColumns.length > 0}
      <div class="drift-section">
        <h3 class="drift-section-title blue">Unmapped FK Columns ({drift.unmappedFkColumns.length})</h3>
        <p class="drift-section-desc">Look like FKs but have no mapping</p>
        {#each drift.unmappedFkColumns as { table, column } (`${table}.${column}`)}
          <div class="drift-item clickable" onclick={() => goTo(table)} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Enter') goTo(table); }}>
            <span class="drift-table">{table}</span>.<span class="drift-col">{column}</span>
          </div>
        {/each}
      </div>
    {/if}

    {#if drift.invalidFkTargets.length > 0}
      <div class="drift-section">
        <h3 class="drift-section-title red">Invalid FK Targets ({drift.invalidFkTargets.length})</h3>
        <p class="drift-section-desc">FK mappings to non-existent tables</p>
        {#each drift.invalidFkTargets as { column, target } (column)}
          <div class="drift-item">
            {column} &rarr; <span class="drift-col">{target}</span>
          </div>
        {/each}
      </div>
    {/if}

    {#if cd && cd.missingDbConstraints.length > 0}
      <div class="drift-section">
        <h3 class="drift-section-title amber">Missing DB Constraints ({cd.missingDbConstraints.length})</h3>
        <p class="drift-section-desc">In config but no actual DB foreign key constraint</p>
        {#each cd.missingDbConstraints as { table, column, target } (`${table}.${column}`)}
          <div class="drift-item clickable" onclick={() => goTo(table)} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Enter') goTo(table); }}>
            <span class="drift-table">{table}</span>.<span class="drift-col">{column}</span> &rarr; {target}
          </div>
        {/each}
      </div>
    {/if}

    {#if cd && cd.extraDbConstraints.length > 0}
      <div class="drift-section">
        <h3 class="drift-section-title blue">Extra DB Constraints ({cd.extraDbConstraints.length})</h3>
        <p class="drift-section-desc">DB has FK constraint but no config mapping</p>
        {#each cd.extraDbConstraints as { table, column, referencedTable } (`${table}.${column}`)}
          <div class="drift-item clickable" onclick={() => goTo(table)} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Enter') goTo(table); }}>
            <span class="drift-table">{table}</span>.<span class="drift-col">{column}</span> &rarr; {referencedTable}
          </div>
        {/each}
      </div>
    {/if}

    {#if cd && cd.mismatchedFkTargets.length > 0}
      <div class="drift-section">
        <h3 class="drift-section-title red">Mismatched FK Targets ({cd.mismatchedFkTargets.length})</h3>
        <p class="drift-section-desc">Config and DB disagree on FK target table</p>
        {#each cd.mismatchedFkTargets as { table, column, configTarget, actualTarget } (`${table}.${column}`)}
          <div class="drift-item clickable" onclick={() => goTo(table)} role="button" tabindex="0" onkeydown={(e) => { if (e.key === 'Enter') goTo(table); }}>
            <span class="drift-table">{table}</span>.<span class="drift-col">{column}</span>: config &rarr; {configTarget}, DB &rarr; {actualTarget}
          </div>
        {/each}
      </div>
    {/if}
  </div>
{/if}
