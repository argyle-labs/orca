<script lang="ts">
  interface Props {
    size?: number;
    strokeWidth?: number;
  }
  let { size = 14, strokeWidth = 1.6 }: Props = $props();
</script>

<svg width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width={strokeWidth} aria-hidden="true">
  <rect x="1" y="1" width="14" height="14" rx="1.5" />
  <line x1="5" y1="1" x2="5" y2="15" />
</svg>
