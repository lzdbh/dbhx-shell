import { createPinia, setActivePinia } from 'pinia'
import { nextTick } from 'vue'
import { beforeEach, describe, expect, it } from 'vitest'

import { DEFAULT_FONT_SIZE, MAX_FONT_SIZE, MIN_FONT_SIZE, useSettingsStore } from './settings'

const KEY = 'dbhx:settings'

beforeEach(() => {
  localStorage.clear()
  setActivePinia(createPinia())
})

describe('settings store', () => {
  it('没存过时用默认字号起步', () => {
    const settings = useSettingsStore()
    expect(settings.fontSize).toBe(DEFAULT_FONT_SIZE)
    expect(settings.zoomPercent()).toBe(100)
  })

  it('Ctrl+滚轮一格一档，往上到顶就停住', () => {
    const settings = useSettingsStore()
    for (let i = 0; i < 40; i++) settings.zoom(1)
    expect(settings.fontSize).toBe(MAX_FONT_SIZE)
    for (let i = 0; i < 60; i++) settings.zoom(-1)
    // 缩到个位数就没法读了，必须停在下限而不是变成 0 或负数
    expect(settings.fontSize).toBe(MIN_FONT_SIZE)
  })

  it('resetZoom 回到默认值，百分比跟着字号走', () => {
    const settings = useSettingsStore()
    settings.zoom(1)
    expect(settings.fontSize).toBe(DEFAULT_FONT_SIZE + 1)
    expect(settings.zoomPercent()).toBeGreaterThan(100)
    settings.resetZoom()
    expect(settings.fontSize).toBe(DEFAULT_FONT_SIZE)
    expect(settings.zoomPercent()).toBe(100)
  })

  it('缩放要落盘：重开应用不该回到默认字号', async () => {
    const settings = useSettingsStore()
    settings.zoom(3)
    // 落盘走 watch，默认异步：不等一下读到的就是磁盘上的旧值
    await nextTick()
    expect(JSON.parse(localStorage.getItem(KEY) ?? '{}').fontSize).toBe(DEFAULT_FONT_SIZE + 3)

    setActivePinia(createPinia())
    const reopened = useSettingsStore()
    expect(reopened.fontSize).toBe(DEFAULT_FONT_SIZE + 3)
  })

  it('盘上的字号越界也要夹回区间', () => {
    localStorage.setItem(KEY, JSON.stringify({ fontSize: 200 }))
    setActivePinia(createPinia())
    expect(useSettingsStore().fontSize).toBe(MAX_FONT_SIZE)

    localStorage.setItem(KEY, JSON.stringify({ fontSize: 0 }))
    setActivePinia(createPinia())
    // 0 号字体不是「看不见」那么无害，xterm 会直接算不出行列
    expect(useSettingsStore().fontSize).toBe(MIN_FONT_SIZE)
  })
})
