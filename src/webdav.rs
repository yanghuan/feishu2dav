use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::{Buf, Bytes};
use dav_server::davpath::DavPath;
use dav_server::fs::{
    DavDirEntry, DavFile, DavFileSystem, DavMetaData, FsError, FsFuture, FsResult, FsStream,
    OpenOptions, ReadDirMeta,
};
use futures_util::stream::{self, StreamExt};
use tempfile::NamedTempFile;
use tracing::{debug, trace, warn};

use crate::cache::{
    DriveCache, FolderMountStatus, NodeKind, VirtualNode, VirtualZeroEntry, build_virtual_nodes,
    dedup_name, sanitize_segment,
};
use crate::config::DocMode;
use crate::error::AppError;
use crate::feishu::{DownloadBody, FeishuClient, FolderMeta, UploadSource};
use crate::oauth::unix_now;

const MAX_MEMORY_WRITE_BYTES: usize = 4 * 1024 * 1024;
const MAX_AUTO_PROBE_PER_DIR: usize = 50;
const CONCURRENT_PROBES_LIMIT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathKind {
    Missing,
    MountPoint,
    Folder,
    ImmutableCloudDoc,
    MutableFile,
}

#[derive(Debug, Clone)]
pub struct DavFsOptions {
    pub root_folder_token: Option<String>,
    pub extra_folder_tokens: Vec<String>,
    pub doc_mode: DocMode,
    pub probe_file_size: bool,
    pub read_only: bool,
    pub max_upload_bytes: Option<u64>,
}

#[derive(Clone)]
pub struct FeishuDavFs {
    client: Arc<FeishuClient>,
    cache: Arc<DriveCache>,
    root_folder_token: Option<String>,
    extra_folder_tokens: Vec<String>,
    doc_mode: DocMode,
    probe_file_size: bool,
    read_only: bool,
    max_upload_bytes: Option<u64>,
}

impl FeishuDavFs {
    pub fn new(client: Arc<FeishuClient>, cache: Arc<DriveCache>, opts: DavFsOptions) -> Self {
        Self {
            client,
            cache,
            root_folder_token: opts.root_folder_token,
            extra_folder_tokens: opts.extra_folder_tokens,
            doc_mode: opts.doc_mode,
            probe_file_size: opts.probe_file_size,
            read_only: opts.read_only,
            max_upload_bytes: opts.max_upload_bytes,
        }
    }

    async fn root_token(&self) -> FsResult<String> {
        if let Some(ref configured) = self.root_folder_token {
            return Ok(configured.clone());
        }
        if let Some(tok) = self.cache.get_root_token().await {
            return Ok(tok);
        }
        let meta = self
            .client
            .get_root_folder_meta()
            .await
            .map_err(FsError::from)?;
        self.cache.set_root_token(meta.token.clone()).await;
        Ok(meta.token)
    }

    async fn list_folder_nodes(&self, folder_token: &str) -> FsResult<Vec<VirtualNode>> {
        if let Some(cached) = self.cache.get_dir(folder_token).await {
            return Ok(cached);
        }
        let items = self
            .client
            .list_files(folder_token)
            .await
            .map_err(FsError::from)?;
        let nodes = build_virtual_nodes(folder_token, items, self.doc_mode, &self.cache).await;
        self.cache.put_dir(folder_token, nodes.clone()).await;
        Ok(nodes)
    }

    async fn get_folder_meta_cached(&self, folder_token: &str) -> FsResult<Option<FolderMeta>> {
        if let Some(cached) = self.cache.get_folder_meta(folder_token).await {
            return Ok(Some(cached));
        }
        match self.client.get_folder_meta(folder_token).await {
            Ok(meta) => {
                self.cache.put_folder_meta(folder_token, meta.clone()).await;
                self.cache
                    .set_mount_status(
                        folder_token,
                        FolderMountStatus::Mounted {
                            name: meta.name.clone(),
                            token: meta.token.clone(),
                        },
                    )
                    .await;
                Ok(Some(meta))
            }
            Err(err)
                if matches!(err, AppError::TokenUnavailable(_))
                    || err.is_token_invalid()
                    || err.is_rate_limited() =>
            {
                Err(FsError::from(&err))
            }
            Err(err) => {
                warn!(
                    folder_token = %folder_token,
                    "skipping extra mounted folder: {err}"
                );
                self.cache
                    .set_mount_status(
                        folder_token,
                        FolderMountStatus::Failed {
                            error: err.to_string(),
                        },
                    )
                    .await;
                Ok(None)
            }
        }
    }

