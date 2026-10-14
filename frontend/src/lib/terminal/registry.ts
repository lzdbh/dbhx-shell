import { Terminal } from '@xterm/xterm'
import type { IDecoration, IMarker } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import { SearchAddon } from '@xterm/addon-search'
// xterm 的骨架样式：视口滚动条、隐藏输入框、宽度测量容器全靠它定位。
// 漏掉它不会报错，但那些辅助元素会直接显示在终端上方（输入框的缩放手柄、
// 一行用于测宽度的重复字符），而且 `.xterm-viewport` 高度塌成 0、滚轮翻不了历史。
import '@xterm/xterm/css/xterm.css'

import type { SessionId } from '../ipc/bindings'
import { clipboardApi } from '../ipc/client'
import { decodeBase64 } from '../bytes'
import type { TerminalTheme } from '../../stores/theme'
import { beep } from './beep'
import { compile, scanLine, type CompiledRule, type HighlightRule } from './highlight'

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
  /**
   * `Ctrl+滚轮` 缩放，一格一档，正负号即方向。
   *
   * 由登记处翻译滚轮、由调用方决定字号：字号是全局偏好，归 settings store 管，
   * 这里碰它就得反过来 import store，终端实例从此拖着一个 Vue 依赖。
   */
  onZoom: (step: number) => void
}

interface Entry {
  term: Terminal
  fit: FitAddon
  search: SearchAddon
  element: HTMLDivElement
  resizeObserver: ResizeObserver | null
  /** 最近一次查找的命中情况，由搜索插件的事件回填；没查过就是 null。 */
  found: FindHit | null
  /** 关键字高亮：一处命中一份（标记 + 装饰），内容、视口或规则一变就整批重来。 */
  highlights: { marker: IMarker, decoration: IDecoration }[]
  /** 上一次重画时光标停在哪一行。绝对行号比它靠后的，都是这一轮才写出来的行。 */
  lastCursorLine: number
  /** 上一次重画时那一行的文本。光标会先跳到下一行、内容才落到上一行，光看行号分不出「刚敲完的命令」和「刚打出的报错」。 */
  lastCursorText: string
}

const entries = new Map<SessionId, Entry>()

/** 查找条件，与 xterm 搜索插件的开关一一对应。 */
export interface FindOptions {
  caseSensitive: boolean
  wholeWord: boolean
  regex: boolean
}

/** 一次查找的落点：`index` 是活动那条（0 起，`-1` = 没落点），`total` 是缓冲区里的全部命中数。 */
export interface FindHit {
  index: number
  total: number
}

/**
 * 命中高亮的配色。
 *
 * 非活动的命中只描边不填充：填充色要么跟着主题走（深色底上压深色字、浅色底上压浅色字，
 * 两种里总有一种糊成一团），要么写死（另一个主题下等于看不见）。描边在两边都读得出来。
 *
 * `activeMatchBackground` 只有 canvas 渲染器会画；我们现在用的是默认的 DOM 渲染器，
 * 它只认 `outline`。所以当前那一条实际靠两件事认出来：搜索插件顺手把它 `select` 了
 * （主题的选择底色），以及描边换了更深的 `activeMatchBorder`。
 */
const FIND_DECORATIONS = {
  matchBorder: '#e0a800',
  matchOverviewRuler: '#e0a800',
  activeMatchBackground: '#ffd54f',
  activeMatchBorder: '#8a6d00',
  activeMatchColorOverviewRuler: '#ff8f00',
}

/**
 * 关键字高亮的规则，全局一份：这是「我看日志时什么算扎眼」的偏好，
 * 不是每会话属性——同一台机器上开十个终端，盯的是同一批词。
 */
let highlightRules: CompiledRule[] = []

