<script lang="ts">
  import AuxList from '$lib/components/AuxList.svelte';
  import AuxRow from '$lib/components/AuxRow.svelte';
  import { peers } from '$lib/stores/peers.svelte';
</script>

{#if peers.candidates.length > 0}
  <AuxList title="Discovered — not yet joined">
    {#each peers.candidates as c (c.pubkey_fp)}
      <AuxRow
        name={c.hostname || c.addr}
        sub={`${c.addr}:${c.port}`}
        statusOk={null}
        actionLabel="+ Add"
        busyLabel="Adding…"
        busy={peers.joiningFp === c.pubkey_fp}
        onaction={() => peers.joinCandidate(c)}
      />
    {/each}
  </AuxList>
{/if}
