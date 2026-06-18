<script lang="ts">
  import { paletteLoader } from '$lib/stores/paletteLoader.svelte';
</script>

{#if paletteLoader.component}
  {@const C = paletteLoader.component}
  <C />
{/if}