/** 换成半透明：装饰元素盖在文字上面画，不透明就把字整个吃掉了。 */
function tint(hex: string): string {
  const value = Number.parseInt(hex.slice(1), 16)
  return `rgba(${(value >> 16) & 0xff}, ${(value >> 8) & 0xff}, ${value & 0xff}, 0.45)`
}

/**
 * 换一批关键字规则，所有终端立刻重画。
 *
 * 设置页每改一个字符都会走到这里，所以它必须便宜且不抛：编译失败的规则在
 * `compile()` 里已经降级成「不参与匹配」，重画只是把旧的装饰摘掉再挂一批。
 */
export function setHighlightRules(rules: HighlightRule[]): void {
  highlightRules = compile(rules)
  entries.forEach((_, id) => { repaintHighlights(id) })
}

/**
 * 重画当前视口里的关键字底色。
 *
 * 只画视口，不画整个回滚缓冲：装饰要占一个锚定标记，八千行全画会在滚动时
 * 拖住渲染；而滚出去的部分本来也看不见，滚回来时 `onScroll` 会再画一次。
 */
function repaintHighlights(id: SessionId): void {
  const entry = entries.get(id)
  if (!entry) return
  for (const held of entry.highlights) {
    held.decoration.dispose()
    held.marker.dispose()
  }
  entry.highlights = []
  const term = entry.term
  const buffer = term.buffer.active
  // 标记的偏移是相对光标行算的，先拿到这个基准
  const anchor = buffer.baseY + buffer.cursorY
  const cursorRow = buffer.getLine(anchor)
  const cursorText = cursorRow ? cursorRow.translateToString(true) : ''
  if (highlightRules.length === 0) {
    entry.lastCursorLine = anchor
    entry.lastCursorText = cursorText
    return
  }
  let shouldRing = false
  // 第一次重画面对的是建实例时攒下来的那批输出（登录横幅、暂存区补写），
  // 内容对用户来说全是「旧的」，不该因为里面恰好有个 error 就吵人。
  const first = entry.lastCursorLine < 0
  for (let row = 0; row < term.rows; row += 1) {
    const lineIndex = buffer.viewportY + row
    const line = buffer.getLine(lineIndex)
    if (!line) continue
    const text = line.translateToString(true)
    const hits = scanLine(text, highlightRules)
    // 光标会先跳到下一行、内容才落进上一行，所以「行号大于上次的光标行」并不足以
    // 判定这一行是新写的：回车之后那一行往往已经等于上次的光标行。补一条文本比对，
    // 就能把「用户刚敲完的命令回显」（文本没变）和「刚打出来的报错」（文本变了）分开。
    const fresh = lineIndex > entry.lastCursorLine
      || (lineIndex === entry.lastCursorLine && text !== entry.lastCursorText)
    for (const hit of hits) {
      const marker = term.registerMarker(lineIndex - anchor)
      // 已经掉出缓冲区的行（标记 line 为 -1）不值得再挂装饰
      if (marker.line < 0) continue
      const decoration = term.registerDecoration({ marker, x: hit.start, width: hit.end - hit.start })
      if (!decoration) continue
      const rule = highlightRules[hit.rule].rule
      decoration.onRender(element => { element.style.backgroundColor = tint(rule.background) })
      entry.highlights.push({ marker, decoration })
      // 响铃只认「这一轮刚刚写完的整行」：行要新（上面算的 fresh），又要已经换行收尾
      // （行号小于这次的光标行）。判据不能用视口底行——报错行后面还跟着提示符，
      // 永远不是最后一行，实测一条铃都响不起来。而光标那一行很可能正是用户正在敲的
      // 命令，敲到 `echo FATAL` 的最后一个字母就响等于自己吓自己。
      if (rule.bell && !first && fresh && lineIndex < anchor) shouldRing = true
    }
  }
  if (shouldRing) beep()
  // 无条件跟进：清屏或回滚区被截断时绝对行号会倒退，停在旧值会把之后的真输出当成历史
  entry.lastCursorLine = anchor
  entry.lastCursorText = cursorText
}

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
  const search = new SearchAddon()
  term.loadAddon(search)
  // 计数只在「有命中」时发事件，所以 `runFind` 里先归零、这里只负责回填。
  // 事件是同步发的（就在 findNext 返回之前），因此那一步能读到当次的值。
  search.onDidChangeResults(result => {
    const held = entries.get(id)
    if (held) held.found = { index: result.resultIndex, total: result.resultCount }
  })
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
  element.addEventListener('wheel', event => {
    // Ctrl+滚轮 = 缩放。必须 preventDefault：不拦的话这条滚轮既会被 xterm 当成
    // 翻历史滚走，又会被 WebView2 当成页面缩放（后者会把整个界面一起放大）。
    if (!event.ctrlKey) return
    event.preventDefault()
    handlers.onZoom(event.deltaY < 0 ? 1 : -1)
  }, { passive: false })
  const entry: Entry = { term, fit, search, element, resizeObserver: null, found: null, highlights: [], lastCursorLine: -1, lastCursorText: '' }
  entries.set(id, entry)
  term.open(element)
  // 关键字高亮跟着三件事重画：内容变了、视口滚了、格子改大小了。屏幕上的行只在这
  // 三个时刻会变，别去 `onRender` 里每帧重扫一遍。
  term.onWriteParsed(() => { repaintHighlights(id) })
  term.onScroll(() => { repaintHighlights(id) })
  term.onResize(() => { repaintHighlights(id) })
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

