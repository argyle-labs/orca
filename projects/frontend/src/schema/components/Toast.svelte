<script lang="ts">
  import { onMount } from 'svelte';

  interface Props {
    timeout?: number;
  }
  let { timeout = 3000 }: Props = $props();

  let visible = $state(true);

  onMount(() => {
    const t = setTimeout(() => (visible = false), timeout);
    return () => clearTimeout(t);
  });
</script>

<div id="toast" class={visible ? 'show' : ''}>
  Scroll to zoom &middot; Drag to pan &middot; Click for details &middot; Double-click to focus
</div>
