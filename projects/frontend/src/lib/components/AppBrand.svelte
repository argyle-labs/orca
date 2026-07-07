<script lang="ts">
  interface Props {
    href?: string;
    label?: string;
  }
  let { href = '/', label = 'orca' }: Props = $props();
</script>

<a class="brand" {href}>{label}</a>

<style>
  .brand {
    font-weight: var(--weight-semibold);
    letter-spacing: 0.04em;
    color: var(--color-text);
    text-decoration: none;
    margin-left: var(--space-2);
  }
</style>
