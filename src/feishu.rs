use std::future::Future;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use std::time::Duration;

use adler2::Adler32;
use bytes::{Bytes, BytesMut};
use reqwest::header::{CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, RANGE};
use reqwest::{Client, Response, StatusCode, multipart};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tokio::time::Instant;
use tracing::{debug, trace, warn};

use crate::error::{AppError, AppResult};
use crate::oauth::OAuthManager;

pub const UPLOAD_ALL_MAX_BYTES: usize = 20 * 1024 * 1024;
const MAX_RETRY_ATTEMPTS: usize = 5;

#[derive(Debug)]
pub enum DownloadBody {
    Stream { resp: Response, skip: u64 },
    Buffered(Bytes),
}

impl DownloadBody {
    /// Streams the next chunk of data, transparently discarding any leading `skip` bytes
    /// (e.g. when an upstream server ignores a `Range` header and responds with `200 OK`).
    pub async fn next_chunk(&mut self) -> AppResult<Option<Bytes>> {
        match self {
            DownloadBody::Buffered(buf) => {
                if buf.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(std::mem::take(buf)))
                }
            }
            DownloadBody::Stream { resp, skip } => loop {
                match resp.chunk().await? {
                    None => return Ok(None),
                    Some(chunk) if chunk.is_empty() => continue,
                    Some(mut chunk) => {
                        if *skip > 0 {
                            let len = chunk.len() as u64;
                            if len <= *skip {
                                *skip -= len;
                                continue;
                            }
                            let _ = chunk.split_to(*skip as usize);
                            *skip = 0;
                        }
                        return Ok(Some(chunk));
                    }
                }
            },
        }
    }

    pub async fn into_bytes(mut self) -> AppResult<Bytes> {
        match self {
            DownloadBody::Buffered(bytes) => Ok(bytes),
            DownloadBody::Stream { .. } => {
                let mut out = BytesMut::new();
                while let Some(chunk) = self.next_chunk().await? {
                    out.extend_from_slice(&chunk);
                }
                Ok(out.freeze())
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum UploadSource {
    Memory(Bytes),
    File(Arc<std::path::PathBuf>, u64),
}

impl UploadSource {
    pub fn len(&self) -> u64 {
        match self {
            UploadSource::Memory(b) => b.len() as u64,
            UploadSource::File(_, len) => *len,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub async fn read_slice(&self, offset: u64, len: usize) -> AppResult<Bytes> {
        match self {
            UploadSource::Memory(b) => {
                let start = usize::try_from(offset).unwrap_or(b.len()).min(b.len());
                let end = start.saturating_add(len).min(b.len());
                Ok(b.slice(start..end))
            }
            UploadSource::File(path, _) => {
                let path = Arc::clone(path);
                tokio::task::spawn_blocking(move || {
                    let mut f = std::fs::File::open(path.as_ref())?;
                    f.seek(SeekFrom::Start(offset))?;
                    let mut buf = vec![0u8; len];
                    let mut total = 0;
                    while total < len {
                        let n = f.read(&mut buf[total..])?;
                        if n == 0 {
                            break;
                        }
                        total += n;
                    }
                    buf.truncate(total);
                    Ok::<_, AppError>(Bytes::from(buf))
                })
                .await
                .map_err(|e| AppError::Io(std::io::Error::other(e)))?
            }
        }
    }
}

/// Simple FIFO time-slot pacer to stay below Feishu Drive's 5 QPS limit.
struct RateLimiter {
    min_interval: Duration,
    next_slot: Mutex<Instant>,
}

impl RateLimiter {
    fn new(max_qps: u32) -> Self {
        let qps = max_qps.max(1);
        let min_interval = Duration::from_millis(1000 / qps as u64);
        Self {
            min_interval,
            next_slot: Mutex::new(Instant::now()),
        }
    }

    async fn acquire(&self) {
        let scheduled = {
            let mut guard = self.next_slot.lock().await;
            let now = Instant::now();
            let slot = (*guard).max(now);
            *guard = slot + self.min_interval;
            slot
        };
        let now = Instant::now();
        if scheduled > now {
            tokio::time::sleep_until(scheduled).await;
        }
    }
}

fn default_none<T>() -> Option<T> {
    None
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
struct FeishuEnvelope<T> {
    code: i64,
    #[serde(default)]
    msg: String,
    #[serde(default = "default_none")]
    data: Option<T>,
}

#[derive(Debug, Deserialize)]
pub struct RootFolderMeta {
    pub token: String,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub user_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FolderMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub token: String,
    #[serde(
        default,
        alias = "createTime",
        alias = "create_time",
        deserialize_with = "deserialize_opt_u64_lenient"
    )]
    pub created_time: Option<u64>,
    #[serde(
        default,
        alias = "modifiedTime",
        alias = "editTime",
        alias = "latest_modify_time",
        deserialize_with = "deserialize_opt_u64_lenient"
    )]
    pub modified_time: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShortcutInfo {
    pub target_type: String,
    pub target_token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DriveFileItem {
    #[serde(alias = "file_token")]
    pub token: String,
    pub name: String,
    #[serde(rename = "type")]
    pub file_type: String,
    #[serde(default)]
    pub parent_token: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_u64_lenient")]
    pub created_time: Option<u64>,
    #[serde(default, deserialize_with = "deserialize_opt_u64_lenient")]
    pub modified_time: Option<u64>,
    #[serde(default)]
    pub owner_id: Option<String>,
    #[serde(default)]
    pub shortcut_info: Option<ShortcutInfo>,
}

#[derive(Debug, Deserialize)]
struct ListFilesData {
    #[serde(default)]
    files: Vec<DriveFileItem>,
    #[serde(default)]
    has_more: bool,
    #[serde(default, alias = "page_token")]
    next_page_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateFolderData {
    token: String,
    #[serde(default)]
    #[allow(dead_code)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UploadTokenData {
    file_token: String,
}

#[derive(Debug, Deserialize)]
struct UploadPrepareData {
    upload_id: String,
    block_size: usize,
    block_num: usize,
}

#[derive(Debug, Deserialize)]
struct TaskData {
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TaskCheckData {
    #[serde(default)]
    status: serde_json::Value,
    #[serde(default)]
    failure_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CopyFileData {
    file: CopiedFileInfo,
}

#[derive(Debug, Deserialize)]
pub struct CopiedFileInfo {
    #[serde(alias = "file_token")]
    pub token: String,
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "type")]
    pub file_type: String,
    #[serde(default)]
    pub parent_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateExportTaskData {
    ticket: String,
}

#[derive(Debug, Deserialize)]
struct ExportTaskStatusEnvelope {
    result: ExportTaskResult,
}

#[derive(Debug, Deserialize)]
struct ExportTaskResult {
    #[serde(default)]
    file_token: Option<String>,
    #[serde(default)]
    file_size: Option<u64>,
    #[serde(default)]
    job_error_msg: Option<String>,
    job_status: i64,
}

pub struct ExportedDoc {
    pub data: Bytes,
    pub size: u64,
}

pub struct FeishuClient {
    oauth: Arc<OAuthManager>,
    http: Client,
    limiter: RateLimiter,
}

impl FeishuClient {
    pub fn new(oauth: Arc<OAuthManager>, http: Client) -> Self {
        let max_qps = oauth.config().max_qps;
        Self {
            oauth,
            http,
            limiter: RateLimiter::new(max_qps),
        }
    }

    pub fn oauth(&self) -> &Arc<OAuthManager> {
        &self.oauth
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{}", self.oauth.config().api_base_url, path)
    }

    /// Executes a Feishu request with QPS pacing, proactive/reactive OAuth token refresh,
    /// and exponential backoff on rate limits (`1061045` / HTTP 429) or transient 5xx responses.
    async fn execute_with_retry<T, F, H, Fut>(
        &self,
        mut build_req: F,
        mut handle_resp: H,
    ) -> AppResult<T>
    where
        F: FnMut(&Client, &str) -> reqwest::RequestBuilder,
        H: FnMut(Response) -> Fut,
        Fut: Future<Output = AppResult<RetryStep<T>>>,
    {
        let mut refreshed_once = false;

        for attempt in 0..MAX_RETRY_ATTEMPTS {
            self.limiter.acquire().await;
            let token = self.oauth.get_valid_access_token().await?;
            let resp = build_req(&self.http, &token).send().await?;
            let status = resp.status();

            if status == StatusCode::UNAUTHORIZED && !refreshed_once {
                refreshed_once = true;
                debug!("received HTTP 401 from Feishu API, refreshing user_access_token");
                self.oauth.force_refresh_if_matches(&token).await?;
                continue;
            }
            if status == StatusCode::TOO_MANY_REQUESTS {
                if attempt + 1 < MAX_RETRY_ATTEMPTS {
                    warn!("Feishu HTTP 429 rate limit (attempt {})", attempt + 1);
                    backoff_sleep(attempt).await;
                    continue;
                }
                return Err(AppError::RateLimited);
            }

            match handle_resp(resp).await? {
                RetryStep::Done(val) => return Ok(val),
                RetryStep::Retry(err) => {
                    if err.is_token_invalid() && !refreshed_once {
                        refreshed_once = true;
                        debug!("Feishu token error ({err}), forcing token refresh");
                        self.oauth.force_refresh_if_matches(&token).await?;
                        continue;
                    }
                    if (err.is_rate_limited() || status.is_server_error())
                        && attempt + 1 < MAX_RETRY_ATTEMPTS
                    {
                        warn!(
                            "Feishu transient/rate-limit error (attempt {}): {err}",
                            attempt + 1
                        );
                        backoff_sleep(attempt).await;
                        continue;
                    }
                    return Err(err);
                }
            }
        }

        Err(AppError::RateLimited)
    }

    async fn call_json<T, F>(&self, build_req: F) -> AppResult<T>
    where
        T: for<'de> Deserialize<'de>,
        F: FnMut(&Client, &str) -> reqwest::RequestBuilder,
    {
        match self.call_json_opt::<T, F>(build_req).await? {
            Some(data) => Ok(data),
            // Some endpoints return code=0 without `data` (e.g. unit responses); try null deserialization
            None => serde_json::from_value(serde_json::Value::Null).map_err(Into::into),
        }
    }

    /// Executes a Feishu API call where `data` in the response may be omitted (`Option<T>`).
    async fn call_json_opt<T, F>(&self, build_req: F) -> AppResult<Option<T>>
    where
        T: for<'de> Deserialize<'de>,
        F: FnMut(&Client, &str) -> reqwest::RequestBuilder,
    {
        self.execute_with_retry(build_req, |resp| async move {
            let status = resp.status();
            let body_bytes = resp.bytes().await?;
            let envelope: FeishuEnvelope<T> = serde_json::from_slice(&body_bytes).map_err(|e| {
                let preview = String::from_utf8_lossy(&body_bytes);
                AppError::FeishuApi {
                    code: -1,
                    msg: format!(
                        "unexpected response (http={}): {e}; body={preview}",
                        status.as_u16()
                    ),
                    http_status: status.as_u16(),
                }
            })?;

            if envelope.code == 0 {
                Ok(RetryStep::Done(envelope.data))
            } else {
                Ok(RetryStep::Retry(AppError::from_feishu_code(
                    envelope.code,
                    envelope.msg,
                    status.as_u16(),
                )))
            }
        })
        .await
    }

    pub async fn get_root_folder_meta(&self) -> AppResult<RootFolderMeta> {
        let url = self.api_url("/open-apis/drive/explorer/v2/root_folder/meta");
        self.call_json(|client, token| client.get(&url).bearer_auth(token))
            .await
    }

    pub async fn get_folder_meta(&self, folder_token: &str) -> AppResult<FolderMeta> {
        let trimmed = folder_token.trim();
        if trimmed.is_empty()
            || trimmed.contains('/')
            || trimmed.contains('?')
            || trimmed.contains('#')
            || trimmed.chars().any(char::is_whitespace)
        {
            return Err(AppError::InvalidInput(format!(
                "invalid folder_token '{folder_token}'"
            )));
        }
        let url = self.api_url(&format!(
            "/open-apis/drive/explorer/v2/folder/{trimmed}/meta"
        ));
        let mut meta: FolderMeta = self
            .call_json(|client, token| client.get(&url).bearer_auth(token))
            .await?;
        if meta.token.trim().is_empty() {
            meta.token = trimmed.to_string();
        }
        if meta.name.trim().is_empty() {
            meta.name = meta.token.clone();
        }
        Ok(meta)
    }

    pub async fn list_files(&self, folder_token: &str) -> AppResult<Vec<DriveFileItem>> {
        let url = self.api_url("/open-apis/drive/v1/files");
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;

        loop {
            let token_param = page_token.clone();
            let page: ListFilesData = self
                .call_json(|client, bearer| {
                    let mut req = client
                        .get(&url)
                        .bearer_auth(bearer)
                        .query(&[("page_size", "200")]);
                    if !folder_token.is_empty() {
                        req = req.query(&[("folder_token", folder_token)]);
                    }
                    if let Some(ref pt) = token_param {
                        req = req.query(&[("page_token", pt.as_str())]);
                    }
                    req
                })
                .await?;

            all.extend(page.files);
            if !page.has_more {
                break;
            }
            match page.next_page_token {
                Some(next) if !next.is_empty() => page_token = Some(next),
                _ => break,
            }
        }

        Ok(all)
    }

    /// Probes the byte length of a binary file using a 1-byte Range request.
    pub async fn probe_file_size(&self, file_token: &str) -> AppResult<u64> {
        let url = self.api_url(&format!("/open-apis/drive/v1/files/{file_token}/download"));

        self.execute_with_retry(
            |client, token| {
                client
                    .get(&url)
                    .bearer_auth(token)
                    .header(RANGE, "bytes=0-0")
            },
            |resp| async move {
                let status = resp.status();
                if status == StatusCode::PARTIAL_CONTENT
                    || status == StatusCode::RANGE_NOT_SATISFIABLE
                {
                    if let Some(cr) = resp
                        .headers()
                        .get(CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        && let Some(total) = parse_content_range_total(cr)
                    {
                        return Ok(RetryStep::Done(total));
                    }
                    if status == StatusCode::RANGE_NOT_SATISFIABLE {
                        return Ok(RetryStep::Done(0));
                    }
                }

                if status == StatusCode::OK {
                    let header_len = resp
                        .headers()
                        .get(CONTENT_LENGTH)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|s| s.parse::<u64>().ok())
                        .or_else(|| resp.content_length());
                    return match inspect_ok_binary_response(resp).await? {
                        CheckedOkResponse::Stream(mut resp) => {
                            if let Some(len) = header_len {
                                return Ok(RetryStep::Done(len));
                            }
                            let mut total = 0u64;
                            while let Some(chunk) = resp.chunk().await? {
                                total = total.saturating_add(chunk.len() as u64);
                            }
                            Ok(RetryStep::Done(total))
                        }
                        CheckedOkResponse::Bytes(bytes) => Ok(RetryStep::Done(bytes.len() as u64)),
                        CheckedOkResponse::FeishuErr(err) => Ok(RetryStep::Retry(err)),
                    };
                }

                Ok(RetryStep::Retry(
                    parse_binary_endpoint_error(status, resp).await,
                ))
            },
        )
        .await
    }

    /// Opens a streaming HTTP download for `file_token` starting at `offset`.
    pub async fn open_download_stream(
        &self,
        file_token: &str,
        offset: u64,
    ) -> AppResult<(DownloadBody, Option<u64>)> {
        let url = self.api_url(&format!("/open-apis/drive/v1/files/{file_token}/download"));

        self.execute_with_retry(
            |client, token| {
                let mut req = client.get(&url).bearer_auth(token);
                if offset > 0 {
                    req = req.header(RANGE, format!("bytes={offset}-"));
                }
                req
            },
            |resp| async move {
                let status = resp.status();
                if status == StatusCode::PARTIAL_CONTENT {
                    let total_size = resp
                        .headers()
                        .get(CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_content_range_total);
                    return Ok(RetryStep::Done((
                        DownloadBody::Stream { resp, skip: 0 },
                        total_size,
                    )));
                }

                if status == StatusCode::OK {
                    let total_size = resp
                        .headers()
                        .get(CONTENT_RANGE)
                        .and_then(|v| v.to_str().ok())
                        .and_then(parse_content_range_total)
                        .or_else(|| {
                            resp.headers()
                                .get(CONTENT_LENGTH)
                                .and_then(|v| v.to_str().ok())
                                .and_then(|s| s.parse::<u64>().ok())
                                .or_else(|| resp.content_length())
                        });
                    return match inspect_ok_binary_response(resp).await? {
                        CheckedOkResponse::Stream(resp) => Ok(RetryStep::Done((
                            DownloadBody::Stream { resp, skip: offset },
                            total_size,
                        ))),
                        CheckedOkResponse::Bytes(bytes) => {
                            let sliced = if offset == 0 {
                                bytes
                            } else if (offset as usize) < bytes.len() {
                                bytes.slice(offset as usize..)
                            } else {
                                Bytes::new()
                            };
                            Ok(RetryStep::Done((
                                DownloadBody::Buffered(sliced),
                                total_size,
                            )))
                        }
                        CheckedOkResponse::FeishuErr(err) => Ok(RetryStep::Retry(err)),
                    };
                }

                Ok(RetryStep::Retry(
                    parse_binary_endpoint_error(status, resp).await,
                ))
            },
        )
        .await
    }

    pub async fn create_folder(&self, parent_token: &str, name: &str) -> AppResult<String> {
        #[derive(Serialize)]
        struct Body<'a> {
            name: &'a str,
            folder_token: &'a str,
        }

        let url = self.api_url("/open-apis/drive/v1/files/create_folder");
        let data: CreateFolderData = self
            .call_json(|client, token| {
                client.post(&url).bearer_auth(token).json(&Body {
                    name,
                    folder_token: parent_token,
                })
            })
            .await?;
        Ok(data.token)
    }

    pub async fn upload_source(
        &self,
        parent_token: &str,
        file_name: &str,
        source: &UploadSource,
    ) -> AppResult<String> {
        let size = source.len();
        if size <= UPLOAD_ALL_MAX_BYTES as u64 {
            let data = source.read_slice(0, size as usize).await?;
            self.upload_all(parent_token, file_name, data).await
        } else {
            self.upload_multipart(parent_token, file_name, source).await
        }
    }

    pub async fn upload_all(
        &self,
        parent_token: &str,
        file_name: &str,
        data: Bytes,
    ) -> AppResult<String> {
        let url = self.api_url("/open-apis/drive/v1/files/upload_all");
        let size = data.len();
        let checksum = adler32_decimal(&data);

        let res: UploadTokenData = self
            .call_json(|client, token| {
                let file_part = multipart::Part::stream_with_length(data.clone(), size as u64)
                    .file_name(file_name.to_string())
                    .mime_str("application/octet-stream")
                    .expect("valid mime");

                let form = multipart::Form::new()
                    .text("file_name", file_name.to_string())
                    .text("parent_type", "explorer".to_string())
                    .text("parent_node", parent_token.to_string())
                    .text("size", size.to_string())
                    .text("checksum", checksum.clone())
                    .part("file", file_part);

                client.post(&url).bearer_auth(token).multipart(form)
            })
            .await?;

        Ok(res.file_token)
    }

    async fn upload_multipart(
        &self,
        parent_token: &str,
        file_name: &str,
        source: &UploadSource,
    ) -> AppResult<String> {
        #[derive(Serialize)]
        struct PrepareBody<'a> {
            file_name: &'a str,
            parent_type: &'static str,
            parent_node: &'a str,
            size: u64,
        }

        #[derive(Serialize)]
        struct FinishBody<'a> {
            upload_id: &'a str,
            block_num: usize,
        }

        let total_size = source.len();
        let prepare_url = self.api_url("/open-apis/drive/v1/files/upload_prepare");
        let prep: UploadPrepareData = self
            .call_json(|client, token| {
                client.post(&prepare_url).bearer_auth(token).json(&PrepareBody {
                    file_name,
                    parent_type: "explorer",
                    parent_node: parent_token,
                    size: total_size,
                })
            })
            .await?;

        let block_size = prep.block_size.max(1);
        let upload_part_url = self.api_url("/open-apis/drive/v1/files/upload_part");

        for seq in 0..prep.block_num {
            let offset = (seq as u64) * (block_size as u64);
            if offset >= total_size {
                break;
            }
            let remaining = (total_size - offset) as usize;
            let chunk_len = remaining.min(block_size);
            let chunk = source.read_slice(offset, chunk_len).await?;
            let checksum = adler32_decimal(&chunk);
            let upload_id = prep.upload_id.clone();

            trace!(
                seq,
                block_num = prep.block_num,
                chunk_len,
                "uploading Feishu multipart chunk"
            );

            let _: Option<serde_json::Value> = self
                .call_json_opt(|client, token| {
                    let part = multipart::Part::stream_with_length(chunk.clone(), chunk_len as u64)
                        .file_name(file_name.to_string())
                        .mime_str("application/octet-stream")
                        .expect("valid mime");

                    let form = multipart::Form::new()
                        .text("upload_id", upload_id.clone())
                        .text("seq", seq.to_string())
                        .text("size", chunk_len.to_string())
                        .text("checksum", checksum.clone())
                        .part("file", part);

                    client
                        .post(&upload_part_url)
                        .bearer_auth(token)
                        .multipart(form)
                })
                .await?;
        }

        let finish_url = self.api_url("/open-apis/drive/v1/files/upload_finish");
        let finished: UploadTokenData = self
            .call_json(|client, token| {
                client.post(&finish_url).bearer_auth(token).json(&FinishBody {
                    upload_id: &prep.upload_id,
                    block_num: prep.block_num,
                })
            })
            .await?;

        Ok(finished.file_token)
    }

    pub async fn delete_file(&self, file_token: &str, file_type: &str) -> AppResult<()> {
        let url = self.api_url(&format!("/open-apis/drive/v1/files/{file_token}"));
        let data: Option<TaskData> = self
            .call_json_opt(|client, token| {
                client
                    .delete(&url)
                    .bearer_auth(token)
                    .query(&[("type", file_type)])
            })
            .await?;

        if let Some(TaskData {
            task_id: Some(task_id),
        }) = data
            && !task_id.is_empty()
        {
            self.wait_for_task(&task_id).await?;
        }
        Ok(())
    }

    pub async fn move_file(
        &self,
        file_token: &str,
        file_type: &str,
        target_folder_token: &str,
    ) -> AppResult<()> {
        #[derive(Serialize)]
        struct Body<'a> {
            #[serde(rename = "type")]
            file_type: &'a str,
            folder_token: &'a str,
        }

        let url = self.api_url(&format!("/open-apis/drive/v1/files/{file_token}/move"));
        let data: Option<TaskData> = self
            .call_json_opt(|client, token| {
                client.post(&url).bearer_auth(token).json(&Body {
                    file_type,
                    folder_token: target_folder_token,
                })
            })
            .await?;

        if let Some(TaskData {
            task_id: Some(task_id),
        }) = data
            && !task_id.is_empty()
        {
            self.wait_for_task(&task_id).await?;
        }
        Ok(())
    }

    pub async fn copy_file(
        &self,
        file_token: &str,
        file_type: &str,
        target_folder_token: &str,
        new_name: &str,
    ) -> AppResult<CopiedFileInfo> {
        #[derive(Serialize)]
        struct Body<'a> {
            name: &'a str,
            #[serde(rename = "type")]
            file_type: &'a str,
            folder_token: &'a str,
        }

        let url = self.api_url(&format!("/open-apis/drive/v1/files/{file_token}/copy"));
        let data: CopyFileData = self
            .call_json(|client, token| {
                client.post(&url).bearer_auth(token).json(&Body {
                    name: new_name,
                    file_type,
                    folder_token: target_folder_token,
                })
            })
            .await?;
        Ok(data.file)
    }

    async fn wait_for_task(&self, task_id: &str) -> AppResult<()> {
        let url = self.api_url("/open-apis/drive/v1/files/task_check");
        for attempt in 0..60 {
            let status: TaskCheckData = self
                .call_json(|client, token| {
                    client
                        .get(&url)
                        .bearer_auth(token)
                        .query(&[("task_id", task_id)])
                })
                .await?;

            match parse_task_status(&status.status) {
                TaskState::Success => return Ok(()),
                TaskState::Failed => {
                    return Err(AppError::FeishuApi {
                        code: -1,
                        msg: status
                            .failure_reason
                            .unwrap_or_else(|| "Feishu async task failed".to_string()),
                        http_status: 500,
                    });
                }
                TaskState::Pending => {
                    let delay_ms = if attempt < 5 { 400 } else { 1000 };
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
        Err(AppError::FeishuApi {
            code: -1,
            msg: format!("timed out waiting for Feishu async task {task_id}"),
            http_status: 504,
        })
    }

    /// Exports a Feishu online document (`doc`, `docx`, `sheet`, `bitable`) to a local binary format.
    pub async fn export_doc(
        &self,
        doc_token: &str,
        doc_type: &str,
        file_extension: &str,
    ) -> AppResult<ExportedDoc> {
        #[derive(Serialize)]
        struct CreateExportBody<'a> {
            file_extension: &'a str,
            token: &'a str,
            #[serde(rename = "type")]
            doc_type: &'a str,
        }

        let create_url = self.api_url("/open-apis/drive/v1/export_tasks");
        let task: CreateExportTaskData = self
            .call_json(|client, token| {
                client
                    .post(&create_url)
                    .bearer_auth(token)
                    .json(&CreateExportBody {
                        file_extension,
                        token: doc_token,
                        doc_type,
                    })
            })
            .await?;

        let poll_url = self.api_url(&format!("/open-apis/drive/v1/export_tasks/{}", task.ticket));
        let mut export_file_token = None;
        let mut export_file_size = None;

        for attempt in 0..45 {
            let status: ExportTaskStatusEnvelope = self
                .call_json(|client, token| {
                    client
                        .get(&poll_url)
                        .bearer_auth(token)
                        .query(&[("token", doc_token)])
                })
                .await?;

            let res = status.result;
            match res.job_status {
                0 => {
                    export_file_token = res.file_token;
                    export_file_size = res.file_size;
                    break;
                }
                1 | 2 => {
                    let delay_ms = if attempt < 5 { 500 } else { 1200 };
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                other => {
                    return Err(AppError::FeishuApi {
                        code: other,
                        msg: res
                            .job_error_msg
                            .unwrap_or_else(|| format!("export job failed with status {other}")),
                        http_status: 500,
                    });
                }
            }
        }

        let file_token = export_file_token.ok_or_else(|| AppError::FeishuApi {
            code: -1,
            msg: "Feishu export task timed out".to_string(),
            http_status: 504,
        })?;

        let download_url = self.api_url(&format!(
            "/open-apis/drive/v1/export_tasks/file/{file_token}/download"
        ));

        self.execute_with_retry(
            |client, token| client.get(&download_url).bearer_auth(token),
            |resp| async move {
                let status = resp.status();
                if status.is_success() {
                    return match inspect_ok_binary_response(resp).await? {
                        CheckedOkResponse::Stream(resp) => {
                            let bytes = resp.bytes().await?;
                            let size = export_file_size.unwrap_or(bytes.len() as u64);
                            Ok(RetryStep::Done(ExportedDoc { data: bytes, size }))
                        }
                        CheckedOkResponse::Bytes(bytes) => {
                            let size = export_file_size.unwrap_or(bytes.len() as u64);
                            Ok(RetryStep::Done(ExportedDoc { data: bytes, size }))
                        }
                        CheckedOkResponse::FeishuErr(err) => Ok(RetryStep::Retry(err)),
                    };
                }
                Ok(RetryStep::Retry(
                    parse_binary_endpoint_error(status, resp).await,
                ))
            },
        )
        .await
    }
}

enum RetryStep<T> {
    Done(T),
    Retry(AppError),
}

enum CheckedOkResponse {
    Stream(Response),
    Bytes(Bytes),
    FeishuErr(AppError),
}

fn maybe_json_error_headers(resp: &Response) -> bool {
    let is_json_ct = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            let ct = ct.trim().to_ascii_lowercase();
            ct.starts_with("application/json") || ct.starts_with("text/json")
        });
    let has_attachment = resp
        .headers()
        .get(CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|cd| cd.to_ascii_lowercase().contains("attachment"));
    let small_len = resp.content_length().is_none_or(|n| n <= 8192);
    is_json_ct && !has_attachment && small_len
}

async fn inspect_ok_binary_response(resp: Response) -> AppResult<CheckedOkResponse> {
    if !maybe_json_error_headers(&resp) {
        return Ok(CheckedOkResponse::Stream(resp));
    }
    let bytes = resp.bytes().await?;
    if bytes.len() <= 8192
        && let Ok(env) = serde_json::from_slice::<FeishuEnvelope<serde_json::Value>>(&bytes)
        && env.code != 0
    {
        return Ok(CheckedOkResponse::FeishuErr(AppError::from_feishu_code(
            env.code,
            env.msg,
            StatusCode::OK.as_u16(),
        )));
    }
    Ok(CheckedOkResponse::Bytes(bytes))
}

enum TaskState {
    Success,
    Pending,
    Failed,
}

fn parse_task_status(val: &serde_json::Value) -> TaskState {
    match val {
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(0) => TaskState::Success,
            Some(1) | Some(2) => TaskState::Pending,
            _ => TaskState::Failed,
        },
        serde_json::Value::String(s) => match s.to_ascii_lowercase().as_str() {
            "success" | "done" | "finished" | "0" => TaskState::Success,
            "fail" | "failed" | "error" => TaskState::Failed,
            _ => TaskState::Pending,
        },
        serde_json::Value::Null => TaskState::Pending,
        _ => TaskState::Pending,
    }
}

async fn parse_binary_endpoint_error(status: StatusCode, resp: Response) -> AppError {
    let bytes = resp.bytes().await.unwrap_or_default();
    if let Ok(env) = serde_json::from_slice::<FeishuEnvelope<serde_json::Value>>(&bytes) {
        return AppError::from_feishu_code(env.code, env.msg, status.as_u16());
    }
    let body = String::from_utf8_lossy(&bytes).to_string();
    AppError::FeishuApi {
        code: -1,
        msg: if body.is_empty() {
            format!("HTTP {}", status.as_u16())
        } else {
            body
        },
        http_status: status.as_u16(),
    }
}

async fn backoff_sleep(attempt: usize) {
    let ms = 400u64.saturating_mul(1u64 << attempt.min(4));
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

pub fn adler32_decimal(data: &[u8]) -> String {
    let mut adler = Adler32::new();
    adler.write_slice(data);
    adler.checksum().to_string()
}

pub fn parse_content_range_total(header: &str) -> Option<u64> {
    let (_, total_part) = header.trim().split_once('/')?;
    if total_part == "*" {
        return None;
    }
    total_part.trim().parse::<u64>().ok()
}

fn deserialize_opt_u64_lenient<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    let val: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    Ok(match val {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Number(n)) => n.as_u64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().ok(),
        Some(_) => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adler32_matches_known_value() {
        assert_eq!(adler32_decimal(b"Wikipedia"), "300286872");
    }

    #[test]
    fn parse_content_range_extracts_total() {
        assert_eq!(parse_content_range_total("bytes 0-0/4096"), Some(4096));
        assert_eq!(parse_content_range_total("bytes */0"), Some(0));
        assert_eq!(parse_content_range_total("bytes 0-10/*"), None);
    }
}
