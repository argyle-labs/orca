<script lang="ts">
  import { systemDetail, configDetail, configUpsert } from '$lib/client/sdk.gen';
  import { unwrap } from '$lib/stores/runTool';
  import Popover from '$lib/components/primitives/Popover.svelte';
  import SegmentedControl from '$lib/components/primitives/SegmentedControl.svelte';
  import Button from '$lib/components/primitives/Button.svelte';

  // peerId is the frontend identifier:
  //   - "local" → resolve to the real machine_id via system.detail (the
  //     daemon keys host_status rows by machine_id_short, NOT by "local")
  //   - any other value is already machine_id_short and used as-is
  // Final key shape: `retention_days:<resolved>` — see
  // projects/db/src/host_status.rs::resolve_per_peer_then_global.
  interface Props { peerId: string; }
  let { peerId }: Props = $props();

  let resolvedId = $state<string | null>(null);

  const PRESETS: { label: string; value: number }[] = [
    { label: 'No history', value: 0 },
    { label: '1 day', value: 1 },
    { label: '7 days', value: 7 },
  ];

  let days = $state<number>(1);
  let saving = $state(false);
  let popoverOpen = $state(false);
  let customInput = $state('');

  let isCustom = $derived(!PRESETS.some((p) => p.value === days));
  let segmentValue = $derived(isCustom ? -1 : days);

  $effect(() => {
    if (!peerId) return;
    void resolveAndLoad(peerId);
  });

  async function resolveAndLoad(id: string) {
    let key = id;
    if (id === 'local') {
      try {
        const detail = await unwrap(systemDetail({ body: {} }));
        if (detail?.machine_id) key = detail.machine_id;
      } catch {
        return;
      }
    }
    resolvedId = key;
    try {
      const data = await unwrap(configDetail({ body: {
        noun: 'host_status',
        name: `retention_days:${key}`,
      } }));
      if (data?.row) {
        const v = parseFloat(data.row.json);
        if (Number.isFinite(v)) days = v;
      } else {
        days = 1;
      }
    } catch {
      // keep default
    }
  }

  async function setDays(d: number) {
    if (!resolvedId) return;
    saving = true;
    try {
      await unwrap(configUpsert({ body: {
        noun: 'host_status',
        name: `retention_days:${resolvedId}`,
        json: String(d),
      } }));
      days = d;
    } catch (e) {
      console.warn('retention set failed:', e);
    } finally {
      saving = false;
    }
  }

  async function applyCustom() {
    const d = parseInt(customInput, 10);
    if (!Number.isFinite(d) || d < 1) return;
    popoverOpen = false;
    await setDays(d);
  }

  function customLabel(): string {
    if (isCustom && days > 0) return `${days}d`;
    return 'Custom';
  }
</script>

<div
  class="retention-picker"
  title="Storage setting — controls how many days of metrics are kept on disk"
>
  <span class="retention-label">Keep history</span>
  <SegmentedControl
    ariaLabel="Keep history"
    value={segmentValue}
    onchange={(v) => setDays(v as number)}
    disabled={saving}
    items={PRESETS}
  >
    {#snippet trailing()}
      <Popover bind:open={popoverOpen} align="end" width={200}>
        {#snippet trigger()}
          <button
            type="button"
            class="seg-custom"
            class:is-active={isCustom}
            aria-haspopup="dialog"
            aria-expanded={popoverOpen}
            disabled={saving}
            onclick={() => {
              customInput = isCustom ? String(days) : '';
              popoverOpen = true;
            }}
          >{customLabel()}</button>
        {/snippet}
        {#snippet children()}
          <div class="custom-popover">
            <p class="custom-popover-label">Days to keep</p>
            <input
              type="number"
              min="1"
              max="365"
              placeholder="e.g. 14"
              bind:value={customInput}
              class="custom-days-input"
              onkeydown={(e) => e.key === 'Enter' && applyCustom()}
            />
            <Button
              variant="primary"
              size="sm"
              onclick={applyCustom}
              disabled={!customInput || parseInt(customInput) < 1}
            >Apply</Button>
          </div>
        {/snippet}
      </Popover>
    {/snippet}
  </SegmentedControl>
</div>

<style>
  .retention-picker {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-shrink: 0;
  }
  .retention-label {
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
    white-space: nowrap;
  }
  .seg-custom {
    background: transparent;
    border: none;
    border-left: 1px solid var(--color-border);
    padding: var(--space-1) var(--space-3);
    font: inherit;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    cursor: pointer;
    white-space: nowrap;
  }
  .seg-custom:hover:not(:disabled):not(.is-active) {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .seg-custom.is-active {
    background: var(--color-accent);
    color: white;
  }
  .seg-custom:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .custom-popover {
    padding: var(--space-3);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .custom-popover-label {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    text-transform: uppercase;
    letter-spacing: 0.06em;
  }
  .custom-days-input {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm, 4px);
    color: var(--color-text);
    font-size: var(--text-sm);
    padding: 4px 8px;
    width: 100%;
    box-sizing: border-box;
  }
  .custom-days-input:focus {
    outline: none;
    border-color: var(--color-accent, #4f86f7);
  }
  .custom-popover :global(.btn) {
    align-self: flex-end;
  }
</style>
