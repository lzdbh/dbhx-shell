import { ref, watch } from 'vue'

import { defineStore } from 'pinia'

/**
 * 本地偏好。
 *
 * 只放「前端自己要用、后端不关心」的东西，所以走 localStorage 而不是 vault：
 * vault 是加密的、跨设备会同步语义的凭据存储，字号和下载目录不属于那里。
 */
const KEY = 'dbhx:settings'

export const useSettingsStore = defineStore('settings', () => {
  /** 下载落地目录。空表示还没配——此时下载按钮会给出明确提示而不是弹系统对话框。 */
  const downloadDir = ref('')
  const fontSize = ref(13)
  /** 面板宽度（px）。拖过一次就记住，省得每次开应用重新调。 */
  const panelWidth = ref(360)

  function load(): void {
    const raw = localStorage.getItem(KEY)
    if (!raw) return
    try {
      const saved = JSON.parse(raw) as Partial<{ downloadDir: string, fontSize: number, panelWidth: number }>
      if (typeof saved.downloadDir === 'string') downloadDir.value = saved.downloadDir
      if (typeof saved.fontSize === 'number') fontSize.value = saved.fontSize
      if (typeof saved.panelWidth === 'number') panelWidth.value = saved.panelWidth
    } catch {
      // 坏数据只影响偏好，不值得为它白屏；下面的 watch 会用合法值覆盖回去
    }
  }

  load()
  watch([downloadDir, fontSize, panelWidth], () => {
    localStorage.setItem(KEY, JSON.stringify({ downloadDir: downloadDir.value, fontSize: fontSize.value, panelWidth: panelWidth.value }))
  })

  return { downloadDir, fontSize, panelWidth }
})
