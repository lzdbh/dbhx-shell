//! 自检：服务器能不能和一个「最小客户端」把协议走通。
//!
//! 这个文件只用于定位问题归属——如果连 russh 自己的客户端都握不上手，
//! 那是 `dbhx-testsrv` 的错；如果这里通了而 `dbhx-ssh` 的测试不通，才是连接层的错。
//! 它不是产品行为规约，所以打印而不是断言细节。

use std::sync::Arc;
use std::time::Duration;

use russh::client::Handler;
use russh::keys::PublicKeyOrCertificate;

use dbhx_testsrv::{TestServer, TestSpec};

/// 只是占位：这个自检只关心协议能不能走通，不关心具体错误内容。
#[derive(Debug)]
struct Error(#[allow(dead_code)] std::io::Error);

impl From<russh::Error> for Error {
    fn from(_: russh::Error) -> Self {
        Error(std::io::Error::other("russh error"))
    }
}

struct MinimalClient;

impl Handler for MinimalClient {
    type Error = Error;

    async fn check_server_key(&mut self, _key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        println!("  -> check_server_key 被调用");
        Ok(true)
    }

    async fn auth_banner(&mut self, banner: &str, _session: &mut russh::client::Session) -> Result<(), Self::Error> {
        println!("  -> auth_banner: {banner}");
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshakes_with_a_minimal_russh_client() {
    let server = TestServer::start(TestSpec::new()).await.unwrap();
    let addr = server.addr;
    let outcome = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut handle = russh::client::connect(Arc::new(russh::client::Config::default()), addr, MinimalClient)
            .await
            .map_err(|e| format!("connect 失败: {e:?}"))?;
        println!("  -> connect 返回，开始认证");
        let auth = handle.authenticate_password("app", "dbhx-test").await.map_err(|e| format!("认证请求失败: {e:?}"))?;
        println!("  -> 认证结果 success={}", auth.success());
        if !auth.success() {
            return Err("服务器拒绝了密码".to_string());
        }
        let channel = handle.channel_open_session().await.map_err(|e| format!("打不开会话通道: {e}"))?;
        channel.request_subsystem(true, "sftp").await.map_err(|e| format!("subsystem 请求失败: {e}"))?;
        println!("  -> subsystem 已确认");
        drop(channel);
        handle.disconnect(russh::Disconnect::ByApplication, "", "en").await.map_err(|e| format!("断开失败: {e}"))?;
        Ok(())
    })
    .await;
    match outcome {
        Ok(Ok(())) => println!("PASS: 完整走通"),
        Ok(Err(msg)) => panic!("FAIL: {msg}"),
        Err(_) => panic!("FAIL: 15 秒未完成，协议卡住"),
    }
    server.shutdown().await;
}
