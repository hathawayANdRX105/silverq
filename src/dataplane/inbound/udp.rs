//! UDP ASSOCIATE：中继 socket 绑定与控制连接生命周期。

use crate::proxy::meow::Registry;
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::runtime::{DialTuning, SharedSelection};
/// 处理 UDP ASSOCIATE：绑中继 socket → 回其地址 → 跑中继循环直到 TCP 断开。
///
/// RFC 1928 要求 TCP 控制连接是 association 的生命周期锚点：TCP 一断，
/// 服务端必须回收该 association 的所有 UDP 状态。这里用 oneshot 通知中继循环退出。
pub(super) async fn handle_udp_associate(
    mut socket: TcpStream,
    registry: &Registry,
    selection: &SharedSelection,
    tuning: DialTuning,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let control_local = socket.local_addr()?;
    let (relay, relay_addr) = match crate::dataplane::udp::bind_relay(control_local).await {
        Ok(v) => v,
        Err(e) => {
            // 0x01 general failure
            socket
                .write_all(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await
                .ok();
            return Err(format!("udp associate: bind relay failed: {e}").into());
        }
    };

    // 成功应答带上中继地址，客户端后续把数据报发到这里
    let mut reply = vec![0x05, 0x00, 0x00];
    match relay_addr {
        SocketAddr::V4(a) => {
            reply.push(0x01);
            reply.extend_from_slice(&a.ip().octets());
            reply.extend_from_slice(&a.port().to_be_bytes());
        }
        SocketAddr::V6(a) => {
            reply.push(0x04);
            reply.extend_from_slice(&a.ip().octets());
            reply.extend_from_slice(&a.port().to_be_bytes());
        }
    }
    socket.write_all(&reply).await?;
    tracing::debug!(%relay_addr, "udp associate established");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let relay_task = tokio::spawn(crate::dataplane::udp::run_relay(
        relay,
        registry.clone(),
        selection.clone(),
        tuning.fallback_attempts,
        shutdown_rx,
    ));

    // TCP 控制连接读到 EOF = 客户端结束 association
    let mut sink = [0u8; 1];
    loop {
        match socket.read(&mut sink).await {
            Ok(0) => break,    // EOF
            Ok(_) => continue, // 控制连接上不应有数据，忽略
            Err(_) => break,
        }
    }
    let _ = shutdown_tx.send(());
    let _ = relay_task.await;
    Ok(())
}
