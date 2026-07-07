<script lang="ts">
  import { relTime } from '$lib/utils/format';
  import { fmtGpu } from '$lib/utils/format';
  import type { PodInstance } from '$lib/client/types.gen';

  interface Props {
    inst: PodInstance;
  }
  let { inst }: Props = $props();
</script>

<dl class="detail-grid">
  <dt>Origin</dt>
  <dd><code>{inst.origin}</code></dd>

  {#if inst.status}
    <dt>Status</dt>
    <dd><code>{inst.status}</code></dd>
  {/if}

  {#if inst.system?.os_name}
    <dt>OS</dt>
    <dd>
      <code>{inst.system.os_name}{inst.system.os_version ? ` ${inst.system.os_version}` : ''}</code>
    </dd>
  {/if}

  {#if inst.version}
    <dt>Version</dt>
    <dd><code>{inst.version}</code></dd>
  {/if}

  {#if inst.target}
    <dt>Target</dt>
    <dd><code>{inst.target}</code></dd>
  {/if}

  {#if inst.system?.gpus?.length}
    <dt>GPU</dt>
    <dd>
      {#each inst.system.gpus as g}
        <code>{fmtGpu(g)}</code>
      {/each}
    </dd>
  {/if}

  <dt>Checked</dt>
  <dd>{relTime(inst.last_checked ?? null)}</dd>
</dl>

<style>
  .detail-grid {
    margin: 0;
    display: grid;
    grid-template-columns: 80px 1fr;
    row-gap: 6px;
    column-gap: var(--space-3);
    font-size: var(--text-xs);
  }
  dt {
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    font-size: 10px;
    padding-top: 2px;
  }
  dd {
    margin: 0;
    color: var(--color-text);
    word-break: break-all;
  }
  code {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
    font-size: var(--text-xs);
  }
</style>
