use anyhow::{Context, anyhow, bail};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;
use zeroclaw_config::schema::WeChatConfig;

const DEFAULT_API_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const API_TIMEOUT: Duration = Duration::from_secs(15);
const QR_POLL_TIMEOUT: Duration = Duration::from_secs(35);
const QR_WAIT_DEFAULT_TIMEOUT_MS: u64 = 480_000;
const QR_SESSION_TTL_MS: u64 = 600_000;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WeChatBindingMode {
    Disabled,
    BoundUsers,
    AllowAll,
    PairingRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeChatBindingStatus {
    pub enabled: bool,
    pub state_dir: PathBuf,
    pub bot_logged_in: bool,
    pub allow_all: bool,
    pub bound: bool,
    pub bound_user_count: usize,
    pub bound_users: Vec<String>,
    pub binding_mode: WeChatBindingMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WeChatQrBindMode {
    #[default]
    Append,
    Replace,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WeChatQrLoginStatus {
    Wait,
    Scaned,
    Confirmed,
    Expired,
    ScanedButRedirect,
    TimedOut,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeChatQrLoginStart {
    pub state_dir: PathBuf,
    pub session_key: String,
    pub qrcode: String,
    pub qrcode_url: String,
    pub status: WeChatQrLoginStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeChatQrLoginWait {
    pub state_dir: PathBuf,
    pub session_key: String,
    pub connected: bool,
    pub status: WeChatQrLoginStatus,
    pub bind_mode: WeChatQrBindMode,
    pub account_id: Option<String>,
    pub user_id: Option<String>,
    pub base_url: Option<String>,
    pub qrcode_url: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct BindingsData {
    #[serde(default)]
    bound_users: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AccountData {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    saved_at: Option<String>,
}

#[derive(Debug, Clone)]
struct ActiveQrLogin {
    state_dir: PathBuf,
    qrcode: String,
    qrcode_url: String,
    current_api_base_url: String,
    started_at_ms: u64,
}

#[derive(Debug, Deserialize)]
struct QrCodeResponse {
    qrcode: String,
    #[serde(default)]
    qrcode_img_content: String,
}

#[derive(Debug, Deserialize)]
struct QrStatusResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    bot_token: Option<String>,
    #[serde(default)]
    ilink_bot_id: Option<String>,
    #[serde(default)]
    ilink_user_id: Option<String>,
    #[serde(default)]
    baseurl: Option<String>,
    #[serde(default)]
    redirect_host: Option<String>,
}

pub fn load_wechat_binding_status(config: Option<&WeChatConfig>) -> WeChatBindingStatus {
    let enabled = config.is_some_and(|wechat| wechat.enabled);
    let state_dir = resolve_wechat_state_dir(config.and_then(|wechat| wechat.state_dir.as_deref()));

    if !enabled {
        return WeChatBindingStatus {
            enabled: false,
            state_dir,
            bot_logged_in: false,
            allow_all: false,
            bound: false,
            bound_user_count: 0,
            bound_users: Vec::new(),
            binding_mode: WeChatBindingMode::Disabled,
        };
    }

    // Current branch uses peer_resolver instead of static allowed_users.
    // For binding status, we only check persisted bound users.
    let bound_users = load_persisted_bound_users(&state_dir);
    let bound = !bound_users.is_empty();
    let binding_mode = if bound {
        WeChatBindingMode::BoundUsers
    } else {
        WeChatBindingMode::PairingRequired
    };

    WeChatBindingStatus {
        enabled: true,
        bot_logged_in: has_persisted_bot_token(&state_dir),
        allow_all: false,
        bound,
        bound_user_count: bound_users.len(),
        bound_users,
        state_dir,
        binding_mode,
    }
}

pub async fn start_wechat_qr_login(
    config: Option<&WeChatConfig>,
) -> anyhow::Result<WeChatQrLoginStart> {
    let wechat = enabled_wechat_config(config)?;
    purge_expired_qr_sessions();

    let state_dir = resolve_wechat_state_dir(wechat.state_dir.as_deref());
    let api_base_url = resolve_wechat_api_base_url(wechat.api_base_url.as_deref())?;
    let client = reqwest::Client::new();
    let url = format!(
        "{}?bot_type=3",
        wechat_api_url(&api_base_url, "get_bot_qrcode")
    );

    let response = client
        .get(url)
        .timeout(API_TIMEOUT)
        .send()
        .await
        .context("WeChat QR login start request failed")?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!("WeChat QR login start failed ({status}): {body}");
    }

    let payload: QrCodeResponse = response
        .json()
        .await
        .context("WeChat QR login start response parse failed")?;
    if payload.qrcode.trim().is_empty() {
        bail!("WeChat QR login start returned empty qrcode");
    }

    let session_key = Uuid::new_v4().to_string();
    active_qr_sessions().lock().insert(
        session_key.clone(),
        ActiveQrLogin {
            state_dir: state_dir.clone(),
            qrcode: payload.qrcode.clone(),
            qrcode_url: payload.qrcode_img_content.clone(),
            current_api_base_url: api_base_url,
            started_at_ms: now_ms(),
        },
    );

    Ok(WeChatQrLoginStart {
        state_dir,
        session_key,
        qrcode: payload.qrcode,
        qrcode_url: payload.qrcode_img_content,
        status: WeChatQrLoginStatus::Wait,
    })
}

pub async fn wait_for_wechat_qr_login(
    config: Option<&WeChatConfig>,
    session_key: &str,
    bind_mode: WeChatQrBindMode,
    timeout_ms: Option<u64>,
) -> anyhow::Result<WeChatQrLoginWait> {
    let wechat = enabled_wechat_config(config)?;
    purge_expired_qr_sessions();

    let default_base_url = resolve_wechat_api_base_url(wechat.api_base_url.as_deref())?;
    let client = reqwest::Client::new();
    let timeout_ms = timeout_ms.unwrap_or(QR_WAIT_DEFAULT_TIMEOUT_MS).max(1_000);
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        let snapshot = {
            let sessions = active_qr_sessions().lock();
            sessions.get(session_key).cloned()
        }
        .ok_or_else(|| anyhow!("WeChat QR session not found: {session_key}"))?;

        if now_ms().saturating_sub(snapshot.started_at_ms) > QR_SESSION_TTL_MS {
            active_qr_sessions().lock().remove(session_key);
            return Ok(WeChatQrLoginWait {
                state_dir: snapshot.state_dir,
                session_key: session_key.to_string(),
                connected: false,
                status: WeChatQrLoginStatus::Expired,
                bind_mode,
                account_id: None,
                user_id: None,
                base_url: None,
                qrcode_url: Some(snapshot.qrcode_url),
            });
        }

        if tokio::time::Instant::now() >= deadline {
            return Ok(WeChatQrLoginWait {
                state_dir: snapshot.state_dir,
                session_key: session_key.to_string(),
                connected: false,
                status: WeChatQrLoginStatus::TimedOut,
                bind_mode,
                account_id: None,
                user_id: None,
                base_url: None,
                qrcode_url: Some(snapshot.qrcode_url),
            });
        }

        let status_url = format!(
            "{}?qrcode={}",
            wechat_api_url(&snapshot.current_api_base_url, "get_qrcode_status"),
            urlencoding::encode(&snapshot.qrcode)
        );

        let response = match tokio::time::timeout(
            QR_POLL_TIMEOUT + Duration::from_secs(5),
            client.get(&status_url).timeout(QR_POLL_TIMEOUT).send(),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(_error)) => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "WeChat QR login wait request error"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!("WeChat QR login wait failed ({status}): {body}");
        }

        let payload: QrStatusResponse = response
            .json()
            .await
            .context("WeChat QR login wait response parse failed")?;

        match payload.status.as_str() {
            "" | "wait" => {}
            "scaned" => {}
            "scaned_but_redirect" => {
                if let Some(host) = payload.redirect_host.as_deref()
                    && !host.trim().is_empty()
                {
                    let mut sessions = active_qr_sessions().lock();
                    if let Some(active) = sessions.get_mut(session_key) {
                        active.current_api_base_url = format!("https://{}", host.trim());
                    }
                }
            }
            "expired" => {
                active_qr_sessions().lock().remove(session_key);
                return Ok(WeChatQrLoginWait {
                    state_dir: snapshot.state_dir,
                    session_key: session_key.to_string(),
                    connected: false,
                    status: WeChatQrLoginStatus::Expired,
                    bind_mode,
                    account_id: None,
                    user_id: None,
                    base_url: None,
                    qrcode_url: Some(snapshot.qrcode_url),
                });
            }
            "confirmed" => {
                let token = payload
                    .bot_token
                    .as_deref()
                    .filter(|token| !token.trim().is_empty())
                    .context("WeChat QR login confirmed without bot_token")?;
                let account_id = payload
                    .ilink_bot_id
                    .as_deref()
                    .filter(|account_id| !account_id.trim().is_empty())
                    .context("WeChat QR login confirmed without ilink_bot_id")?;
                let user_id = payload
                    .ilink_user_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|user_id| !user_id.is_empty())
                    .map(str::to_string);
                let base_url = payload
                    .baseurl
                    .clone()
                    .filter(|base_url| !base_url.trim().is_empty())
                    .or(Some(default_base_url.clone()));

                persist_account_data(
                    &snapshot.state_dir,
                    token,
                    account_id,
                    base_url.as_deref(),
                    user_id.as_deref(),
                )?;
                if let Some(user_id) = user_id.as_deref() {
                    persist_dynamic_bound_users(&snapshot.state_dir, bind_mode, user_id)?;
                }

                active_qr_sessions().lock().remove(session_key);
                return Ok(WeChatQrLoginWait {
                    state_dir: snapshot.state_dir,
                    session_key: session_key.to_string(),
                    connected: true,
                    status: WeChatQrLoginStatus::Confirmed,
                    bind_mode,
                    account_id: Some(account_id.to_string()),
                    user_id,
                    base_url,
                    qrcode_url: Some(snapshot.qrcode_url),
                });
            }
            _other => {
                ::zeroclaw_log::record!(
                    DEBUG,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note),
                    "WeChat QR login wait received unexpected status"
                );
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// 解析微信账号、绑定信息和同步游标的持久化目录。
///
/// 路径按以下优先级生成：显式 `state_dir`，统一数据目录下的
/// `wechat` 子目录。
pub(crate) fn resolve_wechat_state_dir(state_dir: Option<&str>) -> PathBuf {
    if let Some(state_dir) = state_dir {
        return PathBuf::from(state_dir);
    }

    zeroclaw_config::schema::data_dir()
        .expect("WeChat state directory requires a resolvable data directory")
        .join("wechat")
}

fn enabled_wechat_config(config: Option<&WeChatConfig>) -> anyhow::Result<&WeChatConfig> {
    let Some(wechat) = config else {
        bail!("WeChat channel is not configured");
    };
    if !wechat.enabled {
        bail!("WeChat channel is configured but disabled");
    }
    Ok(wechat)
}

fn resolve_wechat_api_base_url(configured: Option<&str>) -> anyhow::Result<String> {
    let base_url = configured
        .unwrap_or(DEFAULT_API_BASE_URL)
        .trim()
        .trim_end_matches('/');
    if !(base_url.starts_with("https://") || (cfg!(test) && base_url.starts_with("http://"))) {
        bail!("WeChat API base URL must use https");
    }
    Ok(base_url.to_string())
}

fn wechat_api_url(base_url: &str, endpoint: &str) -> String {
    format!("{}/ilink/bot/{}", base_url.trim_end_matches('/'), endpoint)
}

fn active_qr_sessions() -> &'static Mutex<HashMap<String, ActiveQrLogin>> {
    static ACTIVE_QR_SESSIONS: OnceLock<Mutex<HashMap<String, ActiveQrLogin>>> = OnceLock::new();
    ACTIVE_QR_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn purge_expired_qr_sessions() {
    let now = now_ms();
    active_qr_sessions()
        .lock()
        .retain(|_, session| now.saturating_sub(session.started_at_ms) <= QR_SESSION_TTL_MS);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn normalized_concrete_users(users: &[String]) -> Vec<String> {
    users
        .iter()
        .filter_map(|user| {
            let normalized = user.trim();
            if normalized.is_empty() || normalized == "*" {
                None
            } else {
                Some(normalized.to_string())
            }
        })
        .fold(Vec::new(), |mut acc, user| {
            if !acc.iter().any(|existing| existing == &user) {
                acc.push(user);
            }
            acc
        })
}

fn load_persisted_bound_users(state_dir: &Path) -> Vec<String> {
    let path = state_dir.join("bindings.json");
    let Ok(data) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };

    match serde_json::from_str::<BindingsData>(&data) {
        Ok(bindings) => normalized_concrete_users(&bindings.bound_users),
        Err(error) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", error)})),
                "WeChat binding status: failed to parse bindings"
            );
            Vec::new()
        }
    }
}

fn has_persisted_bot_token(state_dir: &Path) -> bool {
    let path = state_dir.join("account.json");
    let Ok(data) = std::fs::read_to_string(&path) else {
        return false;
    };

    match serde_json::from_str::<AccountData>(&data) {
        Ok(account) => account
            .token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty()),
        Err(error) => {
            ::zeroclaw_log::record!(
                WARN,
                ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Note)
                    .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                    .with_attrs(::serde_json::json!({"error": format!("{}", error)})),
                "WeChat binding status: failed to parse account"
            );
            false
        }
    }
}

fn persist_dynamic_bound_users(
    state_dir: &Path,
    bind_mode: WeChatQrBindMode,
    user_id: &str,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("Failed to create WeChat state dir {}", state_dir.display()))?;

    let mut bound_users = match bind_mode {
        WeChatQrBindMode::Append => load_persisted_bound_users(state_dir),
        WeChatQrBindMode::Replace => Vec::new(),
    };

    if !bound_users.iter().any(|existing| existing == user_id) {
        bound_users.push(user_id.to_string());
    }

    let path = state_dir.join("bindings.json");
    let json = serde_json::to_vec_pretty(&BindingsData { bound_users })
        .context("Failed to serialize WeChat bindings")?;
    write_private(&path, &json)
        .with_context(|| format!("Failed to persist WeChat bindings {}", path.display()))
}

