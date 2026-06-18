<script lang="ts">
  import { onMount } from 'svelte';
  import { callTool } from '$lib/stores/runTool';
  import Popover from '$lib/components/Popover.svelte';

  const PRESETS = [
    { label: 'No history', value: 0 },
    { label: '1 day', value: 1 },
    { label: '7 days', value: 7 },
    { label: 'Custom', value: -1 },
  ];

  let days = $state<number>(1);
  let saving = $state(false);
  let popoverOpen = $state(false);
  let customInput = $state('');

  let activeSegment = $derived(
    (() => {
      const i = PRESETS.findIndex((p) => p.value === days);
      return i >= 0 && i < 3 ? i : 3;
    })(),
  );

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
    if (activeSegment === 3 && days > 0) return `${days}d`;
    return 'Custom';
  }
</script>

<div
  class="retention-picker"
  title="Storage setting — controls how many days of metrics are kept on disk"
>
  <span class="retention-label">Keep history</span>
  <div class="retention-segment" role="radiogroup" aria-label="Keep history">
    {#each PRESETS as preset, i}
      {#if i < 3}
        <button
          class="segment-btn"
          class:is-active={activeSegment === i}
          role="radio"
          aria-checked={activeSegment === i}
          disabled={saving}
          onclick={() => setDays(preset.value)}
        >{preset.label}</button>
      {:else}
        <Popover bind:open={popoverOpen} align="end" width={200}>
          {#snippet trigger()}
            <button
              class="segment-btn segment-btn-custom"
              class:is-active={activeSegment === 3}
              aria-haspopup="dialog"
              aria-expanded={popoverOpen}
              disabled={saving}
              onclick={() => {
                customInput = activeSegment === 3 ? String(days) : '';
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
              <button
                class="custom-apply-btn"
                onclick={applyCustom}
                disabled={!customInput || parseInt(customInput) < 1}
              >Apply</button>
            </div>
          {/snippet}
        </Popover>
      {/if}
    {/each}
  </div>
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
  .retention-segment {
    display: flex;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 6px;
    padding: 3px;
    gap: 2px;
  }
  .segment-btn {
    flex: 1 1 0;
    min-width: 0;
    background: transparent;
    border: 1px solid transparent;
    color: var(--color-text-muted);
    font-size: var(--text-xs);
    padding: 4px 10px;
    cursor: pointer;
    white-space: nowrap;
    border-radius: 4px;
    transition: background 0.15s, color 0.15s, border-color 0.15s;
  }
  .segment-btn:hover:not(:disabled):not(.is-active) {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .segment-btn.is-active {
    background: color-mix(in srgb, var(--color-accent) 12%, var(--color-surface));
    color: var(--color-accent);
    border-color: var(--color-accent);
    font-weight: 500;
  }
  .segment-btn:disabled {
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
  .custom-apply-btn {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 15%, transparent);
    border: 1px solid var(--color-accent, #4f86f7);
    border-radius: 4px;
    color: var(--color-accent, #4f86f7);
    font-size: var(--text-xs);
    padding: 4px 12px;
    cursor: pointer;
    transition: background 0.15s, color 0.15s;
    align-self: flex-end;
  }
  .custom-apply-btn:hover:not(:disabled) {
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 25%, transparent);
    color: var(--color-text);
  }
  .custom-apply-btn:disabled {
    opacity: 0.4;
    cursor: not-allowed;
  }
</style>
