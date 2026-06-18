<script lang="ts">
  interface Props {
    size?: number;
    strokeWidth?: number;
  }
  let { size = 10, strokeWidth = 1.5 }: Props = $props();
</script>

<svg width={size} height={size} viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width={strokeWidth} aria-hidden="true">
  <path d="M2 4l3 3 3-3" />
</svg>