    async fn merge_extra_root_mounts(
        &self,
        base_root_token: &str,
        mut base_nodes: Vec<VirtualNode>,
    ) -> FsResult<Vec<VirtualNode>> {
        if self.extra_folder_tokens.is_empty() {
            return Ok(base_nodes);
        }

        let mut used_names: HashSet<String> =
            base_nodes.iter().map(|n| n.dav_name.clone()).collect();
        let mut seen_folder_tokens: HashSet<String> = base_nodes
            .iter()
            .filter(|n| n.is_dir())
            .map(|n| n.token.clone())
            .collect();
        seen_folder_tokens.insert(base_root_token.to_string());

        let mut candidates = Vec::new();
        for tok in &self.extra_folder_tokens {
            if seen_folder_tokens.contains(tok.as_str()) {
                let name = if tok == base_root_token {
                    "/ (当前根目录)".to_string()
                } else {
                    base_nodes
                        .iter()
                        .find(|n| n.is_dir() && n.token == *tok)
                        .map(|n| n.dav_name.clone())
                        .unwrap_or_else(|| tok.clone())
                };
                self.cache
                    .set_mount_status(
                        tok,
                        FolderMountStatus::Mounted {
                            name,
                            token: tok.clone(),
                        },
                    )
                    .await;
            } else {
                candidates.push(tok.clone());
            }
        }

        if candidates.is_empty() {
            return Ok(base_nodes);
        }

        let resolved: Vec<(String, FsResult<Option<FolderMeta>>)> = stream::iter(candidates)
            .map(|tok| async move {
                let res = self.get_folder_meta_cached(&tok).await;
                (tok, res)
            })
            .buffered(CONCURRENT_PROBES_LIMIT)
            .collect()
            .await;

        let fallback_ts = self.cache.started_unix_secs();

        for (configured_tok, res) in resolved {
            let Some(meta) = res? else {
                continue;
            };
            let target_token = if meta.token.trim().is_empty() {
                configured_tok
            } else {
                meta.token
            };
            if !seen_folder_tokens.insert(target_token.clone()) {
                continue;
            }
            let raw_name = if meta.name.trim().is_empty() {
                target_token.clone()
            } else {
                meta.name
            };
            let base_name = sanitize_segment(&raw_name);
            let dav_name = dedup_name(&base_name, &target_token, &mut used_names);
            let created_time = meta.created_time.or(meta.modified_time).or(Some(fallback_ts));
            let modified_time = meta.modified_time.or(meta.created_time).or(Some(fallback_ts));
            base_nodes.push(VirtualNode {
                dav_name,
                raw_name,
                token: target_token.clone(),
                feishu_type: "folder".to_string(),
                entry_token: target_token,
                entry_type: "folder".to_string(),
                parent_token: base_root_token.to_string(),
                kind: NodeKind::Folder,
                is_mount: true,
                created_time,
                modified_time,
                known_size: Some(0),
            });
        }

        Ok(base_nodes)
    }

    async fn list_nodes_at(
        &self,
        folder_token: &str,
        include_extra_mounts: bool,
    ) -> FsResult<Vec<VirtualNode>> {
        let nodes = self.list_folder_nodes(folder_token).await?;
        if include_extra_mounts {
            self.merge_extra_root_mounts(folder_token, nodes).await
        } else {
            Ok(nodes)
        }
    }

    async fn descend_folders(
        &self,
        base_root_token: &str,
        folder_segments: &[String],
    ) -> FsResult<String> {
        let mut current_folder = base_root_token.to_string();
        let mut visited = HashSet::new();
        visited.insert(current_folder.clone());

        for (idx, seg) in folder_segments.iter().enumerate() {
            let nodes = self.list_nodes_at(&current_folder, idx == 0).await?;
            let Some(node) = nodes.into_iter().find(|n| n.dav_name == *seg) else {
                return Err(FsError::NotFound);
            };
            if !node.is_dir() {
                return Err(FsError::Forbidden);
            }
            if !visited.insert(node.token.clone()) {
                return Err(FsError::LoopDetected);
            }
            current_folder = node.token;
        }

        Ok(current_folder)
    }

    pub(crate) async fn inspect_path(&self, path: &DavPath) -> FsResult<PathKind> {
        match self.resolve_path(path).await {
            Ok(node) => {
                if node.is_mount {
                    Ok(PathKind::MountPoint)
                } else if node.is_dir() {
                    Ok(PathKind::Folder)
                } else if node.is_immutable_cloud_doc() {
                    Ok(PathKind::ImmutableCloudDoc)
                } else {
                    Ok(PathKind::MutableFile)
                }
            }
            Err(FsError::NotFound) => Ok(PathKind::Missing),
            Err(e) => Err(e),
        }
    }

    async fn resolve_path(&self, path: &DavPath) -> FsResult<VirtualNode> {
        let segments = path_segments(path)?;
        let root_token = self.root_token().await?;

        if segments.is_empty() {
            let fallback_ts = self.cache.started_unix_secs();
            return Ok(VirtualNode {
                dav_name: String::new(),
                raw_name: String::new(),
                token: root_token.clone(),
                feishu_type: "folder".to_string(),
                entry_token: root_token,
                entry_type: "folder".to_string(),
                parent_token: String::new(),
                kind: NodeKind::Folder,
                is_mount: false,
                created_time: Some(fallback_ts),
                modified_time: Some(fallback_ts),
                known_size: Some(0),
            });
        }

        let parent_segments = &segments[..segments.len() - 1];
        let target_name = &segments[segments.len() - 1];
        let parent_folder = self.descend_folders(&root_token, parent_segments).await?;
        let nodes = self
            .list_nodes_at(&parent_folder, parent_segments.is_empty())
            .await?;
        nodes
            .into_iter()
            .find(|n| n.dav_name == *target_name)
            .ok_or(FsError::NotFound)
    }

    async fn resolve_parent(&self, path: &DavPath) -> FsResult<(String, String)> {
        let segments = path_segments(path)?;
        if segments.is_empty() {
            return Err(FsError::Forbidden);
        }
        let root_token = self.root_token().await?;
        let parent_folder = self
            .descend_folders(&root_token, &segments[..segments.len() - 1])
            .await?;
        Ok((parent_folder, segments[segments.len() - 1].clone()))
    }

    async fn ensure_node_metadata(
        &self,
        node: &mut VirtualNode,
        probe_if_missing: bool,
    ) -> FsResult<FeishuMetaData> {
        if node.known_size.is_none()
            && probe_if_missing
            && matches!(node.kind, NodeKind::BinaryFile)
        {
            if let Some(cached) = self.cache.get_size(&node.token, node.modified_time).await {
                node.known_size = Some(cached);
            } else {
                let sz = self
                    .client
                    .probe_file_size(&node.token)
                    .await
                    .map_err(FsError::from)?;
                self.cache
                    .put_size(&node.token, node.modified_time, sz)
                    .await;
                node.known_size = Some(sz);
            }
        }
        Ok(FeishuMetaData::from_node(node))
    }

