import { commands } from './bindings'
import type { ErrorCode, Pong } from './bindings'
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

export function ping(seq: number, echo: string): Promise<Pong> {
  return unwrap(commands.ping(seq, echo))
}

/** D1 专用：让 Rust 侧按指定 code 返回错误，用来验证错误通道与文案映射。 */
export function pingFailure(code: ErrorCode, message: string): Promise<Pong> {
  return unwrap(commands.pingFailure(code, message))
}
