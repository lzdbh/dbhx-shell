# dbhx shell

一个 Rust + Tauri v2 写的 SSH / SFTP 客户端：主机树、多标签终端、分屏与宫格、双栏文件面板、
本机加密的凭据库，以及可选的「主机列表云端同步」（GitHub Gist / Gitee 代码片段，密文上传）。

版本 `0.2.0`，日常在 Windows x64 上使用；安装包由 CI 产出（见下文 `release.yml`），未签名。

## 能做什么

**终端**
- SSH（密码 / keyboard-interactive / 私钥 / ssh-agent）与 telnet；本机 PTY（Windows ConPTY / Unix forkpty）
  —— 密码认证在真机上跑过；私钥与 keyboard-interactive 过了进程内测试服务器的协议路径
  （那边认证一律放行，所以「签名被服务器验通」这件事严格说没证明）；agent 只过了代码路径，**未实测**
- 多标签、上下左右分屏、宫格视图（拖主机进格子、布局持久化）；标签可拖动重排
- 查找栏（Ctrl+F，全部匹配高亮）、关键字着色 + 可选响铃、会话日志一键开关
- 选区复制 / Ctrl+V 粘贴、Ctrl+滚轮或 Ctrl+=/- 调字号、状态栏、快捷命令栏（历史与一键发送）
- TERM 可在设置里覆盖（默认 `xterm-256color`）；服务器登录横幅在会话打开后弹一次通知
- 连接后自动探测远端系统并落到主机记录上（只填「未知」的那条）
- 跳板机链：本机 → 第 1 跳 → … → 目标，逐跳 `direct-tcpip`，每一跳的主机密钥都要过裁决
- 端口转发：本地（本机端口 → 远端目标）、远端（服务器端口 → 本机目标）、动态 SOCKS5；
  配置随主机保存，连接建立时自动启动、断开或改配置时停止（远端转发要求服务器允许 `AllowTcpForwarding`）

**文件面板**
- 每个主机一个独立的双栏窗口：本机 ↔ 远端，同一 SSH 连接复用同一条 SFTP 通道
- 浏览、上传/下载（批量 + 进度托盘）、跨栏拖拽、右键菜单、`...` 上级行；本栏可按名称过滤
- 同名文件先问一次：全部覆盖 / 全部跳过 / 全部重命名，选择对整批生效
- 中断后断点续传：进度只记在本机 `transfer-journal.json`，远端不会多出任何侧车文件
- 新建目录/空文件、重命名、批量删除、chmod
- 用系统关联程序编辑远端文件：取到本机临时目录 → 交给编辑器 → 盯着它保存 → 自动传回。
  同一个文件再点一次会**接管**上一次那趟监听，而不是把你挡在门外

**凭据与主机**
- 主机树带分组、标签、收藏、克隆；双击即连
- 应用锁（PIN），锁屏覆盖层；锁着的时候不会有任何静默拨号
- 主机密钥管理，`~/.ssh/known_hosts` 只读导入、永不写回

**云同步**
- 手动「立即同步」：拉取 → 三方合并 → 重密封 → 推回，报「拉回 N 台 / 冲突 M 条」
- GitHub 与 Gitee 的配置**各存一份**（同步串、令牌、片段 id、基线），切走再切回来原样还在
- 私钥默认**不**同步，需要在设置里显式勾选
- 离线备份：设置 → 安全里把整个库（含私钥）用口令派生密钥加密成一个文件，
  可以带到另一台机器导入；导入以备份为准，本地被覆盖的内容另存为冲突副本

## 架构

```
crates/
  dbhx-model     纯类型与纯函数：不依赖 Tauri、不做 I/O
  dbhx-vault     加密 vault、信封 KDF、快照与合并、主机密钥
  dbhx-ssh       russh 拨号 / 认证 / 通道（actor 独占 transport）
  dbhx-telnet    IAC 对话与字节透传
  dbhx-pty       本地伪终端
  dbhx-session   transport 池、lease 计数、idle park、并发拨号去重
  dbhx-sftp      目录、cwd 跟随、路径、批量传输引擎
  dbhx-sync      云端片段读写：wire（唯一网络出口）/ provider（纯函数）/ sync（编排）
  dbhx-ipc       Tauri plugin：命令契约的唯一来源 + 事件总线 + 会话 Hub
  dbhx-testsrv   进程内 SSH/SFTP 测试服务器，只作为别人的 dev-dependency
src-tauri/       装配层：注册 plugin、建窗口钩子，不写业务
frontend/        Vue 3 + Pinia + AntDV 4.2 + xterm
```

