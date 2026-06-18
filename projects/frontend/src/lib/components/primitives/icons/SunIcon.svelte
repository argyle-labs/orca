<script lang="ts">
  interface Props {
    size?: number;
    strokeWidth?: number;
  }
  let { size = 12, strokeWidth = 1.6 }: Props = $props();
</script>

<svg width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width={strokeWidth} aria-hidden="true">
  <circle cx="8" cy="8" r="3.2" />
  <line x1="8" y1="1.5" x2="8" y2="3" />
  <line x1="8" y1="13" x2="8" y2="14.5" />
  <line x1="1.5" y1="8" x2="3" y2="8" />
  <line x1="13" y1="8" x2="14.5" y2="8" />
</svg>
