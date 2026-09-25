import type { PeripheralInfo } from '@/bindings'
import { commands, events } from '@/bindings'
import { computed, ref } from 'vue'

const STORAGE_KEY = 'powerflow_cached_peripherals'

function getCachedPeripherals(): PeripheralInfo[] {
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (raw) {
      const parsed = JSON.parse(raw)
      if (Array.isArray(parsed))
        return parsed
    }
  }
  catch (_) {}
  return []
}

function saveCachedPeripherals(list: PeripheralInfo[]) {
  try {
    localStorage.setItem(STORAGE_KEY, JSON.stringify(list))
  }
  catch (_) {}
}

function calculatePopoverHeight(count: number): number {
  if (count === 0)
    return 224
  if (count <= 2)
    return 285
  if (count <= 4)
    return 340
  if (count <= 6)
    return 395
  return 450
}

const initialList = getCachedPeripherals()
const peripherals = ref<PeripheralInfo[]>(initialList)
const isLoading = ref(initialList.length === 0)
let prevCount = initialList.length

function updatePeripherals(list: PeripheralInfo[]) {
  const newCount = list.length
  peripherals.value = list
  isLoading.value = false
  saveCachedPeripherals(list)

  if (newCount !== prevCount) {
    prevCount = newCount
    commands.setPopoverHeight(calculatePopoverHeight(newCount))
  }
}

// Fetch initial list immediately
commands.getPeripherals()
  .then((list) => {
    updatePeripherals(list)
  })
  .catch((e) => {
    console.error('Failed to get peripherals:', e)
    isLoading.value = false
  })

// Listen for updates from backend
events.peripheralUpdatedEvent.listen(({ payload }) => {
  updatePeripherals(payload.peripherals)
})

export function usePeripherals() {
  const refresh = async () => {
    try {
      const fresh = await commands.refreshPeripherals()
      updatePeripherals(fresh)
    }
    catch (e) {
      console.error('Failed to refresh peripherals:', e)
    }
  }

  return {
    peripherals,
    isLoading,
    refresh,
    hasPeripherals: computed(() => peripherals.value.length > 0),
  }
}
