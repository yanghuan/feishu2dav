mod common;

use std::process::Command;
use std::sync::atomic::Ordering;

use reqwest::{StatusCode, header};
use tempfile::TempDir;

use common::{
    MockNode, TestServerOpts, sample_token, spawn_mock_feishu, spawn_test_server_with,
};

fn mk_folder(token: &str, name: &str, parent_token: &str) -> MockNode {
    MockNode {
        token: token.to_string(),
        name: name.to_string(),
        file_type: "folder".to_string(),
        parent_token: parent_token.to_string(),
        content: Vec::new(),
        shortcut_target: None,
        url: None,
        created_time: 1_700_000_000,
        modified_time: 1_700_000_000,
    }
}

fn mk_file(token: &str, name: &str, parent_token: &str, content: &[u8]) -> MockNode {
    MockNode {
        token: token.to_string(),
        name: name.to_string(),
        file_type: "file".to_string(),
        parent_token: parent_token.to_string(),
        content: content.to_vec(),
        shortcut_target: None,
        url: None,
        created_time: 1_700_000_000,
        modified_time: 1_700_000_000,
    }
}

#[tokio::test]
async fn extra_folder_tokens_mounts_under_root_caches_meta_timestamps_and_blocks_destructive_ops() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    {
        let mut s = mock.lock().await;
        s.nodes.insert(
            "fld_shared_root".to_string(),
            mk_folder("fld_shared_root", "feishu2davroot", "fld_external"),
        );
        s.nodes.insert(
            "shared_hello".to_string(),
            mk_file("shared_hello", "shared_hello.txt", "fld_shared_root", b"from-feishu2davroot"),
        );
        s.nodes.insert(
            "fld_team_docs".to_string(),
            mk_folder("fld_team_docs", "TeamDocs", "fld_other_user"),
        );
        s.nodes.insert(
            "team_note".to_string(),
            mk_file("team_note", "notes.txt", "fld_team_docs", b"team-content"),
        );
    }

    // Even with --cache-ttl-secs 0 (directory cache disabled), folder metadata remains cached
    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            cache_ttl_secs: 0,
            extra_folder_tokens: vec![
                "fld_shared_root".to_string(),
                "fld_team_docs".to_string(),
            ],
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let mkcol = reqwest::Method::from_bytes(b"MKCOL").unwrap();
    let move_m = reqwest::Method::from_bytes(b"MOVE").unwrap();
    let copy_m = reqwest::Method::from_bytes(b"COPY").unwrap();

    // 1. Default My Space root and extra mounted folders coexist under /dav/ with non-1970 timestamps
    let root_list = client
        .request(propfind.clone(), format!("{srv_url}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(root_list.status(), StatusCode::MULTI_STATUS);
    let root_xml = root_list.text().await.unwrap();
    assert!(root_xml.contains("readme.txt"));
    assert!(root_xml.contains("DesignSpec.docx"));
    assert!(root_xml.contains("feishu2davroot"));
    assert!(root_xml.contains("TeamDocs"));
    assert!(
        !root_xml.contains("1970"),
        "expected non-epoch timestamps on root and mount points, got: {root_xml}"
    );

    let meta_calls_after_first = mock.lock().await.folder_meta_calls.load(Ordering::SeqCst);
    assert_eq!(meta_calls_after_first, 2);

    // Repeat PROPFIND with cache_ttl_secs = 0: folder meta TTL (300s) avoids repeated meta API calls
    let root_list_cached = client
        .request(propfind.clone(), format!("{srv_url}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(root_list_cached.status(), StatusCode::MULTI_STATUS);
    assert_eq!(
        mock.lock().await.folder_meta_calls.load(Ordering::SeqCst),
        meta_calls_after_first
    );

    // 2. Read & COPY files inside extra mounted folders
    let get_shared = client
        .get(format!("{srv_url}/dav/feishu2davroot/shared_hello.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_shared.status(), StatusCode::OK);
    assert_eq!(get_shared.text().await.unwrap(), "from-feishu2davroot");

    let get_team = client
        .get(format!("{srv_url}/dav/TeamDocs/notes.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_team.status(), StatusCode::OK);
    assert_eq!(get_team.text().await.unwrap(), "team-content");

    let copy_file_out = client
        .request(copy_m.clone(), format!("{srv_url}/dav/feishu2davroot/shared_hello.txt"))
        .header("Destination", format!("{srv_url}/dav/copied_out.txt"))
        .send()
        .await
        .unwrap();
    assert!(copy_file_out.status().is_success());
    assert_eq!(
        client
            .get(format!("{srv_url}/dav/copied_out.txt"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "from-feishu2davroot"
    );

    // 3. Create subfolder, upload/read inside extra mount, and MOVE file from My Space root into mount
    let mkcol_sub = client
        .request(mkcol.clone(), format!("{srv_url}/dav/feishu2davroot/sub"))
        .send()
        .await
        .unwrap();
    assert_eq!(mkcol_sub.status(), StatusCode::CREATED);

    let put_nested = client
        .put(format!("{srv_url}/dav/feishu2davroot/sub/nested.txt"))
        .body("nested-write-ok")
        .send()
        .await
        .unwrap();
    assert!(put_nested.status().is_success());

    let move_into_mount = client
        .request(move_m.clone(), format!("{srv_url}/dav/readme.txt"))
        .header("Destination", format!("{srv_url}/dav/feishu2davroot/readme_moved.txt"))
        .send()
        .await
        .unwrap();
    assert!(move_into_mount.status().is_success());

    // 4. Mount point itself rejects DELETE, MOVE, COPY, PUT, and duplicate MKCOL without harming children
    let del_mount = client
        .delete(format!("{srv_url}/dav/feishu2davroot"))
        .send()
        .await
        .unwrap();
    assert_eq!(del_mount.status(), StatusCode::FORBIDDEN);

    let del_mount_slash = client
        .delete(format!("{srv_url}/dav/feishu2davroot/"))
        .send()
        .await
        .unwrap();
    assert_eq!(del_mount_slash.status(), StatusCode::FORBIDDEN);

    let move_mount = client
        .request(move_m, format!("{srv_url}/dav/feishu2davroot"))
        .header("Destination", format!("{srv_url}/dav/renamed_mount"))
        .send()
        .await
        .unwrap();
    assert_eq!(move_mount.status(), StatusCode::FORBIDDEN);

    let copy_mount = client
        .request(copy_m, format!("{srv_url}/dav/feishu2davroot"))
        .header("Destination", format!("{srv_url}/dav/backup_mount"))
        .send()
        .await
        .unwrap();
    assert_eq!(copy_mount.status(), StatusCode::FORBIDDEN);

    let put_over_mount = client
        .put(format!("{srv_url}/dav/feishu2davroot"))
        .body("overwrite")
        .send()
        .await
        .unwrap();
    assert_eq!(put_over_mount.status(), StatusCode::FORBIDDEN);

    let mkcol_existing_mount = client
        .request(mkcol, format!("{srv_url}/dav/feishu2davroot"))
        .send()
        .await
        .unwrap();
    assert_eq!(mkcol_existing_mount.status(), StatusCode::METHOD_NOT_ALLOWED);

    // Every child inside feishu2davroot stayed completely intact
    assert_eq!(
        client
            .get(format!("{srv_url}/dav/feishu2davroot/shared_hello.txt"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "from-feishu2davroot"
    );
    assert_eq!(
        client
            .get(format!("{srv_url}/dav/feishu2davroot/sub/nested.txt"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "nested-write-ok"
    );

    // 5. Dashboard displays mounted extra folder statuses
    let dash_html = client
        .get(format!("{srv_url}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(dash_html.contains("fld_shared_root"));
    assert!(dash_html.contains("已挂载 (feishu2davroot)"));
    assert!(dash_html.contains("已挂载 (TeamDocs)"));

    let s = mock.lock().await;
    assert_eq!(s.root_meta_calls.load(Ordering::SeqCst), 1);
    assert!(s.nodes.contains_key("fld_shared_root"));
    assert!(!s.nodes.values().any(|n| n.name == "backup_mount"));
    assert!(s.nodes.values().any(|n| {
        n.name == "readme_moved.txt" && n.parent_token == "fld_shared_root"
    }));
}

#[tokio::test]
async fn extra_folder_tokens_dedups_collisions_reports_failures_and_propagates_faults() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    {
        let mut s = mock.lock().await;
        s.nodes.insert(
            "fld_in_root".to_string(),
            mk_folder("fld_in_root", "SharedCol", "fld_root"),
        );
        s.nodes.insert(
            "fld_ext_col".to_string(),
            mk_folder("fld_ext_col", "SharedCol", "fld_external"),
        );
        s.nodes.insert(
            "ext_col_file".to_string(),
            mk_file("ext_col_file", "ext.txt", "fld_ext_col", b"ext-colliding-ok"),
        );
    }

    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            cache_ttl_secs: 0,
            extra_folder_tokens: vec![
                "fld_root".to_string(),        // self root -> must be skipped
                "fld_target".to_string(),      // already present via SharedAlias shortcut -> skip duplicate
                "fld_missing_404".to_string(), // inaccessible/missing -> warn, skip, record failure
                "fld_ext_col".to_string(),     // same name as fld_in_root -> deduplicated cleanly
            ],
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();

    let resp = client
        .request(propfind.clone(), format!("{srv_url}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::MULTI_STATUS);
    let xml = resp.text().await.unwrap();
    assert!(xml.contains("SharedCol"));
    assert!(xml.contains("SharedCol%20%5Bfld_ex%5D") || xml.contains("SharedCol [fld_ex]"));
    assert!(xml.contains("SharedAlias"));
    assert!(!xml.contains("SharedTarget"));

    let get_dedup = client
        .get(format!("{srv_url}/dav/SharedCol [fld_ex]/ext.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_dedup.status(), StatusCode::OK);
    assert_eq!(get_dedup.text().await.unwrap(), "ext-colliding-ok");

    // Dashboard surfaces failure details for bad/inaccessible tokens while healthy ones stay active
    let dash_html = client
        .get(format!("{srv_url}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(dash_html.contains("fld_missing_404"));
    assert!(dash_html.contains("挂载失败"));
    assert!(dash_html.contains("已挂载 (SharedCol)"));

    // Rate-limit faults during extra mount metadata lookup propagate 503 + Retry-After
    let dir_rl = TempDir::new().unwrap();
    let (mock_base_rl, mock_rl) = spawn_mock_feishu().await;
    {
        let mut s = mock_rl.lock().await;
        s.nodes.insert(
            "fld_ext_only".to_string(),
            mk_folder("fld_ext_only", "ExtOnly", "fld_external"),
        );
        s.nodes.insert(
            "ext_only_file".to_string(),
            mk_file("ext_only_file", "ext.txt", "fld_ext_only", b"hello"),
        );
        s.force_folder_meta_rate_limit = true;
    }
    let (srv_rl, _) = spawn_test_server_with(
        &mock_base_rl,
        &dir_rl,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            cache_ttl_secs: 0,
            extra_folder_tokens: vec!["fld_ext_only".to_string()],
            ..Default::default()
        },
    )
    .await;

    let rl_propfind = client
        .request(propfind, format!("{srv_rl}/dav/ExtOnly"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(rl_propfind.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rl_propfind.headers().get(header::RETRY_AFTER).unwrap(), "2");

    let rl_get = client
        .get(format!("{srv_rl}/dav/ExtOnly/ext.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(rl_get.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(rl_get.headers().get(header::RETRY_AFTER).unwrap(), "2");
}

#[tokio::test]
async fn extra_folder_tokens_composes_with_custom_root_and_respects_read_only() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    {
        let mut s = mock.lock().await;
        s.nodes.insert(
            "fld_custom_root".to_string(),
            mk_folder("fld_custom_root", "CustomRoot", "fld_external"),
        );
        s.nodes.insert(
            "custom_file".to_string(),
            mk_file("custom_file", "custom.txt", "fld_custom_root", b"custom-root-data"),
        );
        s.nodes.insert(
            "fld_extra_team".to_string(),
            mk_folder("fld_extra_team", "TeamExtra", "fld_external"),
        );
        s.nodes.insert(
            "extra_file".to_string(),
            mk_file("extra_file", "team.txt", "fld_extra_team", b"team-extra-data"),
        );
    }

    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            root_folder_token: Some("fld_custom_root".to_string()),
            extra_folder_tokens: vec!["fld_extra_team".to_string()],
            read_only: true,
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let mkcol = reqwest::Method::from_bytes(b"MKCOL").unwrap();

    // Custom root replaces default My Space while still mounting extra folder tokens beneath it
    let list_resp = client
        .request(propfind, format!("{srv_url}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::MULTI_STATUS);
    let xml = list_resp.text().await.unwrap();
    assert!(xml.contains("custom.txt"));
    assert!(xml.contains("TeamExtra"));
    assert!(!xml.contains("readme.txt"));

    assert_eq!(
        client
            .get(format!("{srv_url}/dav/TeamExtra/team.txt"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "team-extra-data"
    );

    // Read-only mode blocks mutations both inside extra mounts and against the mount point
    assert_eq!(
        client
            .put(format!("{srv_url}/dav/TeamExtra/new.txt"))
            .body("blocked")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .request(mkcol, format!("{srv_url}/dav/TeamExtra/Sub"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        client
            .delete(format!("{srv_url}/dav/TeamExtra/team.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

fn run_status_with_extra_env(dir: &TempDir, tokens_env: &str) -> std::process::Output {
    let missing_tokens = dir.path().join("missing-tokens.json");
    Command::new(env!("CARGO_BIN_EXE_feishu2dav"))
        .current_dir(dir.path())
        .args(["status"])
        .env("FEISHU_APP_ID", "cli_test")
        .env("FEISHU_APP_SECRET", "sec_test")
        .env("FEISHU2DAV_TOKEN_FILE", &missing_tokens)
        .env("FEISHU2DAV_EXTRA_FOLDER_TOKENS", tokens_env)
        .output()
        .unwrap()
}

#[test]
fn env_extra_folder_tokens_parses_commas_and_rejects_urls() {
    let dir = TempDir::new().unwrap();

    let ok_out = run_status_with_extra_env(&dir, "fld_e1, fld_e2 , fld_e1");
    assert!(
        ok_out.status.success(),
        "expected comma-separated FEISHU2DAV_EXTRA_FOLDER_TOKENS to parse cleanly, stderr={}",
        String::from_utf8_lossy(&ok_out.stderr)
    );

    let bad_out =
        run_status_with_extra_env(&dir, "https://my.feishu.cn/drive/folder/fld_bad");
    assert!(!bad_out.status.success());
    let stderr = String::from_utf8_lossy(&bad_out.stderr);
    assert!(
        stderr.contains("invalid extra_folder_tokens"),
        "expected validation error for URL in env var, got: {stderr}"
    );
}
