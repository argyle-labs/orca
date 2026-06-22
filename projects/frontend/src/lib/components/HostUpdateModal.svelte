<script lang="ts">
  import Modal from '$lib/components/primitives/Modal.svelte';
  import HostUpdatePanel from '$lib/components/HostUpdatePanel.svelte';
  import type { PodInstance } from '$lib/client/types.gen';

  interface Props {
    open: boolean;
    inst: PodInstance;
    onclose: () => void;
  }
  let { open, inst, onclose }: Props = $props();
</script>

<Modal
  {open}
  title={`Update ${inst.system?.hostname ?? inst.label}`}
  size="md"
  {onclose}
>
  <HostUpdatePanel {inst} />
</Modal>
