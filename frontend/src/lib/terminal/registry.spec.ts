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
  pasted: string[] = []
  selection = ''
  keyHandler: ((event: KeyboardEvent) => boolean) | null = null

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
  paste(text: string): void { this.pasted.push(text) }
  attachCustomKeyEventHandler(handler: (event: KeyboardEvent) => boolean): void { this.keyHandler = handler }
  hasSelection(): boolean { return this.selection !== '' }
  getSelection(): string { return this.selection }
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

/**
 * 假剪贴板。真实实现是后端的 `local_clipboard_*` 命令，测试里既没有那个进程也不该
 * 碰真系统剪贴板；要测的是「取到文本就粘贴、取不到只是不生效」这两条，所以换成可控对象。
 */
const clip = vi.hoisted(() => {
  const state = { text: '', denied: false }
  return {
    state,
    read: async (): Promise<string> => {
      if (state.denied) throw new Error('剪贴板暂时不可用')
      return state.text
    },
    write: async (value: string): Promise<void> => {
      if (state.denied) throw new Error('剪贴板暂时不可用')
      state.text = value
    },
  }
})

vi.mock('../ipc/client', () => ({ clipboardApi: clip }))

/** 复制/粘贴都挂在 promise 上，`void` 掉了就要给事件循环一次机会才能看到结果。 */
function flush(): Promise<void> {
  return new Promise(resolve => { setTimeout(resolve, 0) })
}

function base64(text: string): string {
  return Buffer.from(text, 'utf8').toString('base64')
}

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

  it('远端登录横幅落在建终端之前也要看得见', () => {
    // 连上主机时，`Welcome to Ubuntu …` 往往在 tab 挂载、格子认领、xterm 造出来之前
    // 就到了。这一段服务端只打一次、不会重画，丢了就永远看不到
    registry.writeTerminal(10, base64('Welcome to Ubuntu 26.04.1 LTS\n'))
    registry.writeTerminal(10, base64('Last login: Wed Oct 14 12:25:52 2026\n'))
    registry.ensureTerminal(10, theme, 13, handlers)
    const written = fake(10)!.writes.map(bytes => new TextDecoder().decode(bytes))
    expect(written.join('')).toContain('Welcome to Ubuntu 26.04.1 LTS')
    // 暂存的几段合成一次写入，别一段一帧地闪
    expect(written).toHaveLength(1)
    // 补写之后暂存区必须清空，否则下次写会把它重复一遍
    registry.writeTerminal(10, base64('prompt$ '))
    expect(fake(10)!.writes).toHaveLength(2)
    expect(new TextDecoder().decode(fake(10)!.writes[1])).toBe('prompt$ ')
  })

  it('没人认领时按上限丢最老的，销毁不留内存', () => {
    const old = base64('x'.repeat(400 * 1024))
    const fresh = 'y'.repeat(200 * 1024)
    registry.writeTerminal(11, old)
    registry.writeTerminal(11, base64(fresh))
    registry.ensureTerminal(11, theme, 13, handlers)
    const joined = fake(11)!.writes.map(bytes => new TextDecoder().decode(bytes)).join('')
    // 超过 512 KB 的部分从头部裁掉，保住最近的输出
    expect(joined).toBe(fresh)

    // 会话销毁时暂存也要一起清掉，否则下一个复用同一 id 的会话会收到别人的字节
    registry.writeTerminal(12, base64('gone'))
    registry.destroyTerminal(12)
    registry.ensureTerminal(12, theme, 13, handlers)
    expect(fake(12)!.writes).toHaveLength(0)
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

describe('终端剪贴板', () => {
  function key(init: KeyboardEventInit): KeyboardEvent {
    return { type: 'keydown', ...init } as KeyboardEvent
  }

  it('有选区时 Ctrl+C 复制选区，不再往 shell 发 ^C', async () => {
    registry.ensureTerminal(20, theme, 13, handlers)
    const term = fake(20)!
    term.selection = 'nginx.err'
    expect(term.keyHandler?.(key({ key: 'c', ctrlKey: true }))).toBe(false)
    await flush()
    expect(clip.state.text).toBe('nginx.err')
  })

  it('没选区时 Ctrl+C 交回终端，中断信号照发', () => {
    registry.ensureTerminal(21, theme, 13, handlers)
    const term = fake(21)!
    term.selection = ''
    expect(term.keyHandler?.(key({ key: 'c', ctrlKey: true }))).toBe(true)
  })

  it('Ctrl+V 从剪贴板取文本，走终端的粘贴通道', async () => {
    clip.state.text = 'systemctl restart nginx\n'
    registry.ensureTerminal(22, theme, 13, handlers)
    const term = fake(22)!
    expect(term.keyHandler?.(key({ key: 'v', metaKey: true }))).toBe(false)
    await flush()
    expect(term.pasted).toEqual(['systemctl restart nginx\n'])
  })

  it('修饰键不齐或带 Shift 的组合一律放行', () => {
    registry.ensureTerminal(23, theme, 13, handlers)
    const term = fake(23)!
    term.selection = 'abc'
    // 裸 C 是打字，Ctrl+Shift+C 留给 shell 自己的绑定
    expect(term.keyHandler?.(key({ key: 'c' }))).toBe(true)
    expect(term.keyHandler?.(key({ key: 'c', ctrlKey: true, shiftKey: true }))).toBe(true)
    expect(term.keyHandler?.(key({ key: 'c', ctrlKey: true, altKey: true }))).toBe(true)
    expect(term.keyHandler?.({ type: 'keyup', key: 'v', ctrlKey: true } as KeyboardEvent)).toBe(true)
  })

  it('右键：有选区先复制选区，没选区才粘贴', async () => {
    const element = registry.ensureTerminal(25, theme, 13, handlers)
    const term = fake(25)!
    clip.state.text = ''
    term.selection = 'error line'
    element.dispatchEvent(new MouseEvent('contextmenu', { bubbles: true, cancelable: true }))
    await flush()
    expect(clip.state.text).toBe('error line')

    term.selection = ''
    clip.state.text = 'ls -l'
    const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true })
    element.dispatchEvent(event)
    await flush()
    expect(term.pasted).toEqual(['ls -l'])
    // 原生菜单一弹出来就既点不到也复制不了
    expect(event.defaultPrevented).toBe(true)
  })

  it('剪贴板命令失败时只是不生效，不能抛出去打断按键', async () => {
    clip.state.denied = true
    registry.ensureTerminal(24, theme, 13, handlers)
    const term = fake(24)!
    term.selection = 'secret'
    expect(term.keyHandler?.(key({ key: 'c', ctrlKey: true }))).toBe(false)
    expect(term.keyHandler?.(key({ key: 'v', ctrlKey: true }))).toBe(false)
    await flush()
    expect(term.pasted).toEqual([])
    clip.state.denied = false
  })
})
