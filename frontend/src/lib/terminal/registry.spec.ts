import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { SessionId } from '../ipc/bindings'
import type { TerminalTheme } from '../../stores/theme'
import type { HighlightRule } from './highlight'

/**
 * 登记处的测试重点是**元素归属**，不是 xterm 本身。
 *
 * 所以把 `@xterm/xterm` 换成假终端：它记录自己开在哪个元素里、被 dispose 过没有。
 * 渲染与回滚缓冲是 xterm 的事，不该由我们测；真正会咬人的是我们自己的两条承诺——
 * 「同一会话不会有两个 DOM 副本」和「换格子不丢日志」。
 */
class FakeMarker {
  disposed = false

  constructor(readonly line: number) {}

  dispose(): void { this.disposed = true }
}

/** 真装饰要等渲染那一帧才发 `onRender`，假的一建好就发：测试关心的是底色有没有落上去。 */
class FakeDecoration {
  readonly element = document.createElement('div')
  disposed = false

  constructor(readonly options: Record<string, unknown>) {}

  onRender(cb: (element: HTMLElement) => void): void { cb(this.element) }
  dispose(): void { this.disposed = true }
}

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
  selectAllCount = 0
  cleared = 0
  keyHandler: ((event: KeyboardEvent) => boolean) | null = null
  /** 被 fit 了几次。字号变化后必须重算行列，验的就是这个数。 */
  fitCount = 0
  /** 关键字高亮要读缓冲区：几行文本 + 视口位置，够喂扫描逻辑就行。 */
  lines: string[] = []
  markers: FakeMarker[] = []
  decorations: FakeDecoration[] = []
  buffer = {
    active: {
      baseY: 0,
      cursorY: 0,
      viewportY: 0,
      getLine: (index: number) => {
        const text = this.lines[index]
        return text === undefined ? undefined : { translateToString: () => text }
      },
    },
  }

  constructor(options: Record<string, unknown>) {
    this.options = options
  }

  // 真 FitAddon 在 loadAddon 时会拿到终端引用，测试要靠它把 fit 记到具体终端头上
  loadAddon(addon: { attach?: (term: { fitCount: number }) => void }): void {
    addon.attach?.(this)
  }
  open(element: HTMLElement): void { this.openedIn = element }
  onData(): { dispose: () => void } { return { dispose() {} } }
  onResize(): { dispose: () => void } { return { dispose() {} } }
  onScroll(): { dispose: () => void } { return { dispose() {} } }
  onWriteParsed(): { dispose: () => void } { return { dispose() {} } }
  /** 真 xterm 的标记锚在「光标行 + 偏移」上，行号随滚动/裁剪变动，这里照抄这个算法。 */
  registerMarker(offset: number): FakeMarker {
    const marker = new FakeMarker(this.buffer.active.baseY + this.buffer.active.cursorY + offset)
    this.markers.push(marker)
    return marker
  }
  registerDecoration(options: Record<string, unknown>): FakeDecoration {
    const decoration = new FakeDecoration(options)
    this.decorations.push(decoration)
    return decoration
  }
  write(data: Uint8Array): void { this.writes.push(data) }
  focus(): void { this.focusCount += 1 }
  dispose(): void { this.disposed = true }
  paste(text: string): void { this.pasted.push(text) }
  /** 右键菜单的「全选」「清屏」：本地状态动作，记个数就够。 */
  selectAll(): void { this.selectAllCount += 1 }
  clear(): void { this.cleared += 1 }
  attachCustomKeyEventHandler(handler: (event: KeyboardEvent) => boolean): void { this.keyHandler = handler }
  hasSelection(): boolean { return this.selection !== '' }
  getSelection(): string { return this.selection }
}

vi.mock('@xterm/xterm', () => ({ Terminal: FakeTerminal }))
/** FitAddon 只记一件事：装到哪个终端上、被 fit 了几次。 */
vi.mock('@xterm/addon-fit', () => ({
  FitAddon: class {
    private term: { fitCount: number } | null = null

    attach(term: { fitCount: number }): void { this.term = term }
    fit(): void {
      if (this.term) this.term.fitCount += 1
    }
    dispose(): void {}
  },
}))

/**
 * 假搜索插件。真实插件要读终端缓冲区才能定位命中，而我们要验的是自己那两条约定：
 * 「计数取插件回填的值」和「关掉查找条要抹掉高亮」。
 *
 * 关键是时机：真插件在 `findNext` 返回**之前**同步发事件，且**零命中时不发**。
 * 所以这里由测试摆好 `result`，非空才发——照这个时机写成异步的话，登记处的
 * 「先归零再查」就永远测不出它存在的意义了。
 */
