<script lang="ts">
  import { goto } from '$app/navigation';
  import type { MeOk } from '$lib/client/types.gen';
  import { signOut } from '$lib/stores/session.svelte';

  let { user }: { user: MeOk } = $props();

  let open = $state(false);
  let root: HTMLDivElement | undefined = $state();

  function handleDocClick(e: MouseEvent) {
    if (!open || !root) return;
    if (!root.contains(e.target as Node)) open = false;
  }

  $effect(() => {
    document.addEventListener('click', handleDocClick);
    return () => document.removeEventListener('click', handleDocClick);
  });

  async function onChangePassword() {
    open = false;
    await goto('/account/password');
  }

  async function onSignOut() {
    open = false;
    await signOut();
  }
</script>

<div class="user-menu" bind:this={root}>
  <button
    class="user-btn"
    onclick={() => (open = !open)}
    aria-haspopup="menu"
    aria-expanded={open}
    title="Account"
  >
    {user.username}
    <span class="role" aria-hidden="true">{user.role}</span>
    <span class="caret" aria-hidden="true">▾</span>
  </button>

  {#if open}
    <div class="dropdown" role="menu">
      <div class="meta">
        <div class="username">{user.username}</div>
        <div class="role-line">{user.role}</div>
      </div>
      <button class="item" role="menuitem" onclick={onChangePassword}>
        Change password
      </button>
      <button class="item" role="menuitem" onclick={onSignOut}>Sign out</button>
    </div>
  {/if}
</div>

<style>
  .user-menu {
    position: relative;
  }
  .user-btn {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    height: 26px;
    padding: 0 8px;
    background: transparent;
    color: var(--color-text-muted);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    cursor: pointer;
    font-size: var(--text-xs);
  }
  .user-btn:hover {
    background: var(--color-surface-2);
    color: var(--color-text);
  }
  .role {
    color: var(--color-text-dim);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
  }
  .caret {
    color: var(--color-text-dim);
  }

  .dropdown {
    position: absolute;
    right: 0;
    top: calc(100% + 4px);
    min-width: 180px;
    background: var(--color-surface);
    border: 1px solid var(--color-border);
    border-radius: 4px;
    box-shadow: 0 8px 24px rgba(0, 0, 0, 0.2);
    padding: 4px;
    z-index: 100;
  }
  .meta {
    padding: 6px 8px;
    border-bottom: 1px solid var(--color-border);
    margin-bottom: 4px;
  }
  .username {
    font-size: var(--text-xs);
    font-weight: var(--weight-semibold);
    color: var(--color-text);
  }
  .role-line {
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--color-text-dim);
  }
  .item {
    display: block;
    width: 100%;
    text-align: left;
    background: transparent;
    border: none;
    padding: 6px 8px;
    color: var(--color-text);
    font-size: var(--text-xs);
    cursor: pointer;
    border-radius: 3px;
  }
  .item:hover {
    background: var(--color-surface-2);
  }
</style>
