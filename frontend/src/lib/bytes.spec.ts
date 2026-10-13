import { describe, expect, it } from 'vitest'

import { decodeBase64, utf8Array } from './bytes'

/** 与 Rust 侧 `ByteBlob` 同一套线格式：标准字母表 + `=` 填充。 */
function encode(bytes: Uint8Array): string {
  return btoa(String.fromCharCode(...bytes))
}

describe('decodeBase64', () => {
  it('keeps bytes above 0x7F that a text-ish decode would truncate', () => {
    // `reset terminal` + 一个 0xFF：这类高位字节是 xterm 序列的常态
    const raw = new Uint8Array([0x1b, 0x5b, 0x63, 0xff, 0x80])
    expect(Array.from(decodeBase64(encode(raw)))).toEqual([0x1b, 0x5b, 0x63, 0xff, 0x80])
  })

  it('decodes the RFC 4648 reference vectors', () => {
    expect(Array.from(decodeBase64('Zm9vYg=='))).toEqual([102, 111, 111, 98])
    expect(Array.from(decodeBase64(''))).toEqual([])
  })
})

describe('utf8Array', () => {
  it('encodes multi-byte input instead of taking char codes', () => {
    // 命令契约是 Vec<u8>，按 charCode 摊开会把手敲的中文截成两个错字节
    expect(utf8Array('é')).toEqual([0xc3, 0xa9])
    expect(utf8Array('\r')).toEqual([13])
  })
})
