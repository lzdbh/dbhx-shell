import type { LiveEndpoint, Protocol } from './ipc/bindings'

/**
 * 会话恢复：把「这次开了哪些会话」记在 localStorage，下次启动让用户一键重开。
 *
 * **只存显示名与拨号参数，不存任何凭据。** 主机 id 也不存——契约上 `SessionInfo`
 * 就没有 hostId（会话层只认 `AuthRef`），所以我们拿 endpoint 回 vault 反查：
 * 查到就用主机现在保存的凭据重连，查不到（记录被删了、或者当初是手输的地址）
 * 只列出来让用户自己选，绝不替我们保管口令。
 *
 * 重连是**用户点一下才做**的，启动时只弹提示：无人值守地往生产机拨号
 * 不该是应用启动那一刻自动发生的事。
 */

const KEY = 'dbhx:session-restore'

/** 一条会话快照。`endpoint` 为本机终端时为 null。 */
export interface SessionSnapshot {
  label: string
  protocol: Protocol
  endpoint: LiveEndpoint | null
}

/** 反查之后的形态：能不能自动重连、要怎么做。 */
export type Restorable =
  | { kind: 'local', label: string }
  | { kind: 'host', label: string, hostId: string }
  /** 主机记录已经不在了，只能让用户重新选一台 */
  | { kind: 'unknown', label: string, endpoint: LiveEndpoint }

interface HostLike {
  id: string
  protocol: Protocol
  endpoint: { host: string, port: number | null, username: string }
}

/** 协议默认端口：反查主机记录时，把「端口为 null」补齐成它拨号时真正用的端口。 */
function defaultPort(protocol: Protocol): number {
  return protocol === 'telnet' ? 23 : 22
}

/** 存的时候只留列表里那些还能用的 tab（error/closed 也留，用户可能想原样重开）。 */
export function save(tabs: { label: string, protocol: Protocol, endpoint: LiveEndpoint | null }[]): void {
  const list: SessionSnapshot[] = tabs.map(tab => ({
    label: tab.label,
    protocol: tab.protocol,
    endpoint: tab.endpoint,
  }))
  write(list)
}

export function write(list: SessionSnapshot[]): void {
  try {
    // 空列表要删键，否则上次的记录会一直赖在提示里
    if (list.length === 0) localStorage.removeItem(KEY)
    else localStorage.setItem(KEY, JSON.stringify(list))
  } catch {
    /* 配额或隐私模式：恢复是锦上添花，不值得为此打断操作 */
  }
}

export function savedSessions(): SessionSnapshot[] {
  let raw: string | null
  try {
    raw = localStorage.getItem(KEY)
  } catch {
    return []
  }
  if (!raw) return []
  try {
    const parsed: unknown = JSON.parse(raw)
    if (!Array.isArray(parsed)) return []
    // 结构会随版本变形，逐条验明再收下；坏一条不能带走整份
    return parsed.filter(isSnapshot)
  } catch {
    return []
  }
}

export function clearSaved(): void {
  try {
    localStorage.removeItem(KEY)
  } catch {
    /* 同上 */
  }
}

function isSnapshot(value: unknown): value is SessionSnapshot {
  if (typeof value !== 'object' || value === null) return false
  const shape = value as { label?: unknown, protocol?: unknown, endpoint?: unknown }
  if (typeof shape.label !== 'string') return false
  if (shape.protocol !== 'ssh' && shape.protocol !== 'local' && shape.protocol !== 'telnet') return false
  if (shape.endpoint === null) return true
  if (typeof shape.endpoint !== 'object') return false
  const endpoint = shape.endpoint as { host?: unknown, port?: unknown, username?: unknown }
  return typeof endpoint.host === 'string' && typeof endpoint.port === 'number' && typeof endpoint.username === 'string'
}

/**
 * 用 endpoint 回主机列表反查 id。
 *
 * 比对的是「拨号身份」而不是 label——label 随时能改，endpoint 才是那台机器。
 * `HostRecord` 的 port 为 null 表示协议默认端口，会话侧的 `LiveEndpoint` 已经把它
 * 落实成数字（SSH 22、telnet 23），所以两边都要按**各自协议的默认**补齐才能对上。
 */
export function resolve(saved: SessionSnapshot[], hosts: HostLike[]): Restorable[] {
  const index = new Map<string, string>()
  hosts.forEach(host => {
    index.set(key(host.endpoint.username, host.endpoint.host, host.endpoint.port ?? defaultPort(host.protocol)), host.id)
  })
  return saved.map((entry): Restorable => {
    if (entry.protocol === 'local') return { kind: 'local', label: entry.label }
    if (entry.endpoint === null) return { kind: 'unknown', label: entry.label, endpoint: fallbackEndpoint(entry.label) }
    const hostId = index.get(key(entry.endpoint.username, entry.endpoint.host, entry.endpoint.port))
    if (hostId) return { kind: 'host', label: entry.label, hostId }
    return { kind: 'unknown', label: entry.label, endpoint: entry.endpoint }
  })
}

function key(username: string, host: string, port: number): string {
  return `${username}@${host}:${port}`
}

/** 快照里 ssh 却缺 endpoint（旧版本或手工改过的存储）——给一个占位，反正走 unknown 分支。 */
function fallbackEndpoint(label: string): LiveEndpoint {
  return { host: label, port: 22, username: '' }
}

/** 提示文案用的地址。telnet 没有用户名，就别渲染成刺眼的 `@host:port`。 */
export function describe(endpoint: LiveEndpoint): string {
  const where = `${endpoint.host}:${endpoint.port}`
  return endpoint.username ? `${endpoint.username}@${where}` : where
}
