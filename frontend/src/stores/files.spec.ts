import { createPinia, setActivePinia } from 'pinia'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import type { DirListing, FileEntry, PanelInfo } from '../lib/ipc/bindings'
import { localApi, sftpApi } from '../lib/ipc/client'
import { useFilesStore } from './files'
import { useSettingsStore } from './settings'
import { useTransfersStore } from './transfers'

vi.mock('../lib/ipc/client', () => ({
  sftpApi: {
    openForSession: vi.fn(),
    openForHost: vi.fn(),
    browse: vi.fn(),
    setFollowing: vi.fn(),
    setEditing: vi.fn(),
    mkdir: vi.fn(),
    createFile: vi.fn(),
    chmod: vi.fn(),
    remove: vi.fn(),
    rename: vi.fn(),
    download: vi.fn(),
    upload: vi.fn(),
    close: vi.fn(),
  },
  // 编辑往返要用本机侧，测试里全部给假实现：真去碰 temp 目录会让用例之间互相残留
  localApi: {
    editDir: vi.fn(async () => 'E:\\tmp\\dbhx-edit'),
    stat: vi.fn(),
    open: vi.fn(async () => undefined),
    remove: vi.fn(async () => undefined),
    createFile: vi.fn(async () => undefined),
    list: vi.fn(),
    home: vi.fn(),
    drives: vi.fn(),
    mkdir: vi.fn(),
    rename: vi.fn(),
  },
  transferApi: { cancel: vi.fn(), running: vi.fn(async () => []) },
}))

const mock = vi.mocked(sftpApi)
const mockLocal = vi.mocked(localApi)

function entry(name: string, kind: FileEntry['kind'] = 'file'): FileEntry {
  return { name, path: `/home/app/${name}`, kind, size: 14, mtime_ms: 1_700_000_000_000, mode: 0o644, owner: 'app', group: 'app' }
}

function listing(path: string, names: string[]): DirListing {
  return { path, entries: names.map(name => entry(name)) }
}

function panelInfo(connection: number, path: string, following = true): PanelInfo {
  return {
    connection,
    endpoint: { host: '10.0.0.1', port: 22, username: 'app' },
    listing: listing(path, ['app.log']),
    following,
  }
}

beforeEach(() => {
  vi.clearAllMocks()
  localStorage.clear()
  setActivePinia(createPinia())
  mock.openForSession.mockResolvedValue(panelInfo(3, '/home/app'))
  mock.browse.mockResolvedValue(listing('/home/app/logs', ['app.log']))
  mock.download.mockResolvedValue(77)
  mock.close.mockResolvedValue(null)
})

describe('files store', () => {
  it('reuses one panel per connection when a second tab opens it', async () => {
    const files = useFilesStore()
    const first = await files.openForSession(1)
    const second = await files.openForSession(2)
    expect(first).not.toBeNull()
    expect(second).toBe(first)
    expect(files.open.length).toBe(1)
    expect(files.panelForSession(1)).toBe(first)
    expect(files.panelForSession(2)).toBe(first)
  })

  it('locks following after a manual browse, the way the server does', async () => {
    const files = useFilesStore()
    const p = await files.openForSession(1)
    expect(p?.following).toBe(true)
    await files.browse(3, 'logs')
    expect(p?.following).toBe(false)
    expect(p?.hold).toBe('user_locked')
    expect(p?.path).toBe('/home/app/logs')
  })

  it('applies a followed listing but only the reason when held back', async () => {
    const files = useFilesStore()
    const p = await files.openForSession(1)
    files.onFollowed({ connection: 3, listing: listing('/var/log', ['nginx.err']), hold: null })
    expect(p?.path).toBe('/var/log')
    expect(p?.entries.map(e => e.name)).toEqual(['nginx.err'])
    files.onFollowed({ connection: 3, listing: null, hold: 'busy' })
    expect(p?.path).toBe('/var/log')
    expect(p?.hold).toBe('busy')
  })

  it('refuses to download before a directory is configured', async () => {
    const files = useFilesStore()
    const settings = useSettingsStore()
    const p = await files.openForSession(1)
    settings.downloadDir = '   '
    const result = await files.download(3, ['/home/app/app.log'])
    expect(result.ok).toBe(false)
    expect(mock.download).not.toHaveBeenCalled()
    expect(p?.error).toContain('下载目录')
  })

  it('registers the batch so the panel can cancel it later', async () => {
    const files = useFilesStore()
    const transfers = useTransfersStore()
    const settings = useSettingsStore()
    settings.downloadDir = 'D:/tmp'
    await files.openForSession(1)
    const result = await files.download(3, ['/home/app/app.log'])
    expect(result.ok).toBe(true)
    expect(mock.download).toHaveBeenCalledWith(3, ['/home/app/app.log'], 'D:/tmp')
    expect(transfers.batchFor(3)).toBe(77)
  })

  it('forgets the session mapping when the panel closes', async () => {
    const files = useFilesStore()
    await files.openForSession(1)
    await files.close(3)
    expect(files.panelForSession(1)).toBeUndefined()
    expect(files.open.length).toBe(0)
  })
})

