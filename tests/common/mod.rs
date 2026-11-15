#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use feishu2dav::config::{Config, DocMode};
use feishu2dav::feishu::adler32_decimal;
use feishu2dav::oauth::{OAuthManager, TokenSet};
use feishu2dav::server::{build_app_state, build_router};

#[derive(Clone, Debug)]
pub struct MockNode {
    pub token: String,
    pub name: String,
    pub file_type: String,
    pub parent_token: String,
    pub content: Vec<u8>,
    pub shortcut_target: Option<(String, String)>,
    pub url: Option<String>,
    pub created_time: u64,
    pub modified_time: u64,
}

#[derive(Clone, Debug)]
struct MockUploadSession {
    parent_node: String,
    file_name: String,
    size: usize,
    block_size: usize,
    parts: HashMap<usize, Vec<u8>>,
}

#[derive(Clone, Debug)]
pub struct MockExportJob {
    doc_token: String,
    doc_type: String,
    ext: String,
    polls: usize,
}

pub struct MockFeishuState {
    app_id: String,
    app_secret: String,
    pub valid_access_token: String,
    pub valid_refresh_token: String,
    pub token_seq: usize,
    pub refresh_calls: AtomicUsize,
    pub root_meta_calls: AtomicUsize,
    pub force_expired_once: bool,
    pub force_rate_limit_once: bool,
    pub force_rate_limit_forever: bool,
    pub ignore_range_on_download: bool,
    pub reject_zero_byte_upload: bool,
    root_token: String,
    pub nodes: HashMap<String, MockNode>,
    uploads: HashMap<String, MockUploadSession>,
    tasks: HashMap<String, usize>,
    pub exports: HashMap<String, MockExportJob>,
    exported_blobs: HashMap<String, Vec<u8>>,
    next_id: usize,
}

impl MockFeishuState {
    fn new() -> Self {
        let mut nodes = HashMap::new();
        nodes.insert(
            "file_init".to_string(),
            MockNode {
                token: "file_init".to_string(),
                name: "readme.txt".to_string(),
                file_type: "file".to_string(),
                parent_token: "fld_root".to_string(),
                content: b"Hello Feishu WebDAV!".to_vec(),
                shortcut_target: None,
                url: Some("https://feishu.cn/file/file_init".to_string()),
                created_time: 1_700_000_000,
                modified_time: 1_700_000_100,
            },
        );
        nodes.insert(
            "doc_init".to_string(),
            MockNode {
                token: "doc_init".to_string(),
                name: "DesignSpec".to_string(),
                file_type: "docx".to_string(),
                parent_token: "fld_root".to_string(),
                content: Vec::new(),
                shortcut_target: None,
                url: Some("https://feishu.cn/docx/doc_init".to_string()),
                created_time: 1_700_000_050,
                modified_time: 1_700_000_200,
            },
        );
        nodes.insert(
            "slides_init".to_string(),
            MockNode {
                token: "slides_init".to_string(),
                name: "PitchDeck".to_string(),
                file_type: "slides".to_string(),
                parent_token: "fld_root".to_string(),
                content: Vec::new(),
                shortcut_target: None,
                url: Some("https://feishu.cn/slides/slides_init".to_string()),
                created_time: 1_700_000_060,
                modified_time: 1_700_000_250,
            },
        );
        nodes.insert(
            "fld_target".to_string(),
            MockNode {
                token: "fld_target".to_string(),
                name: "SharedTarget".to_string(),
                file_type: "folder".to_string(),
                parent_token: "fld_external".to_string(),
                content: Vec::new(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_010,
                modified_time: 1_700_000_010,
            },
        );
        nodes.insert(
            "file_shared".to_string(),
            MockNode {
                token: "file_shared".to_string(),
                name: "shared.txt".to_string(),
                file_type: "file".to_string(),
                parent_token: "fld_target".to_string(),
                content: b"shared-via-shortcut".to_vec(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_020,
                modified_time: 1_700_000_020,
            },
        );
        nodes.insert(
            "sc_init".to_string(),
            MockNode {
                token: "sc_init".to_string(),
                name: "SharedAlias".to_string(),
                file_type: "shortcut".to_string(),
                parent_token: "fld_root".to_string(),
                content: Vec::new(),
                shortcut_target: Some(("folder".to_string(), "fld_target".to_string())),
                url: None,
                created_time: 1_700_000_030,
                modified_time: 1_700_000_030,
            },
        );

        Self {
            app_id: "cli_test_app".to_string(),
            app_secret: "secret_test_123".to_string(),
            valid_access_token: "u-access-initial".to_string(),
            valid_refresh_token: "ur-refresh-initial".to_string(),
            token_seq: 1,
            refresh_calls: AtomicUsize::new(0),
            root_meta_calls: AtomicUsize::new(0),
            force_expired_once: false,
            force_rate_limit_once: false,
            force_rate_limit_forever: false,
            ignore_range_on_download: false,
            reject_zero_byte_upload: true,
            root_token: "fld_root".to_string(),
            nodes,
            uploads: HashMap::new(),
            tasks: HashMap::new(),
            exports: HashMap::new(),
            exported_blobs: HashMap::new(),
            next_id: 100,
        }
    }

    fn alloc_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}_{}", self.next_id)
    }

