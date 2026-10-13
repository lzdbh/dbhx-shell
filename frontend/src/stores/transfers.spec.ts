import { createPinia, setActivePinia } from 'pinia'
import { beforeEach, describe, expect, it, vi } from 'vitest'

import type { TransferProgress } from '../lib/ipc/bindings'
import { transferApi } from '../lib/ipc/client'
import { useTransfersStore } from './transfers'

vi.mock('../lib/ipc/client', () => ({
  transferApi: { cancel: vi.fn(async () => undefined), running: vi.fn(async () => []) },
}))

function progress(id: number, patch: Partial<TransferProgress> = {}): TransferProgress {
  return {
    id,
    kind: 'download',
    path: '/home/app/logs/app.log',
    state: 'running',
    transferred: 100,
    total: 400,
    error: null,
    ...patch,
  }
}

const mock = vi.mocked(transferApi)

beforeEach(() => {
  vi.clearAllMocks()
  // store 是 pinia 单例，每个用例换一根针才不互相污染
  setActivePinia(createPinia())
})

describe('transfers store', () => {
  it('snapshots each progress event by id', () => {
    const store = useTransfersStore()
    store.onProgress(progress(1, { transferred: 10 }))
    store.onProgress(progress(1, { transferred: 300 }))
    expect(store.rows.get(1)?.transferred).toBe(300)
    expect(store.rows.size).toBe(1)
  })

  it('keeps unfinished rows ahead of finished ones', () => {
    const store = useTransfersStore()
    store.onProgress(progress(1, { state: 'completed' }))
    store.onProgress(progress(2))
    store.onProgress(progress(3, { state: 'failed' }))
    expect(store.queue.map(row => row.id)).toEqual([2, 3, 1])
    expect(store.active.map(row => row.id)).toEqual([2])
  })

  it('remembers which connection started a batch', () => {
    const store = useTransfersStore()
    store.noteStart(7, 42)
    store.noteStart(8, 42)
    // 同一连接上的两个批次，取消要落在后发起的那个
    expect(store.batchFor(42)).toBe(8)
    store.onBatchEnded({ batch: 8, completed: 1, failed: 0, cancelled: 0, bytes: 14, error: null })
    expect(store.batchFor(42)).toBe(7)
    expect(store.batches.get(8)?.connection).toBe(42)
  })

  it('drops finished entries only when asked', () => {
    const store = useTransfersStore()
    store.onProgress(progress(1, { state: 'completed' }))
    store.onProgress(progress(2))
    store.noteStart(9, 3)
    store.onBatchEnded({ batch: 9, completed: 1, failed: 0, cancelled: 0, bytes: 2, error: null })
    store.clearFinished()
    expect([...store.rows.keys()]).toEqual([2])
    expect(store.batches.size).toBe(0)
  })

  it('cancels by connection through the newest live batch', async () => {
    const store = useTransfersStore()
    store.noteStart(11, 5)
    expect(await store.cancelFor(5)).toBe(true)
    expect(mock.cancel).toHaveBeenCalledWith(11)
    expect(await store.cancelFor(6)).toBe(false)
  })

  it('picks up batches that were already running', async () => {
    mock.running.mockResolvedValueOnce([{ batch: 12, connection: 3 }])
    const store = useTransfersStore()
    await store.refresh()
    expect(store.runningBatches.map(row => row.batch)).toEqual([12])
    expect(store.batches.get(12)?.connection).toBe(3)
  })
})
