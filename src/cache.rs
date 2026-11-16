use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::sync::{Mutex, RwLock};

use crate::config::DocMode;
use crate::feishu::{DriveFileItem, FolderMeta};
use crate::oauth::unix_now;

const MAX_SIZE_CACHE_ENTRIES: usize = 10_000;
const MAX_EXPORT_CACHE_BYTES: usize = 64 * 1024 * 1024;
const EXPORT_CACHE_TTL: Duration = Duration::from_secs(120);
const FOLDER_META_CACHE_TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Folder,
    BinaryFile,
    ExportDoc { export_ext: &'static str },
    MarkdownDoc,
    UrlShortcut { url: String },
    VirtualZeroFile,
}

#[derive(Debug, Clone)]
pub struct VirtualNode {
    /// WebDAV visible filename (sanitized and deduplicated within parent folder)
    pub dav_name: String,
    /// Original name in Feishu Drive
    pub raw_name: String,
    /// Token used to read/list/copy/move the target resource
    pub token: String,
    /// Resource type used for copy/move/export (`file`, `folder`, `docx`, `sheet`, etc.)
    pub feishu_type: String,
    /// Token used when deleting this entry in the parent folder (differs for shortcuts)
    pub entry_token: String,
    /// Type used when deleting this entry in the parent folder (`shortcut` or `feishu_type`)
    pub entry_type: String,
    pub parent_token: String,
    pub kind: NodeKind,
    /// True for externally configured root mount folders (`--extra-folder-tokens`)
    pub is_mount: bool,
    pub created_time: Option<u64>,
    pub modified_time: Option<u64>,
    pub known_size: Option<u64>,
}

impl VirtualNode {
    pub fn is_dir(&self) -> bool {
        matches!(self.kind, NodeKind::Folder)
    }

    /// Returns true for read-only Feishu cloud documents/links exposed as exported or `.url` files.
    /// Overwriting these via WebDAV PUT must be rejected to avoid deleting the user's cloud document.
    pub fn is_immutable_cloud_doc(&self) -> bool {
        matches!(
            self.kind,
            NodeKind::ExportDoc { .. } | NodeKind::UrlShortcut { .. }
        )
    }

    /// Returns true for any native Feishu cloud document/link (`export`, `markdown`, or `.url`),
    /// which must never be blindly replaced via WebDAV MOVE/COPY Destination overwrite.
    pub fn is_cloud_doc(&self) -> bool {
        matches!(
            self.kind,
            NodeKind::ExportDoc { .. } | NodeKind::MarkdownDoc | NodeKind::UrlShortcut { .. }
        )
    }

    pub fn modified_system_time(&self) -> SystemTime {
        unix_secs_to_system_time(self.modified_time.or(self.created_time))
    }

    pub fn created_system_time(&self) -> SystemTime {
        unix_secs_to_system_time(self.created_time.or(self.modified_time))
    }

