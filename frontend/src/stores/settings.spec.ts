import { createPinia, setActivePinia } from 'pinia'
import { nextTick } from 'vue'
import { beforeEach, describe, expect, it } from 'vitest'

import { DEFAULT_FONT_SIZE, DEFAULT_HIGHLIGHT_COLOR, MAX_FONT_SIZE, MIN_FONT_SIZE, useSettingsStore } from './settings'

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

describe('关键字高亮规则', () => {
  it('新增的规则自带底色与 id，删掉后不再回来', async () => {
    const settings = useSettingsStore()
    expect(settings.highlightRules).toEqual([])
    const first = settings.addHighlightRule()
    expect(first.background).toBe(DEFAULT_HIGHLIGHT_COLOR)
    expect(first.enabled).toBe(true)
    const second = settings.addHighlightRule()
    settings.removeHighlightRule(first.id)
    expect(settings.highlightRules.map(item => item.id)).toEqual([second.id])
    await nextTick()
    expect(JSON.parse(localStorage.getItem(KEY) ?? '{}').highlightRules).toHaveLength(1)
  })

  it('两条规则的 id 不能撞：列表靠它做 key 和删除依据', () => {
    const settings = useSettingsStore()
    const ids = new Set([settings.addHighlightRule().id, settings.addHighlightRule().id, settings.addHighlightRule().id])
    expect(ids.size).toBe(3)
  })

  it('盘上的坏规则整条丢掉，字段缺项回落默认', () => {
    localStorage.setItem(KEY, JSON.stringify({
      highlightRules: [
        { pattern: 'error', background: '#00ff00', regex: true, bell: true },
        { pattern: '' },
        { background: '#00ff00' },
        '不是对象',
        { pattern: 'warn', background: 'red' },
      ],
    }))
    setActivePinia(createPinia())
    const rules = useSettingsStore().highlightRules
    // 空串和缺 pattern 的两条会命中一切或什么都不命中，都不能留
    expect(rules).toHaveLength(2)
    expect(rules[0]).toMatchObject({ pattern: 'error', regex: true, bell: true, caseSensitive: false, enabled: true })
    // 认不出的颜色回落到默认底色，而不是把 `red` 交给 xterm 去猜
    expect(rules[1]).toMatchObject({ pattern: 'warn', background: DEFAULT_HIGHLIGHT_COLOR })
    expect(rules[0].id).not.toBe(rules[1].id)
  })

  it('日志目录留空就是「不指定」，填了要原样记住', async () => {
    const settings = useSettingsStore()
    expect(settings.logDir).toBe('')
    // 会话那边把空串翻译成 null 交给后端默认目录，所以这里绝不能存成别的东西
    settings.logDir = '  D:\\会话日志  '
    await nextTick()
    setActivePinia(createPinia())
    // 首尾空格是复制粘贴时最容易带上的，Windows 上它会变成另一个打不开的目录
    expect(useSettingsStore().logDir).toBe('D:\\会话日志')
  })
})