    async fn export_or_cached_doc(
        &self,
        node: &VirtualNode,
        export_ext: &'static str,
    ) -> FsResult<(Bytes, u64)> {
        if let Some(cached) = self
            .cache
            .get_export(&node.token, node.modified_time, export_ext)
            .await
        {
            return Ok(cached);
        }

        let lock = self
            .cache
            .acquire_export_lock(&node.token, node.modified_time, export_ext)
            .await;
        let _guard = lock.lock().await;

        if let Some(cached) = self
            .cache
            .get_export(&node.token, node.modified_time, export_ext)
            .await
        {
            return Ok(cached);
        }

        let exported = self
            .client
            .export_doc(&node.token, &node.feishu_type, export_ext)
            .await
            .map_err(FsError::from)?;
        self.cache
            .put_export(
                &node.token,
                node.modified_time,
                export_ext,
                exported.data.clone(),
                exported.size,
            )
            .await;
        Ok((exported.data, exported.size))
    }

    async fn delete_node(&self, node: &VirtualNode) -> FsResult<()> {
        match node.kind {
            NodeKind::VirtualZeroFile => {
                self.cache
                    .remove_virtual_zero(&node.parent_token, &node.dav_name)
                    .await;
            }
            _ => {
                self.client
                    .delete_file(&node.entry_token, &node.entry_type)
                    .await
                    .map_err(FsError::from)?;
            }
        }
        self.cache.invalidate_dir(&node.parent_token).await;
        if node.is_dir() {
            self.cache.invalidate_dir(&node.token).await;
        }
        Ok(())
    }
}

