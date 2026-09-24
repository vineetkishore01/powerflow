import type { PeripheralInfo } from '@/bindings'
import { commands, events } from '@/bindings'
import { computed, ref } from 'vue'

const peripherals = ref<PeripheralInfo[]>([])
const isLoading = ref(true)

// Fetch initial list immediately
commands.getPeripherals()
  .then((list) => {
    peripherals.value = list
    isLoading.value = false
  })
  .catch((e) => {
    console.error('Failed to get peripherals:', e)
    isLoading.value = false
  })

// Listen for updates from backend
events.peripheralUpdatedEvent.listen(({ payload }) => {
  peripherals.value = payload.peripherals
  isLoading.value = false
})

export function usePeripherals() {
  const refresh = async () => {
    try {
      peripherals.value = await commands.refreshPeripherals()
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