    fn check_auth(&mut self, headers: &HeaderMap) -> Option<Response> {
        let Some(auth) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
        else {
            return Some(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"code": 99991668, "msg": "missing token"})),
                )
                    .into_response(),
            );
        };
        let token = auth.strip_prefix("Bearer ").unwrap_or("");
        if token != self.valid_access_token {
            return Some(
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"code": 99991668, "msg": "invalid token"})),
                )
                    .into_response(),
            );
        }
        if self.force_expired_once {
            self.force_expired_once = false;
            return Some(
                (
                    StatusCode::OK,
                    Json(json!({"code": 99991677, "msg": "token expire"})),
                )
                    .into_response(),
            );
        }
        if self.force_rate_limit_forever || self.force_rate_limit_once {
            self.force_rate_limit_once = false;
            return Some(
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"code": 1061045, "msg": "rate limit"})),
                )
                    .into_response(),
            );
        }
        None
    }
}

pub type SharedMock = Arc<Mutex<MockFeishuState>>;

pub async fn spawn_mock_feishu() -> (String, SharedMock) {
    let state = Arc::new(Mutex::new(MockFeishuState::new()));
    let app = Router::new()
        .route("/open-apis/authen/v2/oauth/token", post(mock_oauth_token))
        .route("/open-apis/authen/v1/user_info", get(mock_user_info))
        .route(
            "/open-apis/drive/explorer/v2/root_folder/meta",
            get(mock_root_meta),
        )
        .route("/open-apis/drive/v1/files", get(mock_list_files))
        .route(
            "/open-apis/drive/v1/files/create_folder",
            post(mock_create_folder),
        )
        .route("/open-apis/drive/v1/files/upload_all", post(mock_upload_all))
        .route(
            "/open-apis/drive/v1/files/upload_prepare",
            post(mock_upload_prepare),
        )
        .route(
            "/open-apis/drive/v1/files/upload_part",
            post(mock_upload_part),
        )
        .route(
            "/open-apis/drive/v1/files/upload_finish",
            post(mock_upload_finish),
        )
        .route("/open-apis/drive/v1/files/task_check", get(mock_task_check))
        .route(
            "/open-apis/drive/v1/files/{token}/download",
            get(mock_download),
        )
        .route("/open-apis/drive/v1/files/{token}/move", post(mock_move))
        .route("/open-apis/drive/v1/files/{token}/copy", post(mock_copy))
        .route("/open-apis/drive/v1/files/{token}", delete(mock_delete))
        .route("/open-apis/drive/v1/export_tasks", post(mock_create_export))
        .route(
            "/open-apis/drive/v1/export_tasks/{ticket}",
            get(mock_poll_export),
        )
        .route(
            "/open-apis/drive/v1/export_tasks/file/{token}/download",
            get(mock_download_export),
        )
        .layer(DefaultBodyLimit::disable())
        .with_state(Arc::clone(&state));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

#[derive(Deserialize)]
struct MockTokenReq {
    grant_type: String,
    client_id: String,
    client_secret: String,
    code: Option<String>,
    refresh_token: Option<String>,
}

async fn mock_oauth_token(
    State(state): State<SharedMock>,
    Json(req): Json<MockTokenReq>,
) -> Response {
    let mut s = state.lock().await;
    if req.client_id != s.app_id || req.client_secret != s.app_secret {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "code": 20002,
                "error": "invalid_client",
                "error_description": "wrong client credentials"
            })),
        )
            .into_response();
    }

    match req.grant_type.as_str() {
        "authorization_code" => {
            if req.code.as_deref() != Some("valid_auth_code") {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "code": 20065,
                        "error": "invalid_grant",
                        "error_description": "invalid authorization code"
                    })),
                )
                    .into_response();
            }
            s.token_seq += 1;
            s.valid_access_token = format!("u-access-{}", s.token_seq);
            s.valid_refresh_token = format!("ur-refresh-{}", s.token_seq);
            Json(json!({
                "code": 0,
                "access_token": s.valid_access_token,
                "expires_in": 7200,
                "refresh_token": s.valid_refresh_token,
                "refresh_token_expires_in": 604800,
                "scope": "drive:drive drive:export:readonly offline_access",
                "token_type": "Bearer"
            }))
            .into_response()
        }
        "refresh_token" => {
            s.refresh_calls.fetch_add(1, Ordering::SeqCst);
            if req.refresh_token.as_deref() != Some(&s.valid_refresh_token) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({
                        "code": 20037,
                        "error": "invalid_grant",
                        "error_description": "refresh token expired or already consumed"
                    })),
                )
                    .into_response();
            }
            // Rotate both access_token and single-use refresh_token
            s.token_seq += 1;
            s.valid_access_token = format!("u-access-{}", s.token_seq);
            s.valid_refresh_token = format!("ur-refresh-{}", s.token_seq);
            Json(json!({
                "code": 0,
                "access_token": s.valid_access_token,
                "expires_in": 7200,
                "refresh_token": s.valid_refresh_token,
                "refresh_token_expires_in": 604800,
                "scope": "drive:drive drive:export:readonly offline_access",
                "token_type": "Bearer"
            }))
            .into_response()
        }
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 20001, "error": "unsupported_grant_type"})),
        )
            .into_response(),
    }
}

