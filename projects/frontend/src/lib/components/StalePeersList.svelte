<script lang="ts">
  import AuxList from '$lib/components/AuxList.svelte';
  import AuxRow from '$lib/components/AuxRow.svelte';
  import { peers } from '$lib/stores/peers.svelte';
</script>

{#if peers.staleRows.length > 0}
  <AuxList title="Dead / stale — safe to remove">
    {#each peers.staleRows as s (s.peer_id)}
      <AuxRow
        name={s.hostname || s.peer_id}
        sub={`${s.addr}${s.port ? `:${s.port}` : ''}`}
        tag={s.reason}
        statusOk={false}
        actionLabel="Forget"
        busyLabel="Removing…"
        busy={peers.forgettingId === s.peer_id}
        actionVariant="danger"
        onaction={() => peers.forgetPeer(s)}
      />
    {/each}
  </AuxList>
{/if}
