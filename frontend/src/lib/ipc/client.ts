import { commands } from './bindings'
import type {
  ConnectSpec,
  ErrorCode,
  HostDraft,
  HostId,
  IdentityId,
  Pong,
  ProviderKind,
  PtySize,
  SessionId,
  UserReply,
} from './bindings'
import { normalizeIpcError } from './errors'

/** tauri-specta 生成的命令返回的可辨识联合（不抛异常，错误在 `error` 分支里）。 */
type SpectaResult<T> = { status: 'ok', data: T } | { status: 'error', error: unknown }

/**
 * 把 Result 式返回折成异常，让调用侧能用 try/catch 与 `describeIpcError`。
 * 统一在这里抛，前端其他地方不需要知道 tauri-specta 的返回形状。
 */
async function unwrap<T>(result: Promise<SpectaResult<T>>): Promise<T> {
  const settled = await result
  if (settled.status === 'ok') return settled.data
  throw normalizeIpcError(settled.error)
}

/**
 * 命令层按领域分组，页面只认这六个入口。
 *
 * 不直接 re-export `commands` 的原因：生成物是「一个参数一个位置」的形状，
 * 调用点散开之后没人看得出漏了哪个参数；分组之后每个入口都有明确的所有权，
 * 也能在这一层把「前端字符串」翻译成契约枚举。
 */

/** 契约上没有「明文口令」这种返回值，所以入参方向上允许出现明文。 */
export const sessionApi = {
  openRemote: (hostId: HostId, pty: PtySize) => unwrap(commands.sessionOpenRemote(hostId, pty)),
  openLocal: (spec: ConnectSpec, label: string) => unwrap(commands.sessionOpenLocal(spec, label)),
  /** 键盘输入走 `number[]`：每次只有几个字节，不值得为它做 base64。 */
  write: (session: SessionId, bytes: number[]) => unwrap(commands.sessionWrite(session, bytes)),
  resize: (session: SessionId, pty: PtySize) => unwrap(commands.sessionResize(session, pty)),
  close: (session: SessionId) => unwrap(commands.sessionClose(session)),
  list: () => unwrap(commands.sessionList()),
  /**
   * 开始抄录某个会话的输出。`dir` 传 null 时落在后端应用数据目录的 `logs` 下；
   * `fileName` 必须是一个**纯名字**，带分隔符或盘符的会被后端拒绝。
   */
  logStart: (session: SessionId, dir: string | null, fileName: string) =>
    unwrap(commands.sessionLogStart(session, dir, fileName)),
  /** 停止抄录，回落点与已写字节数。没在记时后端报 not_found。 */
  logStop: (session: SessionId) => unwrap(commands.sessionLogStop(session)),
}

export const vaultApi = {
  hosts: () => unwrap(commands.vaultHosts()),
  upsertHost: (draft: HostDraft) => unwrap(commands.vaultUpsertHost(draft)),
  removeHost: (hostId: HostId) => unwrap(commands.vaultRemoveHost(hostId)),
  setPassword: (hostId: HostId, password: string) => unwrap(commands.vaultSetPassword(hostId, password)),
  /**
   * 按需查看已存口令。这是**唯一**一条把明文送回渲染层的调用（用户显式点击才触发），
   * 其余一切自动投影都不含口令——见后端 vault.rs 模块头规则 2 与脱敏审计测试。
   */
  revealPassword: (hostId: HostId) => unwrap(commands.vaultRevealPassword(hostId)),
  identities: () => unwrap(commands.vaultIdentities()),
  addIdentity: (label: string, privateKey: string, passphrase: string | null) =>
    unwrap(commands.vaultAddIdentity(label, privateKey, passphrase)),
  removeIdentity: (identityId: IdentityId) => unwrap(commands.vaultRemoveIdentity(identityId)),
}

