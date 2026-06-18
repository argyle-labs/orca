<script lang="ts">
  import { callTool } from '$lib/stores/runTool';
  import { inferChannel } from '$lib/utils/version';
  import SectionHead from '$lib/components/primitives/SectionHead.svelte';
  import Button from '$lib/components/primitives/Button.svelte';
  import SegmentedControl from '$lib/components/primitives/SegmentedControl.svelte';
  import type { Instance, VersionEntry } from '$lib/types/instance';

  interface Props {
    inst: Instance;
  }
  let { inst }: Props = $props();

  type SystemUpdateResp = {
    current_version: string;
    channel: string;
    pinned_to: string | null;
    available_versions: VersionEntry[];
    latest: string | null;
    notes: string[];
    errors: string[];
    update_available?: boolean | null;
  };

  let versionSelect = $state('');
  let channelSelect = $state('stable');
  let versions = $state<VersionEntry[]>([]);
  let versionsLoading = $state(false);
  let updatePending = $state(false);
  let updateResult = $state<{ notes: string[]; errors: string[] } | null>(null);
  let openedForId = $state<string | null>(null);

  $effect(() => {
    // Reset controls when the host changes; polling updates inst data
    // without changing inst.id and must NOT clobber user selections.
    if (inst.id !== openedForId) {
      openedForId = inst.id;
      versionSelect = inst.version ? `v${inst.version}` : '';
      channelSelect = inferChannel(inst.version, inst.channel);
      versions = inst.availableVersions ?? [];
      updateResult = null;
      if (!(inst.availableVersions ?? []).length) {
        void probeUpdateState();
      }
    } else {
      // Same host, polled data refreshed — pick up new available_versions
      // without clobbering the user's pending selection.
      versions = inst.availableVersions ?? versions;
      if (!versionSelect && inst.version) versionSelect = `v${inst.version}`;
    }
  });

  async function probeUpdateState() {
    versionsLoading = true;
    try {
      const peer = inst.role === 'system' ? inst.peerId : null;
      const r = await callTool<SystemUpdateResp>('systemUpdate', {}, { peer });
      versions = r.available_versions ?? [];
      inst.channel = r.channel;
      inst.pinnedTo = r.pinned_to;
      inst.actionLockUntil = Date.now() + 15000;
      if (r.current_version) inst.version = r.current_version;
      if (r.current_version) versionSelect = `v${r.current_version}`;
      channelSelect = inferChannel(r.current_version, r.channel);
      if (r.latest) {
        inst.updateLatest = r.latest;
        inst.updateAvailable = r.update_available === true;
      }
      if (!versionSelect && r.current_version) {
        versionSelect = `v${r.current_version}`;
      }
    } catch (e) {
      console.warn('update state probe failed:', e);
    } finally {
      versionsLoading = false;
    }
  }

  async function runSystemUpdate(args: Record<string, unknown>) {
    updatePending = true;
    updateResult = null;
    try {
      const peer = inst.role === 'system' ? inst.peerId : null;
      const r = await callTool<SystemUpdateResp>('systemUpdate', args, { peer });
      updateResult = { notes: r.notes ?? [], errors: r.errors ?? [] };
      versions = r.available_versions ?? versions;
      inst.channel = r.channel;
      inst.pinnedTo = r.pinned_to;
      inst.actionLockUntil = Date.now() + 15000;
      if (r.current_version) inst.version = r.current_version;
      if (r.current_version) versionSelect = `v${r.current_version}`;
      channelSelect = inferChannel(r.current_version, r.channel);
      if (r.latest) {
        inst.updateLatest = r.latest;
        inst.updateAvailable = r.update_available === true;
      }
    } catch (e) {
      console.warn('system update failed:', e);
      updateResult = { notes: [], errors: [e instanceof Error ? e.message : String(e)] };
    } finally {
      updatePending = false;
    }
  }

  async function applyUpdateSelection() {
    const args: Record<string, unknown> = {};
    if (channelSelect && channelSelect !== inferChannel(inst.version, inst.channel)) {
      args.channel = channelSelect;
    }
    if (versionSelect && versionSelect !== `v${inst.version ?? ''}`) {
      args.version = versionSelect;
    }
    if (Object.keys(args).length === 0) return;
    await runSystemUpdate(args);
  }