describe('新建 / 改权限 / 批量删除', () => {
  let files: ReturnType<typeof useFilesStore>

  beforeEach(async () => {
    files = useFilesStore()
    await files.openForSession(1)
  })

  it('sends the bare name and re-lists, so the server resolves it against the shown dir', async () => {
    mock.createFile.mockResolvedValue(null)
    mock.browse.mockResolvedValue(listing('/home/app', ['note.txt']))
    expect(await files.createFile(3, 'note.txt')).toBe(true)
    expect(mock.createFile).toHaveBeenCalledWith(3, 'note.txt')
    expect(mock.browse).toHaveBeenLastCalledWith(3, null)
  })

  it("keeps the server's own reason when a name is already taken", async () => {
    mock.createFile.mockRejectedValue(new Error('该名称已存在'))
    expect(await files.createFile(3, 'note.txt')).toBe(false)
    expect(files.panel(3)?.error).toContain('已存在')
  })

  it('chmods the entry path, not the panel path', async () => {
    mock.chmod.mockResolvedValue(null)
    mock.browse.mockResolvedValue(listing('/home/app', ['note.txt']))
    expect(await files.chmod(3, entry('note.txt'), 0o600)).toBe(true)
    expect(mock.chmod).toHaveBeenCalledWith(3, '/home/app/note.txt', 0o600)
  })

  it('stops at the first failed delete and says which entry it was', async () => {
    mock.remove
      .mockResolvedValueOnce(null)
      .mockRejectedValueOnce(new Error('目录非空'))
      .mockResolvedValue(null)
    mock.browse.mockResolvedValue(listing('/home/app', ['a']))
    const ok = await files.removeMany(3, [entry('a'), entry('logs', 'dir'), entry('c')])
    expect(ok).toBe(false)
    expect(mock.remove).toHaveBeenCalledTimes(2)
    expect(files.panel(3)?.error).toContain('logs：目录非空')
  })

  it('reports success when the whole selection went through', async () => {
    mock.remove.mockResolvedValue(null)
    mock.browse.mockResolvedValue(listing('/home/app', []))
    expect(await files.removeMany(3, [entry('a'), entry('b')])).toBe(true)
    expect(mock.remove).toHaveBeenCalledTimes(2)
    expect(files.panel(3)?.error).toBeNull()
  })
})

describe('外部编辑器往返', () => {
  const TEMP = 'E:\\tmp\\dbhx-edit\\note.txt'

  afterEach(() => {
    vi.useRealTimers()
  })

  /** 推进假时钟并让挂在 Promise 上的那串 await 走完。 */
  async function run(ms: number): Promise<void> {
    await vi.advanceTimersByTimeAsync(ms)
  }

  it('takes it down, opens it, then uploads back once the save settles', async () => {
    const files = useFilesStore()
    await files.openForSession(1)
    const note = entry('note.txt')
    const saved = { mtime: 1_700_000_000_000 }

    mock.download.mockResolvedValue(77)
    mock.upload.mockResolvedValue(88)
    mock.browse.mockResolvedValue(listing('/home/app', ['note.txt']))
    mockLocal.stat.mockImplementation(async () => ({ ...note, path: TEMP, mtime_ms: saved.mtime }))

    vi.useFakeTimers()
    const done = files.editRemote(3, note)
    await run(300)
    expect(mock.download).toHaveBeenCalledWith(3, ['/home/app/note.txt'], 'E:\\tmp\\dbhx-edit')
    expect(mockLocal.open).toHaveBeenCalledWith(TEMP)
    expect(files.panel(3)?.busy).toContain('系统编辑器')

    // 用户按了保存：先看到变化，再连续几轮不变，才算「写完了」
    saved.mtime += 60_000
    await run(3_500)
    expect(mock.upload).toHaveBeenCalledWith(3, [TEMP], '/home/app')

    files.cancelEdits(3)
    await run(900)
    await done
    // 本机那份是临时拷贝，收尾必须删掉，不然编辑过的文件会在 temp 里越积越多
    expect(mockLocal.remove).toHaveBeenCalledWith(TEMP, false)
    expect(files.panel(3)?.busy).toBeNull()
  })

  it('refuses a second edit of the same remote file', async () => {
    const files = useFilesStore()
    await files.openForSession(1)
    const note = entry('note.txt')
    mockLocal.stat.mockResolvedValue({ ...note, path: TEMP })
    vi.useFakeTimers()
    const first = files.editRemote(3, note)
    await run(100)
    await files.editRemote(3, note)
    expect(files.panel(3)?.error).toContain('还在编辑中')
    expect(mock.download).toHaveBeenCalledTimes(1)
    files.cancelEdits(3)
    await run(900)
    await first
  })
})
