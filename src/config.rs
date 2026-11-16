use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{AppError, AppResult};

pub const DEFAULT_API_BASE_URL: &str = "https://open.feishu.cn";
pub const DEFAULT_ACCOUNTS_BASE_URL: &str = "https://accounts.feishu.cn";
pub const DEFAULT_SCOPES: &str = "drive:drive drive:export:readonly offline_access";
pub const DEFAULT_LISTEN: &str = "127.0.0.1:4918";
pub const DEFAULT_DAV_PREFIX: &str = "/dav";
pub const DEFAULT_TOKEN_FILE: &str = "feishu2dav-tokens.json";
pub const DEFAULT_MAX_UPLOAD_MB: u64 = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Default)]
#[serde(rename_all = "snake_case")]
pub enum DocMode {
    /// Export Feishu cloud documents (docx/doc -> .docx, sheet/bitable -> .xlsx) on download;
    /// non-exportable cloud docs (slides/mindnote) are exposed as .url shortcuts.
    #[default]
    Export,
    /// Expose upgraded Feishu cloud documents (`docx`) as directly readable and editable `.md` Markdown files;
    /// legacy `doc`/`sheet`/`bitable` still export normally and non-exportable docs remain `.url` shortcuts.
    #[value(name = "markdown", alias = "md")]
    #[serde(alias = "md")]
    Markdown,
    /// Expose all Feishu cloud documents as clickable .url InternetShortcut files.
    Url,
    /// Hide Feishu cloud documents and expose only uploaded binary files and folders.
    #[value(name = "files_only", alias = "files-only")]
    FilesOnly,
}

#[derive(Debug, Parser)]
#[command(
    name = "feishu2dav",
    version,
    about = "Feishu (Lark) Cloud Drive WebDAV server with App ID/Secret + User OAuth"
)]
pub struct Cli {
    /// Optional TOML configuration file path
    #[arg(short, long, env = "FEISHU2DAV_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    /// Feishu App ID (cli_...)
    #[arg(long, env = "FEISHU_APP_ID", global = true)]
    pub app_id: Option<String>,

    /// Feishu App Secret
    #[arg(long, env = "FEISHU_APP_SECRET", global = true)]
    pub app_secret: Option<String>,

    /// HTTP listen address (host:port)
    #[arg(long, env = "FEISHU2DAV_LISTEN", global = true)]
    pub listen: Option<String>,

    /// OAuth redirect URI configured in Feishu Developer Console
    #[arg(long, env = "FEISHU_REDIRECT_URI", global = true)]
    pub redirect_uri: Option<String>,

    /// OAuth token persistence file path
    #[arg(long, env = "FEISHU2DAV_TOKEN_FILE", global = true)]
    pub token_file: Option<PathBuf>,

    /// Feishu Open API base URL (use https://open.larksuite.com for Lark)
    #[arg(long, env = "FEISHU_API_BASE_URL", global = true)]
    pub api_base_url: Option<String>,

    /// Feishu Accounts OAuth base URL (use https://accounts.larksuite.com for Lark)
    #[arg(long, env = "FEISHU_ACCOUNTS_BASE_URL", global = true)]
    pub accounts_base_url: Option<String>,

    /// Space-separated OAuth scopes
    #[arg(long, env = "FEISHU_OAUTH_SCOPES", global = true)]
    pub scopes: Option<String>,

    /// WebDAV mount URL prefix (e.g. /dav or /)
    #[arg(long, env = "FEISHU2DAV_PREFIX", global = true)]
    pub dav_prefix: Option<String>,

    /// Optional Feishu Drive folder token to mount as the WebDAV root (defaults to My Space root)
    #[arg(long, env = "FEISHU2DAV_ROOT_FOLDER_TOKEN", global = true)]
    pub root_folder_token: Option<String>,