async fn mock_user_info(State(state): State<SharedMock>, headers: HeaderMap) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": {
            "name": "Alice Tester",
            "open_id": "ou_alice_123"
        }
    }))
    .into_response()
}

async fn mock_root_meta(State(state): State<SharedMock>, headers: HeaderMap) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    s.root_meta_calls.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": {
            "token": s.root_token,
            "id": s.root_token,
            "user_id": "ou_alice_123"
        }
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ListQuery {
    folder_token: Option<String>,
    page_token: Option<String>,
}

async fn mock_list_files(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let parent = q.folder_token.unwrap_or_else(|| s.root_token.clone());
    let mut items: Vec<&MockNode> = s
        .nodes
        .values()
        .filter(|n| n.parent_token == parent)
        .collect();
    items.sort_by(|a, b| a.token.cmp(&b.token));

    // Simulate 2-page pagination when a folder has > 3 items
    let offset: usize = q
        .page_token
        .as_deref()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let page_limit = 3;
    let end = (offset + page_limit).min(items.len());
    let has_more = end < items.len();
    let next_token = if has_more {
        end.to_string()
    } else {
        String::new()
    };

    let files_json: Vec<Value> = items[offset..end]
        .iter()
        .map(|n| {
            let mut obj = json!({
                "token": n.token,
                "name": n.name,
                "type": n.file_type,
                "parent_token": n.parent_token,
                "created_time": n.created_time.to_string(),
                "modified_time": n.modified_time.to_string(),
                "url": n.url,
            });
            if let Some((ref t_type, ref t_tok)) = n.shortcut_target {
                obj["shortcut_info"] = json!({
                    "target_type": t_type,
                    "target_token": t_tok,
                });
            }
            obj
        })
        .collect();

    Json(json!({
        "code": 0,
        "msg": "success",
        "data": {
            "files": files_json,
            "has_more": has_more,
            "next_page_token": next_token
        }
    }))
    .into_response()
}

