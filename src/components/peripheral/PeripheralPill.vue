<script setup lang="ts">
import type { PeripheralInfo } from '@/bindings'
import {
  Bluetooth,
  Gamepad2,
  Headphones,
  Keyboard,
  Mouse,
  Smartphone,
  Tablet,
  Watch,
  Zap,
} from 'lucide-vue-next'
import { computed } from 'vue'

const props = defineProps<{
  peripheral: PeripheralInfo
}>()

const iconComponent = computed(() => {
  switch (props.peripheral.peripheralType) {
    case 'phone':
      return Smartphone
    case 'tablet':
      return Tablet
    case 'watch':
      return Watch
    case 'headset':
      return Headphones
    case 'mouse':
      return Mouse
    case 'keyboard':
      return Keyboard
    case 'gamepad':
      return Gamepad2
    default:
      return Bluetooth
  }
})

const batteryColorClass = computed(() => {
  const level = props.peripheral.batteryLevel
  if (level <= 10)
    return 'text-red-500 font-bold'
  if (level <= 20)
    return 'text-amber-500 font-medium'
  return 'text-foreground font-medium'
})

const batteryBgClass = computed(() => {
  const level = props.peripheral.batteryLevel
  if (level <= 10)
    return 'bg-red-500/10 border-red-500/30'
  if (level <= 20)
    return 'bg-amber-500/10 border-amber-500/30'
  return 'bg-muted/70 hover:bg-muted border-border/40'
})
</script>

<template>
  <div
    class="inline-flex items-center gap-1.5 px-2.5 py-1 rounded-full border text-xs shadow-2xs transition-colors select-none cursor-default"
    :class="batteryBgClass"
    :title="`${peripheral.name} (${peripheral.batteryLevel}%${peripheral.isCharging ? ', Charging' : ''})${peripheral.via ? ' via ' + peripheral.via : ''}`"
  >
    <component :is="iconComponent" class="size-3.5 text-muted-foreground shrink-0" />
    <span class="max-w-[110px] truncate font-sans text-[11px] leading-tight text-secondary-foreground">
      {{ peripheral.name }}
    </span>

    <!-- Multi-cell (e.g. AirPods Left / Right / Case) -->
    <template v-if="peripheral.cells && peripheral.cells.length > 0">
      <div class="flex items-center gap-1 font-mono text-[10px] tabular-nums">
        <span
          v-for="cell in peripheral.cells"
          :key="cell.name"
          class="flex items-center gap-0.5"
          :class="cell.level <= 20 ? 'text-amber-500' : 'text-foreground'"
        >
          <span class="text-muted-foreground text-[9px]">{{ cell.name }}</span>
          <span>{{ cell.level }}%</span>
        </span>
      </div>
    </template>

    <!-- Single cell level -->
    <template v-else>
      <span class="font-mono text-[11px] tabular-nums" :class="batteryColorClass">
        {{ peripheral.batteryLevel }}%
      </span>
    </template>

    <!-- Charging Zap -->
    <Zap v-if="peripheral.isCharging" class="size-3 text-blue-500 fill-blue-500 shrink-0 animate-pulse" />
  </div>
</template>
