<script lang="ts">
  interface Props {
    onzoomin: () => void;
    onzoomout: () => void;
    onfitall: () => void;
  }
  let { onzoomin, onzoomout, onfitall }: Props = $props();
</script>

<div id="zoom-controls">
  <button title="Zoom in" onclick={onzoomin}>+</button>
  <button title="Zoom out" onclick={onzoomout}>&minus;</button>
  <button title="Fit all" onclick={onfitall}>&#x25A3;</button>
</div>
