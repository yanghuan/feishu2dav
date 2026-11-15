use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use feishu2dav::config::{Cli, Command, Config};
use feishu2dav::oauth::OAuthManager;
use feishu2dav::server::{build_app_state, build_router};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("feishu2dav=info"));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let cli = Cli::parse();
    let config = Config::load_from_cli(&cli)?;

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(120))
        .build()?;

    let oauth = Arc::new(OAuthManager::new(config.clone(), http.clone()));
    let loaded = oauth.load_from_disk().await?;

    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => run_serve(config, oauth, http, loaded).await,
        Command::Auth => run_auth(config, oauth, http).await,
        Command::Status => run_status(config, oauth, http, loaded).await,
    }
}

async fn run_serve(
    config: Config,
    oauth: Arc<OAuthManager>,
    http: reqwest::Client,
    loaded: bool,
) -> anyhow::Result<()> {
    let addr = config.listen_addr()?;
    let listener = TcpListener::bind(addr).await?;
    let state = build_app_state(config.clone(), Arc::clone(&oauth), http);
    let _refresher = oauth.spawn_background_refresher();

    if !loaded {
        let (auth_url, _) = oauth.build_authorize_url().await?;
        warn!("No saved OAuth token found at {}", config.token_file.display());
        println!();
        println!("=== Feishu OAuth Authorization Required ===");
        println!("Open the local login page in your browser:");
        println!("  http://{}/oauth/login", config.listen);
        println!("Or open this Feishu authorization URL directly:");
        println!("  {auth_url}");
        println!("===========================================");
        println!();
    } else {
        let status = oauth.status().await;
        info!(
            user = status.user_name.as_deref().unwrap_or("unknown"),
            "Feishu OAuth token ready"
        );
    }

    if addr.ip().is_unspecified() && config.dav_username.is_none() {
        warn!(
            "Binding to {} without --dav-username/--dav-password! Anyone on the network can access your WebDAV share.",
            config.listen
        );
    }

    let dashboard_url = if config.dav_prefix == "/" || config.callback_path() == "/" {
        format!("http://{}/_dashboard", config.listen)
    } else {
        format!("http://{}/", config.listen)
    };
    let router = build_router(state);
    info!("Dashboard & OAuth: {dashboard_url}");
    info!(
        "WebDAV endpoint:   http://{}{}",
        config.listen, config.dav_prefix
    );
    if let Some(ref root_folder_token) = config.root_folder_token {
        info!(root_folder_token, "Mounting configured Feishu folder as WebDAV root");
    }

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}

async fn run_auth(
    config: Config,
    oauth: Arc<OAuthManager>,
    http: reqwest::Client,
) -> anyhow::Result<()> {
    let addr = config.listen_addr()?;
    let listener = TcpListener::bind(addr).await?;
    let state = build_app_state(config.clone(), Arc::clone(&oauth), http);
    let router = build_router(state);

    let (auth_url, _) = oauth.build_authorize_url().await?;
    println!("Please open the following URL in your browser to authorize Feishu Drive:");
    println!();
    println!("  {auth_url}");
    println!();
    println!("(Or visit http://{}/oauth/login)", config.listen);
    println!("Waiting for OAuth callback on {} ...", config.redirect_uri);

    let mut auth_rx = oauth.subscribe_authorization();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = auth_rx.changed() => {},
                _ = shutdown_signal() => {},
            }
        })
        .await?;

    let status = oauth.status().await;
    if status.authorized {
        println!(
            "Authorization succeeded for {}! Token saved to {}",
            status.user_name.as_deref().unwrap_or("Feishu User"),
            config.token_file.display()
        );
    }
    Ok(())
}

async fn run_status(
    config: Config,
    oauth: Arc<OAuthManager>,
    http: reqwest::Client,
    loaded: bool,
) -> anyhow::Result<()> {
    if !loaded {
        println!(
            "Not authorized: token file '{}' does not exist.",
            config.token_file.display()
        );
        println!("Run `feishu2dav auth` or `feishu2dav serve` and visit /oauth/login.");
        return Ok(());
    }

    let status = oauth.status().await;
    println!("Authorized:         {}", status.authorized);
    println!(
        "User:               {}",
        status.user_name.as_deref().unwrap_or("unknown")
    );
    println!(
        "Access token valid: {} (expires in {:?}s)",
        status.access_token_valid, status.access_expires_in_secs
    );
    println!(
        "Refresh token:      {} (expires in {:?}s)",
        status.refresh_token_present, status.refresh_expires_in_secs
    );
    println!(
        "Scopes:             {}",
        status.scope.as_deref().unwrap_or("unknown")
    );

    let state = build_app_state(config.clone(), oauth, http);
    if let Some(ref custom_root) = config.root_folder_token {
        println!("Drive root token:   {custom_root} (configured custom root)");
        match state.client.list_files(custom_root).await {
            Ok(items) => {
                println!("Custom root check:  ok ({} item(s))", items.len());
            }
            Err(err) => {
                println!("Custom root check:  failed: {err}");
            }
        }
    } else {
        match state.client.get_root_folder_meta().await {
            Ok(root) => {
                println!("Drive root token:   {} (user_id={})", root.token, root.user_id);
            }
            Err(err) => {
                println!("Drive root check failed: {err}");
            }
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("received Ctrl+C, shutting down");
}
