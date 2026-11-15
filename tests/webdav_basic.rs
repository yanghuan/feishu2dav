mod common;

use std::sync::atomic::Ordering;
use std::time::Duration;

use bytes::Bytes;
use reqwest::{StatusCode, header};
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use common::{
    MockNode, TestServerOpts, now_secs, sample_token, spawn_mock_feishu, spawn_test_server,
    spawn_test_server_with,
};
use feishu2dav::oauth::TokenSet;

#[tokio::test]
async fn webdav_crud_streaming_range_multipart_and_docs() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let initial_tok = TokenSet {
        access_token: "u-access-initial".to_string(),
        refresh_token: Some("ur-refresh-initial".to_string()),
        expires_at: now_secs() + 7200,
        refresh_expires_at: Some(now_secs() + 604800),
        scope: Some("drive:drive drive:export:readonly offline_access".to_string()),
        token_type: "Bearer".to_string(),
        obtained_at: now_secs(),
        user_name: Some("Alice".to_string()),
        open_id: None,
    };
    let (srv_url, _) =
        spawn_test_server(&mock_base, &dir, Some(("davuser", "davpass")), Some(initial_tok)).await;

    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let mkcol = reqwest::Method::from_bytes(b"MKCOL").unwrap();
    let move_m = reqwest::Method::from_bytes(b"MOVE").unwrap();
    let copy_m = reqwest::Method::from_bytes(b"COPY").unwrap();

    // 1. Basic auth required for WebDAV
    let unauth = client
        .request(propfind.clone(), format!("{srv_url}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // 2. PROPFIND root lists binary files, exported docs, slides .url, and shortcut folders
    let pf_resp = client
        .request(propfind.clone(), format!("{srv_url}/dav/"))
        .basic_auth("davuser", Some("davpass"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(pf_resp.status(), StatusCode::MULTI_STATUS);
    let pf_xml = pf_resp.text().await.unwrap();
    assert!(pf_xml.contains("readme.txt"));
    assert!(pf_xml.contains("DesignSpec.docx"));
    assert!(pf_xml.contains("PitchDeck.url"));
    assert!(pf_xml.contains("SharedAlias"));

    // 3. Shortcut folder traversal
    let sc_get = client
        .get(format!("{srv_url}/dav/SharedAlias/shared.txt"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(sc_get.status(), StatusCode::OK);
    assert_eq!(sc_get.text().await.unwrap(), "shared-via-shortcut");

    // 4. Reactive token expiry (99991677) + rate limit (1061045) transparent recovery
    {
        let mut s = mock.lock().await;
        s.force_expired_once = true;
    }
    let range_get = client
        .get(format!("{srv_url}/dav/readme.txt"))
        .basic_auth("davuser", Some("davpass"))
        .header(header::RANGE, "bytes=6-11")
        .send()
        .await
        .unwrap();
    assert_eq!(range_get.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range_get.text().await.unwrap(), "Feishu");

    {
        let mut s = mock.lock().await;
        s.force_rate_limit_once = true;
    }
    let url_file = client
        .get(format!("{srv_url}/dav/PitchDeck.url"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(url_file.status(), StatusCode::OK);
    assert!(
        url_file
            .text()
            .await
            .unwrap()
            .contains("https://feishu.cn/slides/slides_init")
    );

    // 5. Exportable online doc GET
    let doc_get = client
        .get(format!("{srv_url}/dav/DesignSpec.docx"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(doc_get.status(), StatusCode::OK);
    assert_eq!(doc_get.text().await.unwrap(), "EXPORTED(docx:doc_init:docx)");

    // 6. MKCOL + duplicate MKCOL check
    let mk_ok = client
        .request(mkcol.clone(), format!("{srv_url}/dav/Work"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(mk_ok.status(), StatusCode::CREATED);

    let mk_dup = client
        .request(mkcol, format!("{srv_url}/dav/Work"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(mk_dup.status(), StatusCode::METHOD_NOT_ALLOWED);

    // 7. PUT small file, overwrite, and 0-byte fallback
    let put1 = client
        .put(format!("{srv_url}/dav/Work/hello.txt"))
        .basic_auth("davuser", Some("davpass"))
        .body("version-1")
        .send()
        .await
        .unwrap();
    assert_eq!(put1.status(), StatusCode::CREATED);

    let put_overwrite = client
        .put(format!("{srv_url}/dav/Work/hello.txt"))
        .basic_auth("davuser", Some("davpass"))
        .body("version-2-updated")
        .send()
        .await
        .unwrap();
    assert_eq!(put_overwrite.status(), StatusCode::NO_CONTENT);

    let get_v2 = client
        .get(format!("{srv_url}/dav/Work/hello.txt"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_v2.text().await.unwrap(), "version-2-updated");

    // 0-byte upload fallback when Feishu rejects 0-byte upload_all
    let put_zero = client
        .put(format!("{srv_url}/dav/Work/empty.txt"))
        .basic_auth("davuser", Some("davpass"))
        .body(Bytes::new())
        .send()
        .await
        .unwrap();
    assert_eq!(put_zero.status(), StatusCode::CREATED);
    let get_zero = client
        .get(format!("{srv_url}/dav/Work/empty.txt"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_zero.status(), StatusCode::OK);
    assert!(get_zero.bytes().await.unwrap().is_empty());

    // 8. Large >20MB file multipart upload_prepare/upload_part/upload_finish + download roundtrip
    let big_size = 21 * 1024 * 1024 + 123;
    let big_data: Vec<u8> = (0..big_size).map(|i| (i % 251) as u8).collect();
    let put_big = client
        .put(format!("{srv_url}/dav/Work/big.bin"))
        .basic_auth("davuser", Some("davpass"))
        .timeout(Duration::from_secs(60))
        .body(big_data.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(put_big.status(), StatusCode::CREATED);

    let get_big = client
        .get(format!("{srv_url}/dav/Work/big.bin"))
        .basic_auth("davuser", Some("davpass"))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .unwrap();
    assert_eq!(get_big.status(), StatusCode::OK);
    let downloaded_big = get_big.bytes().await.unwrap();
    assert_eq!(downloaded_big.len(), big_size);
    assert_eq!(downloaded_big.as_ref(), big_data.as_slice());

    // 9. File rename, COPY, and folder rename (MOVE)
    let rename_file = client
        .request(move_m.clone(), format!("{srv_url}/dav/Work/hello.txt"))
        .basic_auth("davuser", Some("davpass"))
        .header("Destination", format!("{srv_url}/dav/Work/renamed.txt"))
        .send()
        .await
        .unwrap();
    assert!(rename_file.status().is_success());

    let copy_file = client
        .request(copy_m, format!("{srv_url}/dav/Work/renamed.txt"))
        .basic_auth("davuser", Some("davpass"))
        .header("Destination", format!("{srv_url}/dav/copied.txt"))
        .send()
        .await
        .unwrap();
    assert!(copy_file.status().is_success());
    assert_eq!(
        client
            .get(format!("{srv_url}/dav/copied.txt"))
            .basic_auth("davuser", Some("davpass"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "version-2-updated"
    );

    let rename_folder = client
        .request(move_m, format!("{srv_url}/dav/Work"))
        .basic_auth("davuser", Some("davpass"))
        .header("Destination", format!("{srv_url}/dav/Projects"))
        .send()
        .await
        .unwrap();
    assert!(rename_folder.status().is_success());

    let after_rename = client
        .get(format!("{srv_url}/dav/Projects/renamed.txt"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(after_rename.status(), StatusCode::OK);
    assert_eq!(after_rename.text().await.unwrap(), "version-2-updated");

    // 10. Recursive DELETE folder
    let del_folder = client
        .delete(format!("{srv_url}/dav/Projects"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert!(del_folder.status().is_success());

    let gone = client
        .get(format!("{srv_url}/dav/Projects/renamed.txt"))
        .basic_auth("davuser", Some("davpass"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn range_200_fallback_partial_put_read_only_and_fault_codes() {
    let (mock_base, mock) = spawn_mock_feishu().await;

    // 1. Unauthorized OAuth returns 401 on WebDAV
    let dir_unauth = TempDir::new().unwrap();
    let (srv_unauth, _) = spawn_test_server(&mock_base, &dir_unauth, None, None).await;
    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let unauth_dav = client
        .request(propfind.clone(), format!("{srv_unauth}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(unauth_dav.status(), StatusCode::UNAUTHORIZED);

    // 2. Read-only mode blocks mutations with 403 Forbidden
    let dir_ro = TempDir::new().unwrap();
    let (srv_ro, _) = spawn_test_server_with(
        &mock_base,
        &dir_ro,
        TestServerOpts {
            read_only: true,
            preauth_token: Some(sample_token()),
            ..Default::default()
        },
    )
    .await;
    let ro_put = client
        .put(format!("{srv_ro}/dav/new.txt"))
        .body("blocked")
        .send()
        .await
        .unwrap();
    assert_eq!(ro_put.status(), StatusCode::FORBIDDEN);
    let mkcol = reqwest::Method::from_bytes(b"MKCOL").unwrap();
    assert_eq!(
        client
            .request(mkcol, format!("{srv_ro}/dav/NewDir"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );

    // 3. Upstream ignoring Range (returning 200 OK stream) still delivers exact offset bytes
    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            max_upload_mb: 1,
            cache_ttl_secs: 0,
            ..Default::default()
        },
    )
    .await;
    {
        let mut s = mock.lock().await;
        s.ignore_range_on_download = true;
    }
    let range_200 = client
        .get(format!("{srv_url}/dav/readme.txt"))
        .header(header::RANGE, "bytes=6-11")
        .send()
        .await
        .unwrap();
    assert_eq!(range_200.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range_200.text().await.unwrap(), "Feishu");
    mock.lock().await.ignore_range_on_download = false;

    // 4. Streaming partial PUT (Content-Range) patches existing binary file cleanly
    let put_seed = client
        .put(format!("{srv_url}/dav/patch.bin"))
        .body("ABCDEF")
        .send()
        .await
        .unwrap();
    assert_eq!(put_seed.status(), StatusCode::CREATED);

    let patch_put = client
        .put(format!("{srv_url}/dav/patch.bin"))
        .header(header::CONTENT_RANGE, "bytes 2-3/6")
        .body("XY")
        .send()
        .await
        .unwrap();
    assert!(patch_put.status().is_success());
    let after_patch = client
        .get(format!("{srv_url}/dav/patch.bin"))
        .send()
        .await
        .unwrap();
    assert_eq!(after_patch.text().await.unwrap(), "ABXYEF");

    // 5. Upload exceeding max_upload_mb (1 MiB) is rejected early with 413 Payload Too Large
    // Prove header-only rejection without sending the oversized body bytes over TCP
    let addr = srv_url.strip_prefix("http://").unwrap();
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let req_hdrs = format!(
        "PUT /dav/early_413.bin HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 5242880\r\nConnection: close\r\n\r\n"
    );
    tcp.write_all(req_hdrs.as_bytes()).await.unwrap();
    let mut resp_buf = [0u8; 512];
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tcp.read(&mut resp_buf),
    )
    .await
    .expect("server must reject oversized Content-Length immediately without waiting for body")
    .unwrap();
    let head = String::from_utf8_lossy(&resp_buf[..n]);
    assert!(
        head.starts_with("HTTP/1.1 413"),
        "expected immediate 413 response, got: {head}"
    );

    let put_range_too_big = client
        .put(format!("{srv_url}/dav/toolarge_range.bin"))
        .header(header::CONTENT_RANGE, "bytes 0-2097151/*")
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(put_range_too_big.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // Chunked streaming upload (no Content-Length) exceeding max_upload_mb (1 MiB) hits WriteSpool 413
    let stream_chunks = vec![
        Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'z'; 1024 * 1024])),
        Ok::<Bytes, std::io::Error>(Bytes::from(vec![b'z'; 64])),
    ];
    let put_too_big = client
        .put(format!("{srv_url}/dav/toolarge.bin"))
        .body(reqwest::Body::wrap_stream(futures_util::stream::iter(
            stream_chunks,
        )))
        .send()
        .await
        .unwrap();
    assert_eq!(put_too_big.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // 6. Persistent rate-limit on probe/GET returns 503 + Retry-After rather than 200 empty body
    mock.lock().await.force_rate_limit_forever = true;
    let rate_limited_get = client
        .get(format!("{srv_url}/dav/readme.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(rate_limited_get.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(rate_limited_get.headers().contains_key(header::RETRY_AFTER));
}

#[tokio::test]
async fn copy_move_rejects_invalid_or_foreign_destination_fail_closed() {
    let (mock_base, _mock) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    assert_eq!(
        client
            .put(format!("{srv_url}/dav/keep.txt"))
            .body("important")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );

    let move_m = reqwest::Method::from_bytes(b"MOVE").unwrap();
    let copy_m = reqwest::Method::from_bytes(b"COPY").unwrap();

    // Missing Destination header -> 400 Bad Request
    assert_eq!(
        client
            .request(move_m.clone(), format!("{srv_url}/dav/keep.txt"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::BAD_REQUEST
    );

    // Invalid/malformed/traversal/fragment/non-HTTP Destinations -> 400 Bad Request
    for bad_dest in [
        "",
        "not-a-valid-url",
        "//evil.com/dav/keep2.txt",
        "/dav/../../outside",
        &format!("{srv_url}/dav/keep2.txt#frag"),
        "file:///dav/keep2.txt",
    ] {
        for m in [&move_m, &copy_m] {
            let resp = client
                .request(m.clone(), format!("{srv_url}/dav/keep.txt"))
                .header("Destination", bad_dest)
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "expected 400 fail-closed for {m} Destination='{bad_dest}'"
            );
        }
    }

    // Valid URL/path outside mounted --dav-prefix ('/dav') -> 502 Bad Gateway
    for foreign_dest in [
        "/other/keep2.txt".to_string(),
        "/dav/../outside".to_string(),
        "/davother/keep2.txt".to_string(),
        format!("{srv_url}/other/keep2.txt"),
    ] {
        for m in [&move_m, &copy_m] {
            let resp = client
                .request(m.clone(), format!("{srv_url}/dav/keep.txt"))
                .header("Destination", &foreign_dest)
                .send()
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::BAD_GATEWAY,
                "expected 502 fail-closed for {m} Destination='{foreign_dest}'"
            );
        }
    }

    // Source file remains untouched
    let keep = client
        .get(format!("{srv_url}/dav/keep.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(keep.status(), StatusCode::OK);
    assert_eq!(keep.text().await.unwrap(), "important");
}

#[tokio::test]
async fn custom_root_folder_token_mounts_shared_folder_as_dav_root() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    {
        let mut s = mock.lock().await;
        s.nodes.insert(
            "shared_note".to_string(),
            MockNode {
                token: "shared_note".to_string(),
                name: "shared.txt".to_string(),
                file_type: "file".to_string(),
                parent_token: "fld_shared_custom".to_string(),
                content: b"cross-tenant-ok".to_vec(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_000,
                modified_time: 1_700_000_000,
            },
        );
        s.nodes.insert(
            "fld_shared_sub".to_string(),
            MockNode {
                token: "fld_shared_sub".to_string(),
                name: "sub".to_string(),
                file_type: "folder".to_string(),
                parent_token: "fld_shared_custom".to_string(),
                content: Vec::new(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_000,
                modified_time: 1_700_000_000,
            },
        );
    }

    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            root_folder_token: Some("fld_shared_custom".to_string()),
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let list_resp = client
        .request(propfind, format!("{srv_url}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::MULTI_STATUS);
    let list_xml = list_resp.text().await.unwrap();
    assert!(list_xml.contains("shared.txt"));
    assert!(list_xml.contains("sub"));
    assert!(!list_xml.contains("readme.txt"));

    let get_resp = client
        .get(format!("{srv_url}/dav/shared.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    assert_eq!(get_resp.text().await.unwrap(), "cross-tenant-ok");

    let put_resp = client
        .put(format!("{srv_url}/dav/uploaded.txt"))
        .body("written-to-custom-root")
        .send()
        .await
        .unwrap();
    assert!(put_resp.status().is_success());

    let get_uploaded = client
        .get(format!("{srv_url}/dav/uploaded.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_uploaded.status(), StatusCode::OK);
    assert_eq!(get_uploaded.text().await.unwrap(), "written-to-custom-root");

    let dash_html = client
        .get(format!("{srv_url}/"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(dash_html.contains("fld_shared_custom"));

    let s = mock.lock().await;
    assert_eq!(s.root_meta_calls.load(Ordering::SeqCst), 0);
    assert!(
        s.nodes
            .values()
            .any(|n| n.name == "uploaded.txt" && n.parent_token == "fld_shared_custom"),
        "expected uploaded file parent_token to match configured custom root"
    );
}
