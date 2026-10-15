//! 云同步信封：用户自存的同步串 → Argon2id → 32 字节同步密钥 → AES-256-GCM。
//!
//! 为什么要有第二层密钥：本地 `master.key` 是本机随机生成的，换一台设备根本拿不到，
//! 把它原样上传对第二台设备毫无意义。所以云端那份快照必须用一把「跨设备可重现」的
//! 密钥重新密封，而用户愿意自己抄下来带去第二台设备的只有这一串字符。
//!
//! 文件头带着 KDF 参数，是为了将来能把参数升上去而老快照仍然解得开；
//! 但读取时**不接受比当前更弱的参数**——否则一个攻击者可构造的云端文件就能把
//! 我们的 KDF 悄悄降级，「按可被公开下载设防」这个前提会被它自己掏空。

use argon2::{Algorithm, Argon2, Params, Version};
use rand::RngCore;
use zeroize::Zeroizing;

use crate::crypto::MasterKey;
use crate::error::VaultError;

/// 信封格式版本。版本不同 ⇒ 头部布局或 KDF 强度可以不同，读取时按版本分派。
const VERSION: u8 = 1;
/// 认得的最高版本。比它新的一律拒绝，而不是「尽力解解看」：
/// 解错了会报成「同步串不对」，那是最难查的一类事故。
const MAX_VERSION: u8 = VERSION;

/// 64 MiB。写死不开放配置，理由见模块头。
const KDF_MEM_KIB: u32 = 64 * 1024;
const KDF_TURNS: u32 = 3;
const KDF_PARALLELISM: u32 = 1;
const KDF_OUT_LEN: u32 = 32;

/// 16 字节随机盐，存在文件头里。同一串同步密封两次必须得到两个不同的信封。
pub(crate) const SALT_LEN: usize = 16;
/// 头部：magic(8) | version(1) | m_kib(4) | t(4) | p(4) | out_len(4) | salt(16)
const HEADER_LEN: usize = 8 + 1 + 4 * 4 + SALT_LEN;
const MAGIC: &[u8; 8] = b"DBHXSYN1";

/// 同步串的最小长度。低于这个长度的东西多半是人随手敲的短语，而云端密文按
/// 「谁都能下载、无限次离线爆破」设防，那种串撑不住。程序生成的串是 32 位，
/// 所以这条只拦得住明显不达标的输入，代价是零摩擦。
pub const MIN_SYNC_STR_LEN: usize = 16;

/// 归一化用户输入的同步串：去掉所有空白字符（含粘贴常带的 CRLF、全角空格之外的
/// 各种空白）。**必须在录入那一次做掉并存归一化后的形式**，而不是每次派生前现做——
/// 否则「第一次带空格存进去、第二次不带」这种问题会以「密钥错误」的形式浮出来，
/// 而报错长得和真的密钥错误一模一样。
pub fn normalize(input: &str) -> String {
    input.chars().filter(|c| !c.is_whitespace()).collect()
}

/// 归一化后是否够格当同步串。
pub fn is_usable(sync_str: &str) -> bool {
    sync_str.chars().count() >= MIN_SYNC_STR_LEN
}

/// 由同步串与盐派生密钥。派生用的是 `MasterKey`，所以后面可以直接复用
/// `crypto::seal/open`，不必为同步另写一套 AES——少一条代码路径就少一处能写错的地方。
fn derive(sync_str: &str, salt: &[u8], mem_kib: u32, turns: u32, parallelism: u32) -> Result<MasterKey, VaultError> {
    let params = Params::new(mem_kib, turns, parallelism, Some(KDF_OUT_LEN as usize))
        .map_err(|e| VaultError::Malformed(format!("argon2 params: {e}")))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = Zeroizing::new(vec![0u8; KDF_OUT_LEN as usize]);
    argon
        .hash_password_into(sync_str.as_bytes(), salt, &mut out)
        .map_err(|_| VaultError::Crypto)?;
    MasterKey::from_bytes(out.to_vec())
}

/// 新信封的随机盐。派生与写头部**必须用同一份**，所以拿到它之后别再换一个。
pub fn new_salt() -> [u8; SALT_LEN] {
    let mut salt = [0u8; SALT_LEN];
    rand::rng().fill_bytes(&mut salt);
    salt
}

/// 按当前参数派生同步密钥。一次同步只做一次，派生结果复用给逐条重密封与整份快照：
/// 单次派生是秒级，几百条凭据各派生一次等于功能不可用。
pub fn sync_key(sync_str: &str, salt: &[u8; SALT_LEN]) -> Result<MasterKey, VaultError> {
    derive(sync_str, salt, KDF_MEM_KIB, KDF_TURNS, KDF_PARALLELISM)
}