impl DavFileSystem for FeishuDavFs {
    fn open<'a>(
        &'a self,
        path: &'a DavPath,
        options: OpenOptions,
    ) -> FsFuture<'a, Box<dyn DavFile>> {
        Box::pin(async move {
            trace!(?path, ?options, "FeishuDavFs::open");

            let is_write = options.write || options.append || options.truncate || options.create_new;
            if is_write {
                if self.read_only {
                    return Err(FsError::Forbidden);
                }
                let (parent_token, file_name) = self.resolve_parent(path).await?;
                let existing = match self.resolve_path(path).await {
                    Ok(n) => Some(n),
                    Err(FsError::NotFound) => None,
                    Err(e) => return Err(e),
                };

                if options.create_new && existing.is_some() {
                    return Err(FsError::Exists);
                }
                if !options.create && existing.is_none() {
                    return Err(FsError::NotFound);
                }
                if let Some(ref node) = existing
                    && (node.is_dir() || node.is_immutable_cloud_doc())
                {
                    return Err(FsError::Forbidden);
                }

                let mut spool = WriteSpool::new(self.max_upload_bytes);
                let mut cursor = 0u64;

                // Preserve existing bytes for partial PUT (Content-Range), PATCH, or append.
                // Quota (max_upload_bytes) applies to the resulting total file size inside WriteSpool.
                if let Some(ref node) = existing
                    && !options.truncate
                {
                    let existing_len = spool_existing_node(&self.client, node, &mut spool).await?;
                    if options.append {
                        cursor = existing_len;
                    }
                }

                let modified = existing
                    .as_ref()
                    .map(|n| n.modified_system_time())
                    .unwrap_or_else(SystemTime::now);
                let created = existing
                    .as_ref()
                    .map(|n| n.created_system_time())
                    .unwrap_or(modified);

                let file = FeishuWriteFile {
                    client: Arc::clone(&self.client),
                    cache: Arc::clone(&self.cache),
                    parent_token,
                    file_name,
                    existing,
                    spool,
                    pos: cursor,
                    dirty: options.create_new || options.truncate,
                    flushed: false,
                    created,
                    modified,
                };
                return Ok(Box::new(file) as Box<dyn DavFile>);
            }

            // Read mode
            let mut node = self.resolve_path(path).await?;
            if node.is_dir() {
                return Err(FsError::Forbidden);
            }

            match &node.kind {
                NodeKind::VirtualZeroFile => {
                    let meta = FeishuMetaData::from_node(&node);
                    Ok(Box::new(MemoryReadFile::new(Bytes::new(), meta)) as Box<dyn DavFile>)
                }
                NodeKind::UrlShortcut { .. } => {
                    let bytes = node.url_shortcut_bytes().unwrap_or_default();
                    node.known_size = Some(bytes.len() as u64);
                    let meta = FeishuMetaData::from_node(&node);
                    Ok(Box::new(MemoryReadFile::new(bytes, meta)) as Box<dyn DavFile>)
                }
                NodeKind::ExportDoc { export_ext } => {
                    let (bytes, size) = self.export_or_cached_doc(&node, export_ext).await?;
                    node.known_size = Some(size);
                    let meta = FeishuMetaData::from_node(&node);
                    Ok(Box::new(MemoryReadFile::new(bytes, meta)) as Box<dyn DavFile>)
                }
                NodeKind::BinaryFile => {
                    // On read open, strictly require exact size so GET never serves 200 OK with 0 bytes
                    let meta = self.ensure_node_metadata(&mut node, true).await?;
                    Ok(Box::new(FeishuReadFile::new(
                        Arc::clone(&self.client),
                        node,
                        meta,
                    )) as Box<dyn DavFile>)
                }
                NodeKind::Folder => Err(FsError::Forbidden),
            }
        })
    }

    fn read_dir<'a>(
        &'a self,
        path: &'a DavPath,
        _meta: ReadDirMeta,
    ) -> FsFuture<'a, FsStream<Box<dyn DavDirEntry>>> {
        Box::pin(async move {
            trace!(?path, "FeishuDavFs::read_dir");
            let segments = path_segments(path)?;
            let folder = self.resolve_path(path).await?;
            if !folder.is_dir() {
                return Err(FsError::Forbidden);
            }

            let mut nodes = self.list_folder_nodes(&folder.token).await?;
            let mut probed_any = false;
            let mut probe_targets = Vec::new();

            for (idx, node) in nodes.iter_mut().enumerate() {
                if node.known_size.is_none() && matches!(node.kind, NodeKind::BinaryFile) {
                    if let Some(cached) = self.cache.get_size(&node.token, node.modified_time).await {
                        node.known_size = Some(cached);
                        probed_any = true;
                    } else if self.probe_file_size && probe_targets.len() < MAX_AUTO_PROBE_PER_DIR {
                        probe_targets.push((idx, node.token.clone(), node.modified_time));
                    }
                }
            }

            if !probe_targets.is_empty() {
                let probe_results: Vec<_> = stream::iter(probe_targets)
                    .map(|(idx, token, modified_time)| async move {
                        let res = self.client.probe_file_size(&token).await;
                        (idx, token, modified_time, res)
                    })
                    .buffer_unordered(CONCURRENT_PROBES_LIMIT)
                    .collect()
                    .await;

                for (idx, token, modified_time, res) in probe_results {
                    match res {
                        Ok(sz) => {
                            self.cache.put_size(&token, modified_time, sz).await;
                            nodes[idx].known_size = Some(sz);
                            probed_any = true;
                        }
                        Err(err) => {
                            warn!(token = %token, "failed to probe file size during PROPFIND: {err}");
                        }
                    }
                }
            }

            if probed_any {
                self.cache.put_dir(&folder.token, nodes.clone()).await;
            }

            if segments.is_empty() {
                nodes = self.merge_extra_root_mounts(&folder.token, nodes).await?;
            }

            let entries: Vec<FsResult<Box<dyn DavDirEntry>>> = nodes
                .into_iter()
                .map(|node| {
                    let meta = FeishuMetaData::from_node(&node);
                    Ok(Box::new(FeishuDirEntry {
                        name: node.dav_name.into_bytes(),
                        meta,
                    }) as Box<dyn DavDirEntry>)
                })
                .collect();

            Ok(Box::pin(stream::iter(entries)) as FsStream<Box<dyn DavDirEntry>>)
        })
    }

    fn metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        Box::pin(async move {
            trace!(?path, "FeishuDavFs::metadata");
            let mut node = self.resolve_path(path).await?;
            let meta = self
                .ensure_node_metadata(&mut node, self.probe_file_size)
                .await?;
            Ok(Box::new(meta) as Box<dyn DavMetaData>)
        })
    }

    fn symlink_metadata<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, Box<dyn DavMetaData>> {
        self.metadata(path)
    }

    fn create_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            trace!(?path, "FeishuDavFs::create_dir");
            if self.read_only {
                return Err(FsError::Forbidden);
            }
            let (parent_token, name) = self.resolve_parent(path).await?;
            match self.resolve_path(path).await {
                Ok(_) => return Err(FsError::Exists),
                Err(FsError::NotFound) => {}
                Err(e) => return Err(e),
            }

            self.client
                .create_folder(&parent_token, &name)
                .await
                .map_err(FsError::from)?;
            self.cache.invalidate_dir(&parent_token).await;
            Ok(())
        })
    }

    fn remove_dir<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            trace!(?path, "FeishuDavFs::remove_dir");
            if self.read_only {
                return Err(FsError::Forbidden);
            }
            let segments = path_segments(path)?;
            if segments.is_empty() {
                return Err(FsError::Forbidden);
            }
            let node = self.resolve_path(path).await?;
            if !node.is_dir() || node.is_mount {
                return Err(FsError::Forbidden);
            }

            let children = self.list_folder_nodes(&node.token).await?;
            if !children.is_empty() {
                // RFC4918: dav-server maps FsError::Exists -> 409 Conflict when deleting non-empty dir
                return Err(FsError::Exists);
            }

            self.delete_node(&node).await
        })
    }

    fn remove_file<'a>(&'a self, path: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            trace!(?path, "FeishuDavFs::remove_file");
            if self.read_only {
                return Err(FsError::Forbidden);
            }
            let node = self.resolve_path(path).await?;
            if node.is_dir() {
                return Err(FsError::Forbidden);
            }
            self.delete_node(&node).await
        })
    }

    fn rename<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            trace!(?from, ?to, "FeishuDavFs::rename");
            if self.read_only {
                return Err(FsError::Forbidden);
            }
            if from == to {
                return Ok(());
            }

            let from_node = self.resolve_path(from).await?;
            if from_node.is_mount {
                return Err(FsError::Forbidden);
            }
            let (dest_parent_token, dest_name) = self.resolve_parent(to).await?;

            let dest_existing = match self.resolve_path(to).await {
                Ok(n) => {
                    if n.is_dir() || from_node.is_dir() != n.is_dir() || n.is_immutable_cloud_doc() {
                        return Err(FsError::Forbidden);
                    }
                    Some(n)
                }
                Err(FsError::NotFound) => None,
                Err(e) => return Err(e),
            };

            match from_node.kind {
                NodeKind::VirtualZeroFile => {
                    if let Some(ref existing) = dest_existing {
                        self.delete_node(existing).await?;
                    }
                    if let Some(entry) = self
                        .cache
                        .remove_virtual_zero(&from_node.parent_token, &from_node.dav_name)
                        .await
                    {
                        self.cache
                            .put_virtual_zero(&dest_parent_token, &dest_name, entry)
                            .await;
                    }
                    self.cache.invalidate_dir(&from_node.parent_token).await;
                    self.cache.invalidate_dir(&dest_parent_token).await;
                    Ok(())
                }
                NodeKind::Folder => {
                    if from_node.dav_name == dest_name {
                        // Moving folder to another parent without renaming
                        self.client
                            .move_file(&from_node.token, "folder", &dest_parent_token)
                            .await
                            .map_err(FsError::from)?;
                    } else {
                        // Feishu Drive has no native folder-rename endpoint; create new folder,
                        // move direct children over, then remove the empty old folder.
                        let new_folder_token = self
                            .client
                            .create_folder(&dest_parent_token, &dest_name)
                            .await
                            .map_err(FsError::from)?;

                        let raw_children = self
                            .client
                            .list_files(&from_node.token)
                            .await
                            .map_err(FsError::from)?;

                        for child in raw_children {
                            self.client
                                .move_file(&child.token, &child.file_type, &new_folder_token)
                                .await
                                .map_err(FsError::from)?;
                        }

                        for (zname, zentry) in self.cache.list_virtual_zeros(&from_node.token).await {
                            self.cache.remove_virtual_zero(&from_node.token, &zname).await;
                            self.cache
                                .put_virtual_zero(&new_folder_token, &zname, zentry)
                                .await;
                        }

                        self.client
                            .delete_file(&from_node.entry_token, &from_node.entry_type)
                            .await
                            .map_err(FsError::from)?;
                    }

                    self.cache.invalidate_dir(&from_node.token).await;
                    self.cache.invalidate_dir(&from_node.parent_token).await;
                    self.cache.invalidate_dir(&dest_parent_token).await;
                    Ok(())
                }
                _ => {
                    // File / Cloud doc
                    if from_node.dav_name == dest_name {
                        // Pure cross-folder move
                        if let Some(ref existing) = dest_existing {
                            self.delete_node(existing).await?;
                        }
                        self.client
                            .move_file(&from_node.token, &from_node.feishu_type, &dest_parent_token)
                            .await
                            .map_err(FsError::from)?;
                    } else {
                        // Rename (same folder or cross-folder): copy with new name then delete source
                        let target_name = feishu_target_name(&from_node, &dest_name);
                        let copied = self
                            .client
                            .copy_file(
                                &from_node.token,
                                &from_node.feishu_type,
                                &dest_parent_token,
                                &target_name,
                            )
                            .await
                            .map_err(FsError::from)?;

                        if let Some(sz) = from_node.known_size {
                            self.cache.put_size(&copied.token, None, sz).await;
                        }
                        if let Some(ref existing) = dest_existing {
                            self.delete_node(existing).await?;
                        }
                        self.client
                            .delete_file(&from_node.entry_token, &from_node.entry_type)
                            .await
                            .map_err(FsError::from)?;
                    }

                    self.cache.invalidate_dir(&from_node.parent_token).await;
                    self.cache.invalidate_dir(&dest_parent_token).await;
                    Ok(())
                }
            }
        })
    }

    fn copy<'a>(&'a self, from: &'a DavPath, to: &'a DavPath) -> FsFuture<'a, ()> {
        Box::pin(async move {
            trace!(?from, ?to, "FeishuDavFs::copy");
            if self.read_only {
                return Err(FsError::Forbidden);
            }
            if from == to {
                return Ok(());
            }

            let from_node = self.resolve_path(from).await?;
            if from_node.is_dir() {
                return Err(FsError::Forbidden);
            }
            let (dest_parent_token, dest_name) = self.resolve_parent(to).await?;

            let dest_existing = match self.resolve_path(to).await {
                Ok(n) => {
                    if n.is_dir() || n.is_immutable_cloud_doc() {
                        return Err(FsError::Forbidden);
                    }
                    Some(n)
                }
                Err(FsError::NotFound) => None,
                Err(e) => return Err(e),
            };

            match from_node.kind {
                NodeKind::VirtualZeroFile => {
                    if let Some(ref existing) = dest_existing {
                        self.delete_node(existing).await?;
                    }
                    let now = unix_now();
                    self.cache
                        .put_virtual_zero(
                            &dest_parent_token,
                            &dest_name,
                            VirtualZeroEntry {
                                token: format!("vzero_{now}"),
                                created_time: now,
                                modified_time: now,
                            },
                        )
                        .await;
                    self.cache.invalidate_dir(&dest_parent_token).await;
                    Ok(())
                }
                _ => {
                    let target_name = feishu_target_name(&from_node, &dest_name);
                    let copied = self
                        .client
                        .copy_file(
                            &from_node.token,
                            &from_node.feishu_type,
                            &dest_parent_token,
                            &target_name,
                        )
                        .await
                        .map_err(FsError::from)?;

                    if let Some(sz) = from_node.known_size {
                        self.cache.put_size(&copied.token, None, sz).await;
                    }
                    if let Some(ref existing) = dest_existing {
                        self.delete_node(existing).await?;
                    }
                    self.cache.invalidate_dir(&dest_parent_token).await;
                    Ok(())
                }
            }
        })
    }
}