分层靠的是三条硬边界，不是习惯：

- **下层不认识上层。** `dbhx-session` 不认识 vault，凭据从 `plan::PlanSource` 递进来；
  `dbhx-sftp` 只接一条已经开好的 subsystem 裸流。于是这些层的测试不需要网络也不需要 webview。
- **明文只在 `dbhx-vault` 内部解出来。** 秘密通过 `Vault::with_secret` 的回调借出，
  回调之外一律是 `SecretId`。跨 IPC 的明文出口只有一个：`vault_reveal_password`
  （编辑主机时「看一眼已存口令」）。同步串与云端令牌**只进不出**，对外只有两个布尔。
- **英文错误不出后端。** `dbhx-ipc` 的 65 条命令返回的是 `IpcError`：中文文案 + 稳定 code +
  可重试标志。tracing sink 是唯一允许留下英文原文的地方。

## 数据落在哪

应用数据目录是 Tauri 的 `app_data_dir()`，Windows 上是 `%APPDATA%\com.dbhx.shell\`
（macOS `~/Library/Application Support/com.dbhx.shell`，Linux `~/.config/com.dbhx.shell`）：

| 文件 | 内容 |
| --- | --- |
| `vault.json` | 整个库：一份 JSON 文档，`base64(AES-256-GCM(主密钥, 文档))`。先写 `.tmp` 再 rename |
| `master.key` | **兜底**主密钥。系统钥匙串可用时不存在；存在时 Unix 上 0600，Windows 靠用户目录 ACL |
| `known_hosts` | 本机记录的主机密钥（导入自 `~/.ssh/known_hosts` 的那部分不回写） |
| `transfer-journal.json` | 断点续传进度（源大小 + mtime 指纹）。搬完即删；换设备不可续 |
| `logs/` | 会话日志，按会话命名 |

主密钥默认存进系统钥匙串（Windows 凭据管理器 / macOS Keychain / Linux Secret Service）。
首次启动时若发现旧的 `master.key`，会**先写入钥匙串、读回比对一致，再删掉明文文件**；
钥匙串不可用（无桌面会话的 Linux、CI）或迁移失败时，保留/回退到应用目录下的文件，
设置 → 安全页会显示当前来源。**还没有**用户口令派生：能解锁系统钥匙串的人就能解开库。
云端那份密文比本机这份硬——它必须按
「gist 谁都能下载」设防，用 Argon2id 从同步串派生密钥，出库即密文。

## 同步怎么用

1. 设置 → 同步：选一家，生成或粘贴**同步串**（程序生成的是 32 字符，最短 16），
   贴上这家的**访问令牌**。两个秘密分开存，换令牌不会把同步串弄丢。
2. 第一次点「立即同步」会在这家建一个私有片段，并把它的 id 记进本机配置。
3. 第二台设备：贴同一个同步串 + 自己的令牌，再把片段 id 抄进「云端片段 id」——
   光有同步串找不到云上那一份。
4. 只想从头来一份：「重建云端片段」。另一台设备仍跟着老片段，直到有人把新 id 抄过去。

同步只在点按钮时发生，没有开机自动拉取，也没有改完就推。一次要等十秒上下
（两次 Argon2id 派生），这期间其它碰库的命令会排队——合并必须看到一致的本机状态。

## 开发

前置：Rust ≥ 1.89（`rust-version`）、Node ≥ 22（CI 用 22）、pnpm 11.18.0
（与 CI 对齐，pnpm 10→11 改过构建脚本审批语义）。

```bash
pnpm install          # 仓库根；pnpm-workspace.yaml 里有 nodeLinker: hoisted，见下
```

> 工作区建在 exFAT 卷上时 `nodeLinker` 必须是 `hoisted`：默认的 isolated linker 要在
> `node_modules/.pnpm` 之间建软链，而 exFAT 没有 symlink。

**启动**（在仓库根目录）：

```bash
./node_modules/.bin/tauri dev
```

仓库根**没有** `package.json`，所以 `pnpm tauri dev` 会报
`ERR_PNPM_NO_IMPORTER_MANIFEST_FOUND`。`beforeDevCommand` 会自己拉起 vite（1420）。

需要驱动真实界面做功能验证时，带上 WebView2 的调试端口：

```bash
WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS="--remote-debugging-port=9222" ./node_modules/.bin/tauri dev
```

之后 `curl http://127.0.0.1:9222/json/list` 能看到每个窗口。多窗口时
`/json/list` 里主窗与文件窗的 title/URL 一模一样，只能逐个连上去读
`window.__TAURI_INTERNALS__.metadata.currentWindow.label` 来挑。

