import { describe, expect, it } from 'vitest'

import { match, type KeyEvent } from './shortcuts'

/** 造一次按键；只填判定真正用得上的字段。 */
function key(over: Partial<KeyEvent>): KeyEvent {
  return {
    key: '',
    code: '',
    ctrlKey: false,
    altKey: false,
    shiftKey: false,
    metaKey: false,
    target: null,
    ...over,
  }
}

const ctx = { splitActive: false, inTerminal: false }

function fired(event: KeyEvent, options = ctx): string | null {
  const calls: string[] = []
  const handler = match(event, options)
  if (handler === null) return null
  handler({
    newLocal: () => { calls.push('newLocal') },
    closeActive: () => { calls.push('closeActive') },
    nextTab: () => { calls.push('nextTab') },
    prevTab: () => { calls.push('prevTab') },
    selectTab: index => { calls.push(`selectTab:${index}`) },
    toggleSplit: () => { calls.push('toggleSplit') },
    focusNext: () => { calls.push('focusNext') },
    openSettings: () => { calls.push('openSettings') },
  })
  return calls[0] ?? null
}

describe('快捷键判定', () => {
  it('Ctrl 与 Cmd 视为同一个修饰键', () => {
    expect(fired(key({ key: 't', ctrlKey: true, code: 'KeyT' }))).toBe('newLocal')
    expect(fired(key({ key: 't', metaKey: true, code: 'KeyT' }))).toBe('newLocal')
  })

  it('Ctrl+W 关掉当前格子的会话', () => {
    expect(fired(key({ key: 'w', ctrlKey: true, code: 'KeyW' }))).toBe('closeActive')
  })

  it('Ctrl+E 切分屏，Ctrl+, 开设置', () => {
    expect(fired(key({ key: 'e', ctrlKey: true, code: 'KeyE' }))).toBe('toggleSplit')
    expect(fired(key({ key: ',', ctrlKey: true, code: 'Comma' }))).toBe('openSettings')
  })

  it('Ctrl+1..9 选标签，Ctrl+0 不抢', () => {
    expect(fired(key({ key: '1', ctrlKey: true, code: 'Digit1' }))).toBe('selectTab:0')
    expect(fired(key({ key: '9', ctrlKey: true, code: 'Digit9' }))).toBe('selectTab:8')
    // 0 常常是「放大回默认字号」，我们没做，所以必须原样放行
    expect(fired(key({ key: '0', ctrlKey: true, code: 'Digit0' }))).toBeNull()
  })

  it('Ctrl+Tab 向后，Ctrl+Shift+Tab 向前', () => {
    expect(fired(key({ key: 'Tab', code: 'Tab', ctrlKey: true }))).toBe('nextTab')
    expect(fired(key({ key: 'Tab', code: 'Tab', ctrlKey: true, shiftKey: true }))).toBe('prevTab')
  })

  it('Alt+方向键只在分屏时跳格子，终端里放行', () => {
    const split = { splitActive: true, inTerminal: false }
    expect(fired(key({ key: 'ArrowRight', altKey: true }), split)).toBe('focusNext')
    expect(fired(key({ key: 'ArrowLeft', altKey: true }), split)).toBe('focusNext')
    // 单屏没有第二格，抢了就是把 readline 的历史搜索废掉
    expect(fired(key({ key: 'ArrowRight', altKey: true }), ctx)).toBeNull()
    // Alt+Right 在 bash 里是跨词移动，用户就该拿到跨词移动
    expect(fired(key({ key: 'ArrowRight', altKey: true }), { splitActive: true, inTerminal: true })).toBeNull()
  })

  it('裸键与终端里合法的组合一律放行', () => {
    expect(fired(key({ key: 't' }))).toBeNull()
    expect(fired(key({ key: 'T', shiftKey: true }))).toBeNull()
    expect(fired(key({ key: 'c', ctrlKey: true, code: 'KeyC' }))).toBeNull()
    expect(fired(key({ key: 'r', ctrlKey: true, code: 'KeyR' }))).toBeNull()
    expect(fired(key({ key: 'ArrowRight' }))).toBeNull()
  })

  it('Alt 与 Ctrl 同时按下不抢：那是终端控制序列的写法', () => {
    expect(fired(key({ key: 't', altKey: true, ctrlKey: true }))).toBeNull()
  })
})