fn feishu_target_name(from_node: &VirtualNode, dest_dav_name: &str) -> String {
    match &from_node.kind {
        NodeKind::ExportDoc { export_ext } => {
            let suffix = format!(".{export_ext}");
            if dest_dav_name.to_ascii_lowercase().ends_with(&suffix)
                && !from_node.raw_name.to_ascii_lowercase().ends_with(&suffix)
            {
                dest_dav_name[..dest_dav_name.len() - suffix.len()].to_string()
            } else {
                dest_dav_name.to_string()
            }
        }
        _ => dest_dav_name.to_string(),
    }
}

fn path_segments(path: &DavPath) -> FsResult<Vec<String>> {
    let raw = path.as_bytes();
    let mut segs = Vec::new();
    for part in raw.split(|&b| b == b'/') {
        if part.is_empty() {
            continue;
        }
        let s = std::str::from_utf8(part).map_err(|_| FsError::Forbidden)?;
        segs.push(s.to_string());
    }
    Ok(segs)
}

#[derive(Debug, Clone)]
pub struct FeishuMetaData {
    len: u64,
    modified: SystemTime,
    created: SystemTime,
    is_dir: bool,
    etag: Option<String>,
}

impl FeishuMetaData {
    fn from_node(node: &VirtualNode) -> Self {
        let len = node.known_size.unwrap_or(0);
        let modified = node.modified_system_time();
        let created = node.created_system_time();
        let etag = if node.token.is_empty() {
            None
        } else {
            let mod_secs = modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            Some(format!("{}-{mod_secs}-{len}", node.token))
        };
        Self {
            len,
            modified,
            created,
            is_dir: node.is_dir(),
            etag,
        }
    }
}

