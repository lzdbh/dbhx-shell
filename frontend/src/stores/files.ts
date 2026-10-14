import { computed, reactive, ref } from 'vue'

import { defineStore } from 'pinia'

import type { DirListing, FileEntry, FollowHold, PanelInfo, SessionId, SftpFollowed } from '../lib/ipc/bindings'
import { sftpApi } from '../lib/ipc/client'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'
import { useSettingsStore } from './settings'
import { useTransfersStore } from './transfers'

/**
 * 一个开着的文件面板。
 *
 * 键是后端的 `connection`（一条 transport 的 stable 编号），不是会话号：
 * 同一台主机上两个 tab 共用一条连接时，后端刻意只允许一个面板（两个面板跟同一个
 * 终端会互相拽），前端必须按同一个粒度记账，否则第二个面板会显示成「打开成功但其实复用」。
 */
export interface FilePanel {
  connection: number
  path: string
  entries: FileEntry[]
  /** 跟随开关。与服务端的门控同步：手动浏览一次，服务端就锁定，这里也必须跟着关。 */
  following: boolean
  /** 跟随被压住的原因；`null` 表示没有压制。UI 用它解释「面板为什么没动」。 */
  hold: FollowHold | null
  loading: boolean
  error: string | null
}

/** 门控的「停在这里」原因翻成用户看得懂的话。 */
export const HOLD_TEXT: Record<FollowHold, string> = {
  nothing_reported: '终端还没报告过目录',
  user_locked: '已手动浏览，跟随暂停',
  busy: '传输进行中，先不动',
  editing: '正在编辑名称，先不动',
  unreachable: '终端所在目录进不去，已停在这里',
  already_shown: '面板已经在该目录',
  stale_generation: '这是一次过期的目录报告',
  weaker_source: '已有更可靠的目录来源',
}

