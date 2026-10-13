import { commands } from './bindings'
import type {
  ConnectSpec,
  ErrorCode,
  HostDraft,
  HostId,
  IdentityId,
  Pong,
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
}

export const vaultApi = {
  hosts: () => unwrap(commands.vaultHosts()),
  upsertHost: (draft: HostDraft) => unwrap(commands.vaultUpsertHost(draft)),
  removeHost: (hostId: HostId) => unwrap(commands.vaultRemoveHost(hostId)),
  setPassword: (hostId: HostId, password: string) => unwrap(commands.vaultSetPassword(hostId, password)),
  identities: () => unwrap(commands.vaultIdentities()),
  addIdentity: (label: string, privateKey: string, passphrase: string | null) =>
    unwrap(commands.vaultAddIdentity(label, privateKey, passphrase)),
  removeIdentity: (identityId: IdentityId) => unwrap(commands.vaultRemoveIdentity(identityId)),
}

export const hostkeyApi = {
  list: () => unwrap(commands.hostkeysList()),
  forget: (host: string, port: number, algorithm: string) => unwrap(commands.hostkeysForget(host, port, algorithm)),
  importSystem: () => unwrap(commands.hostkeysImport()),
}

export const sftpApi = {
  /** 从终端 tab 打开：复用那条 transport，不会重新登录。 */
  openForSession: (session: SessionId) => unwrap(commands.sftpOpenForSession(session)),
  openForHost: (hostId: HostId) => unwrap(commands.sftpOpenForHost(hostId)),
  browse: (connection: number, path: string | null) =>
    unwrap(commands.sftpBrowse(connection, path)),
  mkdir: (connection: number, path: string, recursive: boolean) => unwrap(commands.sftpMkdir(connection, path, recursive)),
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
