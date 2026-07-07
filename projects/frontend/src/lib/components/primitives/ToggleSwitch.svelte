<script lang="ts">
  interface Props {
    checked: boolean;
    onchange: () => void;
    disabled?: boolean;
    ariaLabel?: string;
  }
  let { checked, onchange, disabled = false, ariaLabel }: Props = $props();
</script>

<button
  type="button"
  class="toggle-switch"
  class:on={checked}
  disabled={disabled}
  onclick={onchange}
  aria-label={ariaLabel}
  role="switch"
  aria-checked={checked}
><span class="toggle-thumb"></span></button>

<style>
  .toggle-switch {
    position: relative;
    display: inline-block;
    width: 36px;
    height: 20px;
    background: var(--color-surface-2);
    border: 1px solid var(--color-border);
    border-radius: 999px;
    cursor: pointer;
    padding: 0;
    transition: background 0.15s;
  }
  .toggle-switch:hover:not(:disabled) { border-color: var(--color-text-dim); }
  .toggle-switch:focus-visible {
    outline: 2px solid var(--color-accent);
    outline-offset: 2px;
  }
  .toggle-switch:disabled { opacity: 0.5; cursor: not-allowed; }
  .toggle-switch.on {
    background: var(--color-accent);
    border-color: var(--color-accent);
  }
  .toggle-thumb {
    position: absolute;
    top: 1px;
    left: 1px;
    width: 16px;
    height: 16px;
    background: white;
    border-radius: 50%;
    transition: transform 0.15s;
  }
  .toggle-switch.on .toggle-thumb {
    transform: translateX(16px);
  }
</style>