interface FakeSearch {
  calls: { text: string, forward: boolean, options: Record<string, unknown> }[]
  cleared: number
}

const searchState = vi.hoisted(() => ({
  instances: [] as FakeSearch[],
  result: null as { resultIndex: number, resultCount: number } | null,
}))

vi.mock('@xterm/addon-search', () => ({
  SearchAddon: class {
    changes: ((result: { resultIndex: number, resultCount: number }) => void)[] = []
    calls: { text: string, forward: boolean, options: Record<string, unknown> }[] = []
    cleared = 0

    constructor() {
      searchState.instances.push(this as unknown as FakeSearch)
    }

    dispose(): void {}
    onDidChangeResults(cb: (result: { resultIndex: number, resultCount: number }) => void): { dispose: () => void } {
      this.changes.push(cb)
      return { dispose() {} }
    }

    findNext(text: string, options: Record<string, unknown>): void { this.lookup(text, true, options) }
    findPrevious(text: string, options: Record<string, unknown>): void { this.lookup(text, false, options) }
    clearDecorations(): void { this.cleared += 1 }
    clearActiveDecoration(): void { this.cleared += 1 }

    private lookup(text: string, forward: boolean, options: Record<string, unknown>): void {
      this.calls.push({ text, forward, options })
      // 真插件在 regex 模式下直接 `RegExp(关键词)`：写到一半就抛。
      // 照抄这一步，才能测出登记处有没有把异常接住。
      if (options.regex) RegExp(text)
      if (searchState.result === null) return
      for (const cb of this.changes) cb(searchState.result)
    }
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

const menuCalls = vi.hoisted(() => ({ list: [] as { x: number, y: number, selection: boolean }[] }))

const handlers = {
  onData: () => {},
  onResize: () => {},
  onZoom: () => {},
  onContextMenu: (at: { x: number, y: number }, selection: boolean) => { menuCalls.list.push({ ...at, selection }) },
}

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

/** 响铃走 Web Audio，测试环境既没有设备也不该真出声；这里只数它被敲了几次。 */
const rings = vi.hoisted(() => ({ count: 0 }))

vi.mock('./beep', () => ({
  beep: () => { rings.count += 1 },
}))

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

  it('右键只报坐标与有没有选区，不再自己替用户决定复制还是粘贴', async () => {
    const element = registry.ensureTerminal(25, theme, 13, handlers)
    const term = fake(25)!
    menuCalls.list.length = 0
    clip.state.text = ''
    term.selection = 'error line'
    term.pasted.length = 0
    const event = new MouseEvent('contextmenu', { bubbles: true, cancelable: true, clientX: 120, clientY: 60 })
    element.dispatchEvent(event)
    await flush()
    // 原生菜单一弹出来就既点不到也复制不了
    expect(event.defaultPrevented).toBe(true)
    expect(menuCalls.list).toEqual([{ x: 120, y: 60, selection: true }])
    // 关键在于「什么都没发生」：动作等用户从菜单里挑
    expect(clip.state.text).toBe('')
    expect(term.pasted).toEqual([])
  })

  it('菜单动作出口：复制/粘贴/全选/清屏各自只碰自己那件事', async () => {
    registry.ensureTerminal(26, theme, 13, handlers)
    const term = fake(26)!
    clip.state.text = ''
    term.selection = 'warn line'
    registry.copySelection(26)
    await flush()
    expect(clip.state.text).toBe('warn line')
    // 没有选区时复制是空操作，不能把剪贴板清掉
    term.selection = ''
    registry.copySelection(26)
    await flush()
    expect(clip.state.text).toBe('warn line')

    clip.state.text = 'df -h'
    registry.pasteClipboard(26)
    await flush()
    expect(term.pasted).toEqual(['df -h'])
    registry.selectAllText(26)
    registry.clearView(26)
    expect(term.selectAllCount).toBe(1)
    expect(term.cleared).toBe(1)
    // 会话不存在时一律静默：菜单弹出到点击之间它可能已经被关掉
    registry.copySelection(999)
    registry.pasteClipboard(999)
    registry.selectAllText(999)
    registry.clearView(999)
    expect(term.pasted).toEqual(['df -h'])
  })

  it('粘贴三条路都通：Ctrl+V、Ctrl+Shift+V、Shift+Insert', async () => {
    registry.ensureTerminal(27, theme, 13, handlers)
    const term = fake(27)!
    clip.state.text = 'top'
    expect(term.keyHandler?.(key({ key: 'v', ctrlKey: true }))).toBe(false)
    expect(term.keyHandler?.(key({ key: 'V', ctrlKey: true, shiftKey: true }))).toBe(false)
    expect(term.keyHandler?.(key({ key: 'Insert', shiftKey: true }))).toBe(false)
    await flush()
    expect(term.pasted).toEqual(['top', 'top', 'top'])
    // 带 Shift 的 C 仍然还给 shell：那是终端自己的绑定，我们只接 V
    expect(term.keyHandler?.(key({ key: 'C', ctrlKey: true, shiftKey: true }))).toBe(true)
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

describe('终端缩放', () => {
  /** happy-dom 没有 WheelEvent，而登记处只读 ctrlKey 与 deltaY 两个字段。 */
  function wheel(deltaY: number, ctrlKey: boolean): Event {
    return Object.assign(new Event('wheel', { bubbles: true, cancelable: true }), { deltaY, ctrlKey })
  }

  it('Ctrl+滚轮换算成缩放档位，向上为正', () => {
    const steps: number[] = []
    const element = registry.ensureTerminal(40, theme, 13, { onData: () => {}, onResize: () => {}, onZoom: s => { steps.push(s) }, onContextMenu: () => {} })
    const up = wheel(-120, true)
    element.dispatchEvent(up)
    element.dispatchEvent(wheel(120, true))
    expect(steps).toEqual([1, -1])
    // 不拦下这条滚轮，WebView2 会把整个界面一起放大
    expect(up.defaultPrevented).toBe(true)
  })

  it('不带 Ctrl 的滚轮原样交给终端翻历史', () => {
    const steps: number[] = []
    const element = registry.ensureTerminal(41, theme, 13, { onData: () => {}, onResize: () => {}, onZoom: s => { steps.push(s) }, onContextMenu: () => {} })
    const plain = wheel(120, false)
    element.dispatchEvent(plain)
    expect(steps).toEqual([])
    expect(plain.defaultPrevented).toBe(false)
  })

  it('改字号要顺手重新 fit，否则行列还是按旧字号算的', () => {
    registry.ensureTerminal(42, theme, 13, handlers)
    const term = fake(42)!
    // ensure 时 fit 过一次，改字号必须再来一次
    expect(term.fitCount).toBe(1)
    term.fitCount = 0
    registry.applyFontSizeAll(18)
    expect(term.options.fontSize).toBe(18)
    // 字号变了宿主尺寸没变，ResizeObserver 不会响；不主动 fit 就半屏被裁掉
    expect(term.fitCount).toBe(1)
  })
})

describe('终端查找', () => {
  const options = { caseSensitive: false, wholeWord: false, regex: false }

  /** 每建一个终端就多一个插件实例，测试都只用刚建的那一个。 */
  function lastSearch(): FakeSearch {
    return searchState.instances[searchState.instances.length - 1]!
  }

  beforeEach(() => {
    searchState.result = null
  })

  it('还没建终端的会话：查不到也不抛', () => {
    expect(registry.findText(9998, 'err', options, true)).toBeNull()
    expect(() => { registry.clearFind(9998) }).not.toThrow()
  })

  it('空串是取消而不是查找：清高亮、不给插件发一次查找', () => {
    registry.ensureTerminal(30, theme, 13, handlers)
    const addon = lastSearch()
    expect(registry.findText(30, '', options, true)).toBeNull()
    expect(addon.calls).toHaveLength(0)
    expect(addon.cleared).toBeGreaterThan(0)
  })

  it('方向决定用 findNext 还是 findPrevious，高亮配色一并交出去', () => {
    registry.ensureTerminal(31, theme, 13, handlers)
    const addon = lastSearch()
    registry.findText(31, 'error', options, true)
    registry.findText(31, 'error', { ...options, caseSensitive: true }, false)
    expect(addon.calls.map(c => [c.text, c.forward])).toEqual([['error', true], ['error', false]])
    expect(addon.calls[0]!.options.decorations).toBeDefined()
    expect(addon.calls[1]!.options.caseSensitive).toBe(true)
  })

  it('命中时报「第几条共几条」，插件给的下标是 0 起的', () => {
    registry.ensureTerminal(32, theme, 13, handlers)
    searchState.result = { resultIndex: 2, resultCount: 5 }
    expect(registry.findText(32, 'warn', options, true)).toEqual({ index: 2, total: 5 })
  })

  it('一条都没中时停在 0 条：不能把上一次的计数留在屏上', () => {
    registry.ensureTerminal(33, theme, 13, handlers)
    searchState.result = { resultIndex: 0, resultCount: 3 }
    expect(registry.findText(33, 'warn', options, true)).toEqual({ index: 0, total: 3 })
    // 换个不存在的词：真插件零命中时不发事件，登记处必须自己归零
    searchState.result = null
    expect(registry.findText(33, 'nothing', options, true)).toEqual({ index: -1, total: 0 })
  })

  it('非法正则不许把异常抛给调用方：按「无匹配」收场，并把高亮收干净', () => {
    registry.ensureTerminal(35, theme, 13, handlers)
    const addon = lastSearch()
    // 用户边打字边查，`ite(m` 这种半截正则是必经之路，不是异常情况
    expect(registry.findText(35, 'ite(m', { ...options, regex: true }, true)).toEqual({ index: -1, total: 0 })
    // 插件抛在它已经清过高亮、却还没记下新关键词的地方；不跟着清一次，
    // 重打同一个词会命中它的缓存，只剩当前那一格、计数永远停在 0
    expect(addon.cleared).toBeGreaterThan(0)
    expect(registry.findText(35, 'ite(m)', { ...options, regex: true }, true)).toEqual({ index: -1, total: 0 })
  })

  it('clearFind 抹掉高亮并把计数收回 null', () => {
    registry.ensureTerminal(34, theme, 13, handlers)
    const addon = lastSearch()
    searchState.result = { resultIndex: 0, resultCount: 1 }
    registry.findText(34, 'warn', options, true)
    registry.clearFind(34)
    // 描边和当前命中那条是两次调用，少一次就还剩一格黄留在屏上
    expect(addon.cleared).toBe(2)
    searchState.result = null
    expect(registry.findText(34, 'warn', options, true)).toEqual({ index: -1, total: 0 })
  })
})

describe('关键字高亮', () => {
  function keyword(over: Partial<HighlightRule> = {}): HighlightRule {
    return { id: 'r', pattern: 'error', regex: false, caseSensitive: false, background: '#ff0000', bell: false, enabled: true, ...over }
  }

  afterEach(() => {
    // 规则是模块级的全局状态，不清掉会串到下一条用例的终端上
    registry.setHighlightRules([])
    // 更要紧的是把终端拆掉：`setHighlightRules` 会重画**所有**已存在的会话，
    // 上一个用例留下的终端会跟着响铃、把计数算到当前用例头上
    for (const id of [40, 41, 42, 43, 44, 45, 46, 47]) registry.destroyTerminal(id)
    rings.count = 0
  })

  /** 假终端的 `decorations` 是创建流水账，还活着的才是屏上的底色。 */
  function live(term: FakeTerminal): FakeDecoration[] {
    return term.decorations.filter(decoration => !decoration.disposed)
  }

  it('命中处挂装饰：列位置与宽度按字符下标算，底色落在元素上', () => {
    registry.ensureTerminal(40, theme, 13, handlers)
    const term = fake(40)
    if (!term) throw new Error('终端没建出来')
    term.rows = 2
    term.lines = ['a error here', 'nothing']
    registry.setHighlightRules([keyword()])
    expect(live(term)).toHaveLength(1)
    expect(term.decorations[0].options).toMatchObject({ x: 2, width: 5 })
    expect(term.decorations[0].element.style.backgroundColor).toContain('255, 0, 0')
  })

  it('标记落在它该落的缓冲区行上：偏移以光标所在行为基准', () => {
    registry.ensureTerminal(41, theme, 13, handlers)
    const term = fake(41)
    if (!term) throw new Error('终端没建出来')
    const buffer = term.buffer.active
    buffer.baseY = 10
    buffer.cursorY = 3
    buffer.viewportY = 6
    term.rows = 2
    // 视口两行是缓冲区的第 6、7 行，只有后者有关键字
    term.lines = ['', '', '', '', '', '', 'nothing', 'error here']
    registry.setHighlightRules([keyword()])
    expect(term.markers.map(m => m.line)).toEqual([7])
  })

  it('规则换第二批时旧装饰全部释放，不留残影', () => {
    registry.ensureTerminal(42, theme, 13, handlers)
    const term = fake(42)
    if (!term) throw new Error('终端没建出来')
    term.rows = 1
    term.lines = ['error error']
    registry.setHighlightRules([keyword()])
    const first = term.decorations.slice()
    expect(first).toHaveLength(2)
    registry.setHighlightRules([keyword({ id: 'r2', pattern: 'nothing' })])
    expect(first.every(d => d.disposed)).toBe(true)
    expect(live(term)).toHaveLength(0)
  })

  it('清空规则等于把屏上所有底色收掉', () => {
    registry.ensureTerminal(43, theme, 13, handlers)
    const term = fake(43)
    if (!term) throw new Error('终端没建出来')
    term.rows = 1
    term.lines = ['error']
    registry.setHighlightRules([keyword()])
    const held = term.decorations.slice()
    registry.setHighlightRules([])
    expect(held.every(d => d.disposed)).toBe(true)
    expect(live(term)).toHaveLength(0)
  })

  /** 把假终端摆成「缓冲区有 n 行、光标停在第 n-1 行」的样子，再让登记处重画一次。 */
  function paint(term: FakeTerminal, lines: string[], cursorLine: number, viewportY = 0): void {
    term.rows = 3
    term.lines = lines
    const buffer = term.buffer.active
    buffer.baseY = 0
    buffer.cursorY = cursorLine
    buffer.viewportY = viewportY
  }

  it('响铃只在新命中行写出来时响一次：同一行反复重画不能一直响', () => {
    registry.ensureTerminal(44, theme, 13, handlers)
    const term = fake(44)
    if (!term) throw new Error('终端没建出来')
    // 建实例时补写的那批（登录横幅、提示符）对用户来说是旧内容，一条都不该响
    paint(term, ['', 'make: error 1', 'prompt$ '], 2)
    registry.setHighlightRules([keyword({ bell: true })])
    expect(rings.count).toBe(0)
    // 新输出的报错行落在第 3 行，光标走到第 4 行、视口跟着滚到底（只画视口内的三行）
    paint(term, ['', 'make: error 1', 'prompt$ ', 'npm error code 1', 'prompt$ '], 4, 2)
    registry.setHighlightRules([keyword({ bell: true })])
    expect(rings.count).toBe(1)
    // 同一屏上因为滚动或改字号再画一次：报错还是那一行，不该再响
    registry.setHighlightRules([keyword({ bell: true })])
    expect(rings.count).toBe(1)
  })

  it('光标那一行不算写完：敲 `echo FATAL` 敲到一半不响，换行收尾才响', () => {
    registry.ensureTerminal(45, theme, 13, handlers)
    const term = fake(45)
    if (!term) throw new Error('终端没建出来')
    const rules = [keyword({ pattern: 'FATAL', bell: true })]
    paint(term, ['', ''], 1)
    registry.setHighlightRules(rules)
    expect(rings.count).toBe(0)
    // 用户正在提示符上打这条命令，回显落在光标行
    paint(term, ['', 'C:\\>echo FATAL', ''], 1)
    registry.setHighlightRules(rules)
    expect(rings.count).toBe(0)
    // 回车：光标先跳到下一行，这一行的命令回显文本一个字没变 → 还是不该响
    paint(term, ['', 'C:\\>echo FATAL', '', ''], 2)
    registry.setHighlightRules(rules)
    expect(rings.count).toBe(0)
    // 输出落到那一行上，光标又往前走了一行：这一下才是「刚打完的一行」
    paint(term, ['', 'C:\\>echo FATAL', 'FATAL', ''], 3)
    registry.setHighlightRules(rules)
    expect(rings.count).toBe(1)
    // 同一屏再画一次（滚动、改字号）：文本没变，不会跟着补响
    registry.setHighlightRules(rules)
    expect(rings.count).toBe(1)
  })

  it('翻回滚区翻到去年的报错不该被吵到', () => {
    registry.ensureTerminal(46, theme, 13, handlers)
    const term = fake(46)
    if (!term) throw new Error('终端没建出来')
    paint(term, ['old error here', '', '', '', '', '', '', 'prompt$ '], 7, 5)
    registry.setHighlightRules([keyword({ bell: true })])
    expect(rings.count).toBe(0)
    // 向上滚到顶部：命中行的编号比上次的光标行还小，属于历史
    paint(term, ['old error here', '', '', '', '', '', '', 'prompt$ '], 7, 0)
    registry.setHighlightRules([keyword({ bell: true })])
    expect(rings.count).toBe(0)
  })

  it('规则没开响铃时一声都不响', () => {
    registry.ensureTerminal(47, theme, 13, handlers)
    const term = fake(47)
    if (!term) throw new Error('终端没建出来')
    paint(term, ['', 'error'], 1)
    registry.setHighlightRules([keyword()])
    expect(live(term)).toHaveLength(1)
    paint(term, ['', 'error', 'error'], 2)
    registry.setHighlightRules([keyword()])
    expect(rings.count).toBe(0)
  })
})
