import { toggleCommandPalette } from '$lib/stores/commandPalette.svelte';

type CommandPaletteModule = typeof import('$lib/components/CommandPalette.svelte');
type CommandPaletteComponent = CommandPaletteModule['default'];

// Lazy module-level cache for the CommandPalette component. The first open()
// call resolves the dynamic import; subsequent opens reuse the cached module.
// Keeps the heavy SDK + palette code off the first-paint critical path.
class PaletteLoader {
  component = $state<CommandPaletteComponent | null>(null);
  private loading = false;

  async open() {
    if (!this.component && !this.loading) {
      this.loading = true;
      try {
        const m = await import('$lib/components/CommandPalette.svelte');
        this.component = m.default;
      } finally {
        this.loading = false;
      }
    }
    toggleCommandPalette();
  }
}

export const paletteLoader = new PaletteLoader();
