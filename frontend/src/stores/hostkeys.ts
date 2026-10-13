import { computed, ref } from 'vue'

import { defineStore } from 'pinia'

import type { HostKeyConflict, HostKeyPending, KnownHost, PassphraseRequest } from '../lib/ipc/bindings'
import { hostkeyApi, requestApi } from '../lib/ipc/client'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'

/**
 * 后端发起的一次询问。
 *
 * 三类询问共用**一条队列**：连接是并发发起的，同一时刻可能既在等主机密钥确认、
 * 又在等某把私钥的口令。一次只弹一个对话框，答完再放下一个——
 * 两个 modal 叠在一起，用户很可能把口令填进密钥确认框里。
 */
export type Ask =
  | { kind: 'host_key', request: number, endpoint: HostKeyPending['endpoint'], presented: KnownHost }
  | { kind: 'passphrase', request: number, identityLabel: string }
  | { kind: 'conflict', endpoint: HostKeyConflict['endpoint'], stored: KnownHost, presented: KnownHost }

export const useHostkeysStore = defineStore('hostkeys', () => {
  const known = ref<KnownHost[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** 导入系统 known_hosts 的条数，只在点过「导入」之后有值。 */
  const importedCount = ref<number | null>(null)

  const asks = ref<Ask[]>([])

  /** 队首就是当前该显示的对话框。 */
  const current = computed<Ask | null>(() => asks.value[0] ?? null)

  async function refresh(): Promise<void> {
    loading.value = true
    try {
      known.value = await hostkeyApi.list()
      error.value = null
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
    } finally {
      loading.value = false
    }
  }

  /** 忘记某台主机的密钥：参数是 host + port，不是 known_hosts 里的 pattern 写法。 */
  async function forget(host: string, port: number, algorithm: string): Promise<boolean> {
    try {
      await hostkeyApi.forget(host, port, algorithm)
      await refresh()
      return true
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
      return false
    }
  }

  async function importSystem(): Promise<boolean> {
    try {
      importedCount.value = await hostkeyApi.importSystem()
      await refresh()
      return true
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
      return false
    }
  }

  /* 下面是事件入口，由 lib/ipc/bus.ts 调用 */

  function onHostKeyPending(event: HostKeyPending): void {
    asks.value.push({ kind: 'host_key', request: event.request, endpoint: event.endpoint, presented: event.presented })
  }

  function onPassphraseRequest(event: PassphraseRequest): void {
    asks.value.push({ kind: 'passphrase', request: event.request, identityLabel: event.identity_label })
  }

  function onHostKeyConflict(event: HostKeyConflict): void {
    // 同一台主机重复冲突时只留第一条，后面的回答也是回答同一个问题
    const duplicate = asks.value.some(a => a.kind === 'conflict' && a.stored.pattern === event.stored.pattern)
    if (!duplicate) asks.value.push({ kind: 'conflict', ...event })
  }

  /* 回答 */

  async function confirmKey(): Promise<void> {
    const ask = asks.value[0]
    if (ask?.kind !== 'host_key') return
    // 先出队再发命令：await 期间后端可能已经推进到下一条询问
    asks.value.shift()
    try {
      await requestApi.replyConfirmed(ask.request)
      await refresh()
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
    }
  }

  /** 拒绝：连接会失败，会话标签页上显示的原因由后端那条错误给出。 */
  async function rejectKey(): Promise<void> {
    const ask = asks.value[0]
    if (ask?.kind !== 'host_key') return
    asks.value.shift()
    try {
      await requestApi.replyCancelled(ask.request)
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
    }
  }

  async function answerPassphrase(text: string): Promise<void> {
    const ask = asks.value[0]
    if (ask?.kind !== 'passphrase') return
    asks.value.shift()
    try {
      await requestApi.replySecret(ask.request, text)
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
    }
  }

  async function skipPassphrase(): Promise<void> {
    const ask = asks.value[0]
    if (ask?.kind !== 'passphrase') return
    asks.value.shift()
    try {
      await requestApi.replyCancelled(ask.request)
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
    }
  }

  function dismissConflict(): void {
    if (asks.value[0]?.kind === 'conflict') asks.value.shift()
  }

  return {
    known,
    loading,
    error,
    importedCount,
    asks,
    current,
    refresh,
    forget,
    importSystem,
    onHostKeyPending,
    onPassphraseRequest,
    onHostKeyConflict,
    confirmKey,
    rejectKey,
    answerPassphrase,
    skipPassphrase,
    dismissConflict,
  }
})
