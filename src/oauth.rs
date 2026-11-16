use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock, watch};
use tracing::{debug, info, warn};
use url::Url;

use crate::config::Config;
use crate::error::{AppError, AppResult};

const ACCESS_REFRESH_LEEWAY_SECS: u64 = 300;
const STATE_TTL_SECS: u64 = 900;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: u64,
    pub refresh_expires_at: Option<u64>,
    pub scope: Option<String>,
    pub token_type: String,
    pub obtained_at: u64,
    pub user_name: Option<String>,
    pub open_id: Option<String>,
}

impl TokenSet {
    pub fn is_access_valid(&self, now: u64, leeway_secs: u64) -> bool {
        self.expires_at > now.saturating_add(leeway_secs)
    }

    pub fn is_refresh_valid(&self, now: u64) -> bool {
        if self.refresh_token.is_none() {
            return false;
        }
        match self.refresh_expires_at {
            Some(exp) => exp > now.saturating_add(30),
            None => true,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct OAuthStatus {
    pub authorized: bool,
    pub access_token_valid: bool,
    pub access_expires_in_secs: Option<i64>,
    pub refresh_token_present: bool,
    pub refresh_expires_in_secs: Option<i64>,
    pub scope: Option<String>,
    pub user_name: Option<String>,
    pub open_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    refresh_token_expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserInfoEnvelope {
    code: i64,
    #[serde(default)]
    data: Option<UserInfoData>,
}

#[derive(Debug, Deserialize)]
struct UserInfoData {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    open_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct AuthCodeExchangeRequest<'a> {
    grant_type: &'static str,
    client_id: &'a str,
    client_secret: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
}

#[derive(Debug, Serialize)]
struct RefreshTokenRequest<'a> {
    grant_type: &'static str,
    client_id: &'a str,
    client_secret: &'a str,
    refresh_token: &'a str,
}

pub struct OAuthManager {
    config: Config,
    http: Client,
    token: RwLock<Option<TokenSet>>,
    refresh_lock: Mutex<()>,
    pending_states: RwLock<HashMap<String, u64>>,
    auth_watch_tx: watch::Sender<u64>,
}

impl OAuthManager {
    pub fn new(config: Config, http: Client) -> Self {
        let (auth_watch_tx, _) = watch::channel(0u64);
        Self {
            config,
            http,
            token: RwLock::new(None),
            refresh_lock: Mutex::new(()),
            pending_states: RwLock::new(HashMap::new()),
            auth_watch_tx,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub async fn load_from_disk(&self) -> AppResult<bool> {
        let path = &self.config.token_file;
        if !path.exists() {
            debug!("token file {} does not exist yet", path.display());
            return Ok(false);
        }
        let raw = tokio::fs::read_to_string(path).await?;
        if raw.trim().is_empty() {
            warn!(
                path = %path.display(),
                "OAuth token file is empty; ignoring and requiring re-authorization"
            );
            return Ok(false);
        }
        let parsed: TokenSet = match serde_json::from_str(&raw) {
            Ok(tok) => tok,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    "corrupted OAuth token file ({err}); ignoring and requiring re-authorization"
                );
                return Ok(false);
            }
        };
        let mut guard = self.token.write().await;
        *guard = Some(parsed);
        info!("loaded OAuth token from {}", path.display());
        Ok(true)
    }

    async fn save_to_disk(&self, token: &TokenSet) -> AppResult<()> {
        save_token_atomic(&self.config.token_file, token).await
    }

    /// Generates a Feishu OAuth authorization URL with a fresh CSRF `state`.
    pub async fn build_authorize_url(&self) -> AppResult<(String, String)> {
        let state = generate_random_state();
        let now = unix_now();
        {
            let mut states = self.pending_states.write().await;
            states.retain(|_, exp| *exp > now);
            states.insert(state.clone(), now + STATE_TTL_SECS);
        }

        let mut url = Url::parse(&format!(
            "{}/open-apis/authen/v1/authorize",
            self.config.accounts_base_url
        ))
        .map_err(|e| AppError::Config(format!("invalid accounts_base_url: {e}")))?;

        {
            let mut qp = url.query_pairs_mut();
            qp.append_pair("client_id", &self.config.app_id);
            qp.append_pair("response_type", "code");
            qp.append_pair("redirect_uri", &self.config.redirect_uri);
            if !self.config.scopes.is_empty() {
                qp.append_pair("scope", &self.config.scopes);
            }
            qp.append_pair("state", &state);
        }

        Ok((url.to_string(), state))
    }

    /// Validates `state` and exchanges an authorization `code` for a `TokenSet`.
    pub async fn handle_callback(&self, code: &str, state: &str) -> AppResult<TokenSet> {
        let now = unix_now();
        {
            let mut states = self.pending_states.write().await;
            states.retain(|_, exp| *exp > now);
            match states.remove(state) {
                Some(exp) if exp > now => {}
                _ => {
                    return Err(AppError::OAuth {
                        error: "invalid_state".to_string(),
                        description: "OAuth state is missing, expired, or already used. Please restart authorization at /oauth/login.".to_string(),
                    });
                }
            }
        }

        let endpoint = format!("{}/open-apis/authen/v2/oauth/token", self.config.api_base_url);
        let body = AuthCodeExchangeRequest {
            grant_type: "authorization_code",
            client_id: &self.config.app_id,
            client_secret: &self.config.app_secret,
            code,
            redirect_uri: &self.config.redirect_uri,
        };

        let resp = self.http.post(&endpoint).json(&body).send().await?;
        let status = resp.status();
        let payload: OAuthTokenResponse = resp.json().await?;
        let mut token_set = parse_token_response(status.as_u16(), payload, None)?;

        if let Some((name, open_id)) = self.try_fetch_user_info(&token_set.access_token).await {
            token_set.user_name = name;
            token_set.open_id = open_id;
        }

        {
            let _lock = self.refresh_lock.lock().await;
            let mut guard = self.token.write().await;
            *guard = Some(token_set.clone());
            self.save_to_disk(&token_set).await?;
        }

        info!(
            user = token_set.user_name.as_deref().unwrap_or("unknown"),
            "Feishu user OAuth authorization completed"
        );
        self.auth_watch_tx.send_modify(|seq| *seq = seq.wrapping_add(1));
        Ok(token_set)
    }

    /// Subscribes to authorization completion events before starting an OAuth listener
    /// so notifications emitted immediately after server bind are never lost.
    pub fn subscribe_authorization(&self) -> watch::Receiver<u64> {
        self.auth_watch_tx.subscribe()
    }

    pub async fn wait_for_authorization(&self) {
        let mut rx = self.subscribe_authorization();
        let _ = rx.changed().await;
    }

    /// Returns a valid `user_access_token`, refreshing proactively if near expiry.
    pub async fn get_valid_access_token(&self) -> AppResult<String> {
        let now = unix_now();
        {
            let guard = self.token.read().await;
            match guard.as_ref() {
                Some(tok) if tok.is_access_valid(now, ACCESS_REFRESH_LEEWAY_SECS) => {
                    return Ok(tok.access_token.clone());
                }
                Some(_) => {}
                None => {
                    return Err(AppError::TokenUnavailable(
                        "not authorized yet; open /oauth/login in browser".to_string(),
                    ));
                }
            }
        }

        self.refresh_token_internal(None).await
    }

    /// Forces a token refresh only if the current token still equals `failed_token`.
    /// Prevents concurrent callers from double-consuming Feishu's single-use `refresh_token`.
    pub async fn force_refresh_if_matches(&self, failed_token: &str) -> AppResult<String> {
        self.refresh_token_internal(Some(failed_token)).await
    }

    pub async fn force_refresh(&self) -> AppResult<String> {
        let current = {
            let guard = self.token.read().await;
            guard.as_ref().map(|t| t.access_token.clone())
        };
        let Some(tok) = current else {
            return Err(AppError::TokenUnavailable(
                "not authorized yet; open /oauth/login in browser".to_string(),
            ));
        };
        self.refresh_token_internal(Some(&tok)).await
    }

    async fn refresh_token_internal(&self, expected_failed_token: Option<&str>) -> AppResult<String> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let now = unix_now();

        let (current_refresh, existing_user_name, existing_open_id) = {
            let guard = self.token.read().await;
            let Some(tok) = guard.as_ref() else {
                return Err(AppError::TokenUnavailable(
                    "not authorized yet; open /oauth/login in browser".to_string(),
                ));
            };

            if let Some(failed) = expected_failed_token {
                // Another task already rotated the token while we waited for refresh_lock.
                if tok.access_token != failed {
                    return Ok(tok.access_token.clone());
                }
            } else if tok.is_access_valid(now, ACCESS_REFRESH_LEEWAY_SECS) {
                return Ok(tok.access_token.clone());
            }

            if !tok.is_refresh_valid(now) {
                return Err(AppError::TokenUnavailable(
                    "refresh_token missing or expired; re-authorize at /oauth/login".to_string(),
                ));
            }

            (
                tok.refresh_token
                    .clone()
                    .expect("checked is_refresh_valid"),
                tok.user_name.clone(),
                tok.open_id.clone(),
            )
        };

        debug!("refreshing Feishu user_access_token using refresh_token");
        let endpoint = format!("{}/open-apis/authen/v2/oauth/token", self.config.api_base_url);
        let body = RefreshTokenRequest {
            grant_type: "refresh_token",
            client_id: &self.config.app_id,
            client_secret: &self.config.app_secret,
            refresh_token: &current_refresh,
        };

        let resp = self.http.post(&endpoint).json(&body).send().await?;
        let status = resp.status();
        let payload: OAuthTokenResponse = resp.json().await?;
        let mut new_token = parse_token_response(
            status.as_u16(),
            payload,
            Some((existing_user_name, existing_open_id)),
        )?;

        if new_token.user_name.is_none()
            && let Some((name, open_id)) = self.try_fetch_user_info(&new_token.access_token).await
        {
            new_token.user_name = name;
            new_token.open_id = open_id;
        }

        {
            let mut guard = self.token.write().await;
            *guard = Some(new_token.clone());
        }
        self.save_to_disk(&new_token).await?;
        info!("refreshed Feishu user_access_token and rotated refresh_token");
        Ok(new_token.access_token)
    }

    pub async fn status(&self) -> OAuthStatus {
        let now = unix_now();
        let guard = self.token.read().await;
        match guard.as_ref() {
            Some(tok) => {
                let access_expires_in = tok.expires_at as i64 - now as i64;
                let refresh_expires_in = tok.refresh_expires_at.map(|e| e as i64 - now as i64);
                OAuthStatus {
                    authorized: true,
                    access_token_valid: tok.is_access_valid(now, 0),
                    access_expires_in_secs: Some(access_expires_in),
                    refresh_token_present: tok.refresh_token.is_some(),
                    refresh_expires_in_secs: refresh_expires_in,
                    scope: tok.scope.clone(),
                    user_name: tok.user_name.clone(),
                    open_id: tok.open_id.clone(),
                }
            }
            None => OAuthStatus {
                authorized: false,
                access_token_valid: false,
                access_expires_in_secs: None,
                refresh_token_present: false,
                refresh_expires_in_secs: None,
                scope: None,
                user_name: None,
                open_id: None,
            },
        }
    }

    async fn try_fetch_user_info(&self, access_token: &str) -> Option<(Option<String>, Option<String>)> {
        let url = format!("{}/open-apis/authen/v1/user_info", self.config.api_base_url);
        let resp = self
            .http
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body: UserInfoEnvelope = resp.json().await.ok()?;
        if body.code != 0 {
            return None;
        }
        body.data.map(|d| (d.name, d.open_id))
    }

    /// Spawns a background task that refreshes the token proactively before expiry.
    pub fn spawn_background_refresher(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(120));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let needs_refresh = {
                    let guard = manager.token.read().await;
                    let now = unix_now();
                    match guard.as_ref() {
                        Some(tok) => {
                            !tok.is_access_valid(now, ACCESS_REFRESH_LEEWAY_SECS)
                                && tok.is_refresh_valid(now)
                        }
                        None => false,
                    }
                };
                if needs_refresh && let Err(err) = manager.refresh_token_internal(None).await {
                    warn!("background OAuth token refresh failed: {err}");
                }
            }
        })
    }
}

