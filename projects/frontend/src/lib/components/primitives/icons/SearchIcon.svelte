<script lang="ts">
  interface Props {
    size?: number;
    strokeWidth?: number;
  }
  let { size = 13, strokeWidth = 1.8 }: Props = $props();
</script>

<svg width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width={strokeWidth} aria-hidden="true">
  <circle cx="6.5" cy="6.5" r="4.5" />
  <line x1="10.5" y1="10.5" x2="14" y2="14" />
</svg>