/**
 * 在当前会话的输出里找下一处（`forward` 为假时找上一处）。
 *
 * 返回 `null` 只有一种情况：这个会话还没有终端。空串按「取消高亮」处理——
 * 查找条一边打字一边调用它，删空时不该把上一次的高亮留在屏上。
 */
export function findText(id: SessionId, text: string, options: FindOptions, forward: boolean): FindHit | null {
  const entry = entries.get(id)
  if (!entry) return null
  if (text === '') {
    clearFind(id)
    return null
  }
  // 先归零：搜索插件在「一条都没中」时不发事件，不重置就会把上一次的计数留在屏上
  entry.found = { index: -1, total: 0 }
  const searchOptions = {
    caseSensitive: options.caseSensitive,
    wholeWord: options.wholeWord,
    regex: options.regex,
    decorations: FIND_DECORATIONS,
  }
  try {
    if (forward) entry.search.findNext(text, searchOptions)
    else entry.search.findPrevious(text, searchOptions)
  } catch (err) {
    // 正则写到一半（`ite(m`）时插件里的 `RegExp()` 会直接抛，而且抛在它「清空上一次
    // 的高亮、但还没记下新关键词」中间——不清干净，接下来重打同一个词会因为命中缓存
    // 而只挪动当前那条，计数永远停在 0。异常也绝不能逃出去：查找条的计数会卡旧值。
    entry.search.clearDecorations()
    console.warn('查找表达式求值失败', err)
  }
  return entry.found
}

/** 抹掉查找高亮。关掉查找条时必须调，否则黄色那格会一直留在屏上。 */
export function clearFind(id: SessionId): void {
  const entry = entries.get(id)
  if (!entry) return
  entry.search.clearDecorations()
  entry.search.clearActiveDecoration()
  entry.found = null
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

/**
 * 字号影响所有终端：它是全局偏好，不是每会话属性。
 *
 * 改完必须立刻重新 fit：宿主元素是 `position:absolute; inset:0` 撑满格子的，
 * 字号变了它的尺寸却一点没变，ResizeObserver 不会替我们响。不重新算行列，
 * 终端会继续按旧字号的行数往新尺寸里画——放大时下半截被裁掉，缩小时空一圈白边。
 */
export function applyFontSizeAll(fontSize: number): void {
  entries.forEach((entry, id) => {
    entry.term.options.fontSize = fontSize
    fitTerminal(id)
  })
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