    /// Optional additional Feishu Drive folder tokens to mount under the WebDAV root
    /// (supports comma-separated values or repeated flags)
    #[arg(
        long,
        alias = "mount-folder-tokens",
        env = "FEISHU2DAV_EXTRA_FOLDER_TOKENS",
        value_delimiter = ',',
        global = true
    )]
    pub extra_folder_tokens: Option<Vec<String>>,

    /// Optional WebDAV HTTP Basic Auth username
    #[arg(long, env = "FEISHU2DAV_USERNAME", global = true)]
    pub dav_username: Option<String>,

    /// Optional WebDAV HTTP Basic Auth password
    #[arg(long, env = "FEISHU2DAV_PASSWORD", global = true)]
    pub dav_password: Option<String>,

    /// Directory listing cache TTL in seconds (0 disables directory caching)
    #[arg(long, env = "FEISHU2DAV_CACHE_TTL", global = true)]
    pub cache_ttl_secs: Option<u64>,

    /// Whether to probe binary file sizes via Range requests during PROPFIND
    #[arg(long, env = "FEISHU2DAV_PROBE_SIZE", global = true)]
    pub probe_file_size: Option<bool>,

    /// Whether to probe Markdown docx sizes during PROPFIND (up to 5 uncached docs per directory)
    #[arg(long, env = "FEISHU2DAV_PROBE_MARKDOWN_SIZE", global = true)]
    pub probe_markdown_size: Option<bool>,

    /// How native Feishu cloud documents (docx/sheet/bitable/mindnote/slides) are exposed
    #[arg(long, value_enum, env = "FEISHU2DAV_DOC_MODE", global = true)]
    pub doc_mode: Option<DocMode>,

    /// Enable read-only WebDAV mode (blocks PUT/DELETE/MKCOL/MOVE/COPY)
    #[arg(long, env = "FEISHU2DAV_READ_ONLY", global = true)]
    pub read_only: Option<bool>,

    /// Disable HTML directory autoindex on WebDAV GET requests
    #[arg(long, global = true)]
    pub no_autoindex: bool,

    /// Serve HTML directory autoindex on WebDAV GET requests (true/false)
    #[arg(long, env = "FEISHU2DAV_AUTOINDEX", global = true)]
    pub autoindex: Option<bool>,

    /// Client-side QPS pacing for Feishu Drive APIs (1..=5)
    #[arg(long, env = "FEISHU2DAV_MAX_QPS", global = true)]
    pub max_qps: Option<u32>,

    /// Maximum single-file upload size in MiB (0 disables the limit)
    #[arg(long, env = "FEISHU2DAV_MAX_UPLOAD_MB", global = true)]
    pub max_upload_mb: Option<u64>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Start the HTTP + WebDAV + OAuth server (default)
    Serve,
    /// Print the Feishu OAuth authorization URL and start callback listener until authorized
    Auth,
    /// Check saved token status and verify Feishu Drive root access
    Status,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    app_id: Option<String>,
    app_secret: Option<String>,
    listen: Option<String>,
    redirect_uri: Option<String>,
    token_file: Option<PathBuf>,
    api_base_url: Option<String>,
    accounts_base_url: Option<String>,
    scopes: Option<String>,
    dav_prefix: Option<String>,
    root_folder_token: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_string_or_vec")]
    extra_folder_tokens: Option<Vec<String>>,
    dav_username: Option<String>,
    dav_password: Option<String>,
    cache_ttl_secs: Option<u64>,
    probe_file_size: Option<bool>,
    probe_markdown_size: Option<bool>,
    doc_mode: Option<DocMode>,
    read_only: Option<bool>,
    autoindex: Option<bool>,
    max_qps: Option<u32>,
    max_upload_mb: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub app_id: String,
    pub app_secret: String,
    pub listen: String,
    pub redirect_uri: String,
    pub token_file: PathBuf,
    pub api_base_url: String,
    pub accounts_base_url: String,
    pub scopes: String,
    pub dav_prefix: String,
    pub root_folder_token: Option<String>,
    pub extra_folder_tokens: Vec<String>,
    pub dav_username: Option<String>,
    pub dav_password: Option<String>,
    pub cache_ttl_secs: u64,
    pub probe_file_size: bool,
    pub probe_markdown_size: bool,
    pub doc_mode: DocMode,
    pub read_only: bool,
    pub autoindex: bool,
    pub max_qps: u32,
    pub max_upload_mb: u64,
}

