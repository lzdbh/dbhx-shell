//! 本地终端的启动参数，以及它到 `portable_pty::CommandBuilder` 的翻译。
//!
//! 翻译单独拆出来是因为它是这个 crate 里唯一能被完整单元测试的部分：
//! argv、环境变量、cwd 的组装全是纯逻辑，不碰伪终端。

use std::path::Path;

use dbhx_model::{ConnectSpec, PtySize};
use portable_pty::{CommandBuilder, PtySize as WirePtySize};

use crate::error::PtyError;

/// 起一个本地 shell（或任意程序）需要的全部信息。
///
/// `program` 为 `None` 时用系统的默认 shell：Windows 取 `ComSpec`（即 cmd.exe），
/// Unix 取 passwd 里的 shell。注意 `new_default_prog()` 构造出来的 builder 一旦
/// 调 `arg()` 就会 panic，所以「没给程序却给了参数」必须在这一层就判死，
/// 不能留到 spawn 时炸。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSpec {
    pub program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub term: String,
    pub pty: PtySize,
    /// 追加/覆盖的环境变量。基底是当前进程的环境（portable-pty 会带上）。
    pub env: Vec<(String, String)>,
}

impl Default for LocalSpec {
    fn default() -> Self {
        Self {
            program: None,
            args: Vec::new(),
            cwd: None,
            term: "xterm-256color".to_string(),
            pty: PtySize::default(),
            env: Vec::new(),
        }
    }
}

impl LocalSpec {
    /// 从会话层的统一入口参数里取本地终端需要的那几个字段。
    /// SSH 专用字段（endpoint/auth）在这里没有意义，直接忽略。
    pub fn from_connect(spec: &ConnectSpec) -> Self {
        Self {
            program: spec.shell.clone(),
            args: Vec::new(),
            cwd: spec.cwd.clone(),
            term: spec.term.clone(),
            pty: spec.pty.clone(),
            env: Vec::new(),
        }
    }

    pub fn program(mut self, program: impl Into<String>) -> Self {
        self.program = Some(program.into());
        self
    }

    pub fn args(mut self, args: &[&str]) -> Self {
        self.args = args.iter().map(|a| a.to_string()).collect();
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn pty(mut self, pty: PtySize) -> Self {
        self.pty = pty;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub(crate) fn wire_size(&self) -> WirePtySize {
        to_wire_size(&self.pty)
    }

    /// 校验 + 组装。任何校验都必须发生在 `openpty` 之前：
    /// pty 是先开再 spawn 的，参数写错却已经占了内核对象就会泄漏句柄。
    pub(crate) fn build(&self) -> Result<(CommandBuilder, WirePtySize), PtyError> {
        if self.program.is_none() && !self.args.is_empty() {
            return Err(PtyError::BadSpec(
                "使用默认 shell 时不能附带参数，请同时指定程序".to_string(),
            ));
        }
        if let Some(cwd) = &self.cwd {
            // portable-pty 对不存在的 cwd 会静默回落到 USERPROFILE，
            // 表现是「设置没生效」，所以这里必须先拦下来。
            if !Path::new(cwd).is_dir() {
                return Err(PtyError::BadCwd(cwd.clone()));
            }
        }

        let mut cmd = match &self.program {
            Some(program) => {
                let mut b = CommandBuilder::new(program);
                for arg in &self.args {
                    b.arg(arg);
                }
                b
            }
            None => CommandBuilder::new_default_prog(),
        };
        cmd.env("TERM", &self.term);
        if let Some(cwd) = &self.cwd {
            cmd.cwd(cwd);
        }
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        Ok((cmd, self.wire_size()))
    }
}

/// dbhx-model 的 `PtySize` 只有行列；wire 版还带像素尺寸，多数内核只看不用，填 0。
pub(crate) fn to_wire_size(pty: &PtySize) -> WirePtySize {
    WirePtySize {
        rows: pty.rows,
        cols: pty.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn env<'a>(cmd: &'a CommandBuilder, key: &str) -> Option<&'a OsStr> {
        cmd.get_env(key)
    }

    #[test]
    fn term_and_args_land_in_the_builder() {
        let (cmd, _) = LocalSpec::default()
            .program("nvim")
            .args(&["-c", "set number"])
            .env("EDITOR", "nvim")
            .build()
            .unwrap();
        let argv = cmd.get_argv();
        assert_eq!(argv.len(), 3, "程序本身要占 argv[0]: {argv:?}");
        assert_eq!(argv[1], "-c");
        assert_eq!(argv[2], "set number");
        assert_eq!(env(&cmd, "TERM"), Some(OsStr::new("xterm-256color")));
        assert_eq!(env(&cmd, "EDITOR"), Some(OsStr::new("nvim")));
    }

    #[test]
    fn custom_term_overrides_the_default() {
        let spec = LocalSpec { term: "vt100".into(), ..Default::default() };
        let (cmd, _) = spec.build().unwrap();
        assert_eq!(env(&cmd, "TERM"), Some(OsStr::new("vt100")));
    }

    #[test]
    fn default_shell_takes_the_no_argument_branch() {
        let (cmd, _) = LocalSpec::default().build().unwrap();
        assert!(cmd.is_default_prog());
    }

    #[test]
    fn args_without_a_program_are_rejected() {
        let spec = LocalSpec { args: vec!["-l".into()], ..Default::default() };
        assert!(matches!(spec.build(), Err(PtyError::BadSpec(_))));
    }

    #[test]
    fn missing_cwd_fails_before_openpty() {
        let err = LocalSpec::default().cwd("Z:/definitely/not/here").build().unwrap_err();
        assert!(matches!(err, PtyError::BadCwd(_)));
    }

    #[test]
    fn an_existing_cwd_is_passed_through() {
        let spec = LocalSpec::default().cwd(std::env::temp_dir().display().to_string());
        let (cmd, _) = spec.build().unwrap();
        assert!(cmd.get_cwd().is_some());
    }

    #[test]
    fn wire_size_leaves_pixels_zero() {
        let size = to_wire_size(&PtySize { rows: 40, cols: 120 });
        assert_eq!((size.rows, size.cols), (40, 120));
        assert_eq!((size.pixel_width, size.pixel_height), (0, 0));
    }

    #[test]
    fn connect_spec_contributes_only_the_local_fields() {
        let spec = ConnectSpec {
            protocol: dbhx_model::Protocol::Local,
            endpoint: None,
            auth: None,
            pty: PtySize { rows: 30, cols: 100 },
            shell: Some("/bin/zsh".into()),
            cwd: None,
            term: "xterm".into(),
        };
        let local = LocalSpec::from_connect(&spec);
        assert_eq!(local.program.as_deref(), Some("/bin/zsh"));
        assert_eq!(local.pty, PtySize { rows: 30, cols: 100 });
        assert_eq!(local.term, "xterm");
        assert!(local.args.is_empty());
    }
}