/// 用已派生的密钥与**派生它时那份盐**封一份明文。
pub fn seal_with(key: &MasterKey, salt: &[u8; SALT_LEN], plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
    let sealed = crate::crypto::seal(key, plaintext)?;
    let mut out = Vec::with_capacity(HEADER_LEN + sealed.len());
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    // 头部一律小端：这是要落盘和跨设备读的字节序，不能跟着宿主平台走
    out.extend_from_slice(&KDF_MEM_KIB.to_le_bytes());
    out.extend_from_slice(&KDF_TURNS.to_le_bytes());
    out.extend_from_slice(&KDF_PARALLELISM.to_le_bytes());
    out.extend_from_slice(&KDF_OUT_LEN.to_le_bytes());
    out.extend_from_slice(salt);
    out.extend_from_slice(&sealed);
    Ok(out)
}

/// 校验头部并取出密文体，**不派生密钥**。
///
/// 拆成两步不是为了好看：调用方要先拿到盐才能只做一次派生，
/// 然后拿同一把密钥既解开整份快照、又解开里面的每一条密文。
/// 版本与参数在这一步就硬校验，降级攻击在读到密钥之前就会被挡掉。
pub fn header_of(blob: &[u8]) -> Result<(&[u8; SALT_LEN], &[u8]), VaultError> {
    if blob.len() <= HEADER_LEN {
        return Err(VaultError::Malformed("sync blob is too short".into()));
    }
    let (header, sealed) = blob.split_at(HEADER_LEN);
    if &header[..8] != MAGIC {
        return Err(VaultError::Malformed("sync blob magic mismatch".into()));
    }
    let version = header[8];
    if version > MAX_VERSION {
        return Err(VaultError::Malformed(format!(
            "sync blob version {version} is newer than supported {MAX_VERSION}"
        )));
    }
    let u32_at = |i: usize| -> u32 {
        u32::from_le_bytes(header[i..i + 4].try_into().expect("header 已按定长切好"))
    };
    let (mem_kib, turns, parallelism, out_len) = (u32_at(9), u32_at(13), u32_at(17), u32_at(21));
    if version != VERSION
        || (mem_kib, turns, parallelism, out_len)
            != (KDF_MEM_KIB, KDF_TURNS, KDF_PARALLELISM, KDF_OUT_LEN)
    {
        // 目前只认 v1 那一组参数。将来加 v2 时在这里按版本分派，而不是放松校验。
        return Err(VaultError::Malformed(format!(
            "sync blob kdf params (v{version}, {mem_kib}/{turns}/{parallelism}/{out_len}) are not accepted"
        )));
    }
    let salt = header[25..HEADER_LEN]
        .try_into()
        .expect("头部切片按 SALT_LEN 定长切好");
    Ok((salt, sealed))
}

/// 把明文快照封成云端 blob。盐随机生成，不需要调用方保存。
pub fn seal_blob(sync_str: &str, plaintext: &[u8]) -> Result<Vec<u8>, VaultError> {
    let salt = new_salt();
    let key = sync_key(sync_str, &salt)?;
    seal_with(&key, &salt, plaintext)
}

/// 解开云端 blob。密钥错、blob 被改过、版本不认识——都返回 `Crypto`/`Malformed`，
/// 由上层决定文案；这里刻意不区分「串错了」和「文件坏了」，避免给攻击者多一条判据。
pub fn open_blob(sync_str: &str, blob: &[u8]) -> Result<Vec<u8>, VaultError> {
    let (salt, sealed) = header_of(blob)?;
    let key = sync_key(sync_str, salt)?;
    crate::crypto::open(&key, sealed).map(|plain| plain.to_vec())
}

