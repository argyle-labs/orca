<script lang="ts">
  import { onMount, onDestroy, untrack } from 'svelte';
  import { goto } from '$app/navigation';
  import { page } from '$app/stores';
  import { peers } from '$lib/stores/peers.svelte';
  import { proxmoxClusters, resolvePeerClusters } from '$lib/stores/proxmoxClusters.svelte';
  import { buildDisplayInstances, buildDisplayRows } from '$lib/utils/instanceTree';
  import { viewModeFromUrl, setViewMode } from '$lib/utils/viewMode.svelte';
  import PairingModal from '$lib/components/PairingModal.svelte';
  import SegmentedControl from '$lib/components/SegmentedControl.svelte';
  import InboundOffersBanner from '$lib/components/InboundOffersBanner.svelte';
  import HostDrawer from '$lib/components/HostDrawer.svelte';
  import SystemsPageHeader from '$lib/components/SystemsPageHeader.svelte';
  import InstancesGrid from '$lib/components/InstancesGrid.svelte';
  import DiscoveredCandidatesList from '$lib/components/DiscoveredCandidatesList.svelte';
  import StalePeersList from '$lib/components/StalePeersList.svelte';
  import type { PageData } from './$types';

  let { data }: { data: PageData } = $props();

  untrack(() => peers.seed(data));

  let selectedInstId = $state<string | null>(null);
  let pairModalOpen = $state(false);
  let pairModalMode = $state<'invite' | 'accept'>('accept');
  let pairModalInitialCode = $state('');
  let collapsed = $state<Set<string>>(new Set());

  let clusterByPeer = $derived(
    resolvePeerClusters(peers.instances, proxmoxClusters.byIp, proxmoxClusters.byHost),
  );
  let view = $derived(viewModeFromUrl($page.url));
  let displayInstances = $derived(buildDisplayInstances(peers.instances, view, collapsed));
  let displayRows = $derived(
    buildDisplayRows(displayInstances, view, clusterByPeer, proxmoxClusters.summaries),
  );
  let selectedInst = $derived(peers.instances.find((i) => i.id === selectedInstId) ?? null);

  function openPair(mode: 'invite' | 'accept', code = '') {
    pairModalMode = mode;
    pairModalInitialCode = code;
    pairModalOpen = true;
  }

  function toggleCollapsed(peerId: string) {
    const next = new Set(collapsed);
    if (next.has(peerId)) next.delete(peerId);
    else next.add(peerId);
    collapsed = next;
  }

  onMount(() => {
    peers.start();
    proxmoxClusters.start();
  });

  onDestroy(() => {
    peers.stop();
    proxmoxClusters.stop();
  });
</script>

<section class="page">
  <SystemsPageHeader onInvite={() => openPair('invite')} onAccept={() => openPair('accept')} />

  <InboundOffersBanner offers={peers.inboundOffers} onaccept={() => openPair('accept')} />

  <SegmentedControl
    ariaLabel="View mode"
    items={[
      { label: 'Tree', value: 'tree' },
      { label: 'Table', value: 'table' },
    ]}
    value={view}
    onchange={(v) => setViewMode($page.url, v as 'tree' | 'table')}
  />

  <InstancesGrid
    rows={displayRows}
    {view}
    {collapsed}
    onactivate={(peerId) => goto(`/systems/${peerId}`)}
    ontoggle={toggleCollapsed}
  />

  {#if peers.instances.filter((i) => i.role === 'system').length === 0}
    <p class="hint">
      No paired systems yet. Run <code>orca pod init</code> to become a founder,
      or click <strong>+ Pair with code</strong> above and paste a code from
      <code>orca pod pair &lt;this-host&gt;</code> on the inviter.
    </p>
  {/if}

  <DiscoveredCandidatesList />
  <StalePeersList />
</section>

<PairingModal
  open={pairModalOpen}
  initialMode={pairModalMode}
  initialCode={pairModalInitialCode}
  onclose={() => (pairModalOpen = false)}
  onpaired={() => peers.refreshPodPeers()}
/>

<HostDrawer inst={selectedInst} onclose={() => (selectedInstId = null)} />

<style>
  .page {
    max-width: var(--content-max);
    margin: 0 auto;
    padding: var(--space-6);
    display: flex;
    flex-direction: column;
    gap: var(--space-5);
  }
  .hint {
    color: var(--color-text-dim);
    font-size: var(--text-xs);
    margin: 0;
  }
</style>
