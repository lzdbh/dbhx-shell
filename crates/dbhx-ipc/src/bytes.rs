//! 终端字节流的线格式：base64 字符串。
//!
//! 为什么不走 `Vec<u8>`：serde_json 把字节数组写成 `[27, 91, 65, ...]`，
//! 一个 4 KiB 的输出块会变成 16 KB 的文本，而 webview 那头还要把每个数字塞回数组。
//! 终端输出的消费者（xterm.js）要的是 `Uint8Array`，base64 解码一步到位，
//! 体积只膨胀 33%，不是 300%。
//!
//! 自己实现而不是引 `base64` crate：本 crate 是契约层，依赖越少，
//! CI 上「契约生成」这一步就越不容易被无关的解析失败拖住。
//! 表是标准字母表（RFC 4648），带 `=` 填充——前端 `atob` 之外还要解二进制，
//! 用 `Uint8Array.from(atob(...))` 那种写法会丢字节，所以我们只用这版编解码。

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use specta::Type;

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// 反查表：`'A' => 0`，非字母表字符为 `0xFF`。
const fn reverse_table() -> [u8; 256] {
    let mut t = [0xFFu8; 256];
    let mut i = 0;
    while i < 64 {
        t[ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    t
}

static REV: [u8; 256] = reverse_table();

fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

fn decode(input: &str) -> Result<Vec<u8>, String> {
    let bytes = input.as_bytes();
    if bytes.len() % 4 != 0 {
        return Err("base64 length must be a multiple of 4".into());
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            let v = if *c == b'=' {
                // 填充只允许出现在末尾，否则是别人拼错的串
                if i < 2 {
                    return Err("base64 padding in the middle".into());
                }
                0
            } else {
                let v = REV[*c as usize];
                if v == 0xFF {
                    return Err("byte outside the base64 alphabet".into());
                }
                v as u32
            };
            n = (n << 6) | v;
        }
        out.push((n >> 16) as u8);
        if chunk[2] != b'=' {
            out.push((n >> 8) as u8);
        }
        if chunk[3] != b'=' {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// 一段终端字节。跨 IPC 时是 base64 字符串，TS 侧就是 `string`。
///
/// `Serialize`/`Deserialize` 是下面手写的，所以这里**不能**再挂 `#[serde(...)]` 属性——
/// 没有 derive 时那个属性无处生效，编译期直接报「not an attribute」。
/// 导出成 TS `string` 靠 `#[specta(type = String)]`，它只影响类型生成、不参与序列化。
#[derive(Clone, PartialEq, Eq, Type)]
#[specta(type = String)]
pub struct ByteBlob(String);

impl ByteBlob {
    pub fn new(bytes: &[u8]) -> Self {
        Self(encode(bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        decode(&self.0)
    }
}

impl fmt::Debug for ByteBlob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 终端输出里可能有用户刚敲的口令（回显之前也在缓冲里），所以正文不打日志
        write!(f, "ByteBlob({} chars)", self.0.len())
    }
}

impl Serialize for ByteBlob {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ByteBlob {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let text = String::deserialize(d)?;
        // 入口就校验：解码失败宁可在命令层报错，也不要留一个解不出来的串到下游
        decode(&text).map_err(serde::de::Error::custom)?;
        Ok(Self(text))
    }
}

impl From<Vec<u8>> for ByteBlob {
    fn from(v: Vec<u8>) -> Self {
        Self::new(&v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_chunk_length() {
        // 1..=7 覆盖三种余数（0/1/2）与多块拼接
        for len in 0..=7 {
            let raw: Vec<u8> = (0..len).map(|i| i as u8 * 31).collect();
            let blob = ByteBlob::new(&raw);
            assert_eq!(blob.to_bytes().unwrap(), raw, "len={len}");
        }
    }

    #[test]
    fn matches_the_rfc_4648_reference_vectors() {
        for (plain, expected) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg==")] {
            assert_eq!(encode(plain.as_bytes()), expected);
            assert_eq!(decode(expected).unwrap(), plain.as_bytes());
        }
    }

    #[test]
    fn high_bytes_survive_which_atob_style_decoding_would_not() {
        // xterm 的 ESC/CSI 序列全是 >0x7F 的字节，这类字节最容易被「文本化」的编码弄坏
        let raw = vec![0x1b, 0x5b, 0x3f, 0x32, 0x35, 0x68, 0xff, 0x80, 0xc3, 0xa9];
        let blob = ByteBlob::new(&raw);
        assert_eq!(blob.to_bytes().unwrap(), raw);
    }

    #[test]
    fn serializes_as_a_bare_string_not_an_object() {
        let json = serde_json::to_string(&ByteBlob::new(b"ls")).unwrap();
        assert_eq!(json, format!("\"{}\"", encode(b"ls")));
    }

    #[test]
    fn deserialize_rejects_garbage_at_the_boundary() {
        // 前端送来非法串时要立刻失败，而不是等下游解码才发现
        let err = serde_json::from_str::<ByteBlob>("\"!!!not base64!!!\"").unwrap_err();
        assert!(err.to_string().contains("alphabet"), "{err}");
        let short = serde_json::from_str::<ByteBlob>("\"Zm9\"").unwrap_err();
        assert!(short.to_string().contains("multiple of 4"), "{short}");
    }

    #[test]
    fn padding_in_the_middle_is_not_a_valid_encoding() {
        assert!(decode("Z=m9").is_err());
    }

    #[test]
    fn debug_never_prints_the_payload() {
        let blob = ByteBlob::new(b"secret-then-enter\r");
        let text = format!("{blob:?}");
        assert!(!text.contains('Z'), "{text}");
        assert!(text.contains("chars"), "{text}");
    }
}
