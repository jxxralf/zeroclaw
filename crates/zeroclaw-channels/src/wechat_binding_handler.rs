//! 扫码绑定业务接口（独立文件，物理隔离 trait 与业务实现）
//!
//! `webchat` HTTP 入口 → `wechat` 通道 → `wechat_binding` 业务
//!
//! - HTTP 入口层（`webchat`）通过本 trait 调业务，不依赖具体通道类型
//! - 通道层（`wechat::WechatChannel`）实现本 trait，提供业务实现
//! - 业务层（`wechat_binding`）只和 `&WeChatConfig` 打交道，不持任何状态
//!
//! 本文件只放扫码绑定 trait 定义。业务实现（`load_wechat_binding_status`
//! / `start_wechat_qr_login` / `wait_for_wechat_qr_login` 等）放在 `wechat_binding.rs`。

use crate::wechat_binding::{WeChatApiError, WeChatBindingStatus, WeChatQrLoginStart};
use std::pin::Pin;

/// 扫码绑定业务接口契约。
///
/// `wechat_binding` 是**纯业务模块**——业务函数只收 `&WeChatConfig` 参数，
/// 不依赖通道类型。本 trait 是该模块对外的**业务接口契约**，让 webchat
/// HTTP 入口能调扫码绑定业务而**不依赖** `WechatChannel` 具体类型。
pub trait WechatBindingHandler: Send + Sync {
    /// 查询当前 WeChat 绑定状态。
    ///
    /// Webchat handler 只负责 HTTP 编解码，具体实现由 `WechatChannel`
    /// 提供，再由通道从其 Config 来源读取配置并调用 `wechat_binding`
    /// 的状态业务函数。该方法是同步的，因为状态查询只读取本地状态文件。
    fn wechat_binding_status(&self) -> WeChatBindingStatus;

    /// 发起一次新的 WeChat QR 登录。
    ///
    /// `timeout_ms` 只控制后台扫码轮询的等待时长；方法返回时仅表示二维码
    /// 已经申请成功。真正的扫码确认在后台任务中完成，确认成功后由运行时适配层
    /// 使用 daemon 注入的 sender 请求 reload。调用方不需要直接管理后台任务。
    fn wechat_authorize_qr(
        &self,
        timeout_ms: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<WeChatQrLoginStart, WeChatApiError>> + Send + '_>>;
}

use std::sync::{Arc, OnceLock, RwLock};

use crate::wechat_binding::{
    WeChatQrBindMode, spawn_wechat_qr_login_followup, start_wechat_qr_login,
};

/// 注册当前 daemon 周期使用的 WeChat handler。
///
/// Webchat 启动时通过 [`handler`] 读取这个句柄。注册表使用可写锁而不是
/// `OnceLock<Arc<_>>`，因此 daemon 每次 reload、重新创建 `WeChatChannel` 后，
/// 都可以覆盖旧句柄，后续 CLI 发起的绑定请求会落到新的通道实例。
pub fn install_handler(handler: Arc<dyn WechatBindingHandler>) {
    *handler_slot()
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some(handler);
}

pub(crate) fn handler() -> Option<Arc<dyn WechatBindingHandler>> {
    handler_slot()
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// 注入 daemon supervisor 的通知 sender。
///
/// `reload_tx` 是 daemon 创建的 reload watch channel 的发送端 clone；扫码成功
/// 后发送 `true`，daemon 的接收端会触发重新读取配置和重建 Channels。`shutdown_tx`
/// 保持与原扫码流程一致，用于通知对应的组件停止当前运行周期。sender 本身不
/// 复制 watch 状态，只是同一 watch channel 的另一个发送句柄；每次 reload 都会
/// 覆盖为当前周期的 sender。
pub fn install_supervisor_senders(
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    reload_tx: Option<tokio::sync::watch::Sender<bool>>,
) {
    *supervisor_slot()
        .get_or_init(|| RwLock::new(None))
        .write()
        .unwrap_or_else(|error| error.into_inner()) = Some((shutdown_tx, reload_tx));
}

/// 读取当前 daemon 周期的 supervisor sender。
///
/// 返回 clone，使后台 QR 任务可以独立持有 sender；如果尚未注入，则返回 `None`，
/// 调用方会创建一个无订阅者的 shutdown sender，并在没有 reload sender 时记录
/// “没有 daemon supervisor”的结果，而不是让 HTTP 请求 panic。
fn supervisor_senders() -> Option<(
    tokio::sync::watch::Sender<bool>,
    Option<tokio::sync::watch::Sender<bool>>,
)> {
    supervisor_slot()
        .get_or_init(|| RwLock::new(None))
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// 执行 Webchat 发起的一次 QR 绑定。
///
/// 该函数复用 `wechat_binding` 的 QR 创建函数，不在 HTTP 层复制业务逻辑；
/// 创建成功后立即启动后台 `wait_for_wechat_qr_login` 任务，并把当前周期的
/// shutdown/reload sender 传给 follow-up。这样 CLI 每次发起新命令都会获得新的
/// QR session，扫码成功后立即触发当前 daemon 的 reload。
pub(crate) async fn start_for_channel(
    wechat_config: zeroclaw_config::schema::WeChatConfig,
    timeout_ms: Option<u64>,
) -> Result<WeChatQrLoginStart, WeChatApiError> {
    let started = start_wechat_qr_login(Some(&wechat_config)).await?;
    let (shutdown_tx, reload_tx) =
        supervisor_senders().unwrap_or_else(|| (tokio::sync::watch::channel(false).0, None));
    spawn_wechat_qr_login_followup(
        Some(wechat_config),
        started.session_key.clone(),
        WeChatQrBindMode::default(),
        timeout_ms,
        shutdown_tx,
        reload_tx,
        "webchat",
    );
    Ok(started)
}

/// 返回可替换的 WeChat handler 注册槽位。
///
/// `OnceLock` 只用于初始化锁本身；真正的 handler 存放在 `RwLock<Option<_>>` 中，
/// 这样既能保证进程级槽位只初始化一次，又允许每次 daemon reload 替换 handler。
fn handler_slot() -> &'static OnceLock<RwLock<Option<Arc<dyn WechatBindingHandler>>>> {
    static SLOT: OnceLock<RwLock<Option<Arc<dyn WechatBindingHandler>>>> = OnceLock::new();
    &SLOT
}

/// 返回可替换的 supervisor sender 注册槽位。
///
/// 与 handler 槽位相同，外层 `OnceLock` 只保护槽位初始化，内层 `RwLock` 保存
/// 当前 reload 周期的 sender，避免旧周期 sender 阻塞后续绑定。
fn supervisor_slot() -> &'static OnceLock<
    RwLock<
        Option<(
            tokio::sync::watch::Sender<bool>,
            Option<tokio::sync::watch::Sender<bool>>,
        )>,
    >,
> {
    static SLOT: OnceLock<
        RwLock<
            Option<(
                tokio::sync::watch::Sender<bool>,
                Option<tokio::sync::watch::Sender<bool>>,
            )>,
        >,
    > = OnceLock::new();
    &SLOT
}
