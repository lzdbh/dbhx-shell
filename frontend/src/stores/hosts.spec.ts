import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { HostDraft, HostInfo } from '../lib/ipc/bindings'
import { vaultApi } from '../lib/ipc/client'
import { useHostsStore } from './hosts'

vi.mock('../lib/ipc/client', () => ({
  vaultApi: {
    hosts: vi.fn(async () => []),
    identities: vi.fn(async () => []),
    upsertHost: vi.fn(async () => 'new-id'),
    setPassword: vi.fn(async () => undefined),
    removeHost: vi.fn(async () => undefined),
    addIdentity: vi.fn(async () => 'i1'),
    removeIdentity: vi.fn(async () => undefined),
  },
}))

const mock = vi.mocked(vaultApi)

function host(id: string, patch: Partial<HostInfo> = {}): HostInfo {
  return {
    id,
    label: id,
    group: null,
    tags: [],
    favorite: false,
    protocol: 'ssh',
    endpoint: { host: '10.0.0.1', port: null, username: 'app' },
    auth_mode: 'password',
    auth: { kind: 'host', host_id: id },
    keepalive_secs: 30,
    encoding: 'utf8',
    ...patch,
  }
}

function draft(id: string | null): HostDraft {
  return {
    id,
    label: 'db',
    group: null,
    tags: [],
    favorite: false,
    protocol: 'ssh',
    endpoint: { host: '10.0.0.1', port: null, username: 'app' },
    auth_mode: 'password',
    identity_id: null,
    keepalive_secs: 30,
    encoding: 'utf8',
  }
}

beforeEach(() => {
  vi.clearAllMocks()
  setActivePinia(createPinia())
})

describe('hosts store', () => {
  it('groups, then favorites first, then label', () => {
    const hosts = useHostsStore()
    const grouped = hosts.groupsOf([
      host('a', { label: 'beta', group: '生产' }),
      host('b', { label: 'alpha', group: '生产' }),
      host('c', { label: 'zeta', group: '生产', favorite: true }),
      host('d', { label: 'solo' }),
    ])
    expect(grouped.map(g => g.name)).toEqual(['生产', '未分组'])
    expect(grouped[0].hosts.map(h => h.id)).toEqual(['c', 'b', 'a'])
  })

  it('does not clear a stored password when the form leaves it blank', async () => {
    mock.hosts.mockResolvedValueOnce([])
    mock.identities.mockResolvedValueOnce([])
    const hosts = useHostsStore()
    await hosts.save(draft('h1'), null)
    expect(mock.setPassword).not.toHaveBeenCalled()
    await hosts.save(draft('h1'), 'rotated')
    expect(mock.setPassword).toHaveBeenCalledWith('new-id', 'rotated')
  })

  it('shows the default port it dials rather than the stored null', () => {
    const hosts = useHostsStore()
    expect(hosts.subtitle(host('a'))).toBe('app@10.0.0.1:22')
    expect(hosts.subtitle(host('b', { endpoint: { host: '10.0.0.2', port: 2222, username: 'root' } }))).toBe('root@10.0.0.2:2222')
  })

  it('keeps the last error for the caller to display', async () => {
    mock.hosts.mockRejectedValueOnce({ code: 'internal', message: 'vault 未解锁', retryable: false })
    const hosts = useHostsStore()
    await hosts.refresh()
    expect(hosts.error).toContain('vault 未解锁')
    expect(hosts.hosts).toEqual([])
  })
})
