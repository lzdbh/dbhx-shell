import type { ErrorCode, IpcError } from './bindings'

/** 面向用户的错误文案。按 code 索引，绝不解析 message 字符串做判断。 */
const HINTS: Partial<Record<ErrorCode, string>> = {
  auth_failed: '认证被服务器拒绝，请检查用户名、密码或密钥。',
  host_key_unknown: '首次连接该主机，需要确认它的主机密钥。',
  host_key_changed: '主机密钥与已保存记录不一致，可能存在中间人风险。',
  single_channel_bastion: '该堡垒机只允许单条通道，文件面板需独立登录。',
  unreachable: '无法连接到目标，请检查网络与地址。',
  timeout: '连接超时。',
  resume_mismatch: '远端文件已变化，断点续传不成立，需重新传输。',
  channel_closed: '连接已关闭，可能被空闲回收或远端断开。',
  cancelled: '操作已取消。',
}

function isIpcError(value: unknown): value is IpcError {
  return (
    typeof value === 'object'
    && value !== null
    && 'code' in value
    && 'message' in value
    && typeof (value as IpcError).code === 'string'
  )
}

/** IPC 抛出的错误统一归一化成 IpcError；非契约错误包成 internal。 */
export function normalizeIpcError(value: unknown): IpcError {
  if (isIpcError(value)) return value
  return {
    code: 'internal' as ErrorCode,
    message: value instanceof Error ? value.message : String(value),
    retryable: false,
  }
}

export function describeIpcError(err: IpcError): string {
  const hint = HINTS[err.code]
  return hint ? `${hint}（${err.message}）` : err.message
}
