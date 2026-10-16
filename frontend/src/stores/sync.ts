import { ref } from 'vue'

import { defineStore } from 'pinia'

import type { ProviderKind, SyncReport, SyncStatus } from '../lib/ipc/bindings'
import { syncApi } from '../lib/ipc/client'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'

/**
 * 云同步的前端门面：配置读写 + 「立即同步」那一下。
 *
 * 同步串与云端令牌**一次都不落在这里**。store 只保存 `SyncStatus`，那份投影里
 * 两个秘密各是一个布尔——界面据此显示「已配置」，用户想改只能重新贴一遍。
 * 输入框里的明文活在组件的一次提交里，保存成功立刻清空；不写 localStorage，
 * 因为那等于把「解锁这台机器」和「登录云端账号」两把钥匙放进同一个明文文件。
 *
 * 一次同步要等十秒上下（两次 Argon2id 派生）。`busy` 由这里维护，按钮必须据此
 * 进 loading 态——不挡住第二次点击的话，两条同步会排队抢同一把 vault 锁，
 * 第二次的结论是基于第一次改完的状态做的，报给用户的数字就不再对应他按的那一下。
 */
export const useSyncStore = defineStore('sync', () => {
  /** 最后一次读到的配置投影。没读到过时为 null，面板先显示骨架而不是一片空白。 */
  const status = ref<SyncStatus | null>(null)
  const busy = ref(false)
  const error = ref<string | null>(null)
  /** 最近一次同步的结果，面板用它念「拉回 N 台 / 冲突 M 条」。 */
  const last = ref<SyncReport | null>(null)

  async function refresh(): Promise<void> {
    status.value = await syncApi.status()
  }

  /**
   * 所有配置写入都走这一个壳：成功就重读投影，失败就把话翻成人话。
   * 返回是否成功，让组件决定弹 toast 还是就地标红。
   */
  async function apply(call: () => Promise<unknown>): Promise<boolean> {
    error.value = null
    try {
      await call()
      await refresh()
      return true
    } catch (err) {
      error.value = describeIpcError(normalizeIpcError(err))
      return false
    }
  }

  /** 传 null 是「忘记本机存的这一份」，云端数据不动。 */
  const setStr = (text: string | null) => apply(() => syncApi.setStr(text))
  const setToken = (text: string | null) => apply(() => syncApi.setToken(text))
  const setProvider = (provider: ProviderKind) => apply(() => syncApi.setProvider(provider))
  const setFragment = (id: string | null) => apply(() => syncApi.setFragment(id))
  const setPrivateKeys = (enabled: boolean) => apply(() => syncApi.setPrivateKeys(enabled))

  /** 生成一串新同步串。纯本机随机，不落盘，所以没有失败分支。 */
  async function newStr(): Promise<string> {
    return syncApi.newStr()
  }

  /**
   * 跑一次同步。`recreate` 为真时先在云上重建片段再同步。
   *
   * 这两个 code 在这里单独翻一遍，因为它们的通用文案在这个场合会把人带歪：
   * `permission_denied` 同步这条路上只有一个来源——`VaultError::Crypto`，也就是
   * 同步串和云上那份不配对；`auth_failed` 只有云端拒了令牌（`WireError::Unauthorized`）。
   * 照抄通用文案会让人去点解锁、或者去改主机口令，白跑一趟。
   * 两个码在后端必须是分开的（`crates/dbhx-sync/src/wire.rs` 里有测试钉住这一点），
   * 否则这两句只能留一句，用户会照着错的那句去改另一个东西。
   */
  async function syncNow(recreate = false): Promise<SyncReport | null> {
    if (busy.value) return null
    busy.value = true
    error.value = null
    try {
      const report = recreate ? await syncApi.recreate() : await syncApi.now()
      last.value = report
      // 合并改动了本机库，主机条数与片段 id（第一次同步时新建）都变了
      await refresh()
      return report
    } catch (err) {
      const ipc = normalizeIpcError(err)
      const own: Partial<Record<string, string>> = {
        permission_denied: '云上那一份解不开：同步串填错了，或者它属于另一个片段',
        auth_failed: '云端拒了这个令牌：确认它是这一家的、并且有片段读写权限',
      }
      error.value = own[ipc.code] ?? describeIpcError(ipc)
      return null
    } finally {
      busy.value = false
    }
  }

  return {
    status,
    busy,
    error,
    last,
    refresh,
    setStr,
    setToken,
    setProvider,
    setFragment,
    setPrivateKeys,
    newStr,
    syncNow,
  }
})
