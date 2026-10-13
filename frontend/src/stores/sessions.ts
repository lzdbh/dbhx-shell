import { ref } from 'vue'

import { defineStore } from 'pinia'

import type { LiveEndpoint, Protocol, PtySize, SessionId, SessionInfo, SessionStatus } from '../lib/ipc/bindings'
import { sessionApi } from '../lib/ipc/client'
import { utf8Array } from '../lib/bytes'
import { destroyTerminal } from '../lib/terminal/registry'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'

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
  endpoint: LiveEndpoint | null
  pty: PtySize
  cwd: string | null
  /** 出错时的中文说明，标签页上以角标提示，正文不进状态栏 */
  error: string | null
}

const DEFAULT_PTY: PtySize = { rows: 30, cols: 110 }

export const useSessionsStore = defineStore('sessions', () => {
  const tabs = ref<SessionTab[]>([])
  const activeId = ref<SessionId | null>(null)

  function tab(id: SessionId): SessionTab | undefined {
    return tabs.value.find(t => t.id === id)
  }

  function upsert(info: SessionInfo): SessionTab {
    const existing = tab(info.id)
    if (existing) {
      Object.assign(existing, { label: info.label, status: info.status, endpoint: info.live_endpoint, pty: info.pty })
      return existing
    }
    const created: SessionTab = {
      id: info.id,
      label: info.label,
      protocol: info.protocol,
      status: info.status,
      endpoint: info.live_endpoint,
      pty: info.pty,
      cwd: null,
      error: null,
    }
    tabs.value.push(created)
    return created
  }

  /** 打开远端 shell。失败时把错误留在返回值里给调用方显示，不在这里静默建 tab。 */
  async function openRemote(hostId: string, pty: PtySize = DEFAULT_PTY): Promise<{ ok: boolean, error: string | null }> {
    try {
      const info = await sessionApi.openRemote(hostId, pty)
      upsert(info)
      activeId.value = info.id
      return { ok: true, error: null }
    } catch (err) {
      return { ok: false, error: describeIpcError(normalizeIpcError(err)) }
    }
  }

  async function openLocal(label: string, pty: PtySize = DEFAULT_PTY): Promise<{ ok: boolean, error: string | null }> {
    try {
      const info = await sessionApi.openLocal(
        { protocol: 'local', endpoint: null, auth: null, pty, shell: null, cwd: null, term: 'xterm-256color' },
        label,
      )
      upsert(info)
      activeId.value = info.id
      return { ok: true, error: null }
    } catch (err) {
      return { ok: false, error: describeIpcError(normalizeIpcError(err)) }
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

  /* 下面三个是事件入口，由 lib/ipc/bus.ts 调用 */

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

  return { tabs, activeId, tab, openRemote, openLocal, close, send, reportResize, setCwd, setStatus, markEnded, refresh, activate }
})

function describeExit(exitStatus: number | null, signal: string | null): string {
  if (signal) return `进程被信号 ${signal} 终止`
  if (exitStatus === null) return '连接已关闭'
  return `退出码 ${exitStatus}`
}