export const useFilesStore = defineStore('files', () => {
  // 传输进度条要知道批次属于哪个面板，下载目录又归设置管，所以这两个 store 在这里组合
  const settings = useSettingsStore()
  const transfers = useTransfersStore()

  /** `reactive` 的 Map 而不是 `ref`：面板条目多，按 connection 取比整体替换省事。 */
  const panels = reactive(new Map<number, FilePanel>())
  /** 会话 → 面板。右侧面板跟着当前 tab 走，靠这张表找。 */
  const bySession = new Map<SessionId, number>()
  /** 最近一次打开的面板：本地会话没有面板时，右侧仍显示上一次开着的文件目录。 */
  const lastConnection = ref<number | null>(null)
  /** 打开面板失败的原因，只有调用点的 toast 需要它。 */
  const openError = ref<string | null>(null)

  const open = computed(() => [...panels.values()])

  function panel(connection: number): FilePanel | undefined {
    return panels.get(connection)
  }

  function panelForSession(session: SessionId | null): FilePanel | undefined {
    const connection = (session === null ? undefined : bySession.get(session)) ?? lastConnection.value
    return connection === null || connection === undefined ? undefined : panels.get(connection)
  }

  function applyListing(p: FilePanel, listing: DirListing): void {
    p.path = listing.path
    p.entries = listing.entries
    p.loading = false
    p.error = null
  }

  /** 从终端打开：不重新登录，且尽量停在终端当前目录。 */
  async function openForSession(session: SessionId): Promise<FilePanel | null> {
    try {
      const info: PanelInfo = await sftpApi.openForSession(session)
      const existing = panels.get(info.connection)
      const created = existing ?? reactive({
        connection: info.connection,
        path: info.listing.path,
        entries: info.listing.entries,
        following: info.following,
        hold: null,
        loading: false,
        error: null,
      } satisfies FilePanel)
      if (existing) applyListing(created, info.listing)
      panels.set(info.connection, created)
      bySession.set(session, info.connection)
      lastConnection.value = info.connection
      openError.value = null
      return created
    } catch (err) {
      openError.value = describeIpcError(normalizeIpcError(err))
      return null
    }
  }

  /** 从主机树直接开面板（不经过终端）。 */
  async function openForHost(hostId: string): Promise<FilePanel | null> {
    try {
      const info: PanelInfo = await sftpApi.openForHost(hostId)
      const created = panels.get(info.connection) ?? reactive({
        connection: info.connection,
        path: info.listing.path,
        entries: info.listing.entries,
        following: false,
        hold: null,
        loading: false,
        error: null,
      } satisfies FilePanel)
      if (panels.has(info.connection)) applyListing(created, info.listing)
      panels.set(info.connection, created)
      lastConnection.value = info.connection
      openError.value = null
      return created
    } catch (err) {
      openError.value = describeIpcError(normalizeIpcError(err))
      return null
    }
  }

  /** 打开失败的原因，只有调用点的 toast 需要它。 */
  function lastError(err: unknown): string {
    return describeIpcError(normalizeIpcError(err))
  }

  async function browse(connection: number, target: string | null): Promise<void> {
    const p = panels.get(connection)
    if (!p) return
    p.loading = true
    try {
      applyListing(p, await sftpApi.browse(connection, target))
      // 服务端把「用户手动浏览」记成锁定，前端开关必须跟着变，
      // 否则显示的是「跟随中」而行为是不跳——用户只会觉得面板坏了
      p.following = false
      p.hold = 'user_locked'
    } catch (err) {
      p.loading = false
      p.error = lastError(err)
    }
  }

  /** 进入目录项：目录列举，文件不做任何事（预览不在这一批里程碑里）。 */
  async function enter(connection: number, entry: FileEntry): Promise<void> {
    if (entry.kind === 'dir') await browse(connection, entry.name)
  }

  async function toggleFollow(connection: number, following: boolean): Promise<void> {
    const p = panels.get(connection)
    if (!p) return
    p.loading = true
    try {
      const result: SftpFollowed = await sftpApi.setFollowing(connection, following)
      p.following = following
      if (result.listing) applyListing(p, result.listing)
      else {
        p.loading = false
        p.hold = result.hold
      }
    } catch (err) {
      p.loading = false
      p.error = lastError(err)
    }
  }

  /** 用户正在填重命名/新建的输入框：这段时间后端不会跳目录。 */
  async function setEditing(connection: number, editing: boolean): Promise<void> {
    const p = panels.get(connection)
    if (!p) return
    try {
      const result = await sftpApi.setEditing(connection, editing)
      if (result.listing) applyListing(p, result.listing)
      else p.hold = result.hold
    } catch (err) {
      p.error = lastError(err)
    }
  }

  async function mkdir(connection: number, name: string): Promise<boolean> {
    const p = panels.get(connection)
    if (!p) return false
    try {
      await sftpApi.mkdir(connection, name, false)
      await browse(connection, null)
      return true
    } catch (err) {
      p.error = lastError(err)
      return false
    }
  }

  async function remove(connection: number, entry: FileEntry): Promise<boolean> {
    const p = panels.get(connection)
    if (!p) return false
    try {
      await sftpApi.remove(connection, entry.path, entry.kind === 'dir')
      await browse(connection, null)
      return true
    } catch (err) {
      p.error = lastError(err)
      return false
    }
  }

  async function rename(connection: number, from: FileEntry, to: string): Promise<boolean> {
    const p = panels.get(connection)
    if (!p) return false
    try {
      await sftpApi.rename(connection, from.path, to, false)
      await browse(connection, null)
      return true
    } catch (err) {
      p.error = lastError(err)
      return false
    }
  }

  /**
   * 下载远端条目到指定目录。
   *
   * `dir` 省略时落到设置里的下载目录（内嵌面板的老行为）；双栏文件窗口会显式传本机栏
   * 的当前目录，让「下载」就是把右边选中的东西放到左边正看着的那个文件夹。
   * 没有用系统保存对话框：那需要 `tauri-plugin-dialog`（Cargo 依赖 + capabilities 授权 +
   * npm 包三处同时加）。目录为空时直接给出可执行的提示，不静默失败。
   */
  async function download(connection: number, targets: string[], dir?: string): Promise<{ ok: boolean, error: string | null }> {
    const p = panels.get(connection)
    const to = (dir ?? settings.downloadDir).trim()
    if (!to) {
      const message = '请先在设置里指定下载目录'
      if (p) p.error = message
      return { ok: false, error: message }
    }
    try {
      transfers.noteStart(await sftpApi.download(connection, targets, to), connection)
      if (p) p.error = null
      return { ok: true, error: null }
    } catch (err) {
      const error = lastError(err)
      if (p) p.error = error
      return { ok: false, error }
    }
  }

  /** 上传本机文件到面板当前目录。路径来自拖拽，不是浏览器 input（那样拿不到绝对路径）。 */
  async function upload(connection: number, localPaths: string[]): Promise<{ ok: boolean, error: string | null }> {
    const p = panels.get(connection)
    if (localPaths.length === 0) return { ok: false, error: '没有可上传的文件' }
    try {
      transfers.noteStart(await sftpApi.upload(connection, localPaths, null), connection)
      if (p) p.error = null
      return { ok: true, error: null }
    } catch (err) {
      const error = lastError(err)
      if (p) p.error = error
      return { ok: false, error }
    }
  }

  async function close(connection: number): Promise<void> {
    panels.delete(connection)
    for (const [session, value] of bySession) {
      if (value === connection) bySession.delete(session)
    }
    if (lastConnection.value === connection) lastConnection.value = null
    try {
      await sftpApi.close(connection)
    } catch (err) {
      console.warn('关闭面板失败', normalizeIpcError(err).code)
    }
  }

  /* 下面是事件入口，由 lib/ipc/bus.ts 调用 */

  function onFollowed(event: SftpFollowed): void {
    const p = panels.get(event.connection)
    if (!p) return
    if (event.listing) applyListing(p, event.listing)
    else p.hold = event.hold
  }

  function onCwdChanged(connection: number, path: string): void {
    const p = panels.get(connection)
    if (p) p.path = path
  }

  function errorOf(connection: number, message: string): void {
    const p = panels.get(connection)
    if (p) p.error = message
  }

  return {
    panels,
    open,
    openError,
    lastConnection,
    panel,
    panelForSession,
    openForSession,
    openForHost,
    browse,
    enter,
    toggleFollow,
    setEditing,
    mkdir,
    remove,
    rename,
    download,
    upload,
    close,
    onFollowed,
    onCwdChanged,
    errorOf,
  }
})
