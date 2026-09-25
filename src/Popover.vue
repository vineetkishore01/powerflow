<script setup lang="ts">
import { commands } from '@/bindings'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { onMounted, onUnmounted, ref } from 'vue'

useSetup()

const containerRef = ref<HTMLElement | null>(null)
let resizeObserver: ResizeObserver | null = null
let unlisten: UnlistenFn | null = null

function syncHeight() {
  if (!containerRef.value)
    return
  const rect = containerRef.value.getBoundingClientRect()
  const targetHeight = Math.max(224, Math.min(Math.ceil(rect.height) + 12, 520))
  commands.setPopoverHeight(targetHeight)
}

onMounted(async () => {
  if (containerRef.value) {
    resizeObserver = new ResizeObserver(() => {
      syncHeight()
    })
    resizeObserver.observe(containerRef.value)
  }

  unlisten = await listen('popover-opened', () => {
    syncHeight()
  })
})

onUnmounted(() => {
  resizeObserver?.disconnect()
  unlisten?.()
})
</script>

<template>
  <div ref="containerRef" class="w-full">
    <PowerStatusPopover>
      <PowerStatus />
    </PowerStatusPopover>
  </div>
</template>

<style>
body {
  background: transparent;
  overflow: hidden;
  margin: 0;
  padding: 0;
}
</style>

