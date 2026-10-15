import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { IpcError, SessionInfo } from '../lib/ipc/bindings'
import { sessionApi } from '../lib/ipc/client'
import { destroyTerminal } from '../lib/terminal/registry'
import { useSessionsStore } from './sessions'
import { useSettingsStore } from './settings'

vi.mock('../lib/ipc/client', () => ({
  sessionApi: {
    openRemote: vi.fn(),
    openLocal: vi.fn(),
    write: vi.fn(),
    resize: vi.fn(),
    close: vi.fn(),
    list: vi.fn(async () => []),
    logStart: vi.fn(),
    logStop: vi.fn(),
  },
}))

vi.mock('../lib/terminal/registry', () => ({ destroyTerminal: vi.fn() }))

const mock = vi.mocked(sessionApi)

function log(path: string, bytes = 0) {
  return { path, bytes }
}

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
    expect(result).toEqual({ ok: true, error: null, id: 1 })
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

  it('starts a log named after the tab title and remembers the path', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    mock.logStart.mockResolvedValueOnce(log('C:\\app\\logs\\db-1-20261014-120000.log'))

    const result = await sessions.toggleLog(1)
    expect(result.ok).toBe(true)
    // 目录没配过就交回后端默认，而不是前端自己编一个
    expect(mock.logStart).toHaveBeenCalledWith(1, null, expect.stringMatching(/^db-1-\d{8}-\d{6}\.log$/))
    expect(sessions.tab(1)?.logPath).toBe('C:\\app\\logs\\db-1-20261014-120000.log')
  })

  it('passes the configured log directory through untouched', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    const settings = useSettingsStore()
    settings.logDir = 'D:\\会话日志'
    mock.logStart.mockResolvedValueOnce(log('D:\\会话日志\\db-1.log'))
    await sessions.toggleLog(1)
    // 前端不做任何拼接：相对路径后端会拒，这里原样传才说得清是谁的错
    expect(mock.logStart).toHaveBeenCalledWith(1, 'D:\\会话日志', expect.any(String))
    // 偏好是写进 localStorage 的，不收回就会漏给同文件的下一个用例
    settings.logDir = ''
  })

  it('a second click stops the same file and reports how much was written', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    mock.logStart.mockResolvedValueOnce(log('a\\db-1.log'))
    await sessions.toggleLog(1)
    mock.logStop.mockResolvedValueOnce(log('a\\db-1.log', 4096))

    const result = await sessions.toggleLog(1)
    expect(result.text).toContain('4096')
    expect(sessions.tab(1)?.logPath).toBeNull()
    expect(mock.logStop).toHaveBeenCalledWith(1)
  })

  it('a log the backend stopped on its own takes the light down with it', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    mock.logStart.mockResolvedValueOnce(log('a\\db-1.log'))
    await sessions.toggleLog(1)

    sessions.markLogStopped(1, 'a\\db-1.log', '写入失败：磁盘已满')
    expect(sessions.tab(1)?.logPath).toBeNull()
    expect(sessions.logNotice).toContain('磁盘已满')
    // 提示只弹一次：取走即清，否则切标签会把同一句旧话再蹦出来
    expect(sessions.takeLogNotice()).toContain('磁盘已满')
    expect(sessions.logNotice).toBeNull()
  })

  it('refetching the list keeps the recording light on', async () => {
    mock.openRemote.mockResolvedValueOnce(info(1))
    const sessions = useSessionsStore()
    await sessions.openRemote('h1')
    mock.logStart.mockResolvedValueOnce(log('a\\db-1.log'))
    await sessions.toggleLog(1)

    // 记录状态只能从命令返回值和事件里来，session_list 里没有这一栏：
    // 重取列表要是把它抹了，用户会觉得灯自己灭了，而文件还在写
    mock.list.mockResolvedValueOnce([info(1)])
    await sessions.refresh()
    expect(sessions.tab(1)?.logPath).toBe('a\\db-1.log')
  })

  it('a notice about a session that is already gone says nothing', () => {
    const sessions = useSessionsStore()
    sessions.markLogStopped(99, 'a\\gone.log', '写入失败')
    expect(sessions.logNotice).toBeNull()
  })
})
