<script lang="ts">
  interface Props {
    groups: Group[];
  }
  let { groups }: Props = $props();
</script>

{#each groups as group (group.key)}
  {@const isGrouped = group.subs.length > 1 || !!group.subs[0].domain?.group}
  {#if isGrouped}
    <div
      class="group-bg"
      style="left: {group.x}px; top: {group.y}px; width: {group.w}px; height: {group.h}px; border-color: {group.color}25; background: {group.color}06"
    >
      <div class="group-label" style="color: {group.color}">{group.label}</div>
    </div>
  {/if}
  {#each group.subs as sub (sub.domain.key)}
    <div
      class="domain-bg"
      style="left: {sub.x}px; top: {sub.y}px; width: {sub.w}px; height: {sub.h}px; border-color: {sub.domain.color}30; background: {sub.domain.color}08"
    >
      <div class="domain-label" style="color: {sub.domain.color}">{sub.domain.label}</div>
    </div>
  {/each}
{/each}
