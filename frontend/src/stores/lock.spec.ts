import { createPinia, setActivePinia } from 'pinia'
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'

import { lockApi } from '../lib/ipc/client'
import { useLockStore } from './lock'

vi.mock('../lib/ipc/client', () => ({
  lockApi: {
    status: vi.fn(async () => ({ locked: false, has_pin: false })),
    lock: vi.fn(async () => undefined),
    unlock: vi.fn(async () => undefined),
    setPin: vi.fn(async () => undefined),
    clearPin: vi.fn(async () => undefined),
  },
}))

const api = vi.mocked(lockApi)

function status(locked: boolean, hasPin: boolean): void {
  api.status.mockResolvedValue({ locked, has_pin: hasPin })
}

beforeEach(() => {
  vi.clearAllMocks()
  vi.useFakeTimers()
  localStorage.clear()
  setActivePinia(createPinia())
  status(false, false)
  api.lock.mockResolvedValue(null)
  api.unlock.mockResolvedValue(null)
})

afterEach(() => {
  vi.useRealTimers()
})

describe('lock store', () => {
  it('mirrors backend status on refresh', async () => {
    status(true, true)
    const store = useLockStore()
    await store.refresh()
    expect(store.locked).toBe(true)
    expect(store.hasPin).toBe(true)
  })

  it('keeps the lock up on a wrong pin and reports failure', async () => {
    api.unlock.mockRejectedValueOnce({ code: 'permission_denied', message: 'PIN 不正确', retryable: false })
    const store = useLockStore()
    await store.setPin('1234')
    await store.lock()
    expect(store.locked).toBe(true)
    const ok = await store.unlock('0000')
    expect(ok).toBe(false)
    expect(store.locked).toBe(true)
  })

  it('unlocks on the right pin', async () => {
    const store = useLockStore()
    await store.setPin('1234')
    await store.lock()
    expect(await store.unlock('1234')).toBe(true)
    expect(store.locked).toBe(false)
  })

  it('never calls the backend to lock when there is no pin to unlock with', async () => {
    // 空闲计时只在「设了 PIN」时武装；没 PIN 不该有自动上锁这一说
    const store = useLockStore()
    store.setIdleMinutes(5)
    expect(store.hasPin).toBe(false)
    await vi.advanceTimersByTimeAsync(5 * 60_000)
    expect(api.lock).not.toHaveBeenCalled()
  })

  it('auto-locks after the idle window once a pin exists', async () => {
    const store = useLockStore()
    await store.setPin('1234')
    store.setIdleMinutes(5)
    await vi.advanceTimersByTimeAsync(4 * 60_000)
    expect(api.lock).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(1 * 60_000)
    expect(api.lock).toHaveBeenCalledTimes(1)
    expect(store.locked).toBe(true)
  })

  it('activity pushes the auto-lock back', async () => {
    const store = useLockStore()
    await store.setPin('1234')
    store.setIdleMinutes(5)
    await vi.advanceTimersByTimeAsync(4 * 60_000)
    store.activity()
    await vi.advanceTimersByTimeAsync(4 * 60_000)
    expect(api.lock).not.toHaveBeenCalled()
    await vi.advanceTimersByTimeAsync(1 * 60_000)
    expect(api.lock).toHaveBeenCalledTimes(1)
  })

  it('does not arm a timer while already locked', async () => {
    status(false, true)
    const store = useLockStore()
    await store.setPin('1234')
    store.setIdleMinutes(1)
    await store.lock()
    api.lock.mockClear()
    await vi.advanceTimersByTimeAsync(5 * 60_000)
    expect(api.lock).not.toHaveBeenCalled()
  })

  it('turning idle off cancels a pending auto-lock', async () => {
    const store = useLockStore()
    await store.setPin('1234')
    store.setIdleMinutes(5)
    store.setIdleMinutes(0)
    await vi.advanceTimersByTimeAsync(6 * 60_000)
    expect(api.lock).not.toHaveBeenCalled()
  })

  it('clearing the pin unlocks and drops the local flag', async () => {
    const store = useLockStore()
    await store.setPin('1234')
    await store.lock()
    await store.clearPin()
    expect(store.hasPin).toBe(false)
    expect(store.locked).toBe(false)
  })

  it('persists the idle preference across store instances', async () => {
    const store = useLockStore()
    store.setIdleMinutes(15)
    expect(localStorage.getItem('dbhx:lock-idle-mins')).toBe('15')
    setActivePinia(createPinia())
    const reloaded = useLockStore()
    expect(reloaded.idleMinutes).toBe(15)
  })
})