impl Config {
    pub fn load_from_cli(cli: &Cli) -> AppResult<Self> {
        let file_cfg = match &cli.config {
            Some(path) => Self::read_toml(path)?,
            None => {
                let default_path = Path::new("config.toml");
                if default_path.exists() {
                    Self::read_toml(default_path)?
                } else {
                    FileConfig::default()
                }
            }
        };

        let app_id = cli
            .app_id
            .clone()
            .or(file_cfg.app_id)
            .unwrap_or_default()
            .trim()
            .to_string();
        let app_secret = cli
            .app_secret
            .clone()
            .or(file_cfg.app_secret)
            .unwrap_or_default()
            .trim()
            .to_string();

        if app_id.is_empty() {
            return Err(AppError::Config(
                "missing app_id: pass --app-id, set FEISHU_APP_ID, or set app_id in config.toml"
                    .to_string(),
            ));
        }
        if app_secret.is_empty() {
            return Err(AppError::Config(
                "missing app_secret: pass --app-secret, set FEISHU_APP_SECRET, or set app_secret in config.toml"
                    .to_string(),
            ));
        }

        let listen = cli
            .listen
            .clone()
            .or(file_cfg.listen)
            .unwrap_or_else(|| DEFAULT_LISTEN.to_string());

        let parsed_addr: SocketAddr = listen.parse().map_err(|e| {
            AppError::Config(format!("invalid listen address '{listen}': {e}"))
        })?;

        let redirect_uri = cli
            .redirect_uri
            .clone()
            .or(file_cfg.redirect_uri)
            .unwrap_or_else(|| {
                let host = if parsed_addr.ip().is_unspecified() {
                    "127.0.0.1".to_string()
                } else {
                    parsed_addr.ip().to_string()
                };
                format!("http://{host}:{}/oauth/callback", parsed_addr.port())
            });
        url::Url::parse(&redirect_uri).map_err(|e| {
            AppError::Config(format!("invalid redirect_uri '{redirect_uri}': {e}"))
        })?;

        let token_file = cli
            .token_file
            .clone()
            .or(file_cfg.token_file)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TOKEN_FILE));

        let api_base_url = cli
            .api_base_url
            .clone()
            .or(file_cfg.api_base_url)
            .unwrap_or_else(|| DEFAULT_API_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let accounts_base_url = cli
            .accounts_base_url
            .clone()
            .or(file_cfg.accounts_base_url)
            .unwrap_or_else(|| DEFAULT_ACCOUNTS_BASE_URL.to_string())
            .trim_end_matches('/')
            .to_string();

        let scopes = cli
            .scopes
            .clone()
            .or(file_cfg.scopes)
            .unwrap_or_else(|| DEFAULT_SCOPES.to_string())
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");

        let raw_prefix = cli
            .dav_prefix
            .clone()
            .or(file_cfg.dav_prefix)
            .unwrap_or_else(|| DEFAULT_DAV_PREFIX.to_string());
        let dav_prefix = normalize_prefix(&raw_prefix);
        let root_folder_token = normalize_folder_token(
            cli.root_folder_token
                .clone()
                .or(file_cfg.root_folder_token),
        )?;
        let extra_folder_tokens = normalize_folder_tokens(
            cli.extra_folder_tokens
                .clone()
                .or(file_cfg.extra_folder_tokens),
        )?;

        let dav_username = cli.dav_username.clone().or(file_cfg.dav_username);
        let dav_password = cli.dav_password.clone().or(file_cfg.dav_password);
        if dav_username.is_some() != dav_password.is_some() {
            return Err(AppError::Config(
                "dav_username and dav_password must both be set when enabling WebDAV Basic Auth"
                    .to_string(),
            ));
        }

        let cache_ttl_secs = cli
            .cache_ttl_secs
            .or(file_cfg.cache_ttl_secs)
            .unwrap_or(30);
        let probe_file_size = cli
            .probe_file_size
            .or(file_cfg.probe_file_size)
            .unwrap_or(true);
        let probe_markdown_size = cli
            .probe_markdown_size
            .or(file_cfg.probe_markdown_size)
            .unwrap_or(true);
        let doc_mode = cli.doc_mode.or(file_cfg.doc_mode).unwrap_or_default();
        let read_only = cli.read_only.or(file_cfg.read_only).unwrap_or(false);
        let autoindex = if cli.no_autoindex {
            false
        } else {
            cli.autoindex.or(file_cfg.autoindex).unwrap_or(true)
        };
        let max_qps = cli.max_qps.or(file_cfg.max_qps).unwrap_or(4).clamp(1, 5);
        let max_upload_mb = cli
            .max_upload_mb
            .or(file_cfg.max_upload_mb)
            .unwrap_or(DEFAULT_MAX_UPLOAD_MB);

        let cfg = Self {
            app_id,
            app_secret,
            listen,
            redirect_uri,
            token_file,
            api_base_url,
            accounts_base_url,
            scopes,
            dav_prefix,
            root_folder_token,
            extra_folder_tokens,
            dav_username,
            dav_password,
            cache_ttl_secs,
            probe_file_size,
            probe_markdown_size,
            doc_mode,
            read_only,
            autoindex,
            max_qps,
            max_upload_mb,
        };
        cfg.validate_routes()?;
        Ok(cfg)
    }

    pub fn validate_routes(&self) -> AppResult<()> {
        const RESERVED_PATHS: &[&str] = &[
            "/healthz",
            "/_dashboard",
            "/oauth/login",
            "/oauth/status",
            "/oauth/refresh",
        ];

        let cb = self.callback_path();
        let trimmed = if cb == "/" {
            "/"
        } else {
            cb.trim_end_matches('/')
        };
        if RESERVED_PATHS.contains(&trimmed) {
            return Err(AppError::Config(format!(
                "redirect_uri path '{cb}' conflicts with built-in endpoint '{trimmed}'; choose a dedicated path such as '/oauth/callback'"
            )));
        }
        if (self.dav_prefix == "/" && trimmed == "/")
            || (self.dav_prefix != "/"
                && (trimmed == self.dav_prefix
                    || trimmed.starts_with(&format!("{}/", self.dav_prefix))))
        {
            return Err(AppError::Config(format!(
                "redirect_uri path '{cb}' conflicts with --dav-prefix '{}'",
                self.dav_prefix
            )));
        }
        Ok(())
    }

    pub fn max_upload_bytes(&self) -> Option<u64> {
        (self.max_upload_mb > 0).then(|| self.max_upload_mb.saturating_mul(1024 * 1024))
    }

    fn read_toml(path: &Path) -> AppResult<FileConfig> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            AppError::Config(format!(
                "failed to read config file '{}': {e}",
                path.display()
            ))
        })?;
        toml::from_str(&content).map_err(|e| {
            AppError::Config(format!(
                "failed to parse config file '{}': {e}",
                path.display()
            ))
        })
    }

    pub fn listen_addr(&self) -> AppResult<SocketAddr> {
        self.listen.parse().map_err(|e| {
            AppError::Config(format!("invalid listen address '{}': {e}", self.listen))
        })
    }

    pub fn callback_path(&self) -> String {
        url::Url::parse(&self.redirect_uri)
            .map(|u| {
                let p = u.path().to_string();
                if p.is_empty() {
                    "/oauth/callback".to_string()
                } else {
                    p
                }
            })
            .unwrap_or_else(|_| "/oauth/callback".to_string())
    }
}