fn parse_token_response(
    http_status: u16,
    payload: OAuthTokenResponse,
    existing_user: Option<(Option<String>, Option<String>)>,
) -> AppResult<TokenSet> {
    if let Some(code) = payload.code
        && code != 0
    {
        let err = payload.error.unwrap_or_else(|| format!("code_{code}"));
        let desc = payload
            .error_description
            .or(payload.msg)
            .unwrap_or_else(|| "OAuth token request failed".to_string());
        if matches!(code, 20037 | 20065) {
            return Err(AppError::TokenUnavailable(format!("{err}: {desc}")));
        }
        return Err(AppError::OAuth {
            error: err,
            description: desc,
        });
    }

    if let Some(err) = payload.error {
        let desc = payload
            .error_description
            .or(payload.msg)
            .unwrap_or_else(|| "OAuth token request failed".to_string());
        if err == "invalid_grant" {
            return Err(AppError::TokenUnavailable(format!("{err}: {desc}")));
        }
        return Err(AppError::OAuth {
            error: err,
            description: desc,
        });
    }

    if !(200..300).contains(&http_status) {
        return Err(AppError::FeishuApi {
            code: payload.code.unwrap_or(-1),
            msg: payload
                .error_description
                .or(payload.msg)
                .unwrap_or_else(|| format!("HTTP {http_status} from OAuth token endpoint")),
            http_status,
        });
    }

    let access_token = payload.access_token.ok_or_else(|| AppError::OAuth {
        error: "missing_access_token".to_string(),
        description: "Feishu token response did not contain access_token".to_string(),
    })?;
    let now = unix_now();
    let expires_in = payload.expires_in.unwrap_or(7200);
    let refresh_expires_at = payload
        .refresh_token_expires_in
        .map(|s| now.saturating_add(s));
    let (user_name, open_id) = existing_user.unwrap_or((None, None));

    Ok(TokenSet {
        access_token,
        refresh_token: payload.refresh_token,
        expires_at: now.saturating_add(expires_in),
        refresh_expires_at,
        scope: payload.scope,
        token_type: payload.token_type.unwrap_or_else(|| "Bearer".to_string()),
        obtained_at: now,
        user_name,
        open_id,
    })
}

