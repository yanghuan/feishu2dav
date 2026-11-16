# feishu2dav

基于 Rust 实现的飞书（Feishu / Lark）云盘 WebDAV 网关，使用 `app_id` / `app_secret` 配合**用户 OAuth 2.0**（`user_access_token` + 单次轮换 `refresh_token`），将你的飞书「我的空间」挂载为标准 WebDAV 网盘。

## 为什么必须使用用户 OAuth？

- `app_id` / `app_secret` 只是换取令牌的凭证：
  - **应用令牌**（`tenant_access_token`）代表应用自身身份，无法访问用户个人云空间中的普通文件；
  - **用户令牌**（`user_access_token`）通过 OAuth 授权码流程由用户本人签发，代表用户身份操作飞书 Drive OpenAPI，因此可以完整访问和挂载你的个人云盘。
- 配合 `offline_access` 权限，服务端会自动持久化并在并发场景下**互斥轮换**单次有效的 `refresh_token`，实现长期免重新登录。

## 功能特性

- **完整用户 OAuth 2.0 流程**：内置 `/oauth/login`、`/oauth/callback`、`/oauth/status`、`/oauth/refresh` 与可视化状态面板 `/`（当 `--dav-prefix /` 时状态面板位于 `/_dashboard`）；自动防护 CSRF `state`，并发请求自动合并单次 `refresh_token` 刷新并原子写入本地 Token 文件，支持后台提前续期与 `401`/`99991677` 自动无感重试。
- **WebDAV 读写全协议支持**（基于 `dav-server` + `axum`）：
  - `PROPFIND` / `OPTIONS` / `GET` / `HEAD` / `PUT` / `MKCOL` / `DELETE` / `COPY` / `MOVE` / `LOCK` / `UNLOCK`（兼容 Windows 资源管理器、macOS Finder、rclone 等客户端）。
  - 支持 HTTP `Range` 断点续传与流式下载（单次 HTTP 流读取，支持视频/大文件按需拖动）。
  - 支持大文件自动分片上传（`<= 20MB` 使用 `upload_all`，`> 20MB` 使用 `upload_prepare` + 4MB `upload_part` + `upload_finish` 并附带 Adler-32 校验；超过 4MB 自动落盘临时文件避免内存暴涨），可通过 `--max-upload-mb` 控制单文件最大上传体积。对于超过 `--max-upload-mb` 的 `PUT` 请求，服务端会依据 `Content-Length` / `Content-Range` 直接前置返回 `413 Payload Too Large` 并关闭连接（`curl` 等支持 `Expect: 100-continue` 的客户端可优雅收到 `413`；未启用 `100-continue` 且一次性持续推送大包体的 HTTP/1.1 客户端在部分系统回环下可能感知连接提前重置）。
  - 同名普通文件覆盖采用「先上传新文件、成功后再删除旧文件」策略，避免传输中断丢失原文件；**以 `export`/`.url` 暴露的原生飞书云文档禁止通过 WebDAV `PUT` 覆盖写入或 `MOVE`/`COPY` 覆盖目标，且 `MOVE`/`COPY` 禁止直接覆盖已有目录（均返回 `403` / `412`），防止误将在线云文档或整棵目录树递归替换删除；显式 `DELETE`（含删除包含云文档的文件夹）仍可正常移除飞书中的对应项目（如需禁止删除请启用 `--read-only`）**；对飞书可能拒绝的 0 字节文件提供透明虚拟占位兼容。
  - 支持文件重命名（通过飞书 `copy` + `delete`）、跨目录移动（`move`）、以及文件夹重命名（新建目标目录 + 迁移子节点 + 删除旧目录）。
- **飞书云文档支持**（`--doc-mode` 可配置）：
  - `export`（默认）：`docx`/`doc` 自动按 `.docx` 导出，`sheet`/`bitable` 自动按 `.xlsx` 导出（懒加载导出任务 + 并发请求合并 + LRU 内存缓存），`slides`/`mindnote` 暴露为 `.url` 快捷方式；
  - `markdown`（亦支持 `md`）：将新版飞书文档（`docx`）转换为可直接打开、展示精确大小并支持原地编辑保存的 `.md` 文件（基于 DocX Blocks API 双向互转，保持原 `document_id` 与协作权限不变，保存时采用“先追加新块、成功后再删旧块”策略以防中途失败清空原文档；若原文档含图片/画板/多维表格/高亮块/未知行内元素等不可无损转为 Markdown 的内容，保存覆盖时返回 `403 Forbidden` 与中文提示以防误删素材；新建不存在的 `.md` 文件按普通文件上传；注：在 `.md` 中新插入图片语法会退化为 alt 文本、原生 HTML/脚注不会被转为飞书块，且无并发版本合并控制，建议单写者顺序编辑；旧版 `doc` / 表格仍按 `.docx` / `.xlsx` 导出，演示文稿等仍暴露为 `.url`）；
  - `url`：所有云文档暴露为可点击的 `.url` 快捷方式；
  - `files_only`（亦支持 `files-only`）：仅展示普通二进制文件与文件夹。