**门禁**（三条都要绿）：

```bash
cargo test --workspace --no-fail-fast
(cd frontend && ../node_modules/.bin/vue-tsc --noEmit)
./node_modules/.bin/vitest run --root frontend
```

`frontend` 目录下没有 `tsconfig.app.json`，类型检查就是 `--noEmit` 一条。
另有 4 条真联网的直连用例默认 `#[ignore]`，CI 不跑：`dbhx-sync` 的 provider 层两家各一条
（建片段 → 写 → 读回比对 → **测完删掉**），编排层两家各一条（两份本机库真的通过云端相遇，
即双设备同步的真机版）。

```bash
DBHX_TOKEN_FILE=<仓库外的令牌文件> cargo test -p dbhx-sync -- --ignored --nocapture
```

令牌从仓库外的文件读进来，路径写进环境变量——它不进命令行、不进仓库、也不进日志。
改动 `dbhx-sync` 的 provider 之后要手动跑一次，离线测试证明不了「接口真的按我们以为的形状回应」。

**生成物**：`frontend/src/lib/ipc/bindings.ts` 由 `dbhx-ipc` 的 codegen 测试重新生成，
**要提交**。CI 会 `git diff --exit-code` 它，专门抓「Rust 契约改了、忘了重生成 TS」。
CSP 里 `style-src` 只有 `'unsafe-inline'` 不能上 nonce——AntDV 的 cssinjs 与 xterm 都在运行时注入
`<style>`，加了 nonce 它们的样式全废。想验构建产物在真实 CSP 下的表现：
`pnpm --filter frontend build` 之后 `node frontend/scripts/serve-csp-check.mjs`，
浏览器开 `http://127.0.0.1:4173` 看控制台有没有 `Refused to ... Content Security Policy`
（那里的 IPC 必然失败，那是预期的）。

## 测试与验证

- **协议路径靠进程内服务器。** `dbhx-testsrv` 用 `russh::server` + `russh_sftp::server`
  把真通道补上：transport 池握手、认证、shell 通道、SFTP 报文、传输引擎都在 CI 里跑真协议，
  不需要外部 sshd。它认证一律放行、VFS 在内存里，**不是**一个可用的服务器，也不进产品。
- **UI 复现靠 CDP。** 位置点击会被 Windows 前台锁拦掉，所以用调试端口在真实 webview 里求值：
  从 `document.querySelector('#app').__vue_app__.config.globalProperties.$pinia._s` 拿到 store，
  调真的 action、读真的 DOM。改过 Rust 源码的话，`tauri dev` 会自行重编译并**重启应用**，
  用户开着的标签页会被带走。
- **真机验证有边界。** 在自己的测试机上只碰自己新建的文件、测完清零，并用目录基线核对自证。

## CI

流水线一律推 `github` 远端（`https://github.com/lzdbh/dbhx-shell.git`）：`.github/workflows/`
只有 GitHub 执行，另一台托管在内部主仓的远端不跑 Actions，推过去不会有任何构建。

- `.github/workflows/ci.yml`：6 个 target 的 `cargo check`（windows x64/arm64、
  macOS x64/arm64、linux x64 走 x64 runner、linux arm64 走 `ubuntu-24.04-arm`）
  + 3 个 runner 的 `cargo test` + 契约漂移检查 + 前端构建 + 前端 vitest
  + `cargo fmt --check` / `cargo clippy -D warnings` + `cargo-deny`
  （advisories 只报告不阻塞，避免新公告凭空白掉 CI）。推 main 或开 PR 自动跑。
  arm64 Linux 不能跟打包一样只在 x64 runner 上交叉：tree 里 `glib-sys`/`gtk-sys` 这类
  build script 会问 pkg-config 要**目标平台**的 C 库，x64 runner 上没 sysroot 就直接报
  `pkg-config has not been configured to support cross-compilation`。
