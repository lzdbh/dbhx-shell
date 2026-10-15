import { ref } from 'vue'

import { defineStore } from 'pinia'

import type { LiveEndpoint, Protocol, PtySize, SessionId, SessionInfo, SessionStatus } from '../lib/ipc/bindings'
import { sessionApi } from '../lib/ipc/client'
import { utf8Array } from '../lib/bytes'
import { destroyTerminal } from '../lib/terminal/registry'
import { logFileName } from '../lib/terminal/logName'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'
import { useSettingsStore } from './settings'

/**
 * 一个标签页。
 *
 * `cwd` 只来自 `plugin:dbhx:session-cwd` 事件：契约上的 `SessionInfo` 没有目录字段，
 * 后端也不在打开时回读——首次报告之前它就是 `null`，UI 要能接受「还不知道」。
 */
export interface SessionTab {
  id: SessionId
  label: string
  protocol: Protocol
  status: SessionStatus
  /** 来自 vault 的哪台主机；本机终端为 null。文件管理窗口按它开。 */
  hostId: string | null
  endpoint: LiveEndpoint | null
  pty: PtySize
  cwd: string | null
  /** 出错时的中文说明，标签页上以角标提示，正文不进状态栏 */
  error: string | null
  /** 正在记录的日志文件全路径；`null` 表示这个会话没在记。 */
  logPath: string | null
}

const DEFAULT_PTY: PtySize = { rows: 30, cols: 110 }

/** 一次日志开关的结果，直接是给 UI 看的那句话。 */
export interface LogToggleResult {
  ok: boolean
  text: string
}

/**
 * 一次拨号的结果。
 *
 * 除了成不成功，还要把新会话号交回去：宫格视图是「把这台机器放进第三格」，
 * 光知道连上了不够，得知道连上的是哪一个会话。
 */
export type OpenResult = { ok: boolean, error: string | null, id: SessionId | null }

