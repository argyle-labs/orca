<script lang="ts">
  import { onMount, onDestroy, untrack } from 'svelte';
  import { goto } from '$app/navigation';
  import { peers } from '$lib/stores/peers.svelte';
  import { networkTopologyStore } from '$lib/stores/networkTopology.svelte';
  import PairingModal from '$lib/components/PairingModal.svelte';
  import InboundOffersBanner from '$lib/components/InboundOffersBanner.svelte';
  import SystemsPageHeader from '$lib/components/SystemsPageHeader.svelte';
  import TopologyCanvas from '$lib/components/TopologyCanvas.svelte';
  import StalePeersList from '$lib/components/StalePeersList.svelte';
  import type { PageData } from './$types';

  let { data }: { data: PageData } = $props();

  untrack(() =>
    peers.seed({
      instances: data.instances,
      candidates: data.candidates,
      stale: data.stale,
      inboundOffers: data.inboundOffers,
    }),
  );

  let pairModalOpen = $state(false);
  let pairModalMode = $state<'invite' | 'accept'>('accept');
  let pairModalInitialCode = $state('');

  let nodes = $derived(networkTopologyStore.nodes);
  let edges = $derived(networkTopologyStore.edges);

  function openPair(mode: 'invite' | 'accept', code = '') {
    pairModalMode = mode;
    pairModalInitialCode = code;
    pairModalOpen = true;
  }

  // Topology node ids are pod peer ids (`local` for this host); the
  // `/systems/[id]` detail page resolves the same ids, so a click navigates
  // straight to the per-system page rather than opening a drawer.
  function openNode(id: string) {
    void goto(`/systems/${encodeURIComponent(id)}`);
  }

  onMount(() => {
    peers.start();
    networkTopologyStore.start();
  });

  onDestroy(() => {
    peers.stop();
    networkTopologyStore.stop();
  });
</script>

<section class="page">
  <SystemsPageHeader onAddSystem={() => openPair('accept')} />

  <InboundOffersBanner offers={peers.inboundOffers} onaccept={() => openPair('accept')} />

  <TopologyCanvas {nodes} {edges} onSelect={openNode} />

  {#if peers.instances.filter((i) => i.role === 'system').length === 0}
    <p class="hint">
      No paired systems yet. Run <code>orca pod init</code> to become a founder,
      or use <strong>+ System</strong> above to add one from LAN discovery or a
      pairing code.
    </p>
  {/if}

  <StalePeersList />
</section>

<PairingModal
  open={pairModalOpen}
  initialMode={pairModalMode}
  initialCode={pairModalInitialCode}
  onclose={() => (pairModalOpen = false)}
  onpaired={() => peers.refreshPodPeers()}
/>

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
