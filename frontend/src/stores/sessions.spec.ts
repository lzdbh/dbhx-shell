import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { IpcError, SessionInfo } from '../lib/ipc/bindings'
import { sessionApi } from '../lib/ipc/client'
import { destroyTerminal } from '../lib/terminal/registry'
import { useSessionsStore } from './sessions'

vi.mock('../lib/ipc/client', () => ({
  sessionApi: {
    openRemote: vi.fn(),
    openLocal: vi.fn(),
    write: vi.fn(),
    resize: vi.fn(),
    close: vi.fn(),
    list: vi.fn(async () => []),
  },
}))

vi.mock('../lib/terminal/registry', () => ({ destroyTerminal: vi.fn() }))

const mock = vi.mocked(sessionApi)

function info(id: number, patch: Partial<SessionInfo> = {}): SessionInfo {
  return {
    id,
    protocol: 'ssh',
    status: 'connected',
    label: `db-${id}`,
    host_id: null,
    live_endpoint: { host: '10.0.0.1', port: 22, username: 'app' },
    pty: { rows: 30, cols: 110 },
    encoding: 'utf8',
    ...patch,
  }
}

function ipcError(code: IpcError['code'], message: string): IpcError {
  return { code, message, retryable: false }
}

beforeEach(() => {
  vi.clearAllMocks()
  setActivePinia(createPinia())
  mock.close.mockResolvedValue(null)
  mock.resize.mockResolvedValue(null)
  mock.write.mockResolvedValue(null)
})

describe('sessions store', () => {
  it('opens a remote tab and makes it active', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    const result = await sessions.openRemote('h1')
    expect(result).toEqual({ ok: true, error: null })
    expect(sessions.tabs.map(t => t.id)).toEqual([1])
    expect(sessions.activeId).toBe(1)
  })

  it('keeps the failure reason out of the tab list', async () => {
    mock.openRemote.mockRejectedValueOnce(ipcError('auth_failed', 'Permission denied (publickey)'))
    const sessions = useSessionsStore()
    const result = await sessions.openRemote('h1')
    expect(result.ok).toBe(false)
    // 文案来自 errors.ts 的 code 映射，不是裸的英文服务端消息
    expect(result.error).toContain('认证被服务器拒绝')
    expect(sessions.tabs).toHaveLength(0)
  })

  it('sends a resize only when the size actually changed', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    await sessions.reportResize(1, { rows: 30, cols: 110 })
    expect(mock.resize).not.toHaveBeenCalled()
    await sessions.reportResize(1, { rows: 40, cols: 120 })
    expect(mock.resize).toHaveBeenCalledWith(1, { rows: 40, cols: 120 })
  })

  it('leaves an ended tab visible with its exit reason', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    sessions.markEnded(1, 1, null)
    expect(sessions.tabs).toHaveLength(1)
    expect(sessions.tabs[0].status).toBe('closed')
    expect(sessions.tabs[0].error).toBe('退出码 1')
    sessions.markEnded(1, null, 'SIGHUP')
    expect(sessions.tabs[0].error).toBe('进程被信号 SIGHUP 终止')
  })

  it('rebuilds from the backend list without losing event-only fields', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    sessions.setCwd(1, '/var/log')
    mock.list.mockResolvedValueOnce([info(1, { label: '改名了' }), info(2)])
    await sessions.refresh()
    expect(sessions.tab(1)?.cwd).toBe('/var/log')
    expect(sessions.tab(1)?.label).toBe('改名了')
    expect(sessions.tab(2)).toBeDefined()
    // 后端已经没有的会话，前端不该继续显示
    mock.list.mockResolvedValueOnce([info(2)])
    await sessions.refresh()
    expect(sessions.tabs.map(t => t.id)).toEqual([2])
  })

  it('carries the vault host through so the file window can be opened', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1, { host_id: 'h1' }))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    expect(sessions.tab(1)?.hostId).toBe('h1')
    // 重开窗口走的是 session_list：这一趟之后 hostId 还在，按钮才不会变成永久灰着
    mock.list.mockResolvedValueOnce([info(1, { host_id: 'h1' })])
    await sessions.refresh()
    expect(sessions.tab(1)?.hostId).toBe('h1')
  })

  it('writes a failed keystroke onto the tab', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    mock.write.mockRejectedValueOnce(ipcError('channel_closed', 'channel gone'))
    await sessions.send(1, 'ls\r')
    expect(sessions.tab(1)?.error).toContain('连接已关闭')
  })

  it('destroys the terminal before dropping the tab', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    await sessions.close(1)
    expect(destroyTerminal).toHaveBeenCalledWith(1)
    expect(sessions.tabs).toHaveLength(0)
    expect(sessions.activeId).toBeNull()
  })
})
