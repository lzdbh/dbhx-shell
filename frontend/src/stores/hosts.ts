import { computed, ref } from 'vue'

import { defineStore } from 'pinia'

import type { HostDraft, HostId, HostInfo, IdentityInfo } from '../lib/ipc/bindings'
import { vaultApi } from '../lib/ipc/client'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'

/** 没有分组的主机显示在哪个节点下。后端存的是 `None`，这里只在前端给它一个名字。 */
const UNGROUPED = '未分组'

export interface HostGroup {
  name: string
  hosts: HostInfo[]
}

export const useHostsStore = defineStore('hosts', () => {
  const hosts = ref<HostInfo[]>([])
  const identities = ref<IdentityInfo[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)

  /** 分组树：组名升序、组内按标签升序，收藏的排到最前。搜索时按过滤后的列表调用。 */
  function groupsOf(list: HostInfo[]): HostGroup[] {
    const byGroup = new Map<string, HostInfo[]>()
    for (const host of list) {
      const key = host.group ?? UNGROUPED
      const bucket = byGroup.get(key)
      if (bucket) bucket.push(host)
      else byGroup.set(key, [host])
    }
    return [...byGroup.entries()]
      .sort(([a], [b]) => a.localeCompare(b, 'zh-Hans-CN'))
      .map(([name, bucket]) => ({
        name,
        hosts: [...bucket].sort((x, y) => Number(y.favorite) - Number(x.favorite) || x.label.localeCompare(y.label, 'zh-Hans-CN')),
      }))
  }

  const groups = computed(() => groupsOf(hosts.value))

  function groupNames(): string[] {
    return [...new Set(hosts.value.map(h => h.group).filter((g): g is string => !!g))].sort()
  }

  async function refresh(): Promise<void> {
    loading.value = true
    try {
      hosts.value = await vaultApi.hosts()
      identities.value = await vaultApi.identities()
      error.value = null
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
    } finally {
      loading.value = false
    }
  }

  /** 保存主机。口令是独立的一条命令，所以这里只在明文非空时才发——
   *  「改个分组名把口令弄丢了」是被投诉过的行为，前后端各挡一次。 */
  async function save(draft: HostDraft, password: string | null): Promise<HostId | null> {
    try {
      const id = await vaultApi.upsertHost(draft)
      if (password) await vaultApi.setPassword(id, password)
      error.value = null
      await refresh()
      return id
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
      return null
    }
  }

  async function remove(id: HostId): Promise<boolean> {
    try {
      await vaultApi.removeHost(id)
      await refresh()
      return true
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
      return false
    }
  }

  async function addIdentity(label: string, privateKey: string, passphrase: string | null): Promise<boolean> {
    try {
      await vaultApi.addIdentity(label, privateKey, passphrase)
      identities.value = await vaultApi.identities()
      return true
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
      return false
    }
  }

  async function removeIdentity(id: string): Promise<boolean> {
    try {
      await vaultApi.removeIdentity(id)
      identities.value = await vaultApi.identities()
      return true
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
      return false
    }
  }

  function byId(id: HostId): HostInfo | undefined {
    return hosts.value.find(h => h.id === id)
  }

  /** 主机次行文本：SSH 是 `user@host:port`；telnet 无用户名、默认端口 23，只显 `host:port`。 */
  function subtitle(host: HostInfo): string {
    const defaultPort = host.protocol === 'telnet' ? 23 : 22
    const where = `${host.endpoint.host}:${host.endpoint.port ?? defaultPort}`
    return host.endpoint.username ? `${host.endpoint.username}@${where}` : where
  }

  return { hosts, identities, loading, error, groups, groupsOf, groupNames, refresh, save, remove, addIdentity, removeIdentity, byId, subtitle }
})
