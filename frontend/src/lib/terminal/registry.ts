import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
// xterm 的骨架样式：视口滚动条、隐藏输入框、宽度测量容器全靠它定位。
// 漏掉它不会报错，但那些辅助元素会直接显示在终端上方（输入框的缩放手柄、
// 一行用于测宽度的重复字符），而且 `.xterm-viewport` 高度塌成 0、滚轮翻不了历史。
import '@xterm/xterm/css/xterm.css'

import type { SessionId } from '../ipc/bindings'
import { clipboardApi } from '../ipc/client'
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
 * 还没人认领的会话，输出先在这儿排着。
 *
 * 建 tab 与建终端是两件事：`session_open_remote` 返回、tab 进 store、格子挂载、
 * `ensureTerminal` 造实例，中间隔着好几次异步与渲染帧。远端的登录横幅
 * （`Welcome to Ubuntu …`、`Last login: …`）正好落在这段缝里——它只在登录那一刻
 * 打一次，之后没有任何东西会重画它。原先这里直接丢弃，于是连上主机后只剩一个空提示符，
 * 用户以为横幅没发过来。攒起来等实例建好一次性补写就对了。
 *
 * 上限是硬约束：后台 tab 一直没人看也不能把输出堆到内存吃紧。超了就丢最老的——
 * 掉的是最早几屏日志，比整段卡死或 OOM 都轻。
 */
interface Pending {
  chunks: Uint8Array[]
  bytes: number
}

const pending = new Map<SessionId, Pending>()
const PENDING_CAP = 512 * 1024

function stashPending(id: SessionId, data: Uint8Array): void {
  const held = pending.get(id) ?? { chunks: [], bytes: 0 }
  held.chunks.push(data)
  held.bytes += data.byteLength
  while (held.bytes > PENDING_CAP && held.chunks.length > 1) {
    const dropped = held.chunks.shift()
    if (dropped) held.bytes -= dropped.byteLength
  }
  pending.set(id, held)
}

function takePending(id: SessionId): Uint8Array | null {
  const held = pending.get(id)
  pending.delete(id)
  if (!held || held.chunks.length === 0) return null
  if (held.chunks.length === 1) return held.chunks[0] ?? null
  const joined = new Uint8Array(held.bytes)
  let at = 0
  for (const chunk of held.chunks) {
    joined.set(chunk, at)
    at += chunk.byteLength
  }
  return joined
}

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
    // 右键留给粘贴（有选区时先复制选区），选中靠双击取词，这是同类 SSH 客户端的习惯
    rightClickSelectsWord: false,
    theme: toXtermTheme(theme),
  })
  const fit = new FitAddon()
  term.loadAddon(fit)
  term.onData(data => { handlers.onData(data) })
  term.onResize(({ rows, cols }) => { handlers.onResize(rows, cols) })
  const element = document.createElement('div')
  element.className = 'xterm-host'
  term.attachCustomKeyEventHandler(event => handleClipboardKey(term, event))
  element.addEventListener('contextmenu', event => {
    // 不拦的话 WebView2 会弹自己的菜单，用户点不到也复制不了
    event.preventDefault()
    if (term.hasSelection()) void copyText(term.getSelection())
    else void pasteInto(term)
  })
  const entry: Entry = { term, fit, element, resizeObserver: null }
  entries.set(id, entry)
  term.open(element)
  const early = takePending(id)
  if (early) term.write(early)
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

/** 一段 base64 输出写进终端；终端还没建时先进暂存区，建好即补写，一个字节都不丢。 */
export function writeTerminal(id: SessionId, data: string): void {
  const bytes = new Uint8Array(decodeBase64(data))
  const entry = entries.get(id)
  if (entry) entry.term.write(bytes)
  else stashPending(id, bytes)
}

/**
 * 剪贴板按键：有选区时 `Ctrl/Cmd+C` 是复制，没有才让 xterm 把 `^C` 发给 shell；
 * `Ctrl/Cmd+V` 从剪贴板取文本，交给 `term.paste`（它会按远端是否开启括号粘贴模式处理）。
 *
 * 返回 `false` 表示这个键由我们接管，xterm 不再产生对应的输入。
 */
export function handleClipboardKey(term: Terminal, event: KeyboardEvent): boolean {
  if (event.type !== 'keydown') return true
  if (!(event.ctrlKey || event.metaKey) || event.altKey) return true
  // 只认不带 Shift 的组合：`Ctrl+Shift+C` 那类留给 shell 自己的绑定
  if (event.shiftKey) return true
  switch (event.key.toLowerCase()) {
    case 'c':
      if (!term.hasSelection()) return true
      void copyText(term.getSelection())
      return false
    case 'v':
      void pasteInto(term)
      return false
    default:
      return true
  }
}

/**
 * 写剪贴板失败只是复制没生效，不该打断终端。
 *
 * 读写都走后端命令而不是 `navigator.clipboard`——见 client.ts 里 `clipboardApi` 的说明：
 * WebView2 会把读剪贴板当成网页权限，弹一次原生授权气泡，用户拒绝后终端粘贴就再也用不了。
 */
async function copyText(text: string): Promise<void> {
  if (text === '') return
  try {
    await clipboardApi.write(text)
  } catch (err) {
    console.warn('复制到剪贴板失败', err)
  }
}

async function pasteInto(term: Terminal): Promise<void> {
  try {
    const text = await clipboardApi.read()
    if (text !== '') term.paste(text)
  } catch (err) {
    console.warn('读取剪贴板失败', err)
  }
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
  pending.delete(id)
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
