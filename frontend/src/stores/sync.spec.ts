import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { SyncStatus } from '../lib/ipc/bindings'
import { syncApi } from '../lib/ipc/client'
import { useSyncStore } from './sync'

vi.mock('../lib/ipc/client', () => ({
  syncApi: {
    status: vi.fn(),
    newStr: vi.fn(() => 'x'.repeat(32)),
    setStr: vi.fn(async () => undefined),
    setToken: vi.fn(async () => undefined),
    setProvider: vi.fn(async () => undefined),
    setFragment: vi.fn(async () => undefined),
    setPrivateKeys: vi.fn(async () => undefined),
    now: vi.fn(),
    recreate: vi.fn(),
  },
}))

const api = vi.mocked(syncApi)

function projection(over: Partial<SyncStatus> = {}): SyncStatus {
  return {
    provider: 'github',
    has_sync_str: true,
    has_token: true,
    fragment_id: 'abc123',
    include_private_keys: false,
    hosts: 3,
    ...over,
  }
}

beforeEach(() => {
  vi.clearAllMocks()
  localStorage.clear()
  setActivePinia(createPinia())
  api.status.mockResolvedValue(projection())
  api.now.mockResolvedValue({
    outcome: 'pushed',
    hosts: 3,
    imported: 0,
    conflicts: [],
    missing_identities: [],
    missing_credentials: [],
  })
})

describe('sync store', () => {
  it('mirrors the backend projection on refresh', async () => {
    const store = useSyncStore()
    await store.refresh()
    expect(store.status?.fragment_id).toBe('abc123')
    expect(store.status?.hosts).toBe(3)
  })

  it('keeps only the two booleans for the two secrets', async () => {
    // 投影的字段清单是死的：多一个字段就是后端悄悄把值也送出来了
    const store = useSyncStore()
    await store.refresh()
    expect(Object.keys(store.status ?? {}).sort()).toEqual([
      'fragment_id',
      'has_sync_str',
      'has_token',
      'hosts',
      'include_private_keys',
      'provider',
    ])
  })

  it('rereads the projection after a config write', async () => {
    const store = useSyncStore()
    expect(await store.setStr('0123456789abcdefghijKLMNOPqrst')).toBe(true)
    expect(api.setStr).toHaveBeenCalledWith('0123456789abcdefghijKLMNOPqrst')
    expect(api.status).toHaveBeenCalledTimes(1)
    expect(store.error).toBeNull()
  })

  it('reports a refused write instead of pretending it landed', async () => {
    api.setFragment.mockRejectedValueOnce({ code: 'invalid_input', message: '片段 id 只要地址里那一串字母数字', retryable: false })
    const store = useSyncStore()
    expect(await store.setFragment('https://gist.github.com/abc')).toBe(false)
    expect(store.error).toContain('片段 id')
    // 失败之后不重读，界面继续显示上一次真正生效的配置
    expect(api.status).not.toHaveBeenCalled()
  })

  it('forgets the local string without touching the cloud copy', async () => {
    const store = useSyncStore()
    await store.setStr(null)
    expect(api.setStr).toHaveBeenCalledWith(null)
  })

  it('holds busy across a sync and stores the report', async () => {
    let released: () => void = () => {}
    api.now.mockImplementationOnce(() => new Promise((resolve) => {
      released = () => resolve({
        outcome: 'created',
        hosts: 1,
        imported: 1,
        conflicts: [],
        missing_identities: [],
        missing_credentials: [],
      })
    }))
    const store = useSyncStore()
    const running = store.syncNow()
    expect(store.busy).toBe(true)
    // 第二下必须被吃掉：两条同步排队抢同一把锁，报回来的数字对不上用户按的那一下
    expect(await store.syncNow()).toBeNull()
    expect(api.now).toHaveBeenCalledTimes(1)
    released()
    const report = await running
    expect(report?.outcome).toBe('created')
    expect(store.busy).toBe(false)
    expect(store.last?.hosts).toBe(1)
    expect(api.status).toHaveBeenCalledTimes(1)
  })

  it('routes recreate to the recreate command', async () => {
    const store = useSyncStore()
    await store.syncNow(true)
    expect(api.recreate).toHaveBeenCalledTimes(1)
    expect(api.now).not.toHaveBeenCalled()
  })

  it('stamps the moment a sync really ran', async () => {
    vi.useFakeTimers()
    vi.setSystemTime(1_700_000_000_000)
    const store = useSyncStore()
    expect(store.lastSyncAt).toBeNull()
    await store.syncNow()
    expect(store.lastSyncAt).toBe(1_700_000_000_000)
    expect(localStorage.getItem('dbhx:sync-last-at')).toBe('1700000000000')
    vi.useRealTimers()
  })

  it('does not stamp a run that never talked to the cloud', async () => {
    api.now.mockResolvedValueOnce({
      outcome: 'not_configured',
      hosts: 0,
      imported: 0,
      conflicts: [],
      missing_identities: [],
      missing_credentials: [],
    })
    const store = useSyncStore()
    await store.syncNow()
    expect(store.lastSyncAt).toBeNull()
  })

  it('does not stamp a run that stopped to ask about recreating', async () => {
    api.now.mockResolvedValueOnce({
      outcome: 'recreate_required',
      hosts: 0,
      imported: 0,
      conflicts: [],
      missing_identities: [],
      missing_credentials: [],
    })
    const store = useSyncStore()
    await store.syncNow()
    expect(store.lastSyncAt).toBeNull()
  })

  it('keeps the previous stamp when a later run fails', async () => {
    const store = useSyncStore()
    await store.syncNow()
    const stamped = store.lastSyncAt
    api.now.mockRejectedValueOnce({ code: 'unreachable', message: 'tcp connect error', retryable: true })
    expect(await store.syncNow()).toBeNull()
    expect(store.lastSyncAt).toBe(stamped)
  })

  it('reads the stamp back after a reload', async () => {
    localStorage.setItem('dbhx:sync-last-at', '1700000000000')
    setActivePinia(createPinia())
    expect(useSyncStore().lastSyncAt).toBe(1_700_000_000_000)
  })

  it('translates a mismatched sync string into sync wording', async () => {
    // 后端在这条路上给 `permission_denied` 的只有 `VaultError::Crypto` 一种来源
    api.now.mockRejectedValueOnce({ code: 'permission_denied', message: 'vault could not be decrypted', retryable: false })
    const store = useSyncStore()
    expect(await store.syncNow()).toBeNull()
    expect(store.error).toContain('同步串')
    expect(store.error).not.toContain('令牌')
    expect(store.busy).toBe(false)
  })

  it('translates a rejected cloud token into token wording', async () => {
    // 通用文案会说「检查用户名、密码或密钥」，那是主机的凭据，不是这里的令牌
    api.now.mockRejectedValueOnce({
      code: 'auth_failed',
      message: '令牌无效，或者它没有片段（gist）的读写权限',
      retryable: false,
    })
    const store = useSyncStore()
    expect(await store.syncNow()).toBeNull()
    expect(store.error).toContain('令牌')
    expect(store.error).not.toContain('用户名')
    // 这两句不能互换：把被拒的令牌说成同步串填错，用户会去重贴那把根本没坏的钥匙
    expect(store.error).not.toContain('同步串')
  })

  it('leaves a network failure to the shared wording', async () => {
    api.now.mockRejectedValueOnce({ code: 'unreachable', message: 'tcp connect error', retryable: true })
    const store = useSyncStore()
    await store.syncNow()
    expect(store.error).toContain('无法连接到目标')
  })
})