async fn save_token_atomic(path: &Path, token: &TokenSet) -> AppResult<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        tokio::fs::create_dir_all(parent).await?;
    }

    let serialized = serde_json::to_string_pretty(token)?;
    let tmp_name = format!(
        ".{}.tmp.{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("tokens.json"),
        std::process::id()
    );
    let tmp_path = path.with_file_name(tmp_name);
    tokio::fs::write(&tmp_path, serialized.as_bytes()).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        tokio::fs::set_permissions(&tmp_path, perms).await?;
    }

    // On Windows, tokio::fs::rename overwrites existing files via MoveFileExW(MOVEFILE_REPLACE_EXISTING)
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

fn generate_random_state() -> String {
    let mut bytes = [0u8; 24];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DocMode;

    #[tokio::test]
    async fn corrupted_token_file_returns_false_instead_of_failing() {
        let dir = tempfile::tempdir().unwrap();
        let token_path = dir.path().join("tokens.json");
        std::fs::write(&token_path, "{broken json").unwrap();

        let cfg = Config {
            app_id: "cli_test".to_string(),
            app_secret: "sec".to_string(),
            listen: "127.0.0.1:4918".to_string(),
            redirect_uri: "http://127.0.0.1:4918/oauth/callback".to_string(),
            token_file: token_path,
            api_base_url: "https://open.feishu.cn".to_string(),
            accounts_base_url: "https://accounts.feishu.cn".to_string(),
            scopes: "drive:drive offline_access".to_string(),
            dav_prefix: "/dav".to_string(),
            root_folder_token: None,
            extra_folder_tokens: Vec::new(),
            dav_username: None,
            dav_password: None,
            cache_ttl_secs: 30,
            probe_file_size: true,
            doc_mode: DocMode::Export,
            read_only: false,
            autoindex: true,
            max_qps: 4,
            max_upload_mb: 2048,
        };
        let mgr = OAuthManager::new(cfg, Client::new());
        assert!(!mgr.load_from_disk().await.unwrap());
        assert!(!mgr.status().await.authorized);
    }
}
