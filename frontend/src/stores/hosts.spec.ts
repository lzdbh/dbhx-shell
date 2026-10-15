import { createPinia, setActivePinia } from 'pinia'
import { nextTick } from 'vue'

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
    revealPassword: vi.fn(async () => ''),
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
    has_credential: false,
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
  // 分组名单落在 localStorage 里，不清的话上一条测试存的组会混进下一条的树
  localStorage.clear()
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
    // telnet 默认端口 23、无用户名：不该渲染成 `@host:22` 这种串味的地址
    expect(hosts.subtitle(host('c', { protocol: 'telnet', endpoint: { host: '10.0.0.3', port: null, username: '' } }))).toBe('10.0.0.3:23')
  })

  it('keeps the last error for the caller to display', async () => {
    mock.hosts.mockRejectedValueOnce({ code: 'internal', message: 'vault 未解锁', retryable: false })
    const hosts = useHostsStore()
    await hosts.refresh()
    expect(hosts.error).toContain('vault 未解锁')
    expect(hosts.hosts).toEqual([])
  })

  it('新建的空分组也要出现在树上，否则「新建分组」点完看着像没反应', () => {
    const hosts = useHostsStore()
    expect(hosts.addGroup('测试环境')).toBeNull()
    const grouped = hosts.groupsOf([host('a', { group: '生产' })])
    expect(grouped.map(g => g.name)).toEqual(['测试环境', '生产'])
    expect(grouped[0].hosts).toEqual([])
  })

  it('分组名挡住空、挡住「未分组」、挡住重名（不分大小写）', () => {
    const hosts = useHostsStore()
    hosts.hosts = [host('a', { group: 'Prod' })]
    expect(hosts.addGroup('   ')).toContain('不能为空')
    expect(hosts.addGroup('未分组')).toContain('没有分组')
    expect(hosts.addGroup('prod')).toContain('已经有叫')
    expect(hosts.addGroup('Dev')).toBeNull()
  })

  it('有主机的分组不给改名、不给删除：改一半会把这个组劈成两半', () => {
    const hosts = useHostsStore()
    hosts.hosts = [host('a', { group: '生产' })]
    expect(hosts.isEmptyGroup('生产')).toBe(false)
    expect(hosts.renameGroup('生产', '预发')).toContain('还有主机')
    expect(hosts.removeGroup('生产')).toContain('还有主机')
  })

  it('空分组改名后名单跟着换，删除只动自己那一个', () => {
    const hosts = useHostsStore()
    hosts.addGroup('旧名')
    hosts.addGroup('别的')
    expect(hosts.renameGroup('旧名', '新名')).toBeNull()
    expect(hosts.extraGroups).toEqual(['别的', '新名'])
    expect(hosts.removeGroup('别的')).toBeNull()
    expect(hosts.extraGroups).toEqual(['新名'])
  })

  it('分组名单落盘，重启后空分组还在', async () => {
    const hosts = useHostsStore()
    hosts.addGroup(' staging ')
    // watch 默认 pre flush，不排一次微任务的话盘上还是空的
    await nextTick()
    expect(JSON.parse(localStorage.getItem('dbhx:groups') ?? '[]')).toEqual(['staging'])

    setActivePinia(createPinia())
    expect(useHostsStore().extraGroups).toEqual(['staging'])
  })
})
