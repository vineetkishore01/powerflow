<script setup lang="ts">
import { usePeripherals } from '@/composables/usePeripherals'
import { RotateCw } from 'lucide-vue-next'
import { ref } from 'vue'
import PeripheralPill from './PeripheralPill.vue'

const { peripherals, hasPeripherals, refresh, isLoading } = usePeripherals()

const isRefreshing = ref(false)

async function handleRefresh(e: MouseEvent) {
  e.stopPropagation()
  isRefreshing.value = true
  await refresh()
  setTimeout(() => {
    isRefreshing.value = false
  }, 500)
}
</script>

<template>
  <div v-if="hasPeripherals" class="flex flex-col gap-2 pt-2 border-t border-border/40 mt-2">
    <div class="flex items-center justify-between px-0.5">
      <span class="text-[11px] font-medium text-muted-foreground uppercase tracking-wider flex items-center gap-1.5">
        Devices
        <span class="inline-flex items-center justify-center px-1.5 py-0.2 text-[10px] font-mono rounded-full bg-muted text-muted-foreground">
          {{ peripherals.length }}
        </span>
      </span>
      <button
        type="button"
        class="text-muted-foreground hover:text-foreground transition-colors p-1 rounded-sm hover:bg-muted/60"
        title="Refresh Devices"
        @click="handleRefresh"
      >
        <RotateCw class="size-3" :class="{ 'animate-spin': isRefreshing || isLoading }" />
      </button>
    </div>

    <!-- Horizontal Wrap of Compact Pills -->
    <div class="flex flex-wrap items-center gap-1.5 max-h-[240px] overflow-y-auto pr-0.5 pb-1">
      <PeripheralPill
        v-for="device in peripherals"
        :key="device.id"
        :peripheral="device"
      />
    </div>
  </div>
</template>
