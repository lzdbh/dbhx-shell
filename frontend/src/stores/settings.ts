import { ref, watch } from 'vue'

import { defineStore } from 'pinia'

/**
 * 本地偏好。
 *
 * 只放「前端自己要用、后端不关心」的东西，所以走 localStorage 而不是 vault：
 * vault 是加密的、跨设备会同步语义的凭据存储，字号和下载目录不属于那里。
 */
const KEY = 'dbhx:settings'

/** 字号区间与默认值。缩放（Ctrl+滚轮）和设置页的输入框共用，两边才不会各说各话。 */
export const DEFAULT_FONT_SIZE = 13
export const MIN_FONT_SIZE = 10
export const MAX_FONT_SIZE = 28

export const useSettingsStore = defineStore('settings', () => {
  /** 下载落地目录。空表示还没配——此时下载按钮会给出明确提示而不是弹系统对话框。 */
  const downloadDir = ref('')
  const fontSize = ref(DEFAULT_FONT_SIZE)
  /** 面板宽度（px）。拖过一次就记住，省得每次开应用重新调。 */
  const panelWidth = ref(360)

  /**
   * 按 `step` 档缩放字号，越界就停在边界上。
   *
   * 缩放写回的是同一个 `fontSize`：它已经是全局偏好，设置页、缩放、重开应用
   * 三条路必须指向同一个值，否则「缩完再打开设置页」会看到另一个数。
   */
  function zoom(step: number): void {
    const next = Math.round(fontSize.value + step)
    fontSize.value = Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, next))
  }

  function resetZoom(): void {
    fontSize.value = DEFAULT_FONT_SIZE
  }

  /** 状态栏上显示的缩放百分比，以默认字号为 100%。 */
  function zoomPercent(): number {
    return Math.round(fontSize.value / DEFAULT_FONT_SIZE * 100)
  }

  function load(): void {
    const raw = localStorage.getItem(KEY)
    if (!raw) return
    try {
      const saved = JSON.parse(raw) as Partial<{ downloadDir: string, fontSize: number, panelWidth: number }>
      if (typeof saved.downloadDir === 'string') downloadDir.value = saved.downloadDir
      // 字号夹回区间：手工改过的 localStorage 不该让终端一开屏就是 200px
      if (typeof saved.fontSize === 'number') {
        fontSize.value = Math.min(MAX_FONT_SIZE, Math.max(MIN_FONT_SIZE, Math.round(saved.fontSize)))
      }
      if (typeof saved.panelWidth === 'number') panelWidth.value = saved.panelWidth
    } catch {
      // 坏数据只影响偏好，不值得为它白屏；下面的 watch 会用合法值覆盖回去
    }
  }

  load()
  watch([downloadDir, fontSize, panelWidth], () => {
    localStorage.setItem(KEY, JSON.stringify({ downloadDir: downloadDir.value, fontSize: fontSize.value, panelWidth: panelWidth.value }))
  })

  return { downloadDir, fontSize, panelWidth, zoom, resetZoom, zoomPercent }
})
