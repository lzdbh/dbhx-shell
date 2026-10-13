//! 测试用的密钥样本。
//!
//! 内容与 `dbhx-ssh` 的 `fixtures` 完全一致（同一把 `ssh-keygen` 真生成的密钥）。
//! 之所以复制而不是让 `dbhx-ssh` 把它 `pub` 出来：那批夹具是那边「与 OpenSSH 字节级一致」
//! 断言的一部分，为了这边的测试而进公开 API，等于把测试数据变成契约。
//! 两边若要换密钥，必须同时换——`key_is_encrypted` 的判定结果依赖它。

#![cfg(test)]

/// 无口令，注释 `dbhx-test`
pub const PLAIN_ED25519: &str = concat!(
    "-----BEGIN OPENSSH PRIVATE KEY-----\n",
    "b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW\n",
    "QyNTUxOQAAACCqjLiVugxar0txxix3jN0xmTy6HDFxrbnz8hDI48Ij7gAAAJBTtu8yU7bv\n",
    "MgAAAAtzc2gtZWQyNTUxOQAAACCqjLiVugxar0txxix3jN0xmTy6HDFxrbnz8hDI48Ij7g\n",
    "AAAEC6kRqKvR66+jJL8t2p/TwyEpTfMuLnJ29n7et68RRskKqMuJW6DFqvS3HGLHeM3TGZ\n",
    "PLocMXGtufPyEMjjwiPuAAAACWRiaHgtdGVzdAECAwQ=\n",
    "-----END OPENSSH PRIVATE KEY-----\n",
);

/// 口令 `dbhx-pass`，aes256-ctr + bcrypt
pub const ENC_ED25519: &str = concat!(
    "-----BEGIN OPENSSH PRIVATE KEY-----\n",
    "b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jdHIAAAAGYmNyeXB0AAAAGAAAABBqrp7FeO\n",
    "amntYLSx8Hg/CzAAAAGAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIJW61///nXWvtSuR\n",
    "FtwyrH6cAkreKErdHx+F7/j3ljtvAAAAkNBOsXyQNadqhliG5u7xkQzS+v7wqQGaoKtBuN\n",
    "zAVHImMO4D+XPbjDjxeiQMPQCIWjSw0eTEGx9KIs6aFunQNClVNTgNeUVh2oSudSlswCMn\n",
    "UhBbUe8nPE3RgZZatuTyVajhcnridkJxu52r3ykpWUqwNfsLUJmY8tmeKOn/Boy3Dhzjef\n",
    "tb3TRU+T2Zfvdu8A==\n",
    "-----END OPENSSH PRIVATE KEY-----\n",
);

/// PuTTY 的 `.ppk` 头部形状（内容不是可用的键，只用来验证格式识别）
pub const PPK_HEADER: &str = "PuTTY-User-Key-File-3: ssh-ed25519\nEncryption: none\n";