impl DavMetaData for FeishuMetaData {
    fn len(&self) -> u64 {
        self.len
    }

    fn modified(&self) -> FsResult<SystemTime> {
        Ok(self.modified)
    }

    fn created(&self) -> FsResult<SystemTime> {
        Ok(self.created)
    }

    fn is_dir(&self) -> bool {
        self.is_dir
    }

    fn is_symlink(&self) -> bool {
        false
    }

    fn etag(&self) -> Option<String> {
        self.etag.clone()
    }
}

struct FeishuDirEntry {
    name: Vec<u8>,
    meta: FeishuMetaData,
}

impl DavDirEntry for FeishuDirEntry {
    fn name(&self) -> Vec<u8> {
        self.name.clone()
    }

    fn metadata(&'_ self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let m = self.meta.clone();
        Box::pin(async move { Ok(Box::new(m) as Box<dyn DavMetaData>) })
    }
}

#[derive(Debug)]
struct MemoryReadFile {
    data: Bytes,
    pos: usize,
    meta: FeishuMetaData,
}

impl MemoryReadFile {
    fn new(data: Bytes, meta: FeishuMetaData) -> Self {
        Self { data, pos: 0, meta }
    }
}

impl DavFile for MemoryReadFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }

    fn write_buf(&'_ mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }

    fn write_bytes(&'_ mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            if self.pos >= self.data.len() {
                return Ok(Bytes::new());
            }
            let end = self.pos.saturating_add(count).min(self.data.len());
            let slice = self.data.slice(self.pos..end);
            self.pos = end;
            Ok(slice)
        })
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let len = self.data.len() as i128;
            let next = compute_seek_pos(self.pos as i128, len, pos)?;
            self.pos = next as usize;
            Ok(next)
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

struct FeishuReadFile {
    client: Arc<FeishuClient>,
    node: VirtualNode,
    meta: FeishuMetaData,
    pos: u64,
    stream: Option<DownloadBody>,
    buffer: Bytes,
}

impl std::fmt::Debug for FeishuReadFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuReadFile")
            .field("token", &self.node.token)
            .field("pos", &self.pos)
            .field("len", &self.meta.len)
            .finish()
    }
}

impl FeishuReadFile {
    fn new(client: Arc<FeishuClient>, node: VirtualNode, meta: FeishuMetaData) -> Self {
        Self {
            client,
            node,
            meta,
            pos: 0,
            stream: None,
            buffer: Bytes::new(),
        }
    }
}

impl DavFile for FeishuReadFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = self.meta.clone();
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }

    fn write_buf(&'_ mut self, _buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }

    fn write_bytes(&'_ mut self, _buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async { Err(FsError::Forbidden) })
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            if count == 0 || self.pos >= self.meta.len {
                return Ok(Bytes::new());
            }

            if !self.buffer.is_empty() {
                let take = count.min(self.buffer.len());
                let out = self.buffer.split_to(take);
                self.pos += out.len() as u64;
                return Ok(out);
            }

            if self.stream.is_none() {
                let (body, _) = self
                    .client
                    .open_download_stream(&self.node.token, self.pos)
                    .await
                    .map_err(FsError::from)?;
                self.stream = Some(body);
            }

            loop {
                let Some(body) = self.stream.as_mut() else {
                    return Ok(Bytes::new());
                };
                match body.next_chunk().await {
                    Ok(Some(chunk)) if chunk.is_empty() => continue,
                    Ok(Some(mut chunk)) => {
                        if chunk.len() > count {
                            let out = chunk.split_to(count);
                            self.buffer = chunk;
                            self.pos += out.len() as u64;
                            return Ok(out);
                        }
                        self.pos += chunk.len() as u64;
                        return Ok(chunk);
                    }
                    Ok(None) => {
                        self.stream = None;
                        return Ok(Bytes::new());
                    }
                    Err(err) => {
                        warn!(token = %self.node.token, "download stream read error: {err}");
                        self.stream = None;
                        return Err(FsError::from(err));
                    }
                }
            }
        })
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let target = compute_seek_pos(self.pos as i128, self.meta.len as i128, pos)?;
            if target == self.pos {
                return Ok(target);
            }

            // Fast forward within currently buffered chunk
            if target > self.pos {
                let delta = target - self.pos;
                if delta <= self.buffer.len() as u64 {
                    let _ = self.buffer.split_to(delta as usize);
                    self.pos = target;
                    return Ok(target);
                }
            }

            self.stream = None;
            self.buffer = Bytes::new();
            self.pos = target;
            Ok(target)
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Debug)]
enum WriteSpool {
    Memory {
        buf: Vec<u8>,
        max_bytes: Option<u64>,
    },
    File {
        file: Arc<std::sync::Mutex<NamedTempFile>>,
        path: Arc<std::path::PathBuf>,
        len: u64,
        max_bytes: Option<u64>,
    },
}