    pub fn url_shortcut_bytes(&self) -> Option<Bytes> {
        match &self.kind {
            NodeKind::UrlShortcut { url } => {
                let body = format!("[InternetShortcut]\r\nURL={url}\r\n");
                Some(Bytes::from(body))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct CachedDir {
    nodes: Vec<VirtualNode>,
    fetched_at: Instant,
}

#[derive(Debug, Clone)]
struct CachedFolderMeta {
    meta: FolderMeta,
    fetched_at: Instant,
}

#[derive(Debug)]
struct CachedSize {
    size: u64,
    modified_time: Option<u64>,
    last_used_ms: AtomicU64,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct ExportCacheKey {
    token: String,
    modified_time: Option<u64>,
    ext: &'static str,
}

#[derive(Debug, Clone)]
pub struct CachedExport {
    pub data: Bytes,
    pub size: u64,
    pub unconvertible_reasons: Vec<String>,
    cached_at: Instant,
    last_used: Instant,
}

impl CachedExport {
    pub fn new(data: Bytes, size: u64, unconvertible_reasons: Vec<String>) -> Self {
        let now = Instant::now();
        Self {
            data,
            size,
            unconvertible_reasons,
            cached_at: now,
            last_used: now,
        }
    }
}

#[derive(Debug, Clone)]
pub struct VirtualZeroEntry {
    pub token: String,
    pub created_time: u64,
    pub modified_time: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderMountStatus {
    Mounted { name: String, token: String },
    Failed { error: String },
}

pub struct DriveCache {
    epoch: Instant,
    started_unix_secs: u64,
    dir_ttl: Duration,
    root_token: RwLock<Option<String>>,
    dirs: RwLock<HashMap<String, CachedDir>>,
    folder_metas: RwLock<HashMap<String, CachedFolderMeta>>,
    mount_statuses: RwLock<HashMap<String, FolderMountStatus>>,
    sizes: RwLock<HashMap<String, CachedSize>>,
    exports: RwLock<HashMap<ExportCacheKey, CachedExport>>,
    export_locks: Mutex<HashMap<ExportCacheKey, Arc<Mutex<()>>>>,
    virtual_zeros: RwLock<HashMap<(String, String), VirtualZeroEntry>>,
}

impl DriveCache {
    pub fn new(cache_ttl_secs: u64) -> Self {
        Self {
            epoch: Instant::now(),
            started_unix_secs: unix_now(),
            dir_ttl: Duration::from_secs(cache_ttl_secs),
            root_token: RwLock::new(None),
            dirs: RwLock::new(HashMap::new()),
            folder_metas: RwLock::new(HashMap::new()),
            mount_statuses: RwLock::new(HashMap::new()),
            sizes: RwLock::new(HashMap::new()),
            exports: RwLock::new(HashMap::new()),
            export_locks: Mutex::new(HashMap::new()),
            virtual_zeros: RwLock::new(HashMap::new()),
        }
    }

    pub fn started_unix_secs(&self) -> u64 {
        self.started_unix_secs
    }

    fn now_millis(&self) -> u64 {
        self.epoch.elapsed().as_millis().min(u64::MAX as u128) as u64
    }

    pub async fn get_root_token(&self) -> Option<String> {
        self.root_token.read().await.clone()
    }

    pub async fn set_root_token(&self, token: String) {
        *self.root_token.write().await = Some(token);
    }

    pub async fn get_dir(&self, folder_token: &str) -> Option<Vec<VirtualNode>> {
        if self.dir_ttl.is_zero() {
            return None;
        }
        let guard = self.dirs.read().await;
        let cached = guard.get(folder_token)?;
        if cached.fetched_at.elapsed() <= self.dir_ttl {
            Some(cached.nodes.clone())
        } else {
            None
        }
    }

    pub async fn put_dir(&self, folder_token: &str, nodes: Vec<VirtualNode>) {
        if self.dir_ttl.is_zero() {
            return;
        }
        let mut guard = self.dirs.write().await;
        guard.insert(
            folder_token.to_string(),
            CachedDir {
                nodes,
                fetched_at: Instant::now(),
            },
        );
    }

    pub async fn invalidate_dir(&self, folder_token: &str) {
        self.dirs.write().await.remove(folder_token);
    }

    pub async fn get_folder_meta(&self, folder_token: &str) -> Option<FolderMeta> {
        let ttl = self.dir_ttl.max(FOLDER_META_CACHE_TTL);
        let guard = self.folder_metas.read().await;
        let cached = guard.get(folder_token)?;
        if cached.fetched_at.elapsed() <= ttl {
            Some(cached.meta.clone())
        } else {
            None
        }
    }

    pub async fn put_folder_meta(&self, folder_token: &str, meta: FolderMeta) {
        self.folder_metas.write().await.insert(
            folder_token.to_string(),
            CachedFolderMeta {
                meta,
                fetched_at: Instant::now(),
            },
        );
    }

    pub async fn set_mount_status(&self, folder_token: &str, status: FolderMountStatus) {
        if self
            .mount_statuses
            .read()
            .await
            .get(folder_token)
            .is_some_and(|existing| existing == &status)
        {
            return;
        }
        self.mount_statuses
            .write()
            .await
            .insert(folder_token.to_string(), status);
    }

    pub async fn get_mount_status(&self, folder_token: &str) -> Option<FolderMountStatus> {
        self.mount_statuses.read().await.get(folder_token).cloned()
    }

    pub async fn clear_all(&self) {
        *self.root_token.write().await = None;
        self.dirs.write().await.clear();
        self.folder_metas.write().await.clear();
        self.mount_statuses.write().await.clear();
        self.sizes.write().await.clear();
        self.exports.write().await.clear();
        self.export_locks.lock().await.clear();
        self.virtual_zeros.write().await.clear();
    }

    pub async fn get_size(&self, token: &str, modified_time: Option<u64>) -> Option<u64> {
        let mut needs_mt_backfill = false;
        let result = {
            let guard = self.sizes.read().await;
            let entry = guard.get(token)?;
            if let (Some(cached_mt), Some(req_mt)) = (entry.modified_time, modified_time)
                && cached_mt != req_mt
            {
                return None;
            }
            if entry.modified_time.is_none() && modified_time.is_some() {
                needs_mt_backfill = true;
            }
            entry
                .last_used_ms
                .store(self.now_millis(), Ordering::Relaxed);
            entry.size
        };

        if needs_mt_backfill {
            let mut guard = self.sizes.write().await;
            if let Some(entry) = guard.get_mut(token)
                && entry.modified_time.is_none()
            {
                entry.modified_time = modified_time;
            }
        }
        Some(result)
    }

    pub async fn put_size(&self, token: &str, modified_time: Option<u64>, size: u64) {
        let now_ms = self.now_millis();
        let mut guard = self.sizes.write().await;
        if guard.len() >= MAX_SIZE_CACHE_ENTRIES && !guard.contains_key(token) {
            // Evict the oldest 20% of entries instead of flushing the entire size cache
            let evict_count = (MAX_SIZE_CACHE_ENTRIES / 5).max(1);
            let mut entries: Vec<(String, u64)> = guard
                .iter()
                .map(|(k, v)| (k.clone(), v.last_used_ms.load(Ordering::Relaxed)))
                .collect();
            entries.sort_by_key(|(_, ts)| *ts);
            for (k, _) in entries.into_iter().take(evict_count) {
                guard.remove(&k);
            }
        }
        guard.insert(
            token.to_string(),
            CachedSize {
                size,
                modified_time,
                last_used_ms: AtomicU64::new(now_ms),
            },
        );
    }

    pub async fn acquire_export_lock(
        &self,
        token: &str,
        modified_time: Option<u64>,
        ext: &'static str,
    ) -> Arc<Mutex<()>> {
        let key = ExportCacheKey {
            token: token.to_string(),
            modified_time,
            ext,
        };
        let mut guard = self.export_locks.lock().await;
        if guard.len() > 256 {
            guard.retain(|_, lock| Arc::strong_count(lock) > 1);
        }
        Arc::clone(guard.entry(key).or_insert_with(|| Arc::new(Mutex::new(()))))
    }

    pub async fn get_export(
        &self,
        token: &str,
        modified_time: Option<u64>,
        ext: &'static str,
    ) -> Option<(Bytes, u64)> {
        self.get_cached_export_entry(token, modified_time, ext)
            .await
            .map(|e| (e.data, e.size))
    }

    pub async fn get_markdown(
        &self,
        token: &str,
        modified_time: Option<u64>,
    ) -> Option<CachedExport> {
        self.get_cached_export_entry(token, modified_time, "md")
            .await
    }

    async fn get_cached_export_entry(
        &self,
        token: &str,
        modified_time: Option<u64>,
        ext: &'static str,
    ) -> Option<CachedExport> {
        let key = ExportCacheKey {
            token: token.to_string(),
            modified_time,
            ext,
        };
        let mut guard = self.exports.write().await;
        let entry = guard.get_mut(&key)?;
        if entry.cached_at.elapsed() <= EXPORT_CACHE_TTL {
            entry.last_used = Instant::now();
            Some(entry.clone())
        } else {
            guard.remove(&key);
            None
        }
    }

    pub async fn put_export(
        &self,
        token: &str,
        modified_time: Option<u64>,
        ext: &'static str,
        data: Bytes,
        size: u64,
    ) {
        self.put_cached_export_entry(token, modified_time, ext, data, size, Vec::new())
            .await;
    }

    pub async fn put_markdown(
        &self,
        token: &str,
        modified_time: Option<u64>,
        data: Bytes,
        size: u64,
        unconvertible_reasons: Vec<String>,
    ) {
        self.put_cached_export_entry(token, modified_time, "md", data, size, unconvertible_reasons)
            .await;
    }

    pub async fn invalidate_markdown(&self, token: &str) {
        self.exports
            .write()
            .await
            .retain(|k, _| !(k.token == token && k.ext == "md"));
    }

    async fn put_cached_export_entry(
        &self,
        token: &str,
        modified_time: Option<u64>,
        ext: &'static str,
        data: Bytes,
        size: u64,
        unconvertible_reasons: Vec<String>,
    ) {
        if data.len() > MAX_EXPORT_CACHE_BYTES / 2 {
            return;
        }
        let mut guard = self.exports.write().await;
        guard.retain(|_, v| v.cached_at.elapsed() <= EXPORT_CACHE_TTL);
        let mut current_bytes: usize = guard.values().map(|v| v.data.len()).sum();
        while current_bytes + data.len() > MAX_EXPORT_CACHE_BYTES && !guard.is_empty() {
            let Some(oldest_key) = guard
                .iter()
                .min_by_key(|(_, v)| v.last_used)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(removed) = guard.remove(&oldest_key) {
                current_bytes = current_bytes.saturating_sub(removed.data.len());
            }
        }
        let now = Instant::now();
        guard.insert(
            ExportCacheKey {
                token: token.to_string(),
                modified_time,
                ext,
            },
            CachedExport {
                data,
                size,
                unconvertible_reasons,
                cached_at: now,
                last_used: now,
            },
        );
    }

    pub async fn put_virtual_zero(&self, parent_token: &str, name: &str, entry: VirtualZeroEntry) {
        self.virtual_zeros
            .write()
            .await
            .insert((parent_token.to_string(), name.to_string()), entry);
    }

    pub async fn remove_virtual_zero(&self, parent_token: &str, name: &str) -> Option<VirtualZeroEntry> {
        self.virtual_zeros
            .write()
            .await
            .remove(&(parent_token.to_string(), name.to_string()))
    }

    pub async fn prune_virtual_zeros(&self, parent_token: &str, real_names: &HashSet<String>) {
        if real_names.is_empty() {
            return;
        }
        self.virtual_zeros
            .write()
            .await
            .retain(|(p, name), _| !(p == parent_token && real_names.contains(name)));
    }

    pub async fn list_virtual_zeros(&self, parent_token: &str) -> Vec<(String, VirtualZeroEntry)> {
        self.virtual_zeros
            .read()
            .await
            .iter()
            .filter(|((p, _), _)| p == parent_token)
            .map(|((_, name), entry)| (name.clone(), entry.clone()))
            .collect()
    }
}

struct PreparedItem {
    item: DriveFileItem,
    target_token: String,
    target_type: String,
    kind: NodeKind,
    base_name: String,
}

pub async fn build_virtual_nodes(
    parent_token: &str,
    items: Vec<DriveFileItem>,
    doc_mode: DocMode,
    cache: &DriveCache,
) -> Vec<VirtualNode> {
    let mut prepared = Vec::with_capacity(items.len());

    for item in items {
        let (target_token, target_type) = match &item.shortcut_info {
            Some(sc) => (sc.target_token.clone(), sc.target_type.clone()),
            None => (item.token.clone(), item.file_type.clone()),
        };

        let fallback_url = item
            .url
            .clone()
            .unwrap_or_else(|| format!("https://feishu.cn/{target_type}/{target_token}"));

        let (kind, base_name) = match target_type.as_str() {
            "folder" => (NodeKind::Folder, sanitize_segment(&item.name)),
            "file" => (NodeKind::BinaryFile, sanitize_segment(&item.name)),
            "docx" => match doc_mode {
                DocMode::Export => (
                    NodeKind::ExportDoc { export_ext: "docx" },
                    ensure_extension(&sanitize_segment(&item.name), "docx"),
                ),
                DocMode::Markdown => (
                    NodeKind::MarkdownDoc,
                    ensure_extension(&sanitize_segment(&item.name), "md"),
                ),
                DocMode::Url => (
                    NodeKind::UrlShortcut { url: fallback_url },
                    ensure_extension(&sanitize_segment(&item.name), "url"),
                ),
                DocMode::FilesOnly => continue,
            },
            "doc" => match doc_mode {
                DocMode::Export | DocMode::Markdown => (
                    NodeKind::ExportDoc { export_ext: "docx" },
                    ensure_extension(&sanitize_segment(&item.name), "docx"),
                ),
                DocMode::Url => (
                    NodeKind::UrlShortcut { url: fallback_url },
                    ensure_extension(&sanitize_segment(&item.name), "url"),
                ),
                DocMode::FilesOnly => continue,
            },
            "sheet" | "bitable" => match doc_mode {
                DocMode::Export | DocMode::Markdown => (
                    NodeKind::ExportDoc { export_ext: "xlsx" },
                    ensure_extension(&sanitize_segment(&item.name), "xlsx"),
                ),
                DocMode::Url => (
                    NodeKind::UrlShortcut { url: fallback_url },
                    ensure_extension(&sanitize_segment(&item.name), "url"),
                ),
                DocMode::FilesOnly => continue,
            },
            _ => match doc_mode {
                DocMode::Export | DocMode::Markdown | DocMode::Url => (
                    NodeKind::UrlShortcut { url: fallback_url },
                    ensure_extension(&sanitize_segment(&item.name), "url"),
                ),
                DocMode::FilesOnly => continue,
            },
        };

        prepared.push(PreparedItem {
            item,
            target_token,
            target_type,
            kind,
            base_name,
        });
    }

    // Sort deterministically so duplicate filenames never swap WebDAV names across refreshes
    prepared.sort_by(|a, b| {
        a.base_name
            .cmp(&b.base_name)
            .then_with(|| {
                a.item
                    .created_time
                    .unwrap_or(0)
                    .cmp(&b.item.created_time.unwrap_or(0))
            })
            .then_with(|| a.item.token.cmp(&b.item.token))
    });

    let mut nodes = Vec::with_capacity(prepared.len());
    let mut used_names: HashSet<String> = HashSet::new();

    for entry in prepared {
        let dav_name = dedup_name(&entry.base_name, &entry.item.token, &mut used_names);
        let known_size = match &entry.kind {
            NodeKind::Folder | NodeKind::VirtualZeroFile => Some(0),
            NodeKind::UrlShortcut { url } => {
                Some(format!("[InternetShortcut]\r\nURL={url}\r\n").len() as u64)
            }
            NodeKind::BinaryFile => {
                cache
                    .get_size(&entry.target_token, entry.item.modified_time)
                    .await
            }
            NodeKind::ExportDoc { export_ext } => cache
                .get_export(&entry.target_token, entry.item.modified_time, export_ext)
                .await
                .map(|(_, s)| s),
            NodeKind::MarkdownDoc => cache
                .get_markdown(&entry.target_token, entry.item.modified_time)
                .await
                .map(|e| e.size),
        };

        nodes.push(VirtualNode {
            dav_name,
            raw_name: entry.item.name,
            token: entry.target_token,
            feishu_type: entry.target_type,
            entry_token: entry.item.token,
            entry_type: entry.item.file_type,
            parent_token: parent_token.to_string(),
            kind: entry.kind,
            is_mount: false,
            created_time: entry.item.created_time,
            modified_time: entry.item.modified_time,
            known_size,
        });
    }

    // Prune obsolete virtual-zero placeholders once a real item occupies the same name
    cache.prune_virtual_zeros(parent_token, &used_names).await;

    // Merge virtual 0-byte fallback files that do not collide with any real item
    for (name, z) in cache.list_virtual_zeros(parent_token).await {
        if !used_names.contains(&name) {
            used_names.insert(name.clone());
            nodes.push(VirtualNode {
                dav_name: name.clone(),
                raw_name: name,
                token: z.token.clone(),
                feishu_type: "virtual_zero".to_string(),
                entry_token: z.token,
                entry_type: "virtual_zero".to_string(),
                parent_token: parent_token.to_string(),
                kind: NodeKind::VirtualZeroFile,
                is_mount: false,
                created_time: Some(z.created_time),
                modified_time: Some(z.modified_time),
                known_size: Some(0),
            });
        }
    }

    nodes
}

pub(crate) fn sanitize_segment(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            _ => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "untitled".to_string()
    } else {
        trimmed.to_string()
    }
}

fn ensure_extension(name: &str, ext: &str) -> String {
    let suffix = format!(".{ext}");
    if name.to_ascii_lowercase().ends_with(&suffix) {
        name.to_string()
    } else {
        format!("{name}{suffix}")
    }
}

pub(crate) fn dedup_name(base: &str, token: &str, used: &mut HashSet<String>) -> String {
    if used.insert(base.to_string()) {
        return base.to_string();
    }
    let short: String = token.chars().take(6).collect();
    let candidate = if let Some((stem, ext)) = base.rsplit_once('.')
        && !stem.is_empty()
    {
        format!("{stem} [{short}].{ext}")
    } else {
        format!("{base} [{short}]")
    };

    if used.insert(candidate.clone()) {
        return candidate;
    }

    for idx in 2..1000 {
        let numbered = format!("{candidate} ({idx})");
        if used.insert(numbered.clone()) {
            return numbered;
        }
    }
    format!("{base}_{token}")
}

fn unix_secs_to_system_time(ts: Option<u64>) -> SystemTime {
    match ts {
        Some(secs) => UNIX_EPOCH + Duration::from_secs(secs),
        None => UNIX_EPOCH,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_item(token: &str, name: &str, file_type: &str, created: u64) -> DriveFileItem {
        DriveFileItem {
            token: token.to_string(),
            name: name.to_string(),
            file_type: file_type.to_string(),
            parent_token: Some("root".to_string()),
            created_time: Some(created),
            modified_time: Some(created + 10),
            url: None,
            owner_id: None,
            shortcut_info: None,
        }
    }

    #[test]
    fn sanitize_segment_replaces_windows_and_posix_illegal_chars() {
        assert_eq!(sanitize_segment("a/b\\c:d*e?f\"g<h>i|j\0k"), "a_b_c_d_e_f_g_h_i_j_k");
        assert_eq!(sanitize_segment("  ..  "), "untitled");
        assert_eq!(sanitize_segment("   "), "untitled");
    }

    #[tokio::test]
    async fn dedup_name_is_deterministic_regardless_of_input_order() {
        let cache = DriveCache::new(30);
        let a = mk_item("tok_b", "report.txt", "file", 200);
        let b = mk_item("tok_a", "report.txt", "file", 100);

        let n1 = build_virtual_nodes("root", vec![a.clone(), b.clone()], DocMode::Export, &cache).await;
        let n2 = build_virtual_nodes("root", vec![b, a], DocMode::Export, &cache).await;

        assert_eq!(n1[0].token, "tok_a");
        assert_eq!(n1[0].dav_name, "report.txt");
        assert_eq!(n1[1].token, "tok_b");
        assert_eq!(n1[1].dav_name, "report [tok_b].txt");
        assert_eq!(n1[0].dav_name, n2[0].dav_name);
        assert_eq!(n1[1].dav_name, n2[1].dav_name);
    }

    #[tokio::test]
    async fn size_cache_matches_initial_none_modified_time_and_prunes_virtual_zero() {
        let cache = DriveCache::new(30);
        cache.put_size("tok_1", None, 42).await;
        assert_eq!(cache.get_size("tok_1", Some(1000)).await, Some(42));
        // Updated modified time misses cache
        assert_eq!(cache.get_size("tok_1", Some(1001)).await, None);

        cache
            .put_virtual_zero(
                "root",
                "note.txt",
                VirtualZeroEntry {
                    token: "vzero_1".to_string(),
                    created_time: 10,
                    modified_time: 10,
                },
            )
            .await;

        let with_real = build_virtual_nodes(
            "root",
            vec![mk_item("tok_real", "note.txt", "file", 50)],
            DocMode::Export,
            &cache,
        )
        .await;
        assert_eq!(with_real.len(), 1);
        assert_eq!(with_real[0].token, "tok_real");

        // Once the real file disappears, the obsolete virtual-zero entry stays pruned
        let empty = build_virtual_nodes("root", vec![], DocMode::Export, &cache).await;
        assert!(empty.is_empty());
    }
}