- **快捷方式透明解析**：自动识别飞书 Drive 中的 `shortcut` 节点并带环路检测。
- **速率限制保护**：内置令牌时间槽限流器（默认 4 QPS，低于飞书 Drive 5 QPS 上限）并对 `1061045` / HTTP 429 自动退避重试；限流彻底耗尽时向 WebDAV 客户端返回 `503 Service Unavailable` + `Retry-After`，未授权/凭据失效返回 `401 Unauthorized`。
- **可选 HTTP Basic Auth**：通过 `--dav-username` / `--dav-password` 同时保护 WebDAV 挂载点以及管理接口（`/`、`/_dashboard`、`/oauth/login`、`/oauth/status`、`/oauth/refresh`）。

## 飞书开放平台应用配置

1. 前往 [飞书开放平台](https://open.feishu.cn/app) 创建一个**企业自建应用**（或商店应用）。
2. 在 **凭证与基础信息** 中获取 `App ID`（`cli_...`）和 `App Secret`。
3. 在 **安全设置 -> 重定向 URL** 中添加回调地址（默认配置下为）：
   ```text
   http://127.0.0.1:4918/oauth/callback
   ```
4. 在 **权限管理** 中为应用开通以下**用户身份权限**（User Scope）：
   - `drive:drive`（查看、评论、编辑和管理云空间中的所有文件）
   - `drive:export:readonly`（导出云文档，若使用 `--doc-mode export`）
   - 若启用 `--doc-mode markdown` 直接读写飞书 `docx` 文档，请额外勾选 `docx:document:readonly` 与 `docx:document:write_only`（或直接勾选 `docx:document`）并在 `--scopes` 中包含它们（例如 `"drive:drive drive:export:readonly docx:document:readonly docx:document:write_only offline_access"`）
   - `offline_access`（获取 `refresh_token` 以支持自动续期）
   - 可选：`auth:user.id:read`（用于在状态页面显示当前授权用户的名称）
5. 创建版本并**发布应用**（自建应用需管理员审核通过后权限才生效）。

## 快速开始

### 1. 编译运行

```bash
cargo build --release
```

### 2. 配置方式（支持配置文件、环境变量或命令行参数）

复制示例配置 [config.example.toml](config.example.toml) 为 `config.toml` 并填入 `app_id` / `app_secret`，或直接使用环境变量/CLI 参数：

```bash
export FEISHU_APP_ID="cli_xxxxxxxxxxxxxxxx"
export FEISHU_APP_SECRET="xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"

# 方式 A：先完成交互式 OAuth 授权，授权成功后退出
cargo run -- auth

# 方式 B：直接启动服务（若尚未授权，启动后会打印授权链接）
cargo run -- serve
```

常用参数说明：

| 参数 / 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `--app-id` / `FEISHU_APP_ID` | 必填 | 飞书应用 App ID |
| `--app-secret` / `FEISHU_APP_SECRET` | 必填 | 飞书应用 App Secret |
| `--listen` / `FEISHU2DAV_LISTEN` | `127.0.0.1:4918` | HTTP 监听地址 |
| `--redirect-uri` / `FEISHU_REDIRECT_URI` | `http://127.0.0.1:<port>/oauth/callback` | 需与飞书后台安全设置完全一致 |
| `--token-file` / `FEISHU2DAV_TOKEN_FILE` | `feishu2dav-tokens.json` | OAuth 令牌持久化路径 |
| `--dav-prefix` / `FEISHU2DAV_PREFIX` | `/dav` | WebDAV 挂载前缀（支持 `/dav` 或 `/`） |
| `--root-folder-token` / `FEISHU2DAV_ROOT_FOLDER_TOKEN` | 无 | 指定要挂载为 WebDAV 根目录的飞书文件夹 token（默认挂载「我的空间」根目录） |
| `--extra-folder-tokens`（兼容别名 `--mount-folder-tokens`） / `FEISHU2DAV_EXTRA_FOLDER_TOKENS` | 无 | 在默认根目录（或 `--root-folder-token`）下额外挂载的飞书文件夹 token 列表，支持逗号分隔或重复传参（自动从飞书获取文件夹名并按 300s TTL 缓存；无权/失效 token 会记录告警并跳过，面板状态随访问最多 300s 刷新，实时诊断请运行 `feishu2dav status`；挂载点本身禁止 `DELETE`/`MOVE`/`COPY`，内部文件/子目录可正常读写；传入空值 `""` 可清空配置文件中的同名列表） |
| `--dav-username` / `FEISHU2DAV_USERNAME` | 无 | 可选 WebDAV Basic Auth 用户名 |
| `--dav-password` / `FEISHU2DAV_PASSWORD` | 无 | 可选 WebDAV Basic Auth 密码 |
| `--doc-mode` / `FEISHU2DAV_DOC_MODE` | `export` | 云文档模式：`export` / `markdown`（别名 `md`）/ `url` / `files_only`（兼容 `files-only`） |
| `--probe-file-size` / `FEISHU2DAV_PROBE_SIZE` | `true` | 是否在列目录时并发探测普通二进制文件精确大小 |
| `--probe-markdown-size` / `FEISHU2DAV_PROBE_MARKDOWN_SIZE` | `true` | 是否在 `markdown` 模式下自动探测 `.md` 文档精确大小（每次 `PROPFIND` 列目录最多探测 5 个未缓存文档，已打开/缓存过的文档始终返回精确大小） |
| `--cache-ttl-secs` / `FEISHU2DAV_CACHE_TTL` | `30` | 目录列表缓存 TTL（秒） |
| `--read-only` / `FEISHU2DAV_READ_ONLY` | `false` | 只读模式（禁止写入/删除/移动/复制） |
| `--autoindex` / `--no-autoindex` / `FEISHU2DAV_AUTOINDEX` | `true` | 是否在浏览器 GET WebDAV 目录时返回 HTML 文件列表 |
| `--max-qps` / `FEISHU2DAV_MAX_QPS` | `4` | 飞书 API 客户端限流 QPS（范围 `1..=5`） |
| `--max-upload-mb` / `FEISHU2DAV_MAX_UPLOAD_MB` | `2048` | 单文件最大上传限制 MiB（`0` 表示不限制） |

> 国际版 Lark 用户可指定 `--api-base-url https://open.larksuite.com --accounts-base-url https://accounts.larksuite.com`。

### 3. 完成授权与检查状态

1. 启动服务后浏览器打开 `http://127.0.0.1:4918/` 或直接访问 `http://127.0.0.1:4918/oauth/login`。
2. 在飞书页面登录并点击同意授权，回调成功后即可挂载 WebDAV。
3. 也可运行 `cargo run -- status` 检查本地 Token 状态并测试飞书根目录连通性。

## 挂载 WebDAV 示例

- **Windows（CMD / 资源管理器映射网络驱动器）**：
  ```cmd
  net use Z: http://127.0.0.1:4918/dav
  ```
  > 注：Windows 默认仅允许 HTTPS 的 WebDAV 认证或需要调整 `BasicAuthLevel` 注册表项；纯本机 `127.0.0.1` 不开启 Basic Auth 时可直接挂载，局域网/公网部署请配合反向代理启用 HTTPS 与 `--dav-username/--dav-password`。
- **macOS Finder**：`Cmd + K` 连接服务器，输入 `http://127.0.0.1:4918/dav`。
- **rclone**：
  ```bash
  rclone lsd :webdav: --webdav-url http://127.0.0.1:4918/dav
  ```

## Docker 部署（GitHub Container Registry）

GitHub Actions 会为 `main` 分支及 `v*` 标签自动构建多架构镜像（`linux/amd64`、`linux/arm64`）并发布至 `ghcr.io/yanghuan/feishu2dav`：

```bash
docker run -d \
  --name feishu2dav \
  -p 4918:4918 \
  -v feishu2dav-data:/data \
  -e FEISHU_APP_ID="cli_xxxxxxxxxxxxxxxx" \
  -e FEISHU_APP_SECRET="xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx" \
  ghcr.io/yanghuan/feishu2dav:latest
```

> 容器内默认监听 `0.0.0.0:4918`，OAuth 回调默认指向 `http://127.0.0.1:4918/oauth/callback`，令牌持久化在 `/data/feishu2dav-tokens.json`。如需挂载共享文件夹作为根目录，可追加 `-e FEISHU2DAV_ROOT_FOLDER_TOKEN="fldcnxxxxxxxxxxxxxxxx"`；如需在保留默认「我的空间」根目录的同时把一个或多个共享文件夹额外挂载到根目录下，可追加 `-e FEISHU2DAV_EXTRA_FOLDER_TOKENS="fldcnxxxxxxxxxxxxxxxx,fldcnyyyyyyyyyyyyyyyy"`；局域网/公网部署请按实际访问域名设置 `-e FEISHU_REDIRECT_URI="https://your-domain/oauth/callback"`。

## 运行测试

```bash
cargo test
```
