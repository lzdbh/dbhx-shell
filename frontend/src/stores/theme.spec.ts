import { createPinia, setActivePinia } from 'pinia'
import { nextTick } from 'vue'
import { beforeEach, describe, expect, it } from 'vitest'

import { DARK, LIGHT } from '../styles/palette'
import { applyStoredTheme, useThemeStore } from './theme'

const KEY = 'dbhx:theme'

/** 主窗口改了主题：`storage` 事件只在别的文档里触发，所以要手工模拟那一下。 */
function emitStored(dark: boolean, colorPrimary: string): void {
  localStorage.setItem(KEY, JSON.stringify({ dark, colorPrimary }))
  window.dispatchEvent(new StorageEvent('storage', { key: KEY, newValue: localStorage.getItem(KEY) }))
}

beforeEach(() => {
  localStorage.clear()
  setActivePinia(createPinia())
})

describe('theme store', () => {
  it('没存过时按深色系起步', () => {
    const theme = useThemeStore()
    expect(theme.dark).toBe(true)
    expect(theme.colorPrimary).toBe('#1677ff')
  })

  it('切换即落盘，晚挂载的窗口读到同一份', async () => {
    const first = useThemeStore()
    first.toggleDark()
    first.colorPrimary = '#ff7a45'
    // 落盘走 watch，默认是异步的：不等一下 nextTick 读到的就是磁盘上的旧值
    await nextTick()
    expect(JSON.parse(localStorage.getItem(KEY) ?? '{}')).toEqual({ dark: false, colorPrimary: '#ff7a45' })

    // 文件管理窗口是独立 webview、独立 Pinia：它挂载时必须从磁盘上拿回主窗口的选择，
    // 否则它永远是默认深色
    setActivePinia(createPinia())
    const second = useThemeStore()
    expect(second.dark).toBe(false)
    expect(second.colorPrimary).toBe('#ff7a45')
  })

  it('已经开着的窗口跟着另一个窗口的切换走', () => {
    const theme = useThemeStore()
    emitStored(false, '#13c2c2')
    expect(theme.dark).toBe(false)
    expect(theme.colorPrimary).toBe('#13c2c2')
  })

  it('坏数据不会把界面带崩，也照样能被真实切换覆盖', async () => {
    localStorage.setItem(KEY, '{not json')
    setActivePinia(createPinia())
    const theme = useThemeStore()
    expect(theme.dark).toBe(true)

    theme.toggleDark()
    await nextTick()
    expect(JSON.parse(localStorage.getItem(KEY) ?? '{}').dark).toBe(false)
  })

  it('终端调色板与 dark 同源派生', () => {
    const theme = useThemeStore()
    expect(theme.terminalTheme.id).toBe('dbhx-dark')
    theme.toggleDark()
    expect(theme.terminalTheme.id).toBe('dbhx-light')
  })

  it('变量写在 <html> 上，界面侧只读变量', async () => {
    const root = document.documentElement
    // 挂载前那一步：main.ts 就是叫这个函数，晚一步第一帧是白底
    applyStoredTheme()
    expect(root.dataset.theme).toBe('dark')
    expect(root.style.getPropertyValue('--bg-app')).toBe(DARK.bgApp)
    expect(root.style.getPropertyValue('--pane-bg')).toBe(DARK.paneBg)

    const theme = useThemeStore()
    theme.toggleDark()
    await nextTick()
    expect(root.dataset.theme).toBe('light')
    expect(root.style.getPropertyValue('--bg-app')).toBe(LIGHT.bgApp)
    // 终端格子与界面用的是同一张表，不然会看见一圈色差缝
    expect(theme.terminalTheme.background).toBe(LIGHT.paneBg)
  })

  it('主色换了，淡底跟着现算，不是留着一个蓝', async () => {
    const theme = useThemeStore()
    applyStoredTheme()
    theme.toggleDark()
    theme.colorPrimary = '#ff7a45'
    await nextTick()
    const root = document.documentElement
    expect(root.style.getPropertyValue('--accent')).toBe('#ff7a45')
    expect(root.style.getPropertyValue('--accent-soft')).toBe('rgba(255, 122, 69, 0.1)')
    expect(root.style.getPropertyValue('--accent-line')).toBe('rgba(255, 122, 69, 0.45)')
  })
})
