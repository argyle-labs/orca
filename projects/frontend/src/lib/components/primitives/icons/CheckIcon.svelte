<script lang="ts">
  interface Props {
    size?: number;
    strokeWidth?: number;
  }
  let { size = 12, strokeWidth = 2 }: Props = $props();
</script>

<svg width={size} height={size} viewBox="0 0 12 12" fill="none" stroke="currentColor" stroke-width={strokeWidth} aria-hidden="true">
  <path d="M2 6.5l2.5 2.5L10 3" />
</svg>
