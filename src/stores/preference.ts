import type { StatusBarItem, Theme } from '@/bindings'
import { defineStore } from 'pinia'
import { ref } from 'vue'

export const usePreference = defineStore('preference', () => {
  const theme = ref<Theme>('system')
  const animationsEnabled = ref(true)
  const updateInterval = ref(1500)
  const language = ref('en')
  const statusBarItem = ref<StatusBarItem>('system')
  const statusBarShowCharging = ref(true)
  const discoverIosOverBluetooth = ref(false)

  return {
    theme,
    animationsEnabled,
    updateInterval,
    language,
    statusBarItem,
    statusBarShowCharging,
    discoverIosOverBluetooth,
  }
}, {
  tauri: {
    saveOnChange: true,
    saveStrategy: 'debounce',
    saveInterval: 1000,
  },
})

const VALID_STATUS_BAR_ITEMS: StatusBarItem[] = ['system', 'screen', 'heatpipe']
const MIN_INTERVAL = 500
const MAX_INTERVAL = 60_000

export function usePreferenceAsync() {
  const preference = usePreference()
  const isLoading = ref(true)
  preference.$tauri.start().then(() => {
    if (!VALID_STATUS_BAR_ITEMS.includes(preference.statusBarItem)) {
      console.warn('[preference] invalid statusBarItem', preference.statusBarItem, 'reset to system')
      preference.statusBarItem = 'system'
    }
    if (preference.updateInterval < MIN_INTERVAL || preference.updateInterval > MAX_INTERVAL) {
      console.warn('[preference] invalid updateInterval', preference.updateInterval, 'reset to 1500')
      preference.updateInterval = 1500
    }
    isLoading.value = false
  })
  return { preference, isLoading }
}
