import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { SessionId } from '../ipc/bindings'
import type { TerminalTheme } from '../../stores/theme'

/**
 * 登记处的测试重点是**元素归属**，不是 xterm 本身。
 *
 * 所以把 `@xterm/xterm` 换成假终端：它记录自己开在哪个元素里、被 dispose 过没有。
 * 渲染与回滚缓冲是 xterm 的事，不该由我们测；真正会咬人的是我们自己的两条承诺——
 * 「同一会话不会有两个 DOM 副本」和「换格子不丢日志」。
 */
class FakeTerminal {
  options: Record<string, unknown>
  rows = 0
  cols = 0
  openedIn: HTMLElement | null = null
  disposed = false
  focusCount = 0
  writes: Uint8Array[] = []

  constructor(options: Record<string, unknown>) {
    this.options = options
  }

  loadAddon(): void {}
  open(element: HTMLElement): void { this.openedIn = element }
  onData(): { dispose: () => void } { return { dispose() {} } }
  onResize(): { dispose: () => void } { return { dispose() {} } }
  write(data: Uint8Array): void { this.writes.push(data) }
  focus(): void { this.focusCount += 1 }
  dispose(): void { this.disposed = true }
}

vi.mock('@xterm/xterm', () => ({ Terminal: FakeTerminal }))
vi.mock('@xterm/addon-fit', () => ({
  FitAddon: class {
    fit(): void {}
    dispose(): void {}
  },
}))

// happy-dom 不提供 ResizeObserver；登记处只用它触发重新 fit，测试里不需要真观察
globalThis.ResizeObserver ??= class {
  observe(): void {}
  unobserve(): void {}
  disconnect(): void {}
} as unknown as typeof ResizeObserver

const theme: TerminalTheme = {
  id: 'spec',
  background: '#000',
  foreground: '#eee',
  cursor: '#fff',
  cursorAccent: '#000',
  selectionBackground: '#333',
  ansi: ['#0', '#1', '#2', '#3', '#4', '#5', '#6', '#7', '#8', '#9', '#10', '#11', '#12', '#13', '#14', '#15'],
}

const handlers = { onData: () => {}, onResize: () => {} }

function slot(): HTMLElement {
  const element = document.createElement('div')
  document.body.appendChild(element)
  return element
}

/** 登记处交出的类型是 xterm 的 Terminal，测试要看的是假终端自己记的那些账。 */
function fake(id: SessionId): FakeTerminal | undefined {
  return registry.lookup(id) as unknown as FakeTerminal | undefined
}

const registry = await import('./registry')

beforeEach(() => {
  document.body.innerHTML = ''
})

describe('终端登记处', () => {
  it('同一会话反复 ensure 拿到同一个元素，不会多造终端', () => {
    const a = registry.ensureTerminal(1, theme, 13, handlers)
    const b = registry.ensureTerminal(1, theme, 13, handlers)
    expect(a).toBe(b)
    expect(a.classList.contains('xterm-host')).toBe(true)
    expect(fake(1)?.openedIn).toBe(a)
  })

  it('搬格子是挪 DOM 而不是重建', () => {
    const element = registry.ensureTerminal(2, theme, 13, handlers)
    const left = slot()
    const right = slot()
    registry.attachTerminal(2, left)
    expect(left.children).toHaveLength(1)
    registry.attachTerminal(2, right)
    // 关键点：左边必须空出来，否则同一份输出会画两遍、还会互相抢那个唯一实例
    expect(left.children).toHaveLength(0)
    expect(right.children[0]).toBe(element)
    expect(fake(2)?.disposed).toBe(false)
  })

  it('摘走只在当前宿主上生效：别人接手了就不能再抹', () => {
    registry.ensureTerminal(3, theme, 13, handlers)
    const host = slot()
    const other = slot()
    registry.attachTerminal(3, host)
    // 主格已经认领走了，右格卸载时的摘除必须是空操作，否则屏幕上就剩空白
    registry.attachTerminal(3, other)
    registry.detachTerminal(3, host)
    expect(other.children).toHaveLength(1)
    registry.detachTerminal(3, host)
    expect(other.children).toHaveLength(1)
    registry.detachTerminal(3, other)
    expect(other.children).toHaveLength(0)
    expect(fake(3)).toBeDefined()
  })

  it('销毁把节点一起摘掉，不留空白格子', () => {
    registry.ensureTerminal(4, theme, 13, handlers)
    const host = slot()
    registry.attachTerminal(4, host)
    registry.destroyTerminal(4)
    expect(fake(4)).toBeUndefined()
    expect(document.querySelector('.xterm-host')).toBeNull()
  })

  it('字号是全局偏好，落到每个已存在的终端', () => {
    registry.ensureTerminal(5, theme, 13, handlers)
    registry.ensureTerminal(6, theme, 11, handlers)
    expect(fake(5)?.options.fontSize).toBe(13)
    registry.applyFontSizeAll(16)
    expect(fake(5)?.options.fontSize).toBe(16)
    expect(fake(6)?.options.fontSize).toBe(16)
  })

  it('主题按会话换，调色板要铺满 16 个 ANSI 槽位', () => {
    registry.ensureTerminal(7, theme, 13, handlers)
    registry.applyTheme(7, { ...theme, background: '#123456' })
    const painted = fake(7)?.options.theme as { background: string, brightBlack: string }
    expect(painted.background).toBe('#123456')
    // tmux 状态栏爱用 brightBlack，漏了它就是一条灰带变白
    expect(painted.brightBlack).toBe('#8')
  })

  it('写给还没挂进格子的会话不报错', () => {
    expect(() => { registry.writeTerminal(999, 'aGk=') }).not.toThrow()
    registry.ensureTerminal(8, theme, 13, handlers)
    registry.writeTerminal(8, 'aGk=')
    expect(fake(8)?.writes).toHaveLength(1)
  })

  it('未知会话的 focus 与 fit 都是空操作', () => {
    expect(() => {
      registry.focusTerminal(1234)
      registry.focusTerminal(null)
      registry.fitTerminal(1234)
    }).not.toThrow()
    registry.ensureTerminal(9, theme, 13, handlers)
    registry.focusTerminal(9)
    expect(fake(9)?.focusCount).toBe(1)
  })
})
