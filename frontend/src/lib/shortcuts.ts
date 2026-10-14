/**
 * 全局快捷键。
 *
 * 两件事决定了实现形状：
 * 1. **必须监听在 `document` 的 capture 阶段。** xterm 在自己的容器上也挂了
 *    keydown，而且是冒泡阶段；等事件冒到 document 时，`attachCustomKeyEventHandler`
 *    之外的组合键可能已经被终端消费掉了。capture 让我们先看到每一个按键。
 * 2. **终端里合法的组合不能被抢。** `Ctrl+Shift+C` 在很多 shell 里是命令，
 *    `Alt+b/f` 是 readline 的跨词移动，`Ctrl+T` 在 bash 里是 transpose-chars——
 *    但用户在终端里按 `Ctrl+T` 十次有九次是想开新标签，这是同类客户端的共识，
 *    所以我们抢它，同时留一条 `terminal_pass_through` 的口子给设置页。
 *
 * 这里只做「按键 ⇒ 动作」的判定，不碰任何 store：这样单测不需要起 Vue，
 * 也让 App 保留「这个组合键到底该干什么」的最终解释权。
 */

export interface ShortcutHandlers {
  newLocal: () => void
  closeActive: () => void
  nextTab: () => void
  prevTab: () => void
  selectTab: (index: number) => void
  toggleSplit: () => void
  focusNext: () => void
  openSettings: () => void
  find: () => void
  /** 字号缩放。`zoom` 收 ±1 这样的档位，`resetZoom` 回默认值。 */
  zoom: (step: number) => void
  resetZoom: () => void
}

/** 事件对象里我们真正用得上的三个字段——单测直接造这个形状就够了。 */
export interface KeyEvent {
  key: string
  code?: string
  ctrlKey: boolean
  altKey: boolean
  shiftKey: boolean
  metaKey: boolean
  target: unknown
}

/**
 * 光标是否落在终端里。
 *
 * 判据是 DOM 而不是「我们刚才是不是 focus 了终端」：用户可能在终端里选中了文本、
 * 或者把光标放进了 SFTP 面板的输入框，两种情况我们都不该抢键。
 * 输入框一类由 `isTextEntry` 排除，终端一类由 `.xterm` 类名识别。
 */
function inTerminal(event: KeyEvent): boolean {
  // target 可能是 document（没人聚焦时），它不是 Element，直接判空
  let node: Element | null = event.target instanceof Element ? event.target : null
  while (node !== null) {
    if (node.classList.contains('xterm')) return true
    node = node.parentElement
  }
  return false
}

/** 正在打字的地方：抢键会让用户输入到一半的口令莫名其妙少一个字符。 */
function isTextEntry(event: KeyEvent): boolean {
  const node = event.target as HTMLElement | null
  if (node === null) return false
  const tag = node.tagName
  if (tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT') return true
  return node.isContentEditable === true
}

/**
 * 判定一次按键对应哪个动作。
 *
 * `Cmd` 与 `Ctrl` 视为同一个修饰键（macOS 与其余平台的习惯），`Alt` 单独一档。
 * 返回 null 表示这个键不该被抢，让它继续走原生行为。
 *
 * `inTerminal` 只影响 `Alt+方向键`：那是 readline 的跨词移动，用户在终端里按它
 * 就是想让光标跳一个词；而 `Ctrl+T` / `Ctrl+W` 我们照抢——同类客户端都把这两个
 * 键留给了标签页，终端里想 transpose 的人自有别的写法。
 */
export function match(
  event: KeyEvent,
  ctx: { splitActive: boolean, inTerminal: boolean },
): ((h: ShortcutHandlers) => void) | null {
  const mod = event.ctrlKey || event.metaKey
  // Shift 只出现在 Ctrl+Shift+Tab 这类明确的分档里
  if (mod && !event.altKey) {
    switch (event.key.toLowerCase()) {
      case 't': return h => { h.newLocal() }
      case 'w': return h => { h.closeActive() }
      case 'e': return h => { h.toggleSplit() }
      case ',': return h => { h.openSettings() }
      // readline 里 Ctrl+F 是「光标前移一格」，但同类客户端一律把它给了查找——
      // 单字符移动没人用 Alt 替代，查找却是每次都靠鼠标点菜单的事。
      case 'f': return h => { h.find() }
      // 缩放三件套。`=` 与 `+` 并列为放大：美式布局要打 Ctrl+Shift+= 才出 `+`，
      // 而小键盘的 + 直接就是 '+'，两条路都得认。`-` 与 `0` 不用按 Shift，同理。
      case '=': case '+': return h => { h.zoom(1) }
      case '-': case '_': return h => { h.zoom(-1) }
      case '0': return h => { h.resetZoom() }
      default: break
    }
    if (event.code === 'Tab') {
      return event.shiftKey ? (h => { h.prevTab() }) : (h => { h.nextTab() })
    }
    const digit = /^Digit([1-9])$/.exec(event.code ?? '')
    if (digit) {
      const index = Number(digit[1]) - 1
      return h => { h.selectTab(index) }
    }
    return null
  }
  if (event.altKey && !mod) {
    if (ctx.inTerminal || !ctx.splitActive) return null
    // 两格之间跳焦点；不分屏时没有第二格，让按键回到终端
    if (event.key === 'ArrowRight' || event.key === 'ArrowLeft') {
      return h => { h.focusNext() }
    }
    return null
  }
  return null
}

/** 装好监听，返回卸载函数（App 在 onBeforeUnmount 调用）。 */
export function installShortcuts(
  target: EventTarget,
  getHandlers: () => ShortcutHandlers,
  getSplitActive: () => boolean,
  /**
   * 快捷键总开关。锁屏时传 `() => false`：覆盖层吃掉了界面，但 `document` 的
   * capture 监听比它先看到按键，不拦的话 `Ctrl+T` 会在锁后偷偷开一个终端。
   */
  getEnabled: () => boolean = () => true,
): () => void {
  const onKeydown = (raw: Event) => {
    if (!getEnabled()) return
    const event = raw as unknown as KeyEvent
    // 正在打字时一个键都不抢——除了 Cmd/Ctrl+W，它是关标签而不是输入字符
    if (isTextEntry(event) && !(event.key.toLowerCase() === 'w' && (event.ctrlKey || event.metaKey))) return
    const action = match(event, { splitActive: getSplitActive(), inTerminal: inTerminal(event) })
    if (action === null) return
    raw.preventDefault()
    raw.stopPropagation()
    action(getHandlers())
  }
  target.addEventListener('keydown', onKeydown, true)
  return () => { target.removeEventListener('keydown', onKeydown, true) }
}
