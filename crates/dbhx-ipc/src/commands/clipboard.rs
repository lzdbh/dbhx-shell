//! 系统剪贴板读写。
//!
//! 为什么要在 Rust 侧做而不是前端 `navigator.clipboard`：WebView2 把剪贴板**读**权限
//! 当成网页权限处理，第一次粘贴会弹出系统原生的「xx 想要查看复制到剪贴板的文本和图像
//! [阻止][允许]」气泡。终端粘贴是每几分钟一次的高频操作，每次都拦一道不可接受，
//! 而拒绝之后粘贴就永久失效、没有入口再改。桌面应用的剪贴板属于应用本身，
//! 所以这一层下沉到进程里，走系统 API（Windows clipboard / macOS pasteboard）。
//!
//! 沿用命令层两条铁律：
//! 1. **OS 的英文错误不外泄**。arboard 的 Display 全是英文，只进 `tracing`；
//!    跨 IPC 的是归好类的中文。
//! 2. **纯函数壳分离**。逻辑在 `*_impl`，`#[tauri::command]` 只转发。

use arboard::Error as ClipError;
use dbhx_model::{ErrorCode, IpcError};

/// 把 arboard 的错误翻成中文。分支按「用户下一步该做什么」划分，
/// 而不是照抄库里的枚举名——反正英文原文一律只进日志。
fn clipboard_err(e: ClipError) -> IpcError {
    tracing::debug!(%e, "剪贴板操作失败");
    let msg = match e {
        ClipError::ClipboardNotSupported | ClipError::ClipboardOccupied => "剪贴板暂时不可用，请稍后再试",
        ClipError::ConversionFailure => "剪贴板里的内容不是文本",
        // arboard 的枚举是 `#[non_exhaustive]`，兜底分支必须留着：上游加变体时
        // 这里应该继续编得过，而不是让整个 crate 挂掉。
        _ => "剪贴板操作失败",
    };
    IpcError::new(ErrorCode::Internal, msg, false)
}

/// 读剪贴板文本。空（或内容不是文本格式）返回空串，不当错误——
/// 「没有可粘贴的内容」是常态，让调用方按空串跳过比让它去分辨错误码更省事。
pub fn read_text_impl() -> Result<String, IpcError> {
    let mut clip = arboard::Clipboard::new().map_err(clipboard_err)?;
    text_or_empty(clip.get_text())
}

/// `get_text` 的结果 → 交给前端的文本。单独成函数是为了让上面那条空串规则能离线测。
fn text_or_empty(result: Result<String, ClipError>) -> Result<String, IpcError> {
    match result {
        Ok(text) => Ok(text),
        Err(ClipError::ContentNotAvailable) => Ok(String::new()),
        Err(e) => Err(clipboard_err(e)),
    }
}

/// 写剪贴板文本。
pub fn write_text_impl(text: &str) -> Result<(), IpcError> {
    let mut clip = arboard::Clipboard::new().map_err(clipboard_err)?;
    clip.set_text(text).map_err(clipboard_err)
}

#[tauri::command]
#[specta::specta]
pub async fn local_clipboard_read() -> Result<String, IpcError> {
    read_text_impl()
}

/// 走 `async` 而不是同步命令：剪贴板是全局单线程资源，Windows 上打开失败会重试，
/// 不该把可能的等待压在事件循环线程上。
#[tauri::command]
#[specta::specta]
pub async fn local_clipboard_write(text: String) -> Result<(), IpcError> {
    write_text_impl(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这一段是**不碰系统**就能验证的部分：错误通道的中文映射。
    /// 真正的读写要活体剪贴板，交给 UI 复现。
    #[test]
    fn arboard_english_errors_are_never_forwarded() {
        let occupied = clipboard_err(ClipError::ClipboardOccupied);
        assert_eq!(occupied.message, "剪贴板暂时不可用，请稍后再试");

        let unknown = clipboard_err(ClipError::Unknown { description: "OpenClipboard failed".into() });
        assert_eq!(unknown.message, "剪贴板操作失败");
        for err in [occupied, unknown] {
            assert!(!err.message.contains("clipboard"), "英文原文绝不能跟着 IPC 出去：{}", err.message);
            assert!(!err.retryable, "剪贴板失败不是重试能解决的");
        }
    }

    #[test]
    fn empty_clipboard_reads_as_an_empty_string() {
        assert_eq!(text_or_empty(Ok("pwd\r\n".into())).unwrap(), "pwd\r\n");
        assert_eq!(text_or_empty(Err(ClipError::ContentNotAvailable)).unwrap(), "", "空剪贴板不是错误");
        let err = text_or_empty(Err(ClipError::ConversionFailure)).unwrap_err();
        assert_eq!(err.message, "剪贴板里的内容不是文本");
    }
}