async fn mock_download(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let Some(node) = s.nodes.get(&token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"code": 1062502, "msg": "not found"})),
        )
            .into_response();
    };
    let total = node.content.len();
    if !s.ignore_range_on_download
        && let Some(range_hdr) = headers.get(header::RANGE).and_then(|v| v.to_str().ok())
        && let Some(spec) = range_hdr.strip_prefix("bytes=")
        && let Some((start_s, end_s)) = spec.split_once('-')
    {
        if total == 0 {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            resp.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_static("bytes */0"),
            );
            return resp;
        }
        let start: usize = start_s.parse().unwrap_or(0);
        let end: usize = if end_s.is_empty() {
            total - 1
        } else {
            end_s.parse::<usize>().unwrap_or(total - 1).min(total - 1)
        };
        if start >= total || start > end {
            let mut resp = Response::new(axum::body::Body::empty());
            *resp.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            resp.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{total}")).unwrap(),
            );
            return resp;
        }
        let slice = node.content[start..=end].to_vec();
        let mut resp = Response::new(axum::body::Body::from(slice));
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        resp.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{total}")).unwrap(),
        );
        return resp;
    }

    let mut resp = Response::new(axum::body::Body::from(node.content.clone()));
    resp.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&total.to_string()).unwrap(),
    );
    resp
}

#[derive(Deserialize)]
struct CreateFolderReq {
    pub name: String,
    folder_token: String,
}

async fn mock_create_folder(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    Json(req): Json<CreateFolderReq>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let tok = s.alloc_id("fld");
    let now = now_secs();
    s.nodes.insert(
        tok.clone(),
        MockNode {
            token: tok.clone(),
            name: req.name,
            file_type: "folder".to_string(),
            parent_token: req.folder_token,
            content: Vec::new(),
            shortcut_target: None,
            url: None,
            created_time: now,
            modified_time: now,
        },
    );
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": { "token": tok }
    }))
    .into_response()
}

async fn mock_upload_all(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Response {
    {
        let mut s = state.lock().await;
        if let Some(resp) = s.check_auth(&headers) {
            return resp;
        }
    }

    let mut file_name = String::new();
    let mut parent_node = String::new();
    let mut size: Option<usize> = None;
    let mut checksum: Option<String> = None;
    let mut bytes = Vec::new();

    while let Ok(Some(field)) = mp.next_field().await {
        match field.name() {
            Some("file_name") => file_name = field.text().await.unwrap_or_default(),
            Some("parent_node") => parent_node = field.text().await.unwrap_or_default(),
            Some("size") => size = field.text().await.ok().and_then(|v| v.parse().ok()),
            Some("checksum") => checksum = field.text().await.ok(),
            Some("file") => bytes = field.bytes().await.unwrap_or_default().to_vec(),
            _ => {}
        }
    }

    let mut s = state.lock().await;
    if bytes.is_empty() && s.reject_zero_byte_upload {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 1061002, "msg": "empty file not allowed"})),
        )
            .into_response();
    }
    if size != Some(bytes.len()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 1061002, "msg": "size mismatch"})),
        )
            .into_response();
    }
    if let Some(cs) = checksum
        && cs != adler32_decimal(&bytes)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 1061002, "msg": "checksum mismatch"})),
        )
            .into_response();
    }

    let tok = s.alloc_id("box");
    let now = now_secs();
    s.nodes.insert(
        tok.clone(),
        MockNode {
            token: tok.clone(),
            name: file_name,
            file_type: "file".to_string(),
            parent_token: parent_node,
            content: bytes,
            shortcut_target: None,
            url: None,
            created_time: now,
            modified_time: now,
        },
    );
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": { "file_token": tok }
    }))
    .into_response()
}

