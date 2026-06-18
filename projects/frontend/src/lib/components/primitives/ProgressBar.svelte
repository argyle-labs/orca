<script lang="ts">
  interface Props {
    value: number;
    warn?: number;
    crit?: number;
  }
  let { value, warn = 70, crit = 90 }: Props = $props();
  let pct = $derived(Math.min(100, Math.max(0, value)));
</script>

<div class="bar-wrap">
  <div
    class="bar"
    class:warn={pct > warn && pct <= crit}
    class:crit={pct > crit}
    style="width:{pct}%"
  ></div>
</div>

<style>
  .bar-wrap {
    width: 100%;
    height: 5px;
    background: var(--color-bg);
    border: 1px solid var(--color-border);
    border-radius: 3px;
    overflow: hidden;
  }
  .bar {
    height: 100%;
    background: var(--color-accent, #4f86f7);
    border-radius: 3px;
    transition: width 0.3s ease;
  }
  .bar.warn { background: #e6a817; }
  .bar.crit { background: var(--color-error); }
</style>
