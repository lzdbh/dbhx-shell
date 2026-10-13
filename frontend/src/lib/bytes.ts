/**
 * 跨 IPC 的二进制搬运。
 *
 * 终端输出在线格式上是 base64 字符串（见 `crates/dbhx-ipc/src/bytes.rs`），
 * 而 `atob` 给出的是「每个字符低 8 位是一个字节」的二进制串，
 * 必须逐字符 `charCodeAt` 取出来才不丢 >0x7F 的字节——ESC/CSI 序列全是这类字节，
 * 用 `Uint8Array.from(atob(x))` 那种写法会把它们静默截成 0。
 */

/** base64 ⇒ 原始字节。返回类型写成 `Uint8Array<ArrayBuffer>`：`crypto.subtle` 只收这一种。 */
export function decodeBase64(text: string): Uint8Array<ArrayBuffer> {
  const binary = atob(text)
  const bytes = new Uint8Array(binary.length)
  for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i)
  return bytes
}

/**
 * 键盘输入：命令契约上是 `Vec<u8>`，生成的 TS 入参是 `number[]`。
 * 先按 UTF-8 编码再摊成数组，中文与粘贴的 emoji 才能原样送到远端。
 */
export function utf8Array(text: string): number[] {
  return Array.from(new TextEncoder().encode(text))
}