/**
 * 应用锁：一个纯 UI/授权门的门面。
 *
 * 后端已经把新拨号与凭据读写挡住了，这里只是把「锁没锁、设没设 PIN」搬到界面，
 * 以及把用户敲的 PIN 明文朝内送一次——`status` 永远不回传 PIN。
 */
export const lockApi = {
  // 这条命令返回裸 `LockStatus`（不是 Result），生成物直接给数据；其余几条仍走 unwrap。
  status: () => commands.vaultLockStatus(),
  lock: () => unwrap(commands.vaultLock()),
  unlock: (pin: string) => unwrap(commands.vaultUnlock(pin)),
  setPin: (pin: string) => unwrap(commands.vaultSetLockPin(pin)),
  clearPin: () => unwrap(commands.vaultClearLockPin()),
}

export const hostkeyApi = {
  list: () => unwrap(commands.hostkeysList()),
  forget: (host: string, port: number, algorithm: string) => unwrap(commands.hostkeysForget(host, port, algorithm)),
  importSystem: () => unwrap(commands.hostkeysImport()),
}

/**
 * 云同步配置。同步串与云端令牌**只进不出**：
 * 界面能读到的只有 `has_sync_str` / `has_token` 两个布尔，贴进去的值当场就还给后端，
 * 前端不留任何副本（不写 store 的原始字段、不进 localStorage）。
 */
export const syncApi = {
  // 这条返回裸 `SyncStatus`（同 `lockApi.status`），其余几条仍走 unwrap。
  status: () => commands.syncStatus(),
  /** 生成一串新同步串。**只生成不存**，用户看完确认再调 `setStr`。 */
  newStr: () => commands.syncNewStr(),
  setStr: (text: string | null) => unwrap(commands.syncSetStr(text)),
  setToken: (text: string | null) => unwrap(commands.syncSetToken(text)),
  setProvider: (provider: ProviderKind) => unwrap(commands.syncSetProvider(provider)),
  /** 云端片段的 id。传 null 或空串 = 这台设备不再跟着任何片段。 */
  setFragment: (id: string | null) => unwrap(commands.syncSetFragment(id)),
  setPrivateKeys: (enabled: boolean) => unwrap(commands.syncSetPrivateKeys(enabled)),
  /** 一次要等十秒上下（两次 Argon2id 派生），调用方必须自己置 loading。 */
  now: () => unwrap(commands.syncNow()),
  /** 弃掉本机记着的片段，用本机这份在云上重建一个。另一台设备不会自动跟过来。 */
  recreate: () => unwrap(commands.syncRecreate()),
}

export const sftpApi = {
  /** 从终端 tab 打开：复用那条 transport，不会重新登录。 */
  openForSession: (session: SessionId) => unwrap(commands.sftpOpenForSession(session)),
  openForHost: (hostId: HostId) => unwrap(commands.sftpOpenForHost(hostId)),
  browse: (connection: number, path: string | null) =>
    unwrap(commands.sftpBrowse(connection, path)),
  mkdir: (connection: number, path: string, recursive: boolean) => unwrap(commands.sftpMkdir(connection, path, recursive)),
  /** 建空文件。`path` 可以是裸名字——后端按面板正在显示的目录拼，同名会报「已存在」而不是截断。 */
  createFile: (connection: number, path: string) => unwrap(commands.sftpCreateFile(connection, path)),
  /** `mode` 是八进制权限位（0o600 这种），后端越界会拒。 */
  chmod: (connection: number, path: string, mode: number) => unwrap(commands.sftpChmod(connection, path, mode)),
  remove: (connection: number, path: string, isDir: boolean) => unwrap(commands.sftpRemove(connection, path, isDir)),
  rename: (connection: number, from: string, to: string, overwrite: boolean) =>
    unwrap(commands.sftpRename(connection, from, to, overwrite)),
  close: (connection: number) => unwrap(commands.sftpClose(connection)),
  setFollowing: (connection: number, following: boolean) => unwrap(commands.sftpSetFollowing(connection, following)),
  setEditing: (connection: number, editing: boolean) => unwrap(commands.sftpSetEditing(connection, editing)),
  download: (connection: number, targets: string[], localDir: string) =>
    unwrap(commands.sftpDownload(connection, targets, localDir)),
  upload: (connection: number, localPaths: string[], remoteDir: string | null) =>
    unwrap(commands.sftpUpload(connection, localPaths, remoteDir)),
}

