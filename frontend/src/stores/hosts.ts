import { computed, ref, watch } from 'vue'

import { defineStore } from 'pinia'

import type { HostDraft, HostId, HostInfo, IdentityInfo } from '../lib/ipc/bindings'
import { vaultApi } from '../lib/ipc/client'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'

/** 没有分组的主机显示在哪个节点下。后端存的是 `None`，这里只在前端给它一个名字。 */
export const UNGROUPED = '未分组'

export interface HostGroup {
  name: string
  hosts: HostInfo[]
}

/**
 * 手工建的空分组存在 localStorage，不存在 vault。
 *
 * vault 里根本没有「分组」这个实体——分组只是主机身上的一个字段。于是「先建好分组、
 * 再往里放主机」这个动作没有地方落地：一台主机都没有的分组，后端存不住。所以这份
 * 名字单独记着，等第一台主机落进这个组，它就从「手工记的」变成「从主机算出来的」。
 * 和字号、主题同理，属于本机偏好，不该进加密凭据存储。
 */
const GROUPS_KEY = 'dbhx:groups'

/** 盘上的分组名单还原成合法形状：不是数组、空串、纯空格的一律丢掉，剩下的去重排序。 */
function readGroups(): string[] {
  try {
    const raw = localStorage.getItem(GROUPS_KEY)
    if (!raw) return []
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    const kept = parsed.filter((item): item is string => typeof item === 'string' && item.trim() !== '').map(item => item.trim())
    return [...new Set(kept)].sort((a, b) => a.localeCompare(b, 'zh-Hans-CN'))
  } catch {
    return []
  }
}

export const useHostsStore = defineStore('hosts', () => {
  const hosts = ref<HostInfo[]>([])
  const identities = ref<IdentityInfo[]>([])
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** 手工建出来、还没放主机的分组名。主机的 `group` 字段算出来的分组不进来。 */
  const extraGroups = ref<string[]>(readGroups())

  /** 分组名合不合法。返回错误文案，`null` 表示可以用；`except` 是改名时要绕开自己的旧名。 */
  function groupNameProblem(name: string, except: string | null = null): string | null {
    const key = name.trim()
    if (key === '') return '分组名不能为空'
    if (key === UNGROUPED) return `分组不能叫「${UNGROUPED}」，那个节点代表「没有分组」`
    const taken = new Set(hosts.value.map(h => h.group).filter((g): g is string => !!g))
    for (const group of extraGroups.value) taken.add(group)
    if (except !== null) taken.delete(except)
    if (key !== except && [...taken].some(group => group.toLowerCase() === key.toLowerCase())) return `已经有叫「${key}」的分组了`
    return null
  }

  /** 这个分组节点里现在还有没有主机。空分组只有在自己没有主机时才谈得上改名/删除。 */
  function isEmptyGroup(name: string): boolean {
    return !hosts.value.some(h => (h.group ?? UNGROUPED) === name)
  }

  /**
   * 分组树：组名升序、组内按标签升序，收藏的排到最前。搜索时按过滤后的列表调用。
   *
   * 手工建的空分组也补一个空桶进去，否则「新建分组」点完树上什么都不长——那就是
   * 用户眼中的「没反应」。调用方（主机树）在搜索时自己把空桶滤掉。
   */
  function groupsOf(list: HostInfo[]): HostGroup[] {
    const byGroup = new Map<string, HostInfo[]>()
    for (const host of list) {
      const key = host.group ?? UNGROUPED
      const bucket = byGroup.get(key)
      if (bucket) bucket.push(host)
      else byGroup.set(key, [host])
    }
    for (const name of extraGroups.value) {
      if (!byGroup.has(name)) byGroup.set(name, [])
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

  /** 建一个空分组。成功返回 `null`，失败返回给用户看的文案。 */
  function addGroup(name: string): string | null {
    const problem = groupNameProblem(name)
    if (problem) return problem
    extraGroups.value = [...extraGroups.value, name.trim()].sort((a, b) => a.localeCompare(b, 'zh-Hans-CN'))
    return null
  }

  /**
   * 改名，只改分组名单本身。
   *
   * 有主机的分组改名要逐台重写 `group` 字段，中途失败会把这个组劈成两半——
   * 那不是「改个标签」该有的代价，所以这里挡住：先把组清空。
   */
  function renameGroup(from: string, to: string): string | null {
    if (!isEmptyGroup(from)) return `「${from}」里还有主机，先把它们移到别的分组再改名`
    const problem = groupNameProblem(to, from)
    if (problem) return problem
    const key = to.trim()
    extraGroups.value = extraGroups.value.map(group => (group === from ? key : group)).sort((a, b) => a.localeCompare(b, 'zh-Hans-CN'))
    return null
  }

  /** 删掉一个空分组节点。组里还有主机时不动——删掉名字等于把主机甩进「未分组」。 */
  function removeGroup(name: string): string | null {
    if (!isEmptyGroup(name)) return `「${name}」里还有主机`
    extraGroups.value = extraGroups.value.filter(group => group !== name)
    return null
  }

  watch(extraGroups, list => {
    localStorage.setItem(GROUPS_KEY, JSON.stringify(list))
  }, { deep: true })

  // 和主题一样：另一个窗口（文件管理）改了名单，这边立刻跟上
  window.addEventListener('storage', (event) => {
    if (event.key === GROUPS_KEY) extraGroups.value = readGroups()
  })

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

  return {
    hosts,
    identities,
    loading,
    error,
    extraGroups,
    groups,
    groupsOf,
    groupNames,
    groupNameProblem,
    isEmptyGroup,
    addGroup,
    renameGroup,
    removeGroup,
    refresh,
    save,
    remove,
    addIdentity,
    removeIdentity,
    byId,
    subtitle,
  }
})
