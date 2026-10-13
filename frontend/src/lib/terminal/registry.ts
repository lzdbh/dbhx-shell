import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'

import type { SessionId } from '../ipc/bindings'
import { decodeBase64 } from '../bytes'
import type { TerminalTheme } from '../../stores/theme'

/**
 * xterm 实例的登记处。
 *
 * **这里放的是模块级 `Map`，不是 Pinia state。** 终端实例一旦被 Vue 的 `reactive`
 * 包住，Proxy 会接管它内部的 BufferLine 与写解码器状态；xterm 在几 KB 输出之后就会
 * 因为这些内部数组被反复代理而明显掉帧，甚至在 `write` 回调里读到代理后的值崩掉。
 *
 * 分屏把另一件事也推到这里：**DOM 归本模块管**。终端的宿主元素由登记处创建，
 * `attachTerminal` 只是把它搬进某个格子。于是「切换钉住的会话」「从分屏收回单屏」
 * 都只是 `appendChild`，实例和它的回滚缓冲区从头到尾没有重建过——如果元素写在
 * 组件模板里，Vue 卸载组件就把 canvas 连同一屏日志一起丢了。
 */
export interface TerminalHandlers {
  /** 键盘输入与粘贴，原始字符串 */
  onData: (data: string) => void
  /**  fit 之后的新尺寸，用于向上游报告  */
  onResize: (rows: number, cols: number) => void
}

interface Entry {
  term: Terminal
  fit: FitAddon
  element: HTMLDivElement
  resizeObserver: ResizeObserver | null
}

const entries = new Map<SessionId, Entry>()

/**
 * 取回或惰性创建某个会话的终端元素。
 *
 * 创建时机是「第一次被挂进格子」而不是「tab 出现」：恢复会话、切 tab、分屏都只是
 * 把已存在的元素挪位置，挪多少次都不会丢状态。
 */
export function ensureTerminal(
  id: SessionId,
  theme: TerminalTheme,
  fontSize: number,
  handlers: TerminalHandlers,
): HTMLDivElement {
  const existing = entries.get(id)
  if (existing) return existing.element
  const term = new Terminal({
    allowProposedApi: true,
    scrollback: 8000,
    fontSize,
    // 选中即复制的起点：翻日志时双击选词比拖拽省事
    rightClickSelectsWord: true,
    theme: toXtermTheme(theme),
  })
  const fit = new FitAddon()
  term.loadAddon(fit)
  term.onData(data => { handlers.onData(data) })
  term.onResize(({ rows, cols }) => { handlers.onResize(rows, cols) })
  const element = document.createElement('div')
  element.className = 'xterm-host'
  const entry: Entry = { term, fit, element, resizeObserver: null }
  entries.set(id, entry)
  term.open(element)
  // 格子大小由 CSS 决定（分屏比例、侧栏拖动都会改它），ResizeObserver 是唯一
  // 不用把布局状态反灌进这里的通知方式
  entry.resizeObserver = new ResizeObserver(() => { fitTerminal(id) })
  entry.resizeObserver.observe(element)
  fitTerminal(id)
  return element
}

/** 把终端搬进 `slot`。已经在里面时什么都不做，避免无谓的 reflow。 */
export function attachTerminal(id: SessionId, slot: HTMLElement): void {
  const entry = entries.get(id)
  if (!entry) return
  if (entry.element.parentElement !== slot) slot.appendChild(entry.element)
  fitTerminal(id)
}

/**
 * 只在这个格子确实是它当前宿主时才把元素摘走。
 *
 * 分屏切换时两个格子会一前一后认领同一个会话：如果右格卸载时无条件摘除，
 * 刚被主格接手的终端就会被抹掉，屏幕上只剩空白。当前宿主判断让后到的认领赢。
 */
export function detachTerminal(id: SessionId, from: HTMLElement): void {
  const entry = entries.get(id)
  if (entry && entry.element.parentElement === from) entry.element.remove()
}

export function focusTerminal(id: SessionId | null): void {
  if (id === null) return
  entries.get(id)?.term.focus()
}

export function lookup(id: SessionId): Terminal | undefined {
  return entries.get(id)?.term
}

/** 一段 base64 输出直接写进终端；没有终端（还没挂进格子）时丢弃是安全的：
 *  回滚缓冲由服务端的重绘负责，我们不替它记账。 */
export function writeTerminal(id: SessionId, data: string): void {
  entries.get(id)?.term.write(new Uint8Array(decodeBase64(data)))
}

export function applyTheme(id: SessionId, theme: TerminalTheme): void {
  const entry = entries.get(id)
  if (entry) entry.term.options.theme = toXtermTheme(theme)
}

/** 字号影响所有终端：它是全局偏好，不是每会话属性。 */
export function applyFontSizeAll(fontSize: number): void {
  entries.forEach(entry => { entry.term.options.fontSize = fontSize })
}

export function fitTerminal(id: SessionId): void {
  const entry = entries.get(id)
  if (!entry) return
  try {
    entry.fit.fit()
  } catch {
    // 容器还没尺寸（隐藏格子、启动瞬间）时 FitAddon 会抛；下一次 resize 会补上
  }
}

export function destroyTerminal(id: SessionId): void {
  const entry = entries.get(id)
  if (!entry) return
  entry.resizeObserver?.disconnect()
  entry.term.dispose()
  // dispose 不会替我们摘掉自己造的节点，留着它占位会让下一个会话挂进来时多一块空白
  entry.element.remove()
  entries.delete(id)
}

/**
 * 调色板索引 ⇒ xterm 的具名字段。
 *
 * 顺序是 ANSI 的约定：0-7 常规、8-15 高亮，中间不能漏 `brightBlack`（很多 tmux 状态栏用它）。
 */
const ANSI_NAMES = [
  'black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white',
  'brightBlack', 'brightRed', 'brightGreen', 'brightYellow', 'brightBlue', 'brightMagenta', 'brightCyan', 'brightWhite',
] as const

function toXtermTheme(theme: TerminalTheme) {
  const colors: Record<string, string> = {}
  theme.ansi.forEach((color, index) => {
    colors[ANSI_NAMES[index]] = color
  })
  return {
    background: theme.background,
    foreground: theme.foreground,
    cursor: theme.cursor,
    cursorAccent: theme.cursorAccent,
    selectionBackground: theme.selectionBackground,
    ...colors,
  }
}
