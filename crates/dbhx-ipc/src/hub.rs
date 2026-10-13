//! 会话事件 ⇒ 前端事件 + 文件面板跟随。
//!
//! 这一层是**胶水**，刻意不写业务判断：目录该不该跳、跳到哪儿，全部是
//! `dbhx-sftp::follow::FollowGate` 的决定（那边有完整的离线单测）。
//! 这里只负责「拿到决定之后去做一次 `browse`，再把结果发出去」。
//!
//! 为什么值得单独一个文件：`SessionManager::take_events` 只能有一个读者，
//! 两个读者会随机丢一半字节——所以全应用的会话事件必须**恰好**经过这一个任务。

use std::sync::Arc;

use dbhx_model::SessionId;
use dbhx_session::SessionEvent;
use dbhx_sftp::CwdSource;
use tokio::sync::mpsc;

use crate::events::{BusEvent, FollowHold, SessionCwd, SessionEnded, TermData};
use crate::state::Core;

/// 唯一的会话事件读者。跑在 tauri 的异步运行时上，直到事件流自己结束。
pub async fn run_hub(core: Arc<Core>, mut rx: mpsc::Receiver<SessionEvent>) {
    while let Some(event) = rx.recv().await {
        match event {
            SessionEvent::Data { id, bytes } => {
                // 用 send 而不是 try_send：终端输出不能丢。队列满的时候这里 await，
                // 背压顺着 SSH 通道一路传到对端，那是用户能看见的「卡了一下」。
                let data = TermData { session: id, data: bytes.as_ref().to_vec().into() };
                if core.bus.send(BusEvent::TermData(data)).await.is_err() {
                    tracing::warn!("事件总线已关闭，会话输出无处可去");
                    return;
                }
            }
            SessionEvent::Cwd { id, path } => {
                core.emit(BusEvent::SessionCwd(SessionCwd { session: id, path: path.clone() }));
                follow_cwd(&core, id, &path).await;
            }
            SessionEvent::Ended { id, info } => {
                core.emit(BusEvent::SessionEnded(SessionEnded {
                    session: id,
                    exit_status: info.exit_status,
                    signal: info.signal,
                }));
            }
        }
    }
    tracing::debug!("会话事件流结束");
}

/// 把注册表的接收端接到 Hub 上。只能成功一次，第二次调用拿到 `None`。
pub fn spawn(core: Arc<Core>) -> bool {
    match core.sessions.take_events() {
        Some(rx) => {
            tauri::async_runtime::spawn(run_hub(core, rx));
            true
        }
        None => false,
    }
}

/// 终端报了新目录 ⇒ 问一下门控 ⇒ 该跳就跳。
async fn follow_cwd(core: &Core, session: SessionId, path: &str) {
    let Some(key) = core.sessions.transport_key(session) else { return };
    let connection = core.conn_id_of(&key);

    // 段一：锁内只做决定。段二 `apply_decision` 里的导航是网络往返，
    // 绝不能抱着面板锁等它
    let Ok(decision) = core.decide(connection, |gate, blacklist| {
        let generation = gate.generation();
        gate.report(generation, Some(path), CwdSource::Osc7, blacklist)
    }) else {
        // 这台主机没有开着的文件面板：终端的 `cd` 本来就无处可跟
        return;
    };

    let followed = core.apply_decision(connection, decision).await;
    // 只把「用户能感知的结果」发出去：NothingReported / WeakerSource 这类内部记账
    // 发过去只会让面板多画一次 loading
    match followed.hold {
        Some(hold) if !worth_telling(hold) => {}
        _ => core.emit(BusEvent::SftpFollowed(followed)),
    }
}

/// 哪些「停在这里」的原因值得告诉前端。
///
/// 全列出来而不是写 `Some(_) => emit`：新增的内部记账变体默认应当**不**发，
/// 让面板反复闪的代价比少提示一次大。
fn worth_telling(hold: FollowHold) -> bool {
    matches!(hold, FollowHold::UserLocked | FollowHold::Busy | FollowHold::Editing | FollowHold::Unreachable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::FollowHold;

    #[test]
    fn only_user_visible_holds_are_emitted() {
        assert!(worth_telling(FollowHold::Busy));
        assert!(worth_telling(FollowHold::UserLocked));
        assert!(worth_telling(FollowHold::Unreachable));
        assert!(worth_telling(FollowHold::Editing));
        // 这三个是内部记账，发过去只会让面板反复闪
        assert!(!worth_telling(FollowHold::NothingReported));
        assert!(!worth_telling(FollowHold::AlreadyShown));
        assert!(!worth_telling(FollowHold::WeakerSource));
        assert!(!worth_telling(FollowHold::StaleGeneration));
    }

    #[tokio::test]
    async fn the_hub_can_be_taken_only_once() {
        let (core, _rx) = crate::state::tests_harness("hub-once", crate::state::fake_dialer());
        assert!(spawn(Arc::clone(&core)), "第一个读者必须成功");
        assert!(!spawn(Arc::clone(&core)), "第二个读者会让字节流随机丢一半");
    }
}