#[derive(Deserialize)]
struct UploadPrepareReq {
    file_name: String,
    parent_node: String,
    size: usize,
}

async fn mock_upload_prepare(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    Json(req): Json<UploadPrepareReq>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let block_size = 4 * 1024 * 1024;
    let block_num = req.size.div_ceil(block_size);
    let upload_id = s.alloc_id("upl");
    s.uploads.insert(
        upload_id.clone(),
        MockUploadSession {
            parent_node: req.parent_node,
            file_name: req.file_name,
            size: req.size,
            block_size,
            parts: HashMap::new(),
        },
    );
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": {
            "upload_id": upload_id,
            "block_size": block_size,
            "block_num": block_num
        }
    }))
    .into_response()
}

async fn mock_upload_part(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    mut mp: Multipart,
) -> Response {
    {
        let mut s = state.lock().await;
        if let Some(resp) = s.check_auth(&headers) {
            return resp;
        }
    }

    let mut upload_id = String::new();
    let mut seq = 0usize;
    let mut checksum = None;
    let mut bytes = Vec::new();

    while let Ok(Some(field)) = mp.next_field().await {
        match field.name() {
            Some("upload_id") => upload_id = field.text().await.unwrap_or_default(),
            Some("seq") => seq = field.text().await.ok().and_then(|v| v.parse().ok()).unwrap_or(0),
            Some("checksum") => checksum = field.text().await.ok(),
            Some("file") => bytes = field.bytes().await.unwrap_or_default().to_vec(),
            _ => {}
        }
    }

    if let Some(cs) = checksum
        && cs != adler32_decimal(&bytes)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 1061002, "msg": "part checksum mismatch"})),
        )
            .into_response();
    }

    let mut s = state.lock().await;
    let Some(session) = s.uploads.get_mut(&upload_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"code": 1062502, "msg": "upload session not found"})),
        )
            .into_response();
    };
    if bytes.len() > session.block_size {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 1061002, "msg": "part exceeds block_size"})),
        )
            .into_response();
    }
    session.parts.insert(seq, bytes);
    Json(json!({"code": 0, "msg": "success"})).into_response()
}

#[derive(Deserialize)]
struct UploadFinishReq {
    upload_id: String,
    block_num: usize,
}

async fn mock_upload_finish(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    Json(req): Json<UploadFinishReq>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let Some(session) = s.uploads.remove(&req.upload_id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"code": 1062502, "msg": "upload session not found"})),
        )
            .into_response();
    };
    let mut assembled = Vec::with_capacity(session.size);
    for seq in 0..req.block_num {
        if let Some(part) = session.parts.get(&seq) {
            assembled.extend_from_slice(part);
        }
    }
    let tok = s.alloc_id("box");
    let now = now_secs();
    s.nodes.insert(
        tok.clone(),
        MockNode {
            token: tok.clone(),
            name: session.file_name,
            file_type: "file".to_string(),
            parent_token: session.parent_node,
            content: assembled,
            shortcut_target: None,
            url: None,
            created_time: now,
            modified_time: now,
        },
    );
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": { "file_token": tok }
    }))
    .into_response()
}

#[derive(Deserialize)]
struct MoveReq {
    #[serde(rename = "type")]
    pub file_type: String,
    folder_token: String,
}

