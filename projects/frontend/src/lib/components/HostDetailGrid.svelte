<script lang="ts">
  import { relTime } from '$lib/utils/format';
  import { fmtGpu } from '$lib/utils/format';
  import type { Instance } from '$lib/types/instance';

  interface Props {
    inst: Instance;
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

  {#if inst.sys?.os_name}
    <dt>OS</dt>
    <dd>
      <code>{inst.sys.os_name}{inst.sys.os_version ? ` ${inst.sys.os_version}` : ''}</code>
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

  {#if inst.sys?.gpus?.length}
    <dt>GPU</dt>
    <dd>
      {#each inst.sys.gpus as g}
        <code>{fmtGpu(g)}</code>
      {/each}
    </dd>
  {/if}

  <dt>Checked</dt>
  <dd>{relTime(inst.lastChecked)}</dd>
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