</script>

<SectionHead title="Update">
  {#snippet trailing()}
    <Button
      size="xs"
      onclick={probeUpdateState}
      disabled={versionsLoading || updatePending}
      title="Re-probe this peer's update state"
    >{versionsLoading ? 'Probing…' : 'Refresh'}</Button>
  {/snippet}
</SectionHead>
<div class="update-controls">
  <div class="update-setting-row">
    <span class="update-setting-label">
      Version
      {#if inst.pinnedTo}
        <span class="pin-badge" title={`Pinned to ${inst.pinnedTo} — unpin to follow latest on channel`}>📌</span>
      {/if}
    </span>
    <select class="version-input" bind:value={versionSelect} disabled={updatePending}>
      {#if inst.version && !versions.some((v) => v.tag === `v${inst.version}`)}
        <option value={`v${inst.version}`}>v{inst.version} (current)</option>
      {/if}
      {#each versions as v}
        <option value={v.tag}>{v.tag}{inst.version && v.tag === `v${inst.version}` ? ' (current)' : ''}</option>
      {/each}
    </select>
  </div>

  <div class="update-setting-row">
    <span class="update-setting-label">Channel</span>
    <SegmentedControl
      ariaLabel="Channel"
      value={channelSelect}
      onchange={(v) => (channelSelect = v)}
      disabled={updatePending}
      items={[
        { label: 'stable', value: 'stable', title: 'Select stable channel' },
        { label: 'rc', value: 'rc', title: 'Select rc channel' },
        { label: 'dev', value: 'dev', title: 'Select dev channel' },
      ]}
    />
  </div>

  {#if !inst.pinnedTo && inst.updateAvailable && inst.updateLatest}
    <p class="pinned-hint avail">Update available: <code>{inst.updateLatest}</code></p>
  {/if}

  <div class="update-actions-row">
    <Button
      variant="primary"
      size="sm"
      onclick={applyUpdateSelection}
      disabled={updatePending || (!inst.pinnedTo && `v${inst.version ?? ''}` === versionSelect && inferChannel(inst.version, inst.channel) === channelSelect)}
      title="Apply selected channel and version — selecting a non-latest version pins; selecting latest unpins"
    >{updatePending ? 'Updating…' : 'Apply'}</Button>
  </div>

  {#if updateResult}
    {#if updateResult.notes.length > 0}
      <p class="update-status ok">{updateResult.notes.join(' · ')}</p>
    {/if}
    {#if updateResult.errors.length > 0}
      <p class="err">{updateResult.errors.join(' · ')}</p>
    {/if}
  {/if}
</div>

<style>
  .update-controls {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .version-input {
    background: color-mix(in srgb, var(--color-bg) 60%, transparent);
    border: 1px solid var(--color-border);
    border-radius: 6px;
    padding: 4px 8px;
    color: inherit;
    font-family: var(--font-mono, monospace);
    font-size: var(--text-sm);
    flex: 1;
    min-width: 22ch;
  }
  .version-input:focus {
    outline: none;
    border-color: var(--color-accent, #4ea1ff);
  }
  .update-setting-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .update-setting-label {
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    flex-shrink: 0;
  }
  .update-actions-row {
    display: flex;
    justify-content: flex-end;
    gap: var(--space-2);
  }
  .pinned-hint {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-text-dim);
  }
  .pinned-hint.avail {
    color: var(--color-accent, #4f86f7);
  }
  .pin-badge {
    font-size: var(--text-xs);
    padding: 2px 8px;
    border-radius: 999px;
    background: color-mix(in srgb, var(--color-accent, #4f86f7) 18%, transparent);
    color: var(--color-accent, #4f86f7);
    border: 1px solid color-mix(in srgb, var(--color-accent, #4f86f7) 40%, transparent);
    white-space: nowrap;
  }
  .update-status {
    margin: 0;
    font-size: var(--text-xs);
    color: var(--color-text-muted);
    font-family: var(--font-mono);
  }
  .update-status.ok {
    color: var(--color-success, #4caf50);
  }
  .err {
    color: var(--color-error);
    font-size: var(--text-xs);
    font-family: var(--font-mono);
  }
  code {
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    padding: 1px 5px;
    font-size: var(--text-xs);
  }
</style>
