import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { DirListing, FileEntry, PanelInfo } from '../lib/ipc/bindings'
import { sftpApi } from '../lib/ipc/client'
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
    remove: vi.fn(),
    rename: vi.fn(),
    download: vi.fn(),
    upload: vi.fn(),
    close: vi.fn(),
  },
  transferApi: { cancel: vi.fn(), running: vi.fn(async () => []) },
}))

const mock = vi.mocked(sftpApi)

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
