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
 * 用模块级 Map 是唯一可靠的办法：响应式系统根本不知道这些实例存在。
 *
 * 于是终端字节流的路径也是最短的：`plugin:dbhx:term-data` ⇒ 这里 `write`，
 * 中间不经过任何 store、不产生任何响应式写入。
 */
export interface TerminalHandle {
  term: Terminal
  fit: FitAddon
}

const handles = new Map<SessionId, TerminalHandle>()

export function createTerminal(id: SessionId, theme: TerminalTheme): TerminalHandle {
  const term = new Terminal({
    allowProposedApi: true,
    scrollback: 8000,
    // 选中即复制的起点：翻日志时双击选词比拖拽省事
    rightClickSelectsWord: true,
    theme: toXtermTheme(theme),
  })
  const handle = { term, fit: new FitAddon() }
  handles.set(id, handle)
  return handle
}

export function lookup(id: SessionId): TerminalHandle | undefined {
  return handles.get(id)
}

/** 一段 base64 输出直接写进终端；没有终端（tab 还没挂载）时丢弃是安全的：xterm 的
 *  回滚缓冲由服务端的重绘负责，我们不替它记账。 */
export function writeTerminal(id: SessionId, data: string): void {
  handles.get(id)?.term.write(new Uint8Array(decodeBase64(data)))
}

export function applyTheme(id: SessionId, theme: TerminalTheme): void {
  const handle = handles.get(id)
  if (handle) handle.term.options.theme = toXtermTheme(theme)
}

export function fitTerminal(id: SessionId): void {
  const handle = handles.get(id)
  if (!handle) return
  try {
    handle.fit.fit()
  } catch {
    // 容器还没尺寸（隐藏 tab、启动瞬间）时 FitAddon 会抛；下一次 resize 会补上
  }
}

export function destroyTerminal(id: SessionId): void {
  const handle = handles.get(id)
  if (!handle) return
  handle.term.dispose()
  handles.delete(id)
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
