mod common;

use std::sync::Arc;
use std::sync::atomic::Ordering;

use reqwest::{StatusCode, header};
use serde_json::Value;
use tempfile::TempDir;

use common::{
    TestServerOpts, now_secs, sample_token, spawn_mock_feishu, spawn_test_server,
    spawn_test_server_with,
};
use feishu2dav::oauth::TokenSet;

#[tokio::test]
async fn oauth_flow_and_concurrent_refresh_rotation() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let (srv_url, oauth) = spawn_test_server(&mock_base, &dir, None, None).await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // 1. /oauth/login redirects to Feishu authorize URL with CSRF state
    let login_resp = client
        .get(format!("{srv_url}/oauth/login"))
        .send()
        .await
        .unwrap();
    assert_eq!(login_resp.status(), StatusCode::TEMPORARY_REDIRECT);
    let loc = login_resp
        .headers()
        .get(header::LOCATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let parsed = url::Url::parse(&loc).unwrap();
    let state_param = parsed
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .to_string();
    assert!(loc.contains("response_type=code"));
    assert!(loc.contains("offline_access"));

    // 2. Bad state rejected
    let bad_cb = client
        .get(format!(
            "{srv_url}/oauth/callback?code=valid_auth_code&state=wrong_state"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(bad_cb.status(), StatusCode::BAD_GATEWAY);

    // 3. Valid callback exchanges code and persists token
    let ok_cb = client
        .get(format!(
            "{srv_url}/oauth/callback?code=valid_auth_code&state={state_param}"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_cb.status(), StatusCode::OK);
    let body = ok_cb.text().await.unwrap();
    assert!(body.contains("Alice Tester"));

    let status_json: Value = client
        .get(format!("{srv_url}/oauth/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status_json["authorized"], true);
    assert_eq!(status_json["user_name"], "Alice Tester");
    assert_eq!(status_json["refresh_token_present"], true);

    // 4. Test concurrent single-use refresh_token deduplication:
    // Create a second server whose token on disk is expired and needs refresh.
    let dir2 = TempDir::new().unwrap();
    let current_rt = { mock.lock().await.valid_refresh_token.clone() };
    let current_at = { mock.lock().await.valid_access_token.clone() };
    let expired_token = TokenSet {
        access_token: current_at,
        refresh_token: Some(current_rt),
        expires_at: now_secs().saturating_sub(60), // already expired
        refresh_expires_at: Some(now_secs() + 3600),
        scope: Some("drive:drive offline_access".to_string()),
        token_type: "Bearer".to_string(),
        obtained_at: now_secs().saturating_sub(8000),
        user_name: Some("Alice Tester".to_string()),
        open_id: Some("ou_alice_123".to_string()),
    };
    let (_srv2, oauth2) = spawn_test_server(&mock_base, &dir2, None, Some(expired_token)).await;
    {
        mock.lock().await.refresh_calls.store(0, Ordering::SeqCst);
    }

    let mut handles = Vec::new();
    for _ in 0..10 {
        let mgr = Arc::clone(&oauth2);
        handles.push(tokio::spawn(async move {
            mgr.get_valid_access_token().await.unwrap()
        }));
    }
    let mut results = Vec::new();
    for h in handles {
        results.push(h.await.unwrap());
    }
    let expected_at = { mock.lock().await.valid_access_token.clone() };
    assert!(results.iter().all(|t| t == &expected_at));
    assert_eq!(
        mock.lock().await.refresh_calls.load(Ordering::SeqCst),
        1,
        "refresh_token must be used at most once across concurrent callers"
    );

    // Original oauth instance still holds a valid token too (sanity check)
    assert!(oauth.status().await.authorized);
}


#[tokio::test]
async fn basic_auth_protects_dashboard_oauth_and_root_prefix() {
    let (mock_base, _) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            basic_auth: Some(("admin", "s3cr3t")),
            preauth_token: Some(sample_token()),
            dav_prefix: "/",
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    // /healthz remains unauthenticated
    let hz = client.get(format!("{srv_url}/healthz")).send().await.unwrap();
    assert_eq!(hz.status(), StatusCode::OK);

    // Dashboard and OAuth control endpoints require Basic Auth when configured
    for (method, path) in [
        (reqwest::Method::GET, "/_dashboard"),
        (reqwest::Method::GET, "/oauth/login"),
        (reqwest::Method::GET, "/oauth/status"),
        (reqwest::Method::POST, "/oauth/refresh"),
    ] {
        let resp = client
            .request(method, format!("{srv_url}{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "expected 401 for {path}");
    }

    // With Basic Auth, /_dashboard succeeds even when dav_prefix = "/"
    let dash = client
        .get(format!("{srv_url}/_dashboard"))
        .basic_auth("admin", Some("s3cr3t"))
        .send()
        .await
        .unwrap();
    assert_eq!(dash.status(), StatusCode::OK);
    assert!(dash.text().await.unwrap().contains("feishu2dav"));

    // With dav_prefix = "/", PROPFIND on / lists root entries
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let pf = client
        .request(propfind, format!("{srv_url}/"))
        .basic_auth("admin", Some("s3cr3t"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(pf.status(), StatusCode::MULTI_STATUS);
    assert!(pf.text().await.unwrap().contains("readme.txt"));
}

#[tokio::test]
async fn root_callback_path_serves_oauth_and_dashboard_without_panic() {
    let (mock_base, _mock) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let (srv_root_cb, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            redirect_uri: Some("http://127.0.0.1:4977/".to_string()),
            preauth_token: Some(sample_token()),
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    // '/' routes to oauth_callback (400 when missing code/state), while '/_dashboard' serves UI
    let cb_root = client.get(format!("{srv_root_cb}/")).send().await.unwrap();
    assert_eq!(cb_root.status(), StatusCode::BAD_REQUEST);
    let dash_alt = client
        .get(format!("{srv_root_cb}/_dashboard"))
        .send()
        .await
        .unwrap();
    assert_eq!(dash_alt.status(), StatusCode::OK);
    assert!(dash_alt.text().await.unwrap().contains("feishu2dav"));
}

