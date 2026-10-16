//! 同步密钥的生命周期：一次派生，供整条链路复用。
//!
//! Argon2id 单次派生在这台机器上是秒级的（`envelope` 里那条测试钉住了量级），
//! 而一次同步天然要做两件事：把云端那一份解开、把本地这一份封好。
//! 如果每个密文条目都各派生一次，几百条凭据就是几百次派生——功能会直接不可用。
//! 所以这里把「派生」做成显式的两个入口，密钥留在对象里，调用层拿不到裸 `MasterKey`。
//!
//! **拉取与推送必须用不同的盐**（于是是不同的密钥）。理由不是卫生习惯而是云端的现实：
//! 片段服务会留下历史版本，云端因此可能同时躺着好几份旧密文 —— GitHub 的 gist
//! revision 实测删不掉，Gitee 那边的历史是否保留没测出来。既然换一把新盐不花什么，
//! 就按最坏的那一家设防：复用盐等于让这堆历史密文共用一把密钥，
//! 攻破任意一份就等于攻破全部。

use crate::crypto::MasterKey;
use crate::envelope::{self, SALT_LEN};
use crate::error::VaultError;
use crate::snapshot::SyncSnapshot;

/// 一把已经派生好的同步密钥，连同它对应的盐。
///
/// 盐必须由对象带着走：派生用的盐和写进头部的盐**必须是同一份**，
/// 分两次随机生成会得到一份永远解不开的云端文件。
pub struct DerivedKey {
    key: MasterKey,
    /// 写入头部的那份盐。`None` 表示这份密钥是从云端 blob 解出来的，
    /// 它的盐属于那一份历史密文，不能拿去封下一次要推的内容。
    push_salt: Option<[u8; SALT_LEN]>,
}

impl DerivedKey {
    /// 为「即将往上推的那一份」派生：随机新盐 ⇒ 新密钥，历史 revision 各用各的锁。
    pub fn for_push(sync_str: &str) -> Result<Self, VaultError> {
        let salt = envelope::new_salt();
        Ok(Self {
            key: envelope::sync_key(sync_str, &salt)?,
            push_salt: Some(salt),
        })
    }

    /// 为「云端已经存在的这一份」派生，并顺手把它解开成快照。
    pub fn from_blob(sync_str: &str, blob: &[u8]) -> Result<(Self, SyncSnapshot), VaultError> {
        let (salt, body) = envelope::header_of(blob)?;
        let key = envelope::sync_key(sync_str, salt)?;
        let plain = crate::crypto::open(&key, body)?;
        let snap: SyncSnapshot = serde_json::from_slice(&plain)
            .map_err(|e| VaultError::Malformed(format!("sync snapshot: {e}")))?;
        snap.check_version()
            .map_err(VaultError::Malformed)?;
        Ok((
            Self {
                key,
                push_salt: None,
            },
            snap,
        ))
    }

    /// 把快照封成要上传的 blob。只有 `for_push` 出来的密钥能封：
    /// 拿着一份历史密文的盐去封新内容，等于把两者锁在同一把钥匙上。
    pub fn seal_snapshot(&self, snap: &SyncSnapshot) -> Result<Vec<u8>, VaultError> {
        let salt = self.push_salt.ok_or_else(|| {
            VaultError::Malformed(
                "a key read from a cloud blob cannot seal a new blob; derive a fresh one".into(),
            )
        })?;
        let plain = serde_json::to_vec(snap)
            .map_err(|e| VaultError::Malformed(format!("sync snapshot: {e}")))?;
        envelope::seal_with(&self.key, &salt, &plain)
    }

    pub(crate) fn inner(&self) -> &MasterKey {
        &self.key
    }
}

impl std::fmt::Debug for DerivedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 与 MasterKey 同一约定：密钥材料一律不进 Debug
        f.debug_struct("DerivedKey").finish_non_exhaustive()
    }
}

#[cfg(test)]
impl DerivedKey {
    /// 测试用：跳过派生直接持有一把密钥。
    ///
    /// 导出与合并的行为跟「这把密钥怎么来的」无关，而真实派生一次要好几秒；
    /// 没有这个入口，每条合并规则都要为它付一次 Argon2id，测试会慢到没人愿意跑。
    /// 真走派生那条路径的端到端用例在 `store::tests` 里，只留一次。
    pub(crate) fn for_test(key: MasterKey) -> Self {
        Self {
            key,
            push_salt: Some([7u8; SALT_LEN]),
        }
    }
}
