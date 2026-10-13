//! 时间只做单位换算，不碰文件系统与时区数据库。
//!
//! 全项目的 `*_ms` 字段都用这一个函数取，避免各处 `SystemTime` 写法漂移。

use std::time::{SystemTime, UNIX_EPOCH};

/// Unix epoch 毫秒。系统时钟早于 epoch 时返回 0，而不是 panic。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_ms_is_in_milliseconds_not_seconds() {
        // 2024-01-01 之后的任何时刻，毫秒时间戳必然跨过 1.7e12 这个量级
        assert!(now_ms() > 1_700_000_000_000);
    }
}
