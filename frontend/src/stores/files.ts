import { computed, reactive, ref } from 'vue'

import { defineStore } from 'pinia'

import type { DirListing, FileEntry, FollowHold, PanelInfo, SessionId, SftpFollowed, TransferBatchEnded } from '../lib/ipc/bindings'
import { localApi, sftpApi, transferApi } from '../lib/ipc/client'
import { describeIpcError, normalizeIpcError } from '../lib/ipc/errors'
import { localJoin, parentPath } from '../lib/format'
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
  /** 后台正在这栏里做什么（外部编辑器往返）。`null` 表示空闲。 */
  busy: string | null
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

const sleep = (ms: number): Promise<void> => new Promise(resolve => setTimeout(resolve, ms))

/** 外部编辑器轮询节奏：0.7 秒一次，连续 3 次没再变就当它写完了。 */
const TICK_MS = 700
const QUIET_TICKS = 3
/** 单次编辑会话最多盯半小时，之后静默收摊（不追一个一直没关的记事本）。 */
const WATCH_TICKS = Math.floor(30 * 60 * 1000 / TICK_MS)

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
        busy: null,
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
        busy: null,
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
   * 批量删除。
   *
   * 逐项执行而不是让后端做递归：面板上选中的是一个「混合集合」，一个非空目录失败
   * 不该把后面已经能删的条目连带放弃，但也不能默默跳过——所以停在第一个错误上，
   * 并把是哪个条目说清楚。
   */
  async function removeMany(connection: number, entries: FileEntry[]): Promise<boolean> {
    const p = panels.get(connection)
    if (!p) return false
    let failure: string | null = null
    for (const entry of entries) {
      try {
        await sftpApi.remove(connection, entry.path, entry.kind === 'dir')
      } catch (err) {
        failure = `${entry.name}：${lastError(err)}`
        break
      }
    }
    await browse(connection, null)
    // browse 成功会把 error 清空，所以这条必须补在它后面
    if (failure !== null) p.error = failure
    return failure === null
  }

  /** 在面板正显示的目录里建一个空文件（裸名字，后端按显示目录拼）。 */
  async function createFile(connection: number, name: string): Promise<boolean> {
    const p = panels.get(connection)
    if (!p) return false
    try {
      await sftpApi.createFile(connection, name)
      await browse(connection, null)
      return true
    } catch (err) {
      p.error = lastError(err)
      return false
    }
  }

  /** 改远端权限位。`mode` 是八进制值（0o600 这种），后端越界会拒。 */
  async function chmod(connection: number, entry: FileEntry, mode: number): Promise<boolean> {
    const p = panels.get(connection)
    if (!p) return false
    try {
      await sftpApi.chmod(connection, entry.path, mode)
      await browse(connection, null)
      return true
    } catch (err) {
      p.error = lastError(err)
      return false
    }
  }

  /* ── 外部编辑器往返 ──────────────────────────────────── */

  /**
   * 正在编辑的远端路径 → 取消标记。
   *
   * 键用远端路径而不是本机临时路径：同名文件在不同目录里编辑是两回事，
   * 而临时目录只有一个，用本机路径当键会让第二次编辑「抢」掉第一个的临时文件。
   */
  const edits = new Map<string, { connection: number, cancelled: boolean, done: Promise<void> }>()

  /**
   * 用系统关联程序编辑远端文件：取到本机临时目录 → 打开 → 盯着它保存 → 存回原处。
   *
   * 为什么在前端编排而不是后端：编辑器是外部进程，我们既拿不到它的退出时机，
   * 也不该为了「等它保存」在后端起一个跨会话的任务队列。轮询本机那份拷贝的
   * mtime/size 足够，而且回传走的是上传引擎既有的 create 语义，天然就地覆盖。
   *
   * 收尾会把临时文件删掉：那是本机目录，不留垃圾。
   */
  async function editRemote(connection: number, entry: FileEntry): Promise<void> {
    const p = panels.get(connection)
    if (!p || entry.kind === 'dir') return
    // 同一个文件再点一次，几乎总是「编辑器已经关了，我要再看一眼」。而上一次那份
    // 监听最长要盯半小时（判不了编辑器退出，只能等它自己安静下来），直接拒绝就等于
    // 让用户半小时打不开这个文件。所以先叫它收摊、等它删掉临时副本，再重新取一份。
    // 必须等：新旧两次用的是同一个临时路径，抢着下载会被上一次的收尾删掉。
    const previous = edits.get(entry.path)
    if (previous) {
      previous.cancelled = true
      p.busy = `正在收尾上一次 ${entry.name} 的编辑…`
      await previous.done.catch(() => undefined)
    }
    let finish: () => void = () => {}
    const done = new Promise<void>((resolve) => { finish = resolve })
    const token = { connection, cancelled: false, done }
    edits.set(entry.path, token)
    const note = (text: string | null): void => { p.busy = text }
    let temp = ''
    try {
      const dir = await localApi.editDir()
      temp = localJoin(dir, entry.name)
      note(`正在取 ${entry.name} 到本机…`)
      const down = await sftpApi.download(connection, [entry.path], dir)
      transfers.noteStart(down, connection)
      const taken = await waitBatchEnd(down)
      if (taken && taken.failed > 0) throw new Error(taken.error ?? '取到本机失败')
      if (token.cancelled) return
      note('已交给系统编辑器，保存后自动传回')
      await localApi.open(temp)
      let seen = await localApi.stat(temp)
      while (!token.cancelled) {
        const saved = await waitForSave(temp, seen, token)
        if (saved === null) break
        seen = saved
        note(`正在回传 ${entry.name}…`)
        const up = await sftpApi.upload(connection, [temp], parentPath(entry.path))
        transfers.noteStart(up, connection)
        const sent = await waitBatchEnd(up)
        if (sent && sent.failed > 0) note(`回传失败：${sent.error ?? '未知原因'}`)
        else note('已传回远端，继续监听保存')
        await browse(connection, null)
      }
    } catch (err) {
      p.error = lastError(err)
    } finally {
      // 只清自己那一份：`cancelEdits` 会先把标记从表里摘掉，这时临时文件可能已经
      // 属于接管它的那次编辑了，再删一次就把新会话正在用的文件删了。
      const mine = edits.get(entry.path) === token
      if (mine) edits.delete(entry.path)
      note(null)
      if (temp !== '' && mine) await localApi.remove(temp, false).catch(() => undefined)
      finish()
    }
  }

  /**
   * 等本机那份拷贝「变过并且稳下来」。
   *
   * 连续 QUIET_TICKS 轮属性不再变化才回，避免编辑器分几次写（先截断再写全）时
   * 把半成品传回远端。`stat` 偶发失败（文件正被独占写）只跳过这一轮，不算取消。
   */
  async function waitForSave(target: string, from: FileEntry, token: { cancelled: boolean }): Promise<FileEntry | null> {
    let seen = from
    let changed = false
    let quiet = 0
    for (let tick = 0; tick < WATCH_TICKS; tick += 1) {
      await sleep(TICK_MS)
      if (token.cancelled) return null
      let now: FileEntry
      try {
        now = await localApi.stat(target)
      } catch {
        continue
      }
      if (now.mtime_ms !== seen.mtime_ms || now.size !== seen.size) {
        seen = now
        changed = true
        quiet = 0
        continue
      }
      if (changed) {
        quiet += 1
        if (quiet >= QUIET_TICKS) return seen
      }
    }
    return null
  }

  /**
   * 等一个传输批次收尾。
   *
   * 只能轮询后端：命令只回批次号，而极快完成时 `transfer-batch-ended` 可能比前端
   * 登记更早到，那时候挂事件就永远等不到。返回 `null` 表示「已经不跑了，但没拿到
   * 收尾详情」或「等超时」，调用侧两种都按「继续往下走、别把用户卡住」处理。
   */
  async function waitBatchEnd(batch: number): Promise<TransferBatchEnded | null> {
    for (let tick = 0; tick < 9000; tick += 1) {
      const ended = transfers.batches.get(batch)?.ended ?? null
      if (ended) return ended
      const running = await transferApi.running().catch(() => null)
      if (running !== null && !running.some(info => info.batch === batch)) {
        await sleep(120)
        return transfers.batches.get(batch)?.ended ?? null
      }
      await sleep(200)
    }
    return transfers.batches.get(batch)?.ended ?? null
  }

  /** 关掉文件窗口或面板时收摊：正在等的编辑循环下一轮就会退出。 */
  function cancelEdits(connection?: number): void {
    // 只举旗，不从这里删表：删了之后那一次会话会以为「已经有人接管」而跳过收尾，
    // 临时副本就留在本机了。标记举起来，循环退出时它自己删，接管者也等得到那一步。
    for (const token of edits.values()) {
      if (connection === undefined || token.connection === connection) token.cancelled = true
    }
  }

  /**
   * 下载远端条目到指定目录。
   *
   * `dir` 省略时落到设置里的下载目录（内嵌面板的老行为）；双栏文件窗口会显式传本机栏
   * 的当前目录，让「下载」就是把右边选中的东西放到左边正看着的那个文件夹。
   * 下载这一步不弹系统保存框：一次批量下载弹 N 个框太吵，落点要么在设置里选好（那边有
   * 「选择…」的原生对话框），要么就是本机栏正显示的目录。目录为空时给出可执行的提示，不静默失败。
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
    cancelEdits(connection)
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
    createFile,
    remove,
    removeMany,
    rename,
    chmod,
    editRemote,
    cancelEdits,
    download,
    upload,
    close,
    onFollowed,
    onCwdChanged,
    errorOf,
  }
})
