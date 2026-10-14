import { describe, expect, it } from 'vitest'

import { installShortcuts, match, type KeyEvent, type ShortcutHandlers } from './shortcuts'

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
    find: () => { calls.push('find') },
    zoom: step => { calls.push(`zoom:${step}`) },
    resetZoom: () => { calls.push('resetZoom') },
    toggleQuickBar: () => { calls.push('toggleQuickBar') },
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

  it('Ctrl+F 开查找条：bash 里它是光标前移，但找东西的诉求强得多', () => {
    expect(fired(key({ key: 'f', ctrlKey: true, code: 'KeyF' }))).toBe('find')
    expect(fired(key({ key: 'f', metaKey: true, code: 'KeyF' }))).toBe('find')
  })

  it('Ctrl+1..9 选标签，Ctrl+0 回默认字号', () => {
    expect(fired(key({ key: '1', ctrlKey: true, code: 'Digit1' }))).toBe('selectTab:0')
    expect(fired(key({ key: '9', ctrlKey: true, code: 'Digit9' }))).toBe('selectTab:8')
    // 0 没有第 10 个标签可指，给了「缩放回 100%」
    expect(fired(key({ key: '0', ctrlKey: true, code: 'Digit0' }))).toBe('resetZoom')
  })

  it('Ctrl+= / Ctrl+- 缩放字号，加号与小键盘两条写法都认', () => {
    expect(fired(key({ key: '=', ctrlKey: true, code: 'Equal' }))).toBe('zoom:1')
    // 美式布局的 + 要按 Shift，键盘上报的是 '+' 而不是 '='
    expect(fired(key({ key: '+', ctrlKey: true, shiftKey: true, code: 'Equal' }))).toBe('zoom:1')
    expect(fired(key({ key: '+', ctrlKey: true, code: 'NumpadAdd' }))).toBe('zoom:1')
    expect(fired(key({ key: '-', ctrlKey: true, code: 'Minus' }))).toBe('zoom:-1')
    expect(fired(key({ key: '_', ctrlKey: true, shiftKey: true, code: 'Minus' }))).toBe('zoom:-1')
  })

  it('快捷命令栏要带 Shift：不带 Shift 的 Ctrl+K 还给终端', () => {
    expect(fired(key({ key: 'K', code: 'KeyK', ctrlKey: true, shiftKey: true }))).toBe('toggleQuickBar')
    // readline 的 kill-line（删到行尾）是天天用的键，抢了它用户只会觉得终端坏了
    expect(fired(key({ key: 'k', code: 'KeyK', ctrlKey: true }))).toBeNull()
  })

  it('Ctrl+Tab 向后，Ctrl+Shift+Tab 向前', () => {    expect(fired(key({ key: 'Tab', code: 'Tab', ctrlKey: true }))).toBe('nextTab')
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

describe('installShortcuts 总开关', () => {
  function target(): EventTarget {
    return new EventTarget()
  }

  function ctrlT(): KeyboardEvent {
    return new KeyboardEvent('keydown', { key: 't', code: 'KeyT', ctrlKey: true, bubbles: true })
  }

  function handlers(sink: string[]): ShortcutHandlers {
    return {
      newLocal: () => { sink.push('newLocal') },
      closeActive: () => { sink.push('closeActive') },
      nextTab: () => {},
      prevTab: () => {},
      selectTab: () => {},
      toggleSplit: () => {},
      focusNext: () => {},
      openSettings: () => {},
      find: () => { sink.push('find') },
      zoom: () => {},
      resetZoom: () => {},
      toggleQuickBar: () => { sink.push('toggleQuickBar') },
    }
  }

  /** 终端的收键壳子：真 xterm 就是这样一个藏在 `.xterm` 里的 textarea。 */
  function terminalKeypress(): HTMLElement {
    const host = document.createElement('div')
    host.className = 'xterm'
    const area = document.createElement('textarea')
    host.append(area)
    document.body.append(host)
    return area
  }

  function combo(over: Partial<KeyboardEvent>): KeyboardEvent {
    return new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...over })
  }

  it('enabled 时照常派发', () => {
    const sink: string[] = []
    const el = target()
    const dispose = installShortcuts(el, () => handlers(sink), () => false, () => true)
    el.dispatchEvent(ctrlT())
    dispose()
    expect(sink).toEqual(['newLocal'])
  })

  it('锁屏（enabled 为 false）时一个键都不派发', () => {
    const sink: string[] = []
    const el = target()
    const dispose = installShortcuts(el, () => handlers(sink), () => false, () => false)
    el.dispatchEvent(ctrlT())
    dispose()
    expect(sink).toEqual([])
  })

  it('终端聚焦时照抢：那个 textarea 只是 xterm 收键的壳子，不是打字的地方', () => {
    const sink: string[] = []
    const area = terminalKeypress()
    const dispose = installShortcuts(area, () => handlers(sink), () => false, () => true)
    area.dispatchEvent(combo({ key: 'f', code: 'KeyF', ctrlKey: true }))
    area.dispatchEvent(combo({ key: 'K', code: 'KeyK', ctrlKey: true, shiftKey: true }))
    dispose()
    expect(sink).toEqual(['find', 'toggleQuickBar'])
  })

  it('表单输入框里不抢键，但命令栏开关与关标签这类不产字的组合例外', () => {
    const sink: string[] = []
    const input = document.createElement('input')
    document.body.append(input)
    const dispose = installShortcuts(input, () => handlers(sink), () => false, () => true)
    input.dispatchEvent(combo({ key: 'f', code: 'KeyF', ctrlKey: true }))
    input.dispatchEvent(combo({ key: 'K', code: 'KeyK', ctrlKey: true, shiftKey: true }))
    dispose()
    expect(sink).toEqual(['toggleQuickBar'])
  })
})