export const transferApi = {
  cancel: (batch: number) => unwrap(commands.transferCancel(batch)),
  running: () => unwrap(commands.transferRunning()),
}

/**
 * 本机文件系统浏览。文件管理窗口左半边的数据源。
 *
 * 与 SFTP 那组同形（都返回 `DirListing`），所以前端能用同一套列渲染与导航逻辑。
 * 路径是本机绝对路径，分隔符由后端按平台给（Windows 是 `\`）。
 */
export const localApi = {
  home: () => unwrap(commands.localHome()),
  drives: () => commands.localDrives(),
  /**
   * 弹原生的「选择文件夹」对话框，取消返回 `null`（那不是错误，别 unwrap）。
   * `startPath` 是对话框开在哪个目录，设置页把当前已存的值传进来。
   */
  pickDir: (startPath: string | null) => commands.localPickDir(startPath),
  list: (path: string | null) => unwrap(commands.localList(path)),
  mkdir: (path: string) => unwrap(commands.localMkdir(path)),
  /** 建空文件；同名会拒，绝不截断已有内容。 */
  createFile: (path: string) => unwrap(commands.localCreateFile(path)),
  remove: (path: string, isDir: boolean) => unwrap(commands.localRemove(path, isDir)),
  rename: (from: string, to: string) => unwrap(commands.localRename(from, to)),
  /** 单个条目的属性，跟随符号链接（外部编辑器要的是真实文件的修改时间）。 */
  stat: (path: string) => unwrap(commands.localStat(path)),
  /** 交给系统关联程序打开。进程起得来就算成功，不等它退出。 */
  open: (path: string) => unwrap(commands.localOpen(path)),
  /** 外部编辑器往返用的临时目录（后端保证存在）。 */
  editDir: () => unwrap(commands.localEditDir()),
}

/**
 * 系统剪贴板。
 *
 * 渲染层自己的 `navigator.clipboard` 在这里不能用：WebView2 把「读剪贴板」按网页权限
 * 处理，第一次粘贴会弹系统原生的授权气泡，用户点了「阻止」之后终端粘贴就永久失效，
 * 而且没有入口改回来。桌面应用的剪贴板属于应用本身，所以下沉到 Rust。
 */
export const clipboardApi = {
  /** 剪贴板为空或内容不是文本时返回空串，不报错。 */
  read: () => unwrap(commands.localClipboardRead()),
  write: (text: string) => unwrap(commands.localClipboardWrite(text)),
}

/** 让 Rust 侧弹出（或聚焦）某台主机的文件管理窗口。 */
export const fileWindowApi = {
  open: (hostId: HostId) => unwrap(commands.openFileWindow(hostId)),
}

/** 回答后端主动发起的询问：主机密钥确认、私钥口令。 */
export const requestApi = {
  replyConfirmed: (request: number) => reply(request, { kind: 'confirmed' }),
  replyCancelled: (request: number) => reply(request, { kind: 'cancelled' }),
  /** 口令只在这一个方向上跨界，后端拿到后立刻封进 Zeroizing。 */
  replySecret: (request: number, text: string) => reply(request, { kind: 'secret', text }),
}

function reply(request: number, answer: UserReply) {
  return unwrap(commands.requestReply(request, answer))
}

/** D1 专用：让 Rust 侧按指定 code 返回错误，用来验证错误通道与文案映射。 */
export function pingFailure(code: ErrorCode, message: string): Promise<Pong> {
  return unwrap(commands.pingFailure(code, message))
}

export function ping(seq: number, echo: string): Promise<Pong> {
  return unwrap(commands.ping(seq, echo))
}