async fn mock_move(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
    Json(req): Json<MoveReq>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let Some(node) = s.nodes.get_mut(&token) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"code": 1062502, "msg": "not found"})),
        )
            .into_response();
    };
    node.parent_token = req.folder_token;
    if req.file_type == "folder" {
        let task_id = s.alloc_id("task");
        s.tasks.insert(task_id.clone(), 0);
        return Json(json!({"code": 0, "msg": "success", "data": {"task_id": task_id}}))
            .into_response();
    }
    Json(json!({"code": 0, "msg": "success", "data": {"file_token": token}})).into_response()
}

#[derive(Deserialize)]
struct CopyReq {
    pub name: String,
    #[serde(rename = "type")]
    pub file_type: String,
    folder_token: String,
}

async fn mock_copy(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
    Json(req): Json<CopyReq>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let Some(src) = s.nodes.get(&token).cloned() else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"code": 1062502, "msg": "not found"})),
        )
            .into_response();
    };
    if req.file_type == "folder" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"code": 1061002, "msg": "cannot copy folder"})),
        )
            .into_response();
    }
    let new_tok = s.alloc_id("copy");
    let now = now_secs();
    s.nodes.insert(
        new_tok.clone(),
        MockNode {
            token: new_tok.clone(),
            name: req.name.clone(),
            file_type: src.file_type.clone(),
            parent_token: req.folder_token.clone(),
            content: src.content,
            shortcut_target: None,
            url: src.url,
            created_time: now,
            modified_time: now,
        },
    );
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": {
            "file": {
                "token": new_tok,
                "name": req.name,
                "type": src.file_type,
                "parent_token": req.folder_token
            }
        }
    }))
    .into_response()
}

#[derive(Deserialize)]
struct DeleteQuery {
    #[serde(rename = "type")]
    pub file_type: String,
}

async fn mock_delete(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
    Query(q): Query<DeleteQuery>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    if s.nodes.remove(&token).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"code": 1062502, "msg": "not found"})),
        )
            .into_response();
    }
    if q.file_type == "folder" {
        let task_id = s.alloc_id("task");
        s.tasks.insert(task_id.clone(), 0);
        return Json(json!({"code": 0, "msg": "success", "data": {"task_id": task_id}}))
            .into_response();
    }
    Json(json!({"code": 0, "msg": "success"})).into_response()
}

#[derive(Deserialize)]
struct TaskCheckQuery {
    task_id: String,
}

async fn mock_task_check(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    Query(q): Query<TaskCheckQuery>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let count = s.tasks.entry(q.task_id).or_insert(0);
    *count += 1;
    let status = if *count == 1 { "process" } else { "success" };
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": { "status": status }
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ExportReq {
    file_extension: String,
    pub token: String,
    #[serde(rename = "type")]
    doc_type: String,
}

async fn mock_create_export(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    Json(req): Json<ExportReq>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let ticket = s.alloc_id("ticket");
    s.exports.insert(
        ticket.clone(),
        MockExportJob {
            doc_token: req.token,
            doc_type: req.doc_type,
            ext: req.file_extension,
            polls: 0,
        },
    );
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": { "ticket": ticket }
    }))
    .into_response()
}

async fn mock_poll_export(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    AxumPath(ticket): AxumPath<String>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let Some(job) = s.exports.get_mut(&ticket) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"code": 1062502, "msg": "ticket not found"})),
        )
            .into_response();
    };
    job.polls += 1;
    if job.polls == 1 {
        return Json(json!({
            "code": 0,
            "msg": "success",
            "data": {
                "result": {
                    "job_status": 1,
                    "job_error_msg": "processing"
                }
            }
        }))
        .into_response();
    }
    let payload = format!(
        "EXPORTED({}:{}:{})",
        job.doc_type, job.doc_token, job.ext
    )
    .into_bytes();
    let size = payload.len();
    let ftok = s.alloc_id("expfile");
    s.exported_blobs.insert(ftok.clone(), payload);
    Json(json!({
        "code": 0,
        "msg": "success",
        "data": {
            "result": {
                "file_token": ftok,
                "file_size": size,
                "job_status": 0,
                "job_error_msg": "success"
            }
        }
    }))
    .into_response()
}