fn persist_account_data(
    state_dir: &Path,
    token: &str,
    account_id: &str,
    base_url: Option<&str>,
    user_id: Option<&str>,
) -> anyhow::Result<()> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("Failed to create WeChat state dir {}", state_dir.display()))?;

    let path = state_dir.join("account.json");
    let json = serde_json::to_vec_pretty(&AccountData {
        token: Some(token.to_string()),
        base_url: base_url.map(str::to_string),
        account_id: Some(account_id.to_string()),
        user_id: user_id.map(str::to_string),
        saved_at: Some(chrono::Utc::now().to_rfc3339()),
    })
    .context("Failed to serialize WeChat account data")?;
    write_private(&path, &json)
        .with_context(|| format!("Failed to persist WeChat account data {}", path.display()))
}

fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)?;
    file.write_all(data)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}
// ── wechat 通道句柄（HTTP 入口 → 微信通道 → 扫码绑定业务）─────────
//
// 链路：`webchat` (HTTP 入口) → `wechat` (微信通道) → `wechat_binding` (本模块，纯业务)
// 本模块**不持 Config 句柄**——业务函数收 `&WeChatConfig` 参数；
// Config 由 `WechatChannel::persist` 持有并按需解析。
// 本模块只持一个 `Arc<WechatChannel>` 句柄供 webchat 启动时拿。

