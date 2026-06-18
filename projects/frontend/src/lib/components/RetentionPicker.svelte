<script lang="ts">
  import { onMount } from 'svelte';
  import { callTool } from '$lib/stores/runTool';
  import Popover from '$lib/components/primitives/Popover.svelte';
  import SegmentedControl from '$lib/components/primitives/SegmentedControl.svelte';
  import Button from '$lib/components/primitives/Button.svelte';

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

  onMount(async () => {
    try {
      const data = await callTool<{ row: { json: string } | null }>('configGet', {
        noun: 'host_status',
        name: 'retention_days',
      });
      if (data?.row) {
        const v = parseFloat(data.row.json);
        if (Number.isFinite(v)) days = v;
      }
    } catch {
      // keep default
    }
  });

  async function setDays(d: number) {
    saving = true;
    try {
      await callTool('configSet', {
        noun: 'host_status',
        name: 'retention_days',
        json: String(d),
      });
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
