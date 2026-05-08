<script lang="ts">
  import { untrack } from 'svelte';
  import Button from './Button.svelte';

  let {
    service,
    fields,
    onSave,
  }: {
    service: string;
    fields: { key: string; label: string; placeholder?: string; hint?: string; required?: boolean }[];
    onSave: (config: Record<string, string>) => Promise<void>;
  } = $props();

  let values = $state<Record<string, string>>(
    untrack(() => Object.fromEntries(fields.map(f => [f.key, ''])))
  );
  let saving = $state(false);
  let error = $state('');

  async function save() {
    saving = true;
    error = '';
    try {
      await onSave(values);
    } catch (e: any) {
      error = e.message ?? 'Failed to save';
    } finally {
      saving = false;
    }
  }

  let canSave = $derived(
    fields.filter(f => f.required).every(f => values[f.key]?.trim())
  );
</script>

<div class="setup">
  <div class="setup-icon">⚙</div>
  <h2 class="setup-title">Configure {service}</h2>
  <p class="setup-desc">Set up your workspace connection. You can also configure this via MCP or the Orca API.</p>

  <div class="fields">
    {#each fields as field (field.key)}
      <div class="field">
        <label for="ws-{field.key}">{field.label}{field.required ? ' *' : ''}</label>
        <input
          id="ws-{field.key}"
          type="text"
          placeholder={field.placeholder ?? ''}
          bind:value={values[field.key]}
        />
        {#if field.hint}<p class="hint">{field.hint}</p>{/if}
      </div>
    {/each}
  </div>

  {#if error}<p class="error">{error}</p>{/if}

  <div class="actions">
    <Button variant="primary" onclick={save} disabled={saving || !canSave}>
      {saving ? 'Saving…' : 'Save'}
    </Button>
    <p class="api-hint">
      Or via API: <code>PUT /api/plugins/rebuy/data/{service.toLowerCase()}_config</code>
    </p>
  </div>
</div>

<style>
  .setup {
    max-width: 480px; margin: var(--space-12) auto; padding: var(--space-8);
    background: var(--color-surface); border: 1px solid var(--color-border);
    border-radius: var(--radius-lg); display: flex; flex-direction: column; gap: var(--space-4);
  }
  .setup-icon { font-size: 2rem; }
  .setup-title { margin: 0; font-size: var(--text-xl); }
  .setup-desc { margin: 0; color: var(--color-text-dim); font-size: var(--text-sm); }
  .fields { display: flex; flex-direction: column; gap: var(--space-3); }
  .field { display: flex; flex-direction: column; gap: var(--space-1); }
  label { font-size: var(--text-sm); font-weight: var(--weight-medium); color: var(--color-text-muted); }
  input {
    background: var(--color-surface-2); border: 1px solid var(--color-border);
    border-radius: var(--radius-sm); color: var(--color-text);
    font-size: var(--text-sm); padding: var(--space-2) var(--space-3); outline: none;
  }
  input:focus { border-color: var(--color-accent); }
  .hint { margin: 0; font-size: var(--text-xs); color: var(--color-text-dim); }
  .error { color: var(--color-error); font-size: var(--text-sm); margin: 0; }
  .actions { display: flex; flex-direction: column; gap: var(--space-2); }
  .api-hint { margin: 0; font-size: var(--text-xs); color: var(--color-text-dim); }
  .api-hint code { font-family: var(--font-mono); color: var(--color-text-muted); }
</style>
