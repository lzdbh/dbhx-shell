import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { HostKeyConflict, HostKeyPending, KnownHost } from '../lib/ipc/bindings'
import { hostkeyApi, requestApi } from '../lib/ipc/client'
import { useHostkeysStore } from './hostkeys'

vi.mock('../lib/ipc/client', () => ({
  hostkeyApi: { list: vi.fn(async () => []), forget: vi.fn(async () => 1), importSystem: vi.fn(async () => 0) },
  requestApi: { replyConfirmed: vi.fn(async () => undefined), replyCancelled: vi.fn(async () => undefined), replySecret: vi.fn(async () => undefined) },
}))

const reply = vi.mocked(requestApi)
const keys = vi.mocked(hostkeyApi)

function known(pattern: string, base64_key = 'AAAAC3'): KnownHost {
  return { pattern, algorithm: 'ssh-ed25519', base64_key, added_at_ms: 1, source: 'user_confirmed' }
}

function pending(request: number, host: string): HostKeyPending {
  return { request, endpoint: { host, port: 22, username: 'app' }, presented: known(host) }
}

beforeEach(() => {
  vi.clearAllMocks()
  setActivePinia(createPinia())
  keys.list.mockResolvedValue([])
})

describe('hostkeys store', () => {
  it('shows one ask at a time and moves to the next after an answer', async () => {
    const store = useHostkeysStore()
    store.onHostKeyPending(pending(1, 'a'))
    store.onHostKeyPending(pending(2, 'b'))
    expect(store.current?.kind).toBe('host_key')
    // 第二条还排在队列里，此刻不应该有两个对话框叠着
    expect(store.asks.length).toBe(2)

    await store.confirmKey()
    expect(reply.replyConfirmed).toHaveBeenCalledWith(1)
    expect(store.current?.kind).toBe('host_key')
    await store.confirmKey()
    expect(reply.replyConfirmed).toHaveBeenCalledWith(2)
    expect(store.current).toBeNull()
    expect(keys.list).toHaveBeenCalledTimes(2)
  })

  it('rejecting a key answers the backend and trusts nothing', async () => {
    const store = useHostkeysStore()
    store.onHostKeyPending(pending(5, 'a'))
    await store.rejectKey()
    expect(reply.replyCancelled).toHaveBeenCalledWith(5)
    expect(keys.list).not.toHaveBeenCalled()
    expect(store.current).toBeNull()
  })

  it('conflict has no reply channel, only a dismissal', () => {
    const store = useHostkeysStore()
    const event: HostKeyConflict = {
      endpoint: { host: 'a', port: 22, username: 'app' },
      stored: known('a', 'OLD'),
      presented: known('a', 'NEW'),
    }
    store.onHostKeyConflict(event)
    store.onHostKeyConflict(event)
    expect(store.asks.length).toBe(1)
    expect(store.current?.kind).toBe('conflict')
    store.dismissConflict()
    expect(store.current).toBeNull()
    expect(Object.keys(reply)).toEqual(['replyConfirmed', 'replyCancelled', 'replySecret'])
    expect(reply.replyConfirmed).not.toHaveBeenCalled()
    expect(reply.replyCancelled).not.toHaveBeenCalled()
  })

  it('carries the passphrase to the backend and clears it locally', async () => {
    const store = useHostkeysStore()
    store.onPassphraseRequest({ request: 9, identity_label: 'bastion' })
    await store.answerPassphrase('hunter2')
    expect(reply.replySecret).toHaveBeenCalledWith(9, 'hunter2')
    expect(store.current).toBeNull()
  })

  it('surfaces the queued ask after the passphrase one is answered', async () => {
    const store = useHostkeysStore()
    store.onPassphraseRequest({ request: 10, identity_label: 'k1' })
    store.onPassphraseRequest({ request: 11, identity_label: 'k2' })
    await store.skipPassphrase()
    expect(reply.replyCancelled).toHaveBeenCalledWith(10)
    expect(store.asks.length).toBe(1)
    await store.answerPassphrase('x')
    expect(reply.replySecret).toHaveBeenCalledWith(11, 'x')
  })
})
