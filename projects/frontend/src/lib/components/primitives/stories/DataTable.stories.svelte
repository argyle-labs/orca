<script module lang="ts">
  import { defineMeta } from '@storybook/addon-svelte-csf';
  import DataTable from '../DataTable.svelte';
  import Badge from '../Badge.svelte';

  const { Story } = defineMeta({
    title: 'Primitives/DataTable',
    component: DataTable,
    tags: ['autodocs'],
  });

  type PeerRow = { hostname: string; addr: string; health: 'up' | 'down' | 'unknown'; version: string };
  const peers: PeerRow[] = [
    { hostname: 'thor', addr: '10.10.10.20', health: 'up', version: '0.3.0-rc.23' },
    { hostname: 'frigg', addr: '10.10.10.21', health: 'up', version: '0.3.0-rc.23' },
    { hostname: 'freyr', addr: '10.10.10.30', health: 'down', version: '0.3.0-rc.22' },
    { hostname: 'baldur', addr: '10.10.10.31', health: 'unknown', version: '—' },
  ];
  const columns = [
    { label: 'Host', width: '120px' },
    { label: 'Address', width: '140px' },
    { label: 'Health' },
    { label: 'Version' },
  ];
</script>

<Story name="Pod members">
  {#snippet template()}
    <div style="width:560px;">
      <DataTable {columns} rows={peers}>
        {#snippet row(p)}
          <td>{p.hostname}</td>
          <td><code style="font-size:0.8rem;">{p.addr}</code></td>
          <td>
            <Badge color={p.health === 'up' ? 'green' : p.health === 'down' ? 'red' : 'gray'}>
              {p.health}
            </Badge>
          </td>
          <td><code style="font-size:0.8rem;">{p.version}</code></td>
        {/snippet}
      </DataTable>
    </div>
  {/snippet}
</Story>

<Story name="Loading">
  {#snippet template()}
    <div style="width:560px;">
      <DataTable {columns} rows={[] as PeerRow[]} loading>
        {#snippet row(_p)}<td></td>{/snippet}
      </DataTable>
    </div>
  {/snippet}
</Story>

<Story name="Empty">
  {#snippet template()}
    <div style="width:560px;">
      <DataTable {columns} rows={[] as PeerRow[]} emptyText="No peers discovered">
        {#snippet row(_p)}<td></td>{/snippet}
      </DataTable>
    </div>
  {/snippet}
</Story>
