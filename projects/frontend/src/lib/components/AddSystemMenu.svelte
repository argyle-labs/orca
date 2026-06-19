<script lang="ts">
  import Popover from '$lib/components/primitives/Popover.svelte';
  import Button from '$lib/components/primitives/Button.svelte';
  import Badge from '$lib/components/primitives/Badge.svelte';
  import { peers } from '$lib/stores/peers.svelte';

  interface Props {
    onInvite: () => void;
    onAccept: () => void;
  }
  let { onInvite, onAccept }: Props = $props();

  let open = $state(false);
  let count = $derived(peers.candidates.length);

  async function add(c: (typeof peers.candidates)[number]) {
    open = false;
    await peers.joinCandidate(c);
  }

  function invite() {
    open = false;
    onInvite();
  }

  function accept() {
    open = false;
    onAccept();
  }
</script>

<div class="anchor">
  <Popover bind:open align="end" width={320}>
    {#snippet trigger()}
      <button type="button" class="trigger" onclick={() => (open = !open)} aria-haspopup="menu">
        <span>+ System</span>
        {#if count > 0}
          <span class="count"><Badge color="accent">{count}</Badge></span>
        {/if}
      </button>
    {/snippet}
    {#snippet children()}
      <div class="menu" role="menu">
        {#if count > 0}
          <div class="section-label">Discovered on LAN</div>
          <ul class="discovered">
            {#each peers.candidates as c (c.pubkey_fp)}
              <li class="row">
                <div class="info">
                  <span class="name">{c.hostname || c.addr}</span>
                  <span class="sub">{c.addr}:{c.port}</span>
                </div>
                <Button size="xs" disabled={peers.joiningFp === c.pubkey_fp} onclick={() => add(c)}>
                  {peers.joiningFp === c.pubkey_fp ? 'Adding…' : '+ Add'}
                </Button>
              </li>
            {/each}
          </ul>
          <div class="divider"></div>
        {/if}
        <button type="button" class="menu-item" onclick={invite} role="menuitem">
          <span class="menu-item-title">Add by address…</span>
          <span class="menu-item-sub">Enter host:port manually</span>
        </button>
        <button type="button" class="menu-item" onclick={accept} role="menuitem">
          <span class="menu-item-title">Accept pairing code…</span>
          <span class="menu-item-sub">Paste a code from another host</span>
        </button>
      </div>
    {/snippet}
  </Popover>
</div>

<style>
  .anchor { position: relative; }
  .trigger {
    display: inline-flex;
    align-items: center;
    gap: var(--space-2);
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: var(--radius-sm);
    color: var(--color-text);
    padding: var(--space-2) var(--space-3);
    cursor: pointer;
    font: inherit;
    font-size: var(--text-sm);
  }
  .trigger:hover { background: var(--color-surface-2); }
  .count { display: inline-flex; }
  .menu {
    padding: var(--space-2);
    display: flex;
    flex-direction: column;
    gap: var(--space-1);
  }
  .section-label {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--color-text-dim);
    padding: var(--space-1) var(--space-2);
  }
  .discovered { list-style: none; margin: 0; padding: 0; display: flex; flex-direction: column; gap: var(--space-1); }
  .row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-2);
    padding: var(--space-1) var(--space-2);
    border-radius: var(--radius-sm);
  }
  .row:hover { background: var(--color-surface-2); }
  .info { display: flex; flex-direction: column; min-width: 0; }
  .name { font-size: var(--text-sm); color: var(--color-text); white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
  .sub { font-size: var(--text-xs); color: var(--color-text-dim); font-family: var(--font-mono); }
  .divider { height: 1px; background: var(--color-border); margin: var(--space-1) 0; }
  .menu-item {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 2px;
    background: transparent;
    border: none;
    text-align: left;
    padding: var(--space-2);
    border-radius: var(--radius-sm);
    cursor: pointer;
    font: inherit;
    color: var(--color-text);
  }
  .menu-item:hover { background: var(--color-surface-2); }
  .menu-item-title { font-size: var(--text-sm); }
  .menu-item-sub { font-size: var(--text-xs); color: var(--color-text-dim); }
</style>
