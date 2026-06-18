<script lang="ts">
  import { callTool } from '$lib/stores/runTool';
  import { systemTypeLabel, capabilityLabel } from '$lib/utils/labels';
  import Drawer from '$lib/components/Drawer.svelte';
  import StatusDot from '$lib/components/StatusDot.svelte';
  import IconButton from '$lib/components/IconButton.svelte';
  import Button from '$lib/components/Button.svelte';
  import Badge from '$lib/components/Badge.svelte';
  import SectionHead from '$lib/components/SectionHead.svelte';
  import ToggleSwitch from '$lib/components/ToggleSwitch.svelte';
  import HostDetailGrid from '$lib/components/HostDetailGrid.svelte';
  import HostLiveCharts from '$lib/components/HostLiveCharts.svelte';
  import HostAddressList from '$lib/components/HostAddressList.svelte';
  import HostUpdatePanel from '$lib/components/HostUpdatePanel.svelte';
  import type { Instance } from '$lib/types/instance';
  import type { SystemInfoReport } from '$lib/client/types.gen';

  interface Props {
    inst: Instance | null;
    onclose: () => void;
  }
  let { inst, onclose }: Props = $props();

  let secureToggling = $state(false);
  let detailRefreshing = $state(false);

  let sys = $derived(inst?.sys);
  let typeBadge = $derived(sys?.system_type ? systemTypeLabel(sys.system_type) : '');
  let virtBadge = $derived(
    sys?.virtualization && sys.virtualization !== 'none' ? sys.virtualization : '',
  );
  let capBadges = $derived((sys?.detected_capabilities ?? []).map(capabilityLabel));

  async function refreshDetail() {
    if (!inst) return;
    detailRefreshing = true;
    try {
      const peer = inst.role === 'system' ? inst.peerId : null;
      const s = await callTool<{
        version: string;
        target: string;
        mode?: string;
        channel?: string;
        pinned_to?: string;
        system?: SystemInfoReport | null;
      }>('systemDetail', {}, { peer });
      if (inst) {
        inst.version = s.version ?? inst.version;
        inst.target = s.target ?? inst.target;
        inst.mode = s.mode ?? inst.mode;
        inst.channel = s.channel ?? inst.channel;
        inst.pinnedTo = s.pinned_to ?? inst.pinnedTo;
        inst.sys = s.system ?? inst.sys;
        inst.lastChecked = Date.now();
      }
    } catch (e) {
      console.warn('system.detail refresh failed:', e);
    } finally {
      detailRefreshing = false;
    }
  }

  async function toggleSecure() {
    if (!inst || secureToggling) return;
    secureToggling = true;
    try {
      const next = !(inst.sys?.self_secure ?? false);
      const args: Record<string, unknown> = { self_secure: next };
      if (inst.role === 'system') args.peer_id = inst.peerId;
      const result = await callTool<{ self_secure: boolean }>('podUpdate', args);
      if (inst.sys) {
        inst.sys = { ...inst.sys, self_secure: result.self_secure };
      } else {
        inst.sys = { self_secure: result.self_secure } as SystemInfoReport;
      }
      // Parent's 5s poller reconciles with authoritative source.
    } catch (e) {
      console.warn('self_secure toggle failed:', e);
    } finally {
      secureToggling = false;
    }
  }
</script>

<Drawer open={!!inst} side="right" {onclose} ariaLabel="Host details">
  {#if inst}
    <div class="drawer-header">
      <div class="ident">
        <StatusDot
          ok={inst.health === 'up' ? true : inst.health === 'down' ? false : null}
        />
        <span class="hostname">{inst.sys?.hostname ?? inst.label}</span>
      </div>
      <div class="header-actions">
        <Button
          size="xs"
          onclick={refreshDetail}
          disabled={detailRefreshing}
          title="Force a fresh system.detail probe of this peer"
        >{detailRefreshing ? 'Refreshing…' : 'Refresh'}</Button>
        <IconButton onclick={onclose} title="Close">✕</IconButton>
      </div>
    </div>

    <div class="drawer-body">
      {#if typeBadge || virtBadge || capBadges.length}
        <div class="badges">
          {#if typeBadge}<Badge color="accent">{typeBadge}</Badge>{/if}
          {#if virtBadge}<Badge color="purple">{virtBadge}</Badge>{/if}
          {#each capBadges as cap}<Badge color="gray">{cap}</Badge>{/each}
        </div>
      {/if}

      <HostDetailGrid {inst} />

      <HostLiveCharts {inst} />

      {#if (inst.addresses ?? []).length > 0}
        <SectionHead title="Addresses" />
        <HostAddressList addresses={inst.addresses ?? []} />
      {/if}

      <div
        class="secure-row"
        title="When on, this host is authorized to receive encrypted secrets replicated from other pod members. Independent of pairing."
      >
        <div class="secure-row-text">
          <span class="secure-label">SECURE</span>
          <span class="secure-hint">Can accept secrets from other systems</span>
        </div>
        <ToggleSwitch
          checked={!!inst.sys?.self_secure}
          disabled={secureToggling}
          onchange={toggleSecure}
          ariaLabel="Toggle SECURE (self_secure)"
        />
      </div>

      {#if inst.role === 'system'}
        <div class="paired-line" title="Paired peers in the pod automatically exchange mesh certs. Use Unpair to revoke.">
          <span class="paired-check">✓</span>
          <span>Paired</span>
        </div>
      {/if}

      <HostUpdatePanel {inst} />

      {#if inst.error}
        <div class="err">{inst.error}</div>
      {/if}
    </div>
  {/if}
</Drawer>

<style>
  .drawer-header {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: var(--space-4);
    border-bottom: 1px solid var(--color-border);
    flex-shrink: 0;
  }
  .drawer-body {
    flex: 1;
    overflow-y: auto;
    padding: var(--space-4);
    display: flex;
    flex-direction: column;
    gap: var(--space-4);
  }
  .ident {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .hostname {
    font-weight: var(--weight-semibold);
  }
  .header-actions {
    display: flex;
    gap: 6px;
    align-items: center;
  }
  .badges {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-2);
    margin-bottom: var(--space-3);
  }
  .secure-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3);
    padding: var(--space-3);
    margin: var(--space-3) 0;
    background: color-mix(in srgb, var(--color-surface, #1a1a2e) 80%, transparent);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-md, 8px);
  }
  .secure-row-text {
    display: flex;
    flex-direction: column;
    gap: 4px;
    min-width: 0;
    flex: 1;
  }
  .secure-label {
    font-size: var(--text-xs);
    font-weight: 700;
    letter-spacing: 0.06em;
    color: var(--color-text);
  }
  .secure-hint {
    font-size: var(--text-xs);
    color: var(--color-text-dim);
    line-height: 1.3;
  }
  .paired-line {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: var(--space-3) 0;
    font-size: 11px;
    color: var(--color-text-dim);
  }
  .paired-check {
    color: #22c55e;
    font-weight: 700;
  }
  .err {
    color: var(--color-error);
    font-size: var(--text-xs);
    font-family: var(--font-mono);
  }
</style>