export const useSessionsStore = defineStore('sessions', () => {
  const tabs = ref<SessionTab[]>([])
  const activeId = ref<SessionId | null>(null)
  /**
   * 日志**自己**停了的那次说明。
   *
   * 只有一处会写它：后端写到一半失败（磁盘满是常见的一种）。组件读它弹提示，
   * 弹完立刻清空——放在 store 里是为了让「指示灯复位」和「给用户一句话」出自同一个事实，
   * 不至于指示灯灭了、用户却不知道为什么会了。
   */
  const logNotice = ref<string | null>(null)
  const settings = useSettingsStore()

  function tab(id: SessionId): SessionTab | undefined {
    return tabs.value.find(t => t.id === id)
  }

  function upsert(info: SessionInfo): SessionTab {
    const existing = tab(info.id)
    if (existing) {
      Object.assign(existing, { label: info.label, status: info.status, hostId: info.host_id, endpoint: info.live_endpoint, pty: info.pty })
      return existing
    }
    const created: SessionTab = {
      id: info.id,
      label: info.label,
      protocol: info.protocol,
      status: info.status,
      hostId: info.host_id,
      endpoint: info.live_endpoint,
      pty: info.pty,
      cwd: null,
      error: null,
      logPath: null,
    }
    tabs.value.push(created)
    return created
  }

  /** 打开远端 shell。失败时把错误留在返回值里给调用方显示，不在这里静默建 tab。 */
  async function openRemote(hostId: string, pty: PtySize = DEFAULT_PTY): Promise<OpenResult> {
    try {
      const info = await sessionApi.openRemote(hostId, pty)
      upsert(info)
      activeId.value = info.id
      return { ok: true, error: null, id: info.id }
    } catch (err) {
      return { ok: false, error: describeIpcError(normalizeIpcError(err)), id: null }
    }
  }

  async function openLocal(label: string, pty: PtySize = DEFAULT_PTY): Promise<OpenResult> {
    try {
      const info = await sessionApi.openLocal(
        { protocol: 'local', endpoint: null, auth: null, pty, shell: null, cwd: null, term: 'xterm-256color' },
        label,
      )
      upsert(info)
      activeId.value = info.id
      return { ok: true, error: null, id: info.id }
    } catch (err) {
      return { ok: false, error: describeIpcError(normalizeIpcError(err)), id: null }
    }
  }

  async function close(id: SessionId): Promise<void> {
    // 先摘本地再发命令：终端实例留在内存里比「后端多关一次」糟得多
    destroyTerminal(id)
    tabs.value = tabs.value.filter(t => t.id !== id)
    if (activeId.value === id) activeId.value = tabs.value[0]?.id ?? null
    try {
      await sessionApi.close(id)
    } catch (err) {
      console.warn('关闭会话失败', normalizeIpcError(err).code)
    }
  }

  /** 键盘输入。UTF-8 编码在这层做，组件只管把字符串交出来。 */
  async function send(id: SessionId, text: string): Promise<void> {
    try {
      await sessionApi.write(id, utf8Array(text))
    } catch (err) {
      // 输入失败意味着通道已经没了，标签页要显示出来而不是只弹一条 toast
      const target = tab(id)
      if (target) target.error = describeIpcError(normalizeIpcError(err))
    }
  }

  async function reportResize(id: SessionId, pty: PtySize): Promise<void> {
    const target = tab(id)
    if (!target) return
    if (target.pty.rows === pty.rows && target.pty.cols === pty.cols) return
    target.pty = pty
    try {
      await sessionApi.resize(id, pty)
    } catch (err) {
      console.warn('改尺寸失败', normalizeIpcError(err).code)
    }
  }

  /* 下面是事件入口与日志开关，事件那一组由 lib/ipc/bus.ts 调用 */

  /**
   * 后端自己停了日志（写不下去）。灯必须跟着灭，理由留给 UI 说一句。
   *
   * 按会话号清，不按路径核对：事件说的是「这个会话不再写了」，这就是权威结论。
   * 前端手上那个路径就算和它对不上，也只会让提示里的路径旧一格，不值得为它整体重取。
   */
  function markLogStopped(id: SessionId, path: string, reason: string): void {
    const target = tab(id)
    if (!target) return
    target.logPath = null
    logNotice.value = `${reason}，日志已停在 ${path}`
  }

  function takeLogNotice(): string | null {
    const text = logNotice.value
    logNotice.value = null
    return text
  }

  /**
   * 开始记录某个会话的输出。`dir` 留空时后端落在自己的应用数据目录下。
   *
   * 文件名在这里按**标签页标题 + 本地时刻**生成：同一个会话反复开关会攒出多份日志，
   * 只有秒级时间戳能把它们分开；标题让用户不必打开文件就知道是哪一屏。
   */
  async function startLog(id: SessionId): Promise<LogToggleResult> {
    const target = tab(id)
    if (!target) return { ok: false, text: '会话已经不在了' }
    // 首尾空白是粘路径时带进来的，后端会当成另一个目录，这里先收掉；全空才是「不指定」
    const dir = settings.logDir.trim()
    try {
      const summary = await sessionApi.logStart(id, dir === '' ? null : dir, logFileName(target.label, new Date()))
      target.logPath = summary.path
      return { ok: true, text: `正在记录到 ${summary.path}` }
    } catch (err) {
      return { ok: false, text: describeIpcError(normalizeIpcError(err)) }
    }
  }

  async function stopLog(id: SessionId): Promise<LogToggleResult> {
    const target = tab(id)
    try {
      const summary = await sessionApi.logStop(id)
      if (target) target.logPath = null
      return { ok: true, text: `日志已写入 ${summary.path}（${summary.bytes} 字节）` }
    } catch (err) {
      const error = normalizeIpcError(err)
      // 后端说「没在记」⇒ 前端的亮灯是错的，就地灭掉，别让下一次点击变成「开始」
      if (error.code === 'not_found' && target) target.logPath = null
      return { ok: false, text: describeIpcError(error) }
    }
  }

  /** 标签页上的开关：没在记就开始，正在记就停。 */
  async function toggleLog(id: SessionId): Promise<LogToggleResult> {
    const target = tab(id)
    if (!target) return { ok: false, text: '会话已经不在了' }
    return target.logPath === null ? startLog(id) : stopLog(id)
  }

  function setCwd(id: SessionId, path: string): void {
    const target = tab(id)
    if (target) target.cwd = path
  }

  function setStatus(id: SessionId, status: SessionStatus, error: string | null = null): void {
    const target = tab(id)
    if (target) {
      target.status = status
      target.error = error
    }
  }

  /** 会话结束。tab 不自动消失：用户要能看见退出码并自己关掉。 */
  function markEnded(id: SessionId, exitStatus: number | null, signal: string | null): void {
    const target = tab(id)
    if (!target) return
    target.status = 'closed'
    target.error = exitStatus === 0 ? null : describeExit(exitStatus, signal)
  }

  /** 应用启动时把后端已有的会话拉回来（会话恢复也走这里）。 */
  async function refresh(): Promise<void> {
    try {
      const list = await sessionApi.list()
      // 已有的 tab 保留 cwd 与错误：那两个值只能从事件里来，重取列表不该把它们抹掉
      const known = new Map(tabs.value.map(t => [t.id, t]))
      tabs.value = list.map((info) => {
        const old = known.get(info.id)
        if (!old) return upsert(info)
        old.label = info.label
        old.status = info.status
        old.endpoint = info.live_endpoint
        old.pty = info.pty
        return old
      })
      if (!activeId.value || !tab(activeId.value)) activeId.value = tabs.value[0]?.id ?? null
    } catch (err) {
      console.warn('拉取会话列表失败', normalizeIpcError(err).code)
    }
  }

  function activate(id: SessionId): void {
    activeId.value = id
  }

  return {
    tabs,
    activeId,
    logNotice,
    tab,
    openRemote,
    openLocal,
    close,
    send,
    reportResize,
    setCwd,
    setStatus,
    markEnded,
    markLogStopped,
    takeLogNotice,
    toggleLog,
    refresh,
    activate,
  }
})

function describeExit(exitStatus: number | null, signal: string | null): string {
  if (signal) return `进程被信号 ${signal} 终止`
  if (exitStatus === null) return '连接已关闭'
  return `退出码 ${exitStatus}`
}
