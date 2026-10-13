import { decodeBase64 } from './bytes'

/**
 * known_hosts 第三列 ⇒ OpenSSH 风格的 `SHA256:...` 指纹。
 *
 * 指纹必须和 `ssh-keygen -lf` 对得上，否则用户没法拿它跟运维给的记录核对。
 * OpenSSH 的规则是：对**整段 wire-format blob**（含算法名）做 SHA-256，再 base64url、去掉 `=` 填充。
 * 后端的 `KnownHost.base64_key` 存的就是这一整段，所以这里不需要再拼算法名。
 */
export async function sshFingerprint(base64Key: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', decodeBase64(base64Key))
  const binary = String.fromCharCode(...new Uint8Array(digest))
  return `SHA256:${btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '')}`
}
