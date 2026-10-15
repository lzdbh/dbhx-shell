import { describe, expect, it } from 'vitest'

import { firstActive, menuState, moveActive, nextActive, openMenu, placeMenu, trigger } from './contextMenu'
import type { MenuItem } from './contextMenu'

const sep = (): MenuItem => ({ key: 'sep', separator: true })
const item = (key: string, extra: Partial<MenuItem> = {}): MenuItem => ({ key, label: key, ...extra })

describe('菜单定位', () => {
  it('默认左上角对齐光标', () => {
    expect(placeMenu({ x: 100, y: 80 }, { width: 200, height: 120 }, { width: 1200, height: 800 }))
      .toEqual({ left: 100, top: 80 })
  })

  it('右边放不下就朝左翻，下边放不下就朝上翻', () => {
    const view = { width: 800, height: 600 }
    const size = { width: 200, height: 120 }
    expect(placeMenu({ x: 750, y: 560 }, size, view)).toEqual({ left: 550, top: 440 })
  })

  it('翻完仍然溢出时夹到边缘，不跑出窗口', () => {
    // 光标贴着左上角，而菜单比窗口还大
    const at = placeMenu({ x: 2, y: 3 }, { width: 900, height: 700 }, { width: 800, height: 600 })
    expect(at.left).toBeGreaterThanOrEqual(6)
    expect(at.top).toBeGreaterThanOrEqual(6)
    expect(at.left).toBe(6)
    expect(at.top).toBe(6)
  })
})

describe('键盘走位', () => {
  const list = [item('a'), sep(), item('b', { disabled: true }), item('c')]

  it('跳过分隔线与禁用项', () => {
    expect(nextActive(list, 0, 1)).toBe(3)
    expect(nextActive(list, 3, -1)).toBe(0)
  })

  it('到头之后绕回另一头', () => {
    expect(nextActive(list, 3, 1)).toBe(0)
  })

  it('全是分隔线时没有可选项', () => {
    expect(nextActive([sep(), sep()], 0, 1)).toBe(-1)
    expect(firstActive([sep()])).toBe(-1)
  })

  it('firstActive 取第一个可选的', () => {
    expect(firstActive([sep(), item('x')])).toBe(1)
  })

  it('moveActive 在高亮项之间推进', () => {
    openMenu({ x: 0, y: 0 }, list)
    expect(menuState.active.value).toBe(0)
    moveActive(1)
    expect(menuState.active.value).toBe(3)
  })
})

describe('打开与执行', () => {
  it('空列表不弹，也不把已开的菜单关掉', () => {
    openMenu({ x: 10, y: 10 }, [item('a')])
    openMenu({ x: 99, y: 99 }, [])
    expect(menuState.visible.value).toBe(true)
    expect(menuState.point.value).toEqual({ x: 10, y: 10 })
  })

  it('点完先关菜单再跑动作', () => {
    const seen: string[] = []
    openMenu({ x: 0, y: 0 }, [item('a', { onClick: () => { seen.push(`closed=${menuState.visible.value}`) } })])
    trigger('a')
    expect(seen).toEqual(['closed=false'])
    expect(menuState.visible.value).toBe(false)
  })

  it('禁用项和分隔线点了没反应', () => {
    let ran = false
    openMenu({ x: 0, y: 0 }, [sep(), item('off', { disabled: true, onClick: () => { ran = true } })])
    trigger('off')
    trigger('sep')
    expect(ran).toBe(false)
    // 没执行就不该关
    expect(menuState.visible.value).toBe(true)
  })
})