impl WriteSpool {
    fn new(max_bytes: Option<u64>) -> Self {
        Self::Memory {
            buf: Vec::new(),
            max_bytes,
        }
    }

    fn len(&self) -> u64 {
        match self {
            WriteSpool::Memory { buf, .. } => buf.len() as u64,
            WriteSpool::File { len, .. } => *len,
        }
    }

    async fn write_at(&mut self, offset: u64, data: Bytes) -> FsResult<()> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or(FsError::TooLarge)?;
        let max_bytes = match self {
            WriteSpool::Memory { max_bytes, .. } | WriteSpool::File { max_bytes, .. } => *max_bytes,
        };
        if let Some(limit) = max_bytes
            && end > limit
        {
            return Err(FsError::TooLarge);
        }

        if let WriteSpool::Memory { buf, max_bytes } = self {
            if end <= MAX_MEMORY_WRITE_BYTES as u64 {
                let end_usize = end as usize;
                if buf.len() < end_usize {
                    buf.resize(end_usize, 0);
                }
                buf[offset as usize..end_usize].copy_from_slice(&data);
                return Ok(());
            }

            // Spill memory buffer + incoming chunk to a temporary file on the blocking pool
            let initial = std::mem::take(buf);
            let limit = *max_bytes;
            let new_len = (initial.len() as u64).max(end);
            let (file, path) = tokio::task::spawn_blocking(move || -> FsResult<_> {
                let mut temp = NamedTempFile::new().map_err(|_| FsError::GeneralFailure)?;
                temp.write_all(&initial)
                    .map_err(|_| FsError::GeneralFailure)?;
                let f = temp.as_file_mut();
                f.seek(SeekFrom::Start(offset))
                    .map_err(|_| FsError::GeneralFailure)?;
                f.write_all(&data)
                    .map_err(|_| FsError::GeneralFailure)?;
                let path = Arc::new(temp.path().to_path_buf());
                Ok((Arc::new(std::sync::Mutex::new(temp)), path))
            })
            .await
            .map_err(|_| FsError::GeneralFailure)??;

            *self = WriteSpool::File {
                file,
                path,
                len: new_len,
                max_bytes: limit,
            };
            return Ok(());
        }

        if let WriteSpool::File { file, len, .. } = self {
            let file_arc = Arc::clone(file);
            tokio::task::spawn_blocking(move || -> FsResult<()> {
                let mut guard = file_arc.lock().map_err(|_| FsError::GeneralFailure)?;
                let f = guard.as_file_mut();
                f.seek(SeekFrom::Start(offset))
                    .map_err(|_| FsError::GeneralFailure)?;
                f.write_all(&data)
                    .map_err(|_| FsError::GeneralFailure)?;
                Ok(())
            })
            .await
            .map_err(|_| FsError::GeneralFailure)??;
            if end > *len {
                *len = end;
            }
        }
        Ok(())
    }

    async fn read_at(&self, offset: u64, to_read: usize) -> FsResult<Bytes> {
        match self {
            WriteSpool::Memory { buf, .. } => {
                Ok(Bytes::copy_from_slice(&buf[offset as usize..offset as usize + to_read]))
            }
            WriteSpool::File { file, .. } => {
                let file_arc = Arc::clone(file);
                tokio::task::spawn_blocking(move || -> FsResult<Bytes> {
                    let mut guard = file_arc.lock().map_err(|_| FsError::GeneralFailure)?;
                    let f = guard.as_file_mut();
                    f.seek(SeekFrom::Start(offset))
                        .map_err(|_| FsError::GeneralFailure)?;
                    let mut tmp = vec![0u8; to_read];
                    f.read_exact(&mut tmp)
                        .map_err(|_| FsError::GeneralFailure)?;
                    Ok(Bytes::from(tmp))
                })
                .await
                .map_err(|_| FsError::GeneralFailure)?
            }
        }
    }

    async fn prepare_upload_source(&mut self) -> FsResult<UploadSource> {
        match self {
            WriteSpool::Memory { buf, .. } => Ok(UploadSource::Memory(Bytes::copy_from_slice(buf))),
            WriteSpool::File { file, path, len, .. } => {
                let file_arc = Arc::clone(file);
                tokio::task::spawn_blocking(move || -> FsResult<()> {
                    let mut guard = file_arc.lock().map_err(|_| FsError::GeneralFailure)?;
                    guard
                        .as_file_mut()
                        .flush()
                        .map_err(|_| FsError::GeneralFailure)?;
                    Ok(())
                })
                .await
                .map_err(|_| FsError::GeneralFailure)??;
                Ok(UploadSource::File(Arc::clone(path), *len))
            }
        }
    }
}

struct FeishuWriteFile {
    client: Arc<FeishuClient>,
    cache: Arc<DriveCache>,
    parent_token: String,
    file_name: String,
    existing: Option<VirtualNode>,
    spool: WriteSpool,
    pos: u64,
    dirty: bool,
    flushed: bool,
    created: SystemTime,
    modified: SystemTime,
}

impl std::fmt::Debug for FeishuWriteFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeishuWriteFile")
            .field("parent_token", &self.parent_token)
            .field("file_name", &self.file_name)
            .field("len", &self.spool.len())
            .finish()
    }
}

