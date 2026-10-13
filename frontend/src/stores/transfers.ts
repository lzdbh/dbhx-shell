import { computed, reactive } from 'vue'

import { defineStore } from 'pinia'

import type { TransferBatchEnded, TransferProgress, TransferProgressEvent } from '../lib/ipc/bindings'
import { transferApi } from '../lib/ipc/client'
import { normalizeIpcError } from '../lib/ipc/errors'

/**
 * 一个批次的前端登记。
 *
 * `connection` 是我们自己记下来的：下载/上传命令的返回值**只有一个批次号**，
 * 只有发起它的那个面板才知道连接号。
 *
 * 反过来，逐文件的进度事件里**没有批次号也没有连接号**（契约上 `TransferProgress` 只有
 * `id`），所以前端无法把某个文件精确归给某个批次。这里不假装能做到：
 * 批次只显示聚合结果，文件按 `id` 平铺在队列里，取消按批次发。
 */
export interface BatchRow {
  batch: number
  connection: number | null
  ended: TransferBatchEnded | null
}

const TERMINAL_STATES = new Set(['completed', 'failed', 'cancelled'])

function isTerminal(row: TransferProgress): boolean {
  return TERMINAL_STATES.has(row.state)
}

export const useTransfersStore = defineStore('transfers', () => {
  /** 传输 id ⇒ 最后一次进度。终态条目保留到用户清空，方便回看失败原因。 */
  const rows = reactive(new Map<number, TransferProgress>())
  const batches = reactive(new Map<number, BatchRow>())

  /** 未完成在前，同组内新发起的在前。 */
  const queue = computed(() =>
    [...rows.values()].sort((a, b) => Number(isTerminal(a)) - Number(isTerminal(b)) || b.id - a.id),
  )

  const active = computed(() => [...rows.values()].filter(row => !isTerminal(row)))

  const runningBatches = computed(() => [...batches.values()].filter(row => row.ended === null))

  /** 面板上的「取消」按钮要决定发哪个批次号：取该连接最近发起且还没收尾的那一个。 */
  function batchFor(connection: number): number | null {
    let found: number | null = null
    for (const row of batches.values()) {
      if (row.connection === connection && row.ended === null) found = row.batch
    }
    return found
  }

  function noteStart(batch: number, connection: number): void {
    batches.set(batch, { batch, connection, ended: null })
  }

  async function cancel(batch: number): Promise<boolean> {
    try {
      await transferApi.cancel(batch)
      return true
    } catch {
      // 批次多半已经收尾了，取消失败不需要额外提示
      return false
    }
  }

  async function cancelFor(connection: number): Promise<boolean> {
    const batch = batchFor(connection)
    if (batch === null) return false
    return cancel(batch)
  }

  function clearFinished(): void {
    for (const [id, row] of rows) {
      if (isTerminal(row)) rows.delete(id)
    }
    for (const [batch, row] of batches) {
      if (row.ended) batches.delete(batch)
    }
  }

  /** 应用启动时把后端已经在跑的批次捞回来（前端重开不该中断传输）。 */
  async function refresh(): Promise<void> {
    try {
      for (const info of await transferApi.running()) {
        if (!batches.has(info.batch)) batches.set(info.batch, { batch: info.batch, connection: info.connection, ended: null })
      }
    } catch (err) {
      console.warn('拉取传输批次失败', normalizeIpcError(err).code)
    }
  }

  /* 下面是事件入口，由 lib/ipc/bus.ts 调用 */

  function onProgress(event: TransferProgressEvent): void {
    // 每个事件都是该 id 的完整快照，直接覆盖；节流已经在后端做过，前端不需要合并
    rows.set(event.id, event)
  }

  function onBatchEnded(event: TransferBatchEnded): void {
    batches.set(event.batch, { batch: event.batch, connection: batches.get(event.batch)?.connection ?? null, ended: event })
  }

  return { rows, batches, queue, active, runningBatches, batchFor, noteStart, cancel, cancelFor, clearFinished, refresh, onProgress, onBatchEnded }
})
