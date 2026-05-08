<script lang="ts">
  let { ok }: { ok: boolean | null } = $props();
</script>

<span class="dot {ok === true ? 'ok' : ok === false ? 'fail' : 'unknown'}">●</span>

<style>
  .dot        { font-size: 0.6rem; flex-shrink: 0; line-height: 1; }
  .ok         { color: var(--color-success); }
  .fail       { color: var(--color-error); }
  .unknown    { color: var(--color-text-faint); }
</style>