impl DavFile for FeishuWriteFile {
    fn metadata(&'_ mut self) -> FsFuture<'_, Box<dyn DavMetaData>> {
        let meta = FeishuMetaData {
            len: self.spool.len(),
            modified: self.modified,
            created: self.created,
            is_dir: false,
            etag: None,
        };
        Box::pin(async move { Ok(Box::new(meta) as Box<dyn DavMetaData>) })
    }

    fn write_buf(&'_ mut self, mut buf: Box<dyn Buf + Send>) -> FsFuture<'_, ()> {
        Box::pin(async move {
            while buf.has_remaining() {
                let chunk_len = buf.chunk().len();
                let chunk = buf.copy_to_bytes(chunk_len);
                self.spool.write_at(self.pos, chunk).await?;
                self.pos += chunk_len as u64;
            }
            self.dirty = true;
            self.flushed = false;
            self.modified = SystemTime::now();
            Ok(())
        })
    }

    fn write_bytes(&'_ mut self, buf: Bytes) -> FsFuture<'_, ()> {
        Box::pin(async move {
            let len = buf.len() as u64;
            self.spool.write_at(self.pos, buf).await?;
            self.pos += len;
            self.dirty = true;
            self.flushed = false;
            self.modified = SystemTime::now();
            Ok(())
        })
    }

    fn read_bytes(&'_ mut self, count: usize) -> FsFuture<'_, Bytes> {
        Box::pin(async move {
            let total = self.spool.len();
            if self.pos >= total || count == 0 {
                return Ok(Bytes::new());
            }
            let to_read = (total - self.pos).min(count as u64) as usize;
            let bytes = self.spool.read_at(self.pos, to_read).await?;
            self.pos += bytes.len() as u64;
            Ok(bytes)
        })
    }

    fn seek(&'_ mut self, pos: SeekFrom) -> FsFuture<'_, u64> {
        Box::pin(async move {
            let next = compute_seek_pos(self.pos as i128, self.spool.len() as i128, pos)?;
            self.pos = next;
            Ok(next)
        })
    }

    fn flush(&'_ mut self) -> FsFuture<'_, ()> {
        Box::pin(async move {
            if self.flushed && !self.dirty {
                return Ok(());
            }

            let source = self.spool.prepare_upload_source().await?;
            let size = source.len();

            if size == 0 {
                // Attempt native 0-byte upload first; if Feishu rejects empty files, keep a
                // virtual 0-byte entry so WebDAV touch / Finder / Explorer placeholder PUTs work.
                match self
                    .client
                    .upload_all(&self.parent_token, &self.file_name, Bytes::new())
                    .await
                {
                    Ok(new_token) => {
                        self.cache.put_size(&new_token, None, 0).await;
                        self.cache
                            .remove_virtual_zero(&self.parent_token, &self.file_name)
                            .await;
                        if let Some(ref old) = self.existing
                            && !matches!(old.kind, NodeKind::VirtualZeroFile)
                            && !old.is_immutable_cloud_doc()
                        {
                            let _ = self
                                .client
                                .delete_file(&old.entry_token, &old.entry_type)
                                .await;
                        }
                    }
                    Err(AppError::FeishuApi { .. }) | Err(AppError::InvalidInput(_)) => {
                        debug!(
                            file = %self.file_name,
                            "Feishu rejected 0-byte upload; storing virtual zero-byte entry"
                        );
                        if let Some(ref old) = self.existing
                            && !matches!(old.kind, NodeKind::VirtualZeroFile)
                            && !old.is_immutable_cloud_doc()
                        {
                            self.client
                                .delete_file(&old.entry_token, &old.entry_type)
                                .await
                                .map_err(FsError::from)?;
                        }
                        let now = unix_now();
                        self.cache
                            .put_virtual_zero(
                                &self.parent_token,
                                &self.file_name,
                                VirtualZeroEntry {
                                    token: format!("vzero_{now}"),
                                    created_time: now,
                                    modified_time: now,
                                },
                            )
                            .await;
                    }
                    Err(e) => return Err(FsError::from(e)),
                }

                self.cache.invalidate_dir(&self.parent_token).await;
                self.flushed = true;
                self.dirty = false;
                return Ok(());
            }

            let new_token = self
                .client
                .upload_source(&self.parent_token, &self.file_name, &source)
                .await
                .map_err(FsError::from)?;

            self.cache.put_size(&new_token, None, size).await;
            self.cache
                .remove_virtual_zero(&self.parent_token, &self.file_name)
                .await;

            // Upload succeeded: now safely delete old binary file if overwriting
            if let Some(ref old) = self.existing
                && !matches!(old.kind, NodeKind::VirtualZeroFile)
                && !old.is_immutable_cloud_doc()
                && let Err(err) = self
                    .client
                    .delete_file(&old.entry_token, &old.entry_type)
                    .await
            {
                warn!(old_token = %old.entry_token, "failed to delete overwritten file: {err}");
            }

            self.cache.invalidate_dir(&self.parent_token).await;
            self.flushed = true;
            self.dirty = false;
            Ok(())
        })
    }
}

async fn spool_existing_node(
    client: &FeishuClient,
    node: &VirtualNode,
    spool: &mut WriteSpool,
) -> FsResult<u64> {
    match &node.kind {
        NodeKind::VirtualZeroFile => Ok(0),
        NodeKind::BinaryFile => {
            let (mut body, _) = client
                .open_download_stream(&node.token, 0)
                .await
                .map_err(FsError::from)?;
            let mut offset = 0u64;
            while let Some(chunk) = body.next_chunk().await.map_err(FsError::from)? {
                let chunk_len = chunk.len() as u64;
                spool.write_at(offset, chunk).await?;
                offset += chunk_len;
            }
            Ok(offset)
        }
        NodeKind::ExportDoc { .. } | NodeKind::UrlShortcut { .. } | NodeKind::Folder => {
            Err(FsError::Forbidden)
        }
    }
}

fn compute_seek_pos(current: i128, len: i128, pos: SeekFrom) -> FsResult<u64> {
    let target = match pos {
        SeekFrom::Start(off) => off as i128,
        SeekFrom::Current(delta) => current + delta as i128,
        SeekFrom::End(delta) => len + delta as i128,
    };
    if target < 0 {
        return Err(FsError::Forbidden);
    }
    Ok(target as u64)
}
