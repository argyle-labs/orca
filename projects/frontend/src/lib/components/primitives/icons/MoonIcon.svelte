<script lang="ts">
  interface Props {
    size?: number;
    strokeWidth?: number;
  }
  let { size = 12, strokeWidth = 1.6 }: Props = $props();
</script>

<svg width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width={strokeWidth} aria-hidden="true">
  <path d="M13.5 10.5A6 6 0 0 1 5.5 2.5a6 6 0 1 0 8 8z" />
</svg>