async fn mock_download_export(
    State(state): State<SharedMock>,
    headers: HeaderMap,
    AxumPath(token): AxumPath<String>,
) -> Response {
    let mut s = state.lock().await;
    if let Some(resp) = s.check_auth(&headers) {
        return resp;
    }
    let Some(bytes) = s.exported_blobs.get(&token).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    Response::new(axum::body::Body::from(bytes))
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub struct TestServerOpts<'a> {
    pub basic_auth: Option<(&'a str, &'a str)>,
    pub preauth_token: Option<TokenSet>,
    pub dav_prefix: &'a str,
    pub root_folder_token: Option<String>,
    pub redirect_uri: Option<String>,
    pub doc_mode: DocMode,
    pub read_only: bool,
    pub max_upload_mb: u64,
    pub cache_ttl_secs: u64,
}

impl<'a> Default for TestServerOpts<'a> {
    fn default() -> Self {
        Self {
            basic_auth: None,
            preauth_token: None,
            dav_prefix: "/dav",
            root_folder_token: None,
            redirect_uri: None,
            doc_mode: DocMode::Export,
            read_only: false,
            max_upload_mb: 2048,
            cache_ttl_secs: 30,
        }
    }
}

pub async fn spawn_test_server(
    mock_base: &str,
    dir: &TempDir,
    basic_auth: Option<(&str, &str)>,
    preauth_token: Option<TokenSet>,
) -> (String, Arc<OAuthManager>) {
    spawn_test_server_with(
        mock_base,
        dir,
        TestServerOpts {
            basic_auth,
            preauth_token,
            ..Default::default()
        },
    )
    .await
}

pub async fn spawn_test_server_with(
    mock_base: &str,
    dir: &TempDir,
    opts: TestServerOpts<'_>,
) -> (String, Arc<OAuthManager>) {
    let token_file = dir.path().join("tokens.json");
    if let Some(tok) = opts.preauth_token {
        std::fs::write(&token_file, serde_json::to_string_pretty(&tok).unwrap()).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let cfg = Config {
        app_id: "cli_test_app".to_string(),
        app_secret: "secret_test_123".to_string(),
        listen: addr.to_string(),
        redirect_uri: opts
            .redirect_uri
            .unwrap_or_else(|| format!("http://{addr}/oauth/callback")),
        token_file,
        api_base_url: mock_base.to_string(),
        accounts_base_url: mock_base.to_string(),
        scopes: "drive:drive drive:export:readonly offline_access".to_string(),
        dav_prefix: opts.dav_prefix.to_string(),
        root_folder_token: opts.root_folder_token,
        dav_username: opts.basic_auth.map(|(u, _)| u.to_string()),
        dav_password: opts.basic_auth.map(|(_, p)| p.to_string()),
        cache_ttl_secs: opts.cache_ttl_secs,
        probe_file_size: true,
        doc_mode: opts.doc_mode,
        read_only: opts.read_only,
        autoindex: true,
        max_qps: 5,
        max_upload_mb: opts.max_upload_mb,
    };

    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let oauth = Arc::new(OAuthManager::new(cfg.clone(), http.clone()));
    oauth.load_from_disk().await.unwrap();

    let state = build_app_state(cfg, Arc::clone(&oauth), http);
    let router = build_router(state);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    (format!("http://{addr}"), oauth)
}

pub fn sample_token() -> TokenSet {
    TokenSet {
        access_token: "u-access-initial".to_string(),
        refresh_token: Some("ur-refresh-initial".to_string()),
        expires_at: now_secs() + 7200,
        refresh_expires_at: Some(now_secs() + 604800),
        scope: Some("drive:drive drive:export:readonly offline_access".to_string()),
        token_type: "Bearer".to_string(),
        obtained_at: now_secs(),
        user_name: Some("Alice".to_string()),
        open_id: None,
    }
}