pub fn normalize_prefix(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("/{trimmed}")
    }
}

fn validate_folder_token(field_name: &str, raw: &str) -> AppResult<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.contains('/')
        || trimmed.contains('?')
        || trimmed.contains('#')
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err(AppError::Config(format!(
            "invalid {field_name} '{trimmed}': pass the bare Feishu folder token (e.g. 'fldcn...'), not a full URL or path"
        )));
    }
    Ok(Some(trimmed.to_string()))
}

fn normalize_folder_token(raw: Option<String>) -> AppResult<Option<String>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    validate_folder_token("root_folder_token", &raw)
}

fn normalize_folder_tokens(raw: Option<Vec<String>>) -> AppResult<Vec<String>> {
    let Some(items) = raw else {
        return Ok(Vec::new());
    };
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for item in items {
        if let Some(tok) = validate_folder_token("extra_folder_tokens", &item)?
            && seen.insert(tok.clone())
        {
            out.push(tok);
        }
    }
    Ok(out)
}

fn deserialize_opt_string_or_vec<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrVec {
        Str(String),
        Vec(Vec<String>),
    }

    let opt = Option::<StringOrVec>::deserialize(deserializer)?;
    Ok(opt.map(|val| match val {
        StringOrVec::Str(s) => s.split(',').map(str::to_string).collect(),
        StringOrVec::Vec(v) => v,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_prefix_handles_edge_cases() {
        assert_eq!(normalize_prefix("/"), "/");
        assert_eq!(normalize_prefix(""), "/");
        assert_eq!(normalize_prefix("dav"), "/dav");
        assert_eq!(normalize_prefix("/dav/"), "/dav");
        assert_eq!(normalize_prefix(" /nested/dav/ "), "/nested/dav");
    }

    #[test]
    fn cli_accepts_both_snake_and_kebab_files_only_and_new_flags() {
        let cli_snake = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--doc-mode",
            "files_only",
            "--max-qps",
            "3",
            "--max-upload-mb",
            "128",
            "--autoindex",
            "false",
            "--probe-markdown-size",
            "false",
        ])
        .unwrap();
        let cfg = Config::load_from_cli(&cli_snake).unwrap();
        assert_eq!(cfg.doc_mode, DocMode::FilesOnly);
        assert_eq!(cfg.max_qps, 3);
        assert_eq!(cfg.max_upload_mb, 128);
        assert_eq!(cfg.max_upload_bytes(), Some(128 * 1024 * 1024));
        assert!(!cfg.autoindex);
        assert!(!cfg.probe_markdown_size);

        let cli_kebab = Cli::try_parse_from(["feishu2dav", "--doc-mode", "files-only"]).unwrap();
        assert_eq!(cli_kebab.doc_mode, Some(DocMode::FilesOnly));

        let cli_md = Cli::try_parse_from(["feishu2dav", "--doc-mode", "markdown"]).unwrap();
        assert_eq!(cli_md.doc_mode, Some(DocMode::Markdown));
        let cli_md_alias = Cli::try_parse_from(["feishu2dav", "--doc-mode", "md"]).unwrap();
        assert_eq!(cli_md_alias.doc_mode, Some(DocMode::Markdown));
    }

    #[test]
    fn redirect_uri_root_path_and_conflict_validation() {
        // Root callback '/' is permitted when dav_prefix is non-root (e.g. default '/dav')
        let ok_root_cb = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--redirect-uri",
            "http://127.0.0.1:4977/",
        ])
        .unwrap();
        let cfg = Config::load_from_cli(&ok_root_cb).unwrap();
        assert_eq!(cfg.callback_path(), "/");
        assert_eq!(cfg.dav_prefix, "/dav");

        // Root callback '/' conflicts with root '--dav-prefix /'
        let root_conflict = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--dav-prefix",
            "/",
            "--redirect-uri",
            "http://127.0.0.1:4977/",
        ])
        .unwrap();
        assert!(Config::load_from_cli(&root_conflict).is_err());

        // Callback under dav_prefix or reserved control paths must be rejected cleanly
        for bad_uri in [
            "http://127.0.0.1:4977/dav",
            "http://127.0.0.1:4977/dav/callback",
            "http://127.0.0.1:4977/_dashboard",
            "http://127.0.0.1:4977/healthz",
        ] {
            let cli = Cli::try_parse_from([
                "feishu2dav",
                "--app-id",
                "cli_x",
                "--app-secret",
                "sec_x",
                "--redirect-uri",
                bad_uri,
            ])
            .unwrap();
            assert!(
                Config::load_from_cli(&cli).is_err(),
                "expected config rejection for {bad_uri}"
            );
        }
    }

    #[test]
    fn root_folder_token_normalizes_and_rejects_urls() {
        let cli_trimmed = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--root-folder-token",
            "  fldcnExampleRoot123  ",
        ])
        .unwrap();
        let cfg = Config::load_from_cli(&cli_trimmed).unwrap();
        assert_eq!(
            cfg.root_folder_token.as_deref(),
            Some("fldcnExampleRoot123")
        );

        let cli_empty = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--root-folder-token",
            "   ",
        ])
        .unwrap();
        let cfg_empty = Config::load_from_cli(&cli_empty).unwrap();
        assert_eq!(cfg_empty.root_folder_token, None);

        let cli_url = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--root-folder-token",
            "https://my.feishu.cn/drive/folder/fldcnExampleRoot123",
        ])
        .unwrap();
        assert!(Config::load_from_cli(&cli_url).is_err());
    }

    #[test]
    fn extra_folder_tokens_supports_multiple_comma_alias_toml_and_rejects_urls() {
        let cli_multi = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--extra-folder-tokens",
            " fld_a , , fld_b ",
            "--extra-folder-tokens",
            "fld_a,fld_c",
        ])
        .unwrap();
        let cfg = Config::load_from_cli(&cli_multi).unwrap();
        assert_eq!(cfg.extra_folder_tokens, vec!["fld_a", "fld_b", "fld_c"]);

        let cli_alias = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--mount-folder-tokens",
            "fld_x,fld_y",
        ])
        .unwrap();
        let cfg_alias = Config::load_from_cli(&cli_alias).unwrap();
        assert_eq!(cfg_alias.extra_folder_tokens, vec!["fld_x", "fld_y"]);

        let cli_url = Cli::try_parse_from([
            "feishu2dav",
            "--app-id",
            "cli_x",
            "--app-secret",
            "sec_x",
            "--extra-folder-tokens",
            "https://my.feishu.cn/drive/folder/fld_a",
        ])
        .unwrap();
        assert!(Config::load_from_cli(&cli_url).is_err());

        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("cfg.toml");
        std::fs::write(
            &cfg_path,
            r#"
app_id = "cli_x"
app_secret = "sec_x"
extra_folder_tokens = ["fld_1", "fld_2", "fld_1"]
"#,
        )
        .unwrap();
        let cli_toml = Cli::try_parse_from([
            "feishu2dav",
            "--config",
            cfg_path.to_str().unwrap(),
        ])
        .unwrap();
        let cfg_toml = Config::load_from_cli(&cli_toml).unwrap();
        assert_eq!(cfg_toml.extra_folder_tokens, vec!["fld_1", "fld_2"]);

        // Passing empty CLI value overrides TOML cleanly to empty list
        let cli_clear = Cli::try_parse_from([
            "feishu2dav",
            "--config",
            cfg_path.to_str().unwrap(),
            "--extra-folder-tokens",
            "",
        ])
        .unwrap();
        let cfg_clear = Config::load_from_cli(&cli_clear).unwrap();
        assert!(cfg_clear.extra_folder_tokens.is_empty());

        // String form in TOML also works
        std::fs::write(
            &cfg_path,
            r#"
app_id = "cli_x"
app_secret = "sec_x"
extra_folder_tokens = "fld_s1, fld_s2"
"#,
        )
        .unwrap();
        let cli_str_toml = Cli::try_parse_from([
            "feishu2dav",
            "--config",
            cfg_path.to_str().unwrap(),
        ])
        .unwrap();
        let cfg_str_toml = Config::load_from_cli(&cli_str_toml).unwrap();
        assert_eq!(cfg_str_toml.extra_folder_tokens, vec!["fld_s1", "fld_s2"]);
    }
}