- `.github/workflows/release.yml`：打 `v*` tag（或手动 workflow 填 tag）即触发 5 条产物构建
  —— windows x64/arm64（msi + nsis）、macOS universal（一个 dmg 含 x86_64 + arm64 两个切片）、
  linux x86_64/aarch64（deb + rpm + AppImage），产物先落 workflow artifact，再由收尾的
  `release` job 用 `gh` 汇总挂到同一个 GitHub Release。首跑已出 11 个包。
  aarch64 Linux 用 `ubuntu-24.04-arm` 原生 runner：Debian 没有 `libwebkit2gtk-4.1-dev` 的
  arm64 交叉包，x64 runner 上装不出依赖。**当前无签名证书、无公证**，产物是可安装的裸包，
  SmartScreen / Gatekeeper 会拦。

  两个写死在流程里的决定，都是踩过才定下来的：
  - **不用 `tauri-apps/tauri-action`**：它把底层报错吞成一句
    `Release not found or created`，失败时还顺手删掉自己上传的 workflow artifact，
    五连跑什么都拿不到。打包就是一条 `tauri build`，Release 用 `gh` 建，日志才有可读的地方。
  - **`permissions:` 里显式声明 `contents: write` + `actions: read`**：GitHub 仓库设置里
    Workflow permissions 就算停在默认的 Read-only，workflow 顶层的 `permissions:` 块也能把
    `GITHUB_TOKEN` 抬上去（探针验过：声明后同一个 token 能建 draft release，没声明时 403）；
    而显式声明未列出的作用域会变 `none`，下载别的 job 的 artifact 要 `actions: read`。

  打包命令的调法：`cd src-tauri && node ../node_modules/@tauri-apps/cli/tauri.js build`。
  仓库根没有 package.json，`pnpm tauri …` 在根上直接 `ERR_PNPM_NO_IMPORTER_MANIFEST_FOUND`；
  `tauri build` 也没有 `-p/--project-path`（只有 `-c`），`--no-build` 这个 flag 在 v2 里不存在。

- `src-tauri/wix/main.wxs`：`@tauri-apps/cli` 内置 WiX 模板的**副本**，靠
  `bundle.windows.wix.template` 指过去，为的是修 MSI 升级后任务栏图标变空白。原模板给开始菜单
  快捷方式写了 `Icon="ProductIcon"`，MSI 于是把图标缓存在 `C:\WINDOWS\Installer\{ProductCode}\`
  下；而 `<Product Id="*">` 让 ProductCode 每次构建都换新 GUID（本地装过两次 0.1.0，GUID 就不
  一样），升级删掉旧目录后，之前钉到任务栏的快捷方式指向的图标文件就没了，只剩一张空白页——
  桌面快捷方式反而没事，因为它没设 `Icon`。副本只删了那一行，让它跟桌面快捷方式一样回落到 exe
  自带图标。**这是全量替换模板，不是片段**：升级 Tauri 后要重新取一份内置模板 diff 回来，
  漂移不会报错，只会静默改变打包行为。

工作区躲过两次的坑都写在根 `Cargo.toml` 的注释里：russh 换 `ring` 后端（`aws-lc-rs` 要在
build.rs 里编 C，交叉 arm64 直接崩），存储层不引 `rusqlite/bundled`（同理），
`reqwest` 用 `rustls-tls-native-roots` 读**系统信任库**（装 Steam++ 或公司 SSL 审查网关的机器上，
用 webpki-roots 会让同步功能一律报「证书错误」）。

## 状态

`0.2.0`：M0–M6（连接、终端、文件、外观与恢复、安全、telnet）以及 0.2.0 的跳板链、
端口转发、系统钥匙串、SFTP 冲突/续传、离线备份都已落地；云同步仍是「手动按钮可用、
两家配置互不干扰」。已知限制与未验证项：

- 安装包由 `release.yml` 在 CI 里产出，但**未签名、未公证**；没有自动更新；主密钥已接系统钥匙串，
  但还没有口令派生（钥匙串不可用时退回明文文件）
- **升级注意**：0.2.0 首次启动会把旧的 `master.key` 迁入系统钥匙串并删除明文文件；
  之后不要再用 0.1.x 打开同一个数据目录——旧版本会生成新密钥，现有 `vault.json` 将打不开
- 远端转发需要服务器允许 `AllowTcpForwarding`；跳板链只按选中的列表逐跳连接，
  不会递归展开某一跳自己的 `jump` 配置
- 开机拉取 / 变更防抖推送未做（当前刻意只有手动按钮）
- 未验证：文件面板的拖拽上传、Gitee 已删片段的历史版本、真实的「云端片段不见了 → 重建」往返、
  深色主题下 `a-input-number` 的底色；远端转发只对进程内测试服务器跑过，真实 sshd 未跑。
  双设备真机同步有对应的 `#[ignore]` 用例，但需要真实令牌手动跑，本轮没跑
- telnet 的 IAC 状态机带跨包重组与 `0xFF` 转义；协商策略只认 ECHO / SGA / STATUS / NAWS
  这四个选项，对端要启用就接受、请求我方启用的一律 `DONT`，其余选项礼貌拒绝。
  真实 telnet 服务器上的端到端表现**未验证**（离线用纯函数与进程内 TCP 服务器钉住了协议本身）