/// 生成一把新串：24 随机字节 → base64url 正好 32 字符（192 bit）。
/// 用 base64url 而不是标准 base64：`-_` 不会像 `+/` 那样在双击选中时被截断，
/// 也不会在拷贝途中被某些终端或论坛当成表情符号改写。
pub fn generate_sync_str() -> String {
    let mut raw = [0u8; 24];
    rand::rng().fill_bytes(&mut raw);
    let b64 = crate::store::base64_encode(&raw);
    b64.replace('+', "-").replace('/', "_").trim_end_matches('=').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sync_str() -> String {
        generate_sync_str()
    }

    #[test]
    fn generated_string_is_32_url_safe_chars_and_distinct_each_call() {
        let a = sync_str();
        let b = sync_str();
        assert_eq!(a.chars().count(), 32, "24 字节 base64url 去掉填充应正好 32 字符");
        assert_ne!(a, b);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "生成了不可安全粘贴的字符: {a}"
        );
        assert!(is_usable(&a));
    }

    #[test]
    fn blob_round_trips_and_hides_plaintext() {
        let s = sync_str();
        let plain = r#"{"hosts":[{"label":"生产库-SECRET-MARKER"}]}"#.as_bytes();
        let blob = seal_blob(&s, plain).unwrap();
        assert!(
            !blob
                .windows(12)
                .any(|w| w == b"SECRET-MARKE"),
            "明文标记出现在信封里"
        );
        assert_eq!(open_blob(&s, &blob).unwrap(), plain.to_vec());
    }

    #[test]
    fn same_plaintext_twice_yields_different_blobs_because_of_the_salt() {
        let s = sync_str();
        let a = seal_blob(&s, b"x").unwrap();
        let b = seal_blob(&s, b"x").unwrap();
        assert_ne!(a, b);
        // 盐不同 ⇒ 头部不同；两条都要能解
        assert_ne!(&a[25..HEADER_LEN], &b[25..HEADER_LEN]);
        assert_eq!(open_blob(&s, &a).unwrap(), b"x");
        assert_eq!(open_blob(&s, &b).unwrap(), b"x");
    }

    #[test]
    fn wrong_string_fails_as_crypto_and_not_as_garbage() {
        let blob = seal_blob(&sync_str(), b"secret").unwrap();
        // 错串既不能解密文，也不能被当成「格式坏」——两者都归到解不开这一类
        assert!(matches!(open_blob(&sync_str(), &blob), Err(VaultError::Crypto)));
    }

    #[test]
    fn pasted_whitespace_does_not_change_the_derived_key() {
        let s = sync_str();
        let blob = seal_blob(&s, b"payload").unwrap();
        // 这一条测的是「录入时已归一化」这个约定：上层存的就是 normalize 的结果，
        // 而用户在第二台设备上粘贴时手抖带上换行/空格，也要能解得开。
        for noisy in [format!(" {s}"), format!("{s}\r\n"), format!("{s}\u{00A0}")] {
            assert_eq!(
                open_blob(&normalize(&noisy), &blob).unwrap(),
                b"payload",
                "归一化没能吃掉这段空白"
            );
        }
    }

    #[test]
    fn tampered_body_and_downgraded_kdf_params_are_both_rejected() {
        let s = sync_str();
        let mut blob = seal_blob(&s, b"payload").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(matches!(open_blob(&s, &blob), Err(VaultError::Crypto)));

        // 把 m_kib 改小：一个「看起来能解」的降级头部不该被接受，
        // 否则攻击者只要替换云端文件就能把 KDF 强度降到可爆破。
        let mut downgraded = seal_blob(&s, b"payload").unwrap();
        downgraded[9..13].copy_from_slice(&8u32.to_le_bytes());
        assert!(matches!(open_blob(&s, &downgraded), Err(VaultError::Malformed(_))));

        // 未来版本同理：宁可让用户升级应用，也不「尽力解解看」
        let mut newer = seal_blob(&s, b"payload").unwrap();
        newer[8] = VERSION + 1;
        assert!(matches!(open_blob(&s, &newer), Err(VaultError::Malformed(_))));
    }

    #[test]
    fn truncated_or_foreign_blobs_are_malformed_not_panics() {
        let s = sync_str();
        for bad in [vec![], b"short".to_vec(), vec![0u8; HEADER_LEN], b"not a sync blob at all, but long enough to pass the length check".to_vec()] {
            assert!(
                matches!(open_blob(&s, &bad), Err(VaultError::Malformed(_))),
                "输入 {bad:?} 没有走 Malformed 分支"
            );
        }
    }

    #[test]
    fn too_short_strings_are_marked_unusable() {
        assert!(!is_usable("hunter2"));
        assert!(!is_usable(&"a".repeat(MIN_SYNC_STR_LEN - 1)));
        assert!(is_usable(&"a".repeat(MIN_SYNC_STR_LEN)));
    }

    /// 钉住这台机器上的真实耗时量级。它不是性能优化，而是**产品语义**：
    /// 每次同步都要派生一次，实测 debug 构建单侧 5.09s（release 约莫一个量级更小），
    /// 所以 UI 必须有「正在解密」的反馈，否则在慢机器上就是「点了没反应」。
    /// 谁把参数调大，这条会连带把耗时写进失败信息里，逼他同时改前端。
    #[test]
    fn derive_stays_within_the_budget_the_ui_assumes() {
        // debug 与 release 差一个数量级，用同一条阈值等于要么形同虚设要么天天红。
        // CI 跑 debug，所以 release 那条是给本地 `cargo test --release` 用的。
        let budget_ms: u128 = if cfg!(debug_assertions) { 15_000 } else { 2_000 };
        let started = std::time::Instant::now();
        let s = sync_str();
        let blob = seal_blob(&s, b"payload").unwrap();
        let sealed_in = started.elapsed();
        let _ = open_blob(&s, &blob).unwrap();
        let opened_in = started.elapsed() - sealed_in;
        eprintln!(
            "argon2id {KDF_MEM_KIB}KiB/{KDF_TURNS}x{KDF_PARALLELISM} [{}] 密封 {sealed_in:?} 解开 {opened_in:?}",
            if cfg!(debug_assertions) { "debug" } else { "release" }
        );
        assert!(
            sealed_in.as_millis() < budget_ms && opened_in.as_millis() < budget_ms,
            "单次派生 密封 {sealed_in:?} / 解开 {opened_in:?}，超出 {budget_ms}ms，UI 的进度反馈假设不再成立"
        );
    }
}
