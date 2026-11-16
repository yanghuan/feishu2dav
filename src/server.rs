use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use dav_server::DavHandler;
use dav_server::davpath::DavPath;
use dav_server::fakels::FakeLs;
use dav_server::fs::FsError;
use serde::Deserialize;
use tracing::warn;

use crate::cache::{DriveCache, FolderMountStatus};
use crate::config::Config;
use crate::error::{DavHttpFault, scope_dav_fault};
use crate::feishu::FeishuClient;
use crate::oauth::OAuthManager;
use crate::webdav::{DavFsOptions, FeishuDavFs, PathKind};

#[derive(Clone)]
pub struct AppState {
    pub config: Config,
    pub oauth: Arc<OAuthManager>,
    pub client: Arc<FeishuClient>,
    pub cache: Arc<DriveCache>,
    pub fs: FeishuDavFs,
    pub dav: DavHandler,
}

pub fn build_app_state(config: Config, oauth: Arc<OAuthManager>, http: reqwest::Client) -> AppState {
    let client = Arc::new(FeishuClient::new(Arc::clone(&oauth), http));
    let cache = Arc::new(DriveCache::new(config.cache_ttl_secs));
    let fs = FeishuDavFs::new(
        Arc::clone(&client),
        Arc::clone(&cache),
        DavFsOptions {
            root_folder_token: config.root_folder_token.clone(),
            extra_folder_tokens: config.extra_folder_tokens.clone(),
            doc_mode: config.doc_mode,
            probe_file_size: config.probe_file_size,
            probe_markdown_size: config.probe_markdown_size,
            read_only: config.read_only,
            max_upload_bytes: config.max_upload_bytes(),
        },
    );

    let mut builder = DavHandler::builder()
        .filesystem(Box::new(fs.clone()))
        .locksystem(FakeLs::new())
        .autoindex(config.autoindex)
        .hide_symlinks(false)
        .read_buf_size(64 * 1024);

    if config.dav_prefix != "/" {
        builder = builder.strip_prefix(config.dav_prefix.clone());
    }

    let dav = builder.build_handler();

    AppState {
        config,
        oauth,
        client,
        cache,
        fs,
        dav,
    }
}