/// 错误类型，路由层映射成 HTTP 响应。
#[derive(Debug)]
pub enum WeChatApiError {
    NotConfigured,
    Disabled,
    StartFailed(String),
}

impl std::fmt::Display for WeChatApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WeChatApiError::NotConfigured => write!(f, "WeChat channel is not configured"),
            WeChatApiError::Disabled => write!(f, "WeChat channel is configured but disabled"),
            WeChatApiError::StartFailed(msg) => {
                write!(f, "WeChat QR login start failed: {msg}")
            }
        }
    }
}

impl std::error::Error for WeChatApiError {}

impl From<anyhow::Error> for WeChatApiError {
    fn from(err: anyhow::Error) -> Self {
        WeChatApiError::StartFailed(format!("{err}"))
    }
}

/// 后台轮询 `wait_for_wechat_qr_login`，并在扫码成功后触发 daemon reload。
///
/// 这是「QR 扫码完成 → 通知 daemon 重启以加载新账号」执行体的**单一来源**。
/// `wechat.rs` 在 `handle_authorize_qr_start` 中调用，传入：
/// - `wechat_config`：扫码业务所需的配置快照
/// - `session_key` / `bind_mode` / `timeout_ms`：`start_wechat_qr_login` 返回值
/// - `shutdown_tx` / `reload_tx`：daemon supervisor sender（通过 `WECHAT_SUPERVISOR_SENDERS` 拿）
/// - `log_source`：发起方 module 标识（`"webchat"`）
///
/// 调用方传入的 `shutdown_tx` / `reload_tx` 是 daemon supervisor 持有的
/// `watch::Sender<bool>` 的 clone——它们不复制状态，每个 sender 只是同一
/// supervisor watch 通道的另一个写入端（`watch::Sender::send` 会广播给所有
/// 订阅者）。
pub fn spawn_wechat_qr_login_followup(
    wechat_config: Option<WeChatConfig>,
    session_key: String,
    bind_mode: WeChatQrBindMode,
    timeout_ms: Option<u64>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    reload_tx: Option<tokio::sync::watch::Sender<bool>>,
    log_source: &'static str,
) {
    tokio::spawn(async move {
        let result =
            wait_for_wechat_qr_login(wechat_config.as_ref(), &session_key, bind_mode, timeout_ms)
                .await;

        match result {
            Ok(result) if result.connected => {
                if let Some(reload_tx) = reload_tx {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    let _ = shutdown_tx.send(true);
                    let _ = reload_tx.send(true);
                    ::zeroclaw_log::record!(
                        INFO,
                        ::zeroclaw_log::Event::new(log_source, ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_outcome(::zeroclaw_log::EventOutcome::Success)
                            .with_attrs(::serde_json::json!({
                                "session_key": session_key,
                            })),
                        "WeChat QR login confirmed; daemon reload requested"
                    );
                } else {
                    ::zeroclaw_log::record!(
                        WARN,
                        ::zeroclaw_log::Event::new(log_source, ::zeroclaw_log::Action::Note)
                            .with_category(::zeroclaw_log::EventCategory::Channel)
                            .with_outcome(::zeroclaw_log::EventOutcome::Unknown)
                            .with_attrs(::serde_json::json!({
                                "session_key": session_key,
                            })),
                        "WeChat QR login confirmed, but no daemon supervisor is running; restart manually to load the new account"
                    );
                }
            }
            Ok(result) => {
                ::zeroclaw_log::record!(
                    INFO,
                    ::zeroclaw_log::Event::new(log_source, ::zeroclaw_log::Action::Note)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Success)
                        .with_attrs(::serde_json::json!({
                            "session_key": session_key,
                            "connected": result.connected,
                            "status": format!("{:?}", result.status),
                        })),
                    "WeChat QR login finished"
                );
            }
            Err(error) => {
                ::zeroclaw_log::record!(
                    WARN,
                    ::zeroclaw_log::Event::new(log_source, ::zeroclaw_log::Action::Fail)
                        .with_category(::zeroclaw_log::EventCategory::Channel)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure)
                        .with_attrs(::serde_json::json!({
                            "session_key": session_key,
                            "error": format!("{}", error),
                        })),
                    "WeChat QR login background wait failed"
                );
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        WeChatBindingMode, WeChatQrBindMode, WeChatQrLoginStatus, load_wechat_binding_status,
        resolve_wechat_state_dir, start_wechat_qr_login, wait_for_wechat_qr_login,
    };
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use tempfile::tempdir;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zeroclaw_config::schema::WeChatConfig;

    #[test]
    fn disabled_wechat_reports_disabled_status() {
        let status = load_wechat_binding_status(None);
        assert!(!status.enabled);
        assert!(!status.bound);
        assert_eq!(status.binding_mode, WeChatBindingMode::Disabled);
    }

    #[test]
    fn enabled_wechat_merges_config_and_persisted_bound_users() {
        let temp = tempdir().unwrap();
        let state_dir = temp.path().join("wechat-state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("bindings.json"),
            r#"{"bound_users":["persisted-user@im.wechat","config-user@im.wechat","*",""]}"#,
        )
        .unwrap();
        std::fs::write(
            state_dir.join("account.json"),
            r#"{"token":"persisted-token"}"#,
        )
        .unwrap();

        let status = load_wechat_binding_status(Some(&WeChatConfig {
            enabled: true,
            allowed_users: vec![
                "*".to_string(),
                "config-user@im.wechat".to_string(),
                " ".to_string(),
            ],
            api_base_url: None,
            cdn_base_url: None,
            state_dir: Some(state_dir.display().to_string()),
        }));

        assert!(status.enabled);
        assert!(status.bot_logged_in);
        assert!(status.allow_all);
        assert!(status.bound);
        assert_eq!(status.bound_user_count, 2);
        assert_eq!(
            status.bound_users,
            vec![
                "config-user@im.wechat".to_string(),
                "persisted-user@im.wechat".to_string()
            ]
        );
        assert_eq!(status.binding_mode, WeChatBindingMode::BoundUsers);
    }

    #[test]
    fn enabled_wechat_without_users_requires_pairing() {
        let temp = tempdir().unwrap();
        let state_dir = temp.path().join("wechat-state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let status = load_wechat_binding_status(Some(&WeChatConfig {
            enabled: true,
            allowed_users: Vec::new(),
            api_base_url: None,
            cdn_base_url: None,
            state_dir: Some(state_dir.display().to_string()),
        }));

        assert!(status.enabled);
        assert!(!status.bot_logged_in);
        assert!(!status.allow_all);
        assert!(!status.bound);
        assert_eq!(status.bound_user_count, 0);
        assert!(status.bound_users.is_empty());
        assert_eq!(status.binding_mode, WeChatBindingMode::PairingRequired);
    }

    #[test]
    fn allow_all_without_specific_users_is_not_bound() {
        let temp = tempdir().unwrap();
        let state_dir = temp.path().join("wechat-state");
        std::fs::create_dir_all(&state_dir).unwrap();

        let status = load_wechat_binding_status(Some(&WeChatConfig {
            enabled: true,
            allowed_users: vec!["*".to_string()],
            api_base_url: None,
            cdn_base_url: None,
            state_dir: Some(state_dir.display().to_string()),
        }));

        assert!(status.enabled);
        assert!(status.allow_all);
        assert!(!status.bound);
        assert_eq!(status.bound_user_count, 0);
        assert_eq!(status.binding_mode, WeChatBindingMode::AllowAll);
    }

    #[test]
    fn explicit_state_dir_is_used_verbatim() {
        let literal = PathBuf::from("~/.zeroclaw/wechat");
        let status = load_wechat_binding_status(Some(&WeChatConfig {
            enabled: true,
            allowed_users: Vec::new(),
            api_base_url: None,
            cdn_base_url: None,
            state_dir: Some(literal.display().to_string()),
        }));

        assert_eq!(status.state_dir, literal);
    }

    #[test]
    fn explicit_state_dir_takes_priority_over_claw_lite_config_dir() {
        let _env_guard = wechat_config_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("CLAW_LITE_CONFIG_DIR");
        // SAFETY: access to this environment variable is serialized in these tests.
        unsafe { std::env::set_var("CLAW_LITE_CONFIG_DIR", "/tmp/claw-lite-config") };

        let resolved = resolve_wechat_state_dir(Some("/tmp/explicit-wechat"));

        restore_claw_lite_config_dir(previous);
        assert_eq!(resolved, PathBuf::from("/tmp/explicit-wechat"));
    }

    #[test]
    fn claw_lite_config_dir_is_used_before_default_path() {
        let _env_guard = wechat_config_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os("CLAW_LITE_CONFIG_DIR");
        // SAFETY: access to this environment variable is serialized in these tests.
        unsafe { std::env::set_var("CLAW_LITE_CONFIG_DIR", "/tmp/claw-lite-config") };

        let resolved = resolve_wechat_state_dir(None);

        restore_claw_lite_config_dir(previous);
        assert_eq!(resolved, PathBuf::from("/tmp/claw-lite-config/wechat"));
    }

    fn wechat_config_env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    fn restore_claw_lite_config_dir(previous: Option<std::ffi::OsString>) {
        // SAFETY: access to this environment variable is serialized in these tests.
        unsafe {
            if let Some(previous) = previous {
                std::env::set_var("CLAW_LITE_CONFIG_DIR", previous);
            } else {
                std::env::remove_var("CLAW_LITE_CONFIG_DIR");
            }
        }
    }

    #[tokio::test]
    async fn qr_login_start_returns_qrcode_url() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ilink/bot/get_bot_qrcode"))
            .and(query_param("bot_type", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "qrcode": "qr-token-123",
                "qrcode_img_content": "https://example.com/qr.png"
            })))
            .mount(&server)
            .await;

        let temp = tempdir().unwrap();
        let state_dir = temp.path().join("wechat-state");
        let result = start_wechat_qr_login(Some(&WeChatConfig {
            enabled: true,
            allowed_users: Vec::new(),
            api_base_url: Some(server.uri()),
            cdn_base_url: None,
            state_dir: Some(state_dir.display().to_string()),
        }))
        .await
        .unwrap();

        assert_eq!(result.state_dir, state_dir);
        assert_eq!(result.status, WeChatQrLoginStatus::Wait);
        assert_eq!(result.qrcode, "qr-token-123");
        assert_eq!(result.qrcode_url, "https://example.com/qr.png");
        assert!(!result.session_key.is_empty());
    }

    #[tokio::test]
    async fn qr_login_wait_confirmed_appends_bound_user() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ilink/bot/get_bot_qrcode"))
            .and(query_param("bot_type", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "qrcode": "qr-token-abc",
                "qrcode_img_content": "https://example.com/qr-abc.png"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ilink/bot/get_qrcode_status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "confirmed",
                "bot_token": "bot-token-1",
                "ilink_bot_id": "bot-account-1",
                "ilink_user_id": "new-user@im.wechat",
                "baseurl": server.uri()
            })))
            .mount(&server)
            .await;

        let temp = tempdir().unwrap();
        let state_dir = temp.path().join("wechat-state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("bindings.json"),
            r#"{"bound_users":["old-user@im.wechat"]}"#,
        )
        .unwrap();

        let config = WeChatConfig {
            enabled: true,
            allowed_users: Vec::new(),
            api_base_url: Some(server.uri()),
            cdn_base_url: None,
            state_dir: Some(state_dir.display().to_string()),
        };

        let start = start_wechat_qr_login(Some(&config)).await.unwrap();
        let result = wait_for_wechat_qr_login(
            Some(&config),
            &start.session_key,
            WeChatQrBindMode::Append,
            Some(2_000),
        )
        .await
        .unwrap();

        assert!(result.connected);
        assert_eq!(result.status, WeChatQrLoginStatus::Confirmed);
        assert_eq!(result.account_id.as_deref(), Some("bot-account-1"));
        assert_eq!(result.user_id.as_deref(), Some("new-user@im.wechat"));

        let bindings = std::fs::read_to_string(state_dir.join("bindings.json")).unwrap();
        assert!(bindings.contains("old-user@im.wechat"));
        assert!(bindings.contains("new-user@im.wechat"));

        let account = std::fs::read_to_string(state_dir.join("account.json")).unwrap();
        assert!(account.contains("bot-token-1"));
        assert!(account.contains("bot-account-1"));
    }

    #[tokio::test]
    async fn qr_login_wait_confirmed_replaces_dynamic_bound_users() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ilink/bot/get_bot_qrcode"))
            .and(query_param("bot_type", "3"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "qrcode": "qr-token-replace",
                "qrcode_img_content": "https://example.com/qr-replace.png"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ilink/bot/get_qrcode_status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "status": "confirmed",
                "bot_token": "bot-token-2",
                "ilink_bot_id": "bot-account-2",
                "ilink_user_id": "only-user@im.wechat",
                "baseurl": server.uri()
            })))
            .mount(&server)
            .await;

        let temp = tempdir().unwrap();
        let state_dir = temp.path().join("wechat-state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::write(
            state_dir.join("bindings.json"),
            r#"{"bound_users":["old-user@im.wechat","another-old@im.wechat"]}"#,
        )
        .unwrap();

        let config = WeChatConfig {
            enabled: true,
            allowed_users: Vec::new(),
            api_base_url: Some(server.uri()),
            cdn_base_url: None,
            state_dir: Some(state_dir.display().to_string()),
        };

        let start = start_wechat_qr_login(Some(&config)).await.unwrap();
        let result = wait_for_wechat_qr_login(
            Some(&config),
            &start.session_key,
            WeChatQrBindMode::Replace,
            Some(2_000),
        )
        .await
        .unwrap();

        assert!(result.connected);
        assert_eq!(result.status, WeChatQrLoginStatus::Confirmed);

        let bindings = std::fs::read_to_string(state_dir.join("bindings.json")).unwrap();
        assert!(bindings.contains("only-user@im.wechat"));
        assert!(!bindings.contains("old-user@im.wechat"));
        assert!(!bindings.contains("another-old@im.wechat"));
    }
}
