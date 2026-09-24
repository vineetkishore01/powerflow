<script setup lang="ts">
const target = useTemplateRef<HTMLElement>('target')

provide('isPopover', true)
</script>

<template>
  <div ref="target" class="w-full">
    <slot is-popover />
  </div>
</template>