pub fn build_router(state: AppState) -> Router {
    let callback_path = state.config.callback_path();
    let prefix = state.config.dav_prefix.clone();
    let callback_trimmed = if callback_path == "/" {
        "/"
    } else {
        callback_path.trim_end_matches('/')
    };

    let mut public_router = Router::new().route("/healthz", get(healthz));
    let callback_conflicts = matches!(
        callback_trimmed,
        "/oauth/login" | "/oauth/status" | "/oauth/refresh" | "/_dashboard" | "/healthz"
    ) || (prefix == "/" && callback_trimmed == "/")
        || (prefix != "/"
            && (callback_trimmed == prefix
                || callback_trimmed.starts_with(&format!("{prefix}/"))));
    if !callback_conflicts {
        public_router = public_router.route(&callback_path, get(oauth_callback));
    }

    let mut protected_control = Router::new()
        .route("/_dashboard", get(dashboard))
        .route("/oauth/login", get(oauth_login))
        .route("/oauth/status", get(oauth_status))
        .route("/oauth/refresh", post(oauth_refresh));
    if prefix != "/" && callback_trimmed != "/" {
        protected_control = protected_control.route("/", get(dashboard));
    }
    let protected_control = protected_control.route_layer(middleware::from_fn_with_state(
        state.clone(),
        require_basic_auth,
    ));

    let dav_router = if prefix == "/" {
        Router::new().fallback(handle_dav)
    } else {
        let with_slash = format!("{prefix}/");
        let wildcard = format!("{prefix}/{{*path}}");
        Router::new()
            .route(&prefix, any(handle_dav))
            .route(&with_slash, any(handle_dav))
            .route(&wildcard, any(handle_dav))
    }
    .layer(DefaultBodyLimit::disable());

    public_router
        .merge(protected_control)
        .merge(dav_router)
        .with_state(state)
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn dashboard(State(state): State<AppState>) -> impl IntoResponse {
    let status = state.oauth.status().await;
    let dav_path = if state.config.dav_prefix == "/" {
        "/"
    } else {
        &state.config.dav_prefix
    };

    let auth_badge = if status.authorized && status.access_token_valid {
        format!(
            "<span class='badge ok'>已授权 (User: {})</span>",
            html_escape(status.user_name.as_deref().unwrap_or("Feishu User"))
        )
    } else if status.authorized && status.refresh_token_present {
        "<span class='badge warn'>Access Token 待续期 (拥有 Refresh Token)</span>".to_string()
    } else {
        "<span class='badge err'>未授权 — 请点击下方按钮登录飞书</span>".to_string()
    };

    let expires_text = status
        .access_expires_in_secs
        .map(|s| format!("{s} 秒"))
        .unwrap_or_else(|| "无".to_string());
    let refresh_text = if status.refresh_token_present {
        status
            .refresh_expires_in_secs
            .map(|s| format!("有效 (剩余 {s} 秒)"))
            .unwrap_or_else(|| "已持有".to_string())
    } else {
        "无 (请确认已开启 offline_access 权限)".to_string()
    };
    let root_folder_html = match &state.config.root_folder_token {
        Some(tok) => format!("<code>{}</code> (自定义根目录)", html_escape(tok)),
        None => "我的空间 (默认)".to_string(),
    };
    let extra_mounts_row = if state.config.extra_folder_tokens.is_empty() {
        String::new()
    } else {
        let mut items = Vec::with_capacity(state.config.extra_folder_tokens.len());
        for tok in &state.config.extra_folder_tokens {
            let status_html = match state.cache.get_mount_status(tok).await {
                Some(FolderMountStatus::Mounted { name, .. }) => format!(
                    "<span class='badge ok'>已挂载 ({})</span>",
                    html_escape(&name)
                ),
                Some(FolderMountStatus::Failed { error }) => format!(
                    "<span class='badge err'>挂载失败 ({})</span>",
                    html_escape(&error)
                ),
                None => "<span class='badge warn'>待首次访问验证</span>".to_string(),
            };
            items.push(format!("<div><code>{}</code> {status_html}</div>", html_escape(tok)));
        }
        format!(
            "<tr><td>根目录额外挂载</td><td>{}<div style='font-size:12px;color:#646a73;margin-top:4px'>状态随访问按最多 300s 缓存刷新；实时诊断请运行 <code>feishu2dav status</code></div></td></tr>",
            items.join("")
        )
    };

    let html = format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width,initial-scale=1" />
<title>feishu2dav - 飞书云盘 WebDAV 网关</title>
<style>
  body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; max-width: 760px; margin: 40px auto; padding: 0 20px; color: #1f2329; background: #f5f6f7; }}
  .card {{ background: #fff; border-radius: 12px; padding: 24px; box-shadow: 0 2px 10px rgba(0,0,0,.06); margin-bottom: 18px; }}
  h1 {{ margin-top: 0; font-size: 22px; }}
  .badge {{ display: inline-block; padding: 4px 10px; border-radius: 999px; font-size: 13px; font-weight: 600; }}
  .ok {{ background: #e8ffea; color: #008a20; }}
  .warn {{ background: #fff7e6; color: #ad6800; }}
  .err {{ background: #ffece8; color: #cb2634; }}
  .btn {{ display: inline-block; background: #3370ff; color: #fff; text-decoration: none; padding: 10px 18px; border-radius: 8px; font-weight: 600; margin-top: 10px; }}
  .btn:hover {{ background: #245bdb; }}
  code, pre {{ background: #f2f3f5; padding: 2px 6px; border-radius: 6px; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 13px; }}
  pre {{ padding: 12px; overflow-x: auto; }}
  table {{ width: 100%; border-collapse: collapse; margin-top: 12px; }}
  td {{ padding: 8px 4px; border-bottom: 1px solid #eee; font-size: 14px; }}
  td:first-child {{ color: #646a73; width: 180px; }}
</style>
</head>
<body>
  <div class="card">
    <h1>feishu2dav · 飞书云盘 WebDAV</h1>
    <p>状态：{auth_badge}</p>
    <table>
      <tr><td>App ID</td><td><code>{}</code></td></tr>
      <tr><td>Redirect URI</td><td><code>{}</code></td></tr>
      <tr><td>Scopes</td><td><code>{}</code></td></tr>
      <tr><td>Access Token 剩余</td><td>{}</td></tr>
      <tr><td>Refresh Token</td><td>{}</td></tr>
      <tr><td>WebDAV 挂载路径</td><td><code>http://{}{}</code></td></tr>
      <tr><td>Drive 根目录</td><td>{root_folder_html}</td></tr>
      {extra_mounts_row}
      <tr><td>文档模式</td><td><code>{:?}</code> (只读: <code>{}</code>)</td></tr>
    </table>
    <a class="btn" href="/oauth/login">使用飞书账号授权 / 重新授权</a>
  </div>
  <div class="card">
    <h2>快速挂载</h2>
    <p><strong>Windows (资源管理器 / cmd):</strong></p>
    <pre>net use Z: http://{}{}</pre>
    <p><strong>macOS Finder:</strong> 按 <code>Cmd + K</code>，输入 <code>http://{}{}</code></p>
    <p><strong>rclone:</strong></p>
    <pre>rclone lsd :webdav: --webdav-url http://{}{}</pre>
  </div>
</body>
</html>"#,
        html_escape(&state.config.app_id),
        html_escape(&state.config.redirect_uri),
        html_escape(&state.config.scopes),
        html_escape(&expires_text),
        html_escape(&refresh_text),
        html_escape(&state.config.listen),
        html_escape(dav_path),
        state.config.doc_mode,
        state.config.read_only,
        html_escape(&state.config.listen),
        html_escape(dav_path),
        html_escape(&state.config.listen),
        html_escape(dav_path),
        html_escape(&state.config.listen),
        html_escape(dav_path),
    );

    Html(html)
}

async fn oauth_login(State(state): State<AppState>) -> Response {
    match state.oauth.build_authorize_url().await {
        Ok((url, _state)) => Redirect::temporary(&url).into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to build OAuth URL: {err}"),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

async fn oauth_callback(
    State(state): State<AppState>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    if let Some(err) = query.error {
        let desc = query
            .error_description
            .unwrap_or_else(|| "User denied or OAuth failed".to_string());
        let html = format!(
            "<html><body style='font-family:sans-serif;max-width:640px;margin:40px auto'>\
             <h2>飞书授权失败</h2><p>错误：<code>{}</code></p><p>{}</p>\
             <p><a href='/oauth/login'>重试授权</a></p></body></html>",
            html_escape(&err),
            html_escape(&desc),
        );
        return (StatusCode::BAD_REQUEST, Html(html)).into_response();
    }

    let (Some(code), Some(oauth_state)) = (query.code, query.state) else {
        return (
            StatusCode::BAD_REQUEST,
            Html("<h2>缺少 code 或 state 参数</h2><p><a href='/oauth/login'>重新发起授权</a></p>"),
        )
            .into_response();
    };

    match state.oauth.handle_callback(&code, &oauth_state).await {
        Ok(tokens) => {
            state.cache.clear_all().await;
            let user_display = tokens
                .user_name
                .as_deref()
                .unwrap_or("飞书用户");
            let dav_path = &state.config.dav_prefix;
            let callback_path = state.config.callback_path();
            let dashboard_href =
                if state.config.dav_prefix == "/" || callback_path == "/" {
                    "/_dashboard"
                } else {
                    "/"
                };
            let html = format!(
                r#"<!doctype html>
<html lang="zh-CN"><head><meta charset="utf-8" /><title>授权成功</title></head>
<body style="font-family:sans-serif;max-width:640px;margin:40px auto;line-height:1.6">
  <h2>授权成功！</h2>
  <p>已成功登录飞书账号：<strong>{}</strong></p>
  <p>凭证已持久化保存至：<code>{}</code></p>
  <p>WebDAV 访问地址：<code>http://{}{}</code></p>
  <p><a href="{dashboard_href}">返回状态面板</a></p>
</body></html>"#,
                html_escape(user_display),
                html_escape(&state.config.token_file.display().to_string()),
                html_escape(&state.config.listen),
                html_escape(dav_path),
            );
            (StatusCode::OK, Html(html)).into_response()
        }
        Err(err) => {
            warn!("OAuth callback exchange failed: {err}");
            let html = format!(
                "<html><body style='font-family:sans-serif;max-width:640px;margin:40px auto'>\
                 <h2>换取 user_access_token 失败</h2><p>{}</p>\
                 <p><a href='/oauth/login'>重新授权</a></p></body></html>",
                html_escape(&err.to_string()),
            );
            (StatusCode::BAD_GATEWAY, Html(html)).into_response()
        }
    }
}

async fn oauth_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(state.oauth.status().await)
}

async fn oauth_refresh(State(state): State<AppState>) -> Response {
    match state.oauth.force_refresh().await {
        Ok(_) => Json(state.oauth.status().await).into_response(),
        Err(err) => (
            StatusCode::BAD_GATEWAY,
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn handle_dav(State(state): State<AppState>, req: Request) -> Response {
    if let Some(resp) = unauthorized_if_needed(&state.config, req.headers()) {
        return resp;
    }

    if !state.config.read_only {
        // Early 413 check before dav-server buffers PUT bodies in memory or temp files.
        if req.method() == Method::PUT
            && let Some(limit) = state.config.max_upload_bytes()
        {
            let exceeds_content_len = req
                .headers()
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.trim().parse::<u64>().ok())
                .is_some_and(|len| len > limit);
            let exceeds_range_end =
                parse_content_range_end(req.headers()).is_some_and(|end| end > limit);
            if exceeds_content_len || exceeds_range_end {
                return (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    [(header::CONNECTION, "close")],
                    format!("Upload exceeds max_upload_mb limit ({limit} bytes)"),
                )
                    .into_response();
            }
        }

        // Prevent dav-server from recursively deleting, moving, or tree-copying a virtual root mount folder
        // (RFC4918 DELETE walks children before remove_dir; COPY walks children to copy into My Space).
        let method = req.method().as_str();
        if !state.config.extra_folder_tokens.is_empty()
            && matches!(method, "DELETE" | "MOVE" | "COPY")
            && let Ok(src_path) = parse_request_dav_path(req.uri(), &state.config.dav_prefix)
        {
            let (check_res, fault) = scope_dav_fault(state.fs.inspect_path(&src_path)).await;
            match check_res {
                Ok(PathKind::MountPoint) => {
                    return (
                        StatusCode::FORBIDDEN,
                        "Cannot delete, move, or copy configured extra mount folder",
                    )
                        .into_response();
                }
                Ok(_) => {}
                Err(err) => return dav_error_response(err, fault, "Request path"),
            }
        }

        // dav-server pre-deletes an existing Destination via remove_file before calling
        // rename/copy. Fail-closed on missing/invalid Destination and intercept MOVE/COPY
        // targeting an existing folder or ExportDoc/UrlShortcut here so explicit DELETE
        // works while MOVE/COPY cannot wipe out directories or cloud documents.
        if method == "MOVE" || method == "COPY" {
            let Some(raw_dest) = req
                .headers()
                .get("destination")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty())
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    "Missing or invalid Destination header",
                )
                    .into_response();
            };

            let dest_path = match parse_destination_dav_path(raw_dest, &state.config.dav_prefix) {
                Ok(p) => p,
                Err(DestinationParseError::Invalid) => {
                    return (StatusCode::BAD_REQUEST, "Invalid Destination header").into_response();
                }
                Err(DestinationParseError::PrefixMismatch) => {
                    return (
                        StatusCode::BAD_GATEWAY,
                        "Destination is outside WebDAV mount prefix",
                    )
                        .into_response();
                }
            };

            let (check_res, fault) = scope_dav_fault(state.fs.inspect_path(&dest_path)).await;
            match check_res {
                Ok(
                    kind @ (PathKind::MountPoint
                    | PathKind::Folder
                    | PathKind::CloudDocTarget),
                ) => {
                    let overwrite = !req
                        .headers()
                        .get("overwrite")
                        .and_then(|v| v.to_str().ok())
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("F"));
                    if !overwrite {
                        return (
                            StatusCode::PRECONDITION_FAILED,
                            "Destination already exists and Overwrite is F",
                        )
                            .into_response();
                    }
                    let msg = match kind {
                        PathKind::MountPoint | PathKind::Folder => {
                            "Cannot overwrite existing directory via MOVE/COPY"
                        }
                        PathKind::CloudDocTarget => {
                            "Cannot overwrite native Feishu cloud document or shortcut"
                        }
                        PathKind::Missing | PathKind::MutableFile => unreachable!(),
                    };
                    return (StatusCode::FORBIDDEN, msg).into_response();
                }
                Ok(PathKind::Missing | PathKind::MutableFile) => {}
                Err(err) => {
                    // Fail-closed on destination inspection errors so MOVE/COPY cannot proceed blindly.
                    return dav_error_response(err, fault, "Destination");
                }
            }
        }
    }

    let (dav_resp, fault) = scope_dav_fault(state.dav.handle(req)).await;
    let resp = dav_resp.into_response();
    if let Some(fault) = fault {
        let expected_status = match fault {
            DavHttpFault::RateLimited => StatusCode::BAD_GATEWAY,
            DavHttpFault::TokenUnavailable | DavHttpFault::ForbiddenWithMessage(_) => {
                StatusCode::FORBIDDEN
            }
        };
        if resp.status() == expected_status {
            return dav_fault_response(fault);
        }
    }
    resp
}

fn dav_error_response(err: FsError, fault: Option<DavHttpFault>, target: &str) -> Response {
    if let Some(fault) = fault {
        return dav_fault_response(fault);
    }
    match err {
        FsError::Forbidden => (StatusCode::FORBIDDEN, format!("{target} is forbidden")).into_response(),
        FsError::LoopDetected => (
            StatusCode::LOOP_DETECTED,
            format!("Shortcut loop detected while resolving {target}"),
        )
            .into_response(),
        _ => (
            StatusCode::BAD_GATEWAY,
            format!("Failed to verify {target} against Feishu Drive"),
        )
            .into_response(),
    }
}

fn dav_fault_response(fault: DavHttpFault) -> Response {
    match fault {
        DavHttpFault::RateLimited => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::RETRY_AFTER, "2")],
            "Feishu Drive API rate limited; please retry shortly",
        )
            .into_response(),
        DavHttpFault::TokenUnavailable => (
            StatusCode::UNAUTHORIZED,
            "Feishu OAuth authorization required or expired; please authorize at /oauth/login",
        )
            .into_response(),
        DavHttpFault::ForbiddenWithMessage(msg) => (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            msg,
        )
            .into_response(),
    }
}

fn parse_content_range_end(headers: &HeaderMap) -> Option<u64> {
    let raw = headers.get(header::CONTENT_RANGE)?.to_str().ok()?;
    let rest = raw.trim().strip_prefix("bytes ")?.trim();
    let (range_part, _) = rest.split_once('/')?;
    let (_, end_str) = range_part.split_once('-')?;
    let end: u64 = end_str.trim().parse().ok()?;
    end.checked_add(1)
}

enum DestinationParseError {
    Invalid,
    PrefixMismatch,
}

fn parse_request_dav_path(
    uri: &axum::http::Uri,
    prefix: &str,
) -> Result<DavPath, DestinationParseError> {
    normalize_dav_path(uri.path(), prefix)
}

fn parse_destination_dav_path(
    raw_dest: &str,
    prefix: &str,
) -> Result<DavPath, DestinationParseError> {
    if raw_dest.starts_with("//") {
        return Err(DestinationParseError::Invalid);
    }
    if raw_dest.starts_with('/') {
        return normalize_dav_path(raw_dest, prefix);
    }
    let parsed = url::Url::parse(raw_dest).map_err(|_| DestinationParseError::Invalid)?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.fragment().is_some() {
        return Err(DestinationParseError::Invalid);
    }
    normalize_dav_path(parsed.path(), prefix)
}

fn normalize_dav_path(raw_path: &str, prefix: &str) -> Result<DavPath, DestinationParseError> {
    let mut dav_path = DavPath::new(raw_path).map_err(|_| DestinationParseError::Invalid)?;
    if prefix != "/" {
        let full = dav_path.as_bytes();
        let pfx = prefix.as_bytes();
        if full != pfx && !(full.starts_with(pfx) && full.get(pfx.len()) == Some(&b'/')) {
            return Err(DestinationParseError::PrefixMismatch);
        }
        dav_path
            .set_prefix(prefix)
            .map_err(|_| DestinationParseError::PrefixMismatch)?;
    }
    Ok(dav_path)
}

async fn require_basic_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(resp) = unauthorized_if_needed(&state.config, req.headers()) {
        return resp;
    }
    next.run(req).await
}

fn unauthorized_if_needed(config: &Config, headers: &HeaderMap) -> Option<Response> {
    let (Some(expected_user), Some(expected_pass)) = (&config.dav_username, &config.dav_password)
    else {
        return None;
    };
    if check_basic_auth(headers, expected_user, expected_pass) {
        return None;
    }
    let mut resp = Response::new(axum::body::Body::from("Unauthorized"));
    *resp.status_mut() = StatusCode::UNAUTHORIZED;
    resp.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"feishu2dav\""),
    );
    Some(resp)
}

fn check_basic_auth(headers: &HeaderMap, expected_user: &str, expected_pass: &str) -> bool {
    let Some(raw) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };

    let Some(encoded) = raw
        .strip_prefix("Basic ")
        .or_else(|| raw.strip_prefix("basic "))
    else {
        return false;
    };

    let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
        return false;
    };
    let Ok(creds) = std::str::from_utf8(&decoded) else {
        return false;
    };
    let Some((user, pass)) = creds.split_once(':') else {
        return false;
    };

    constant_time_eq(user.as_bytes(), expected_user.as_bytes())
        && constant_time_eq(pass.as_bytes(), expected_pass.as_bytes())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn html_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}
