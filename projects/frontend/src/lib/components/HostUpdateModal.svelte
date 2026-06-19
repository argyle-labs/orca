<script lang="ts">
  import Modal from '$lib/components/primitives/Modal.svelte';
  import HostUpdatePanel from '$lib/components/HostUpdatePanel.svelte';
  import type { Instance } from '$lib/types/instance';

  interface Props {
    open: boolean;
    inst: Instance;
    onclose: () => void;
  }
  let { open, inst, onclose }: Props = $props();
</script>

<Modal
  {open}
  title={`Update ${inst.sys?.hostname ?? inst.label}`}
  size="md"
  {onclose}
>
  <HostUpdatePanel {inst} />
</Modal>
