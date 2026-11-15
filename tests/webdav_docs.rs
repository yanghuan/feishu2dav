mod common;

use reqwest::StatusCode;
use tempfile::TempDir;

use common::{
    MockNode, TestServerOpts, sample_token, spawn_mock_feishu, spawn_test_server,
    spawn_test_server_with,
};
use feishu2dav::config::DocMode;

#[tokio::test]
async fn cloud_doc_overwrite_protection_concurrent_export_and_doc_modes() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    let dir_export = TempDir::new().unwrap();
    let (srv_export, _) = spawn_test_server(&mock_base, &dir_export, None, Some(sample_token())).await;
    let client = reqwest::Client::new();

    // Concurrent GETs of the same ExportDoc coalesce into a single export_tasks creation
    let mut gets = Vec::new();
    for _ in 0..4 {
        gets.push(client.get(format!("{srv_export}/dav/DesignSpec.docx")).send());
    }
    for fut in gets {
        let resp = fut.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.text().await.unwrap(), "EXPORTED(docx:doc_init:docx)");
    }
    assert_eq!(mock.lock().await.exports.len(), 1);

    // Overwriting an ExportDoc or UrlShortcut via PUT must fail with 403 Forbidden
    for target in ["DesignSpec.docx", "PitchDeck.url"] {
        let put_doc = client
            .put(format!("{srv_export}/dav/{target}"))
            .body("overwrite attempt")
            .send()
            .await
            .unwrap();
        assert_eq!(put_doc.status(), StatusCode::FORBIDDEN, "PUT over {target} must be blocked");
    }

    // Overwriting an ExportDoc / UrlShortcut via MOVE / COPY must be blocked before pre-delete
    client
        .put(format!("{srv_export}/dav/scratch.txt"))
        .body("temp")
        .send()
        .await
        .unwrap();
    let move_m = reqwest::Method::from_bytes(b"MOVE").unwrap();
    let copy_m = reqwest::Method::from_bytes(b"COPY").unwrap();
    let mv_over = client
        .request(move_m.clone(), format!("{srv_export}/dav/scratch.txt"))
        .header("Destination", format!("{srv_export}/dav/DesignSpec.docx"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        mv_over.status(),
        StatusCode::FORBIDDEN,
        "MOVE overwrite (Overwrite: T) over cloud doc must reject with 403"
    );
    let mv_over_f = client
        .request(move_m, format!("{srv_export}/dav/scratch.txt"))
        .header("Destination", format!("{srv_export}/dav/DesignSpec.docx"))
        .header("Overwrite", "F")
        .send()
        .await
        .unwrap();
    assert_eq!(
        mv_over_f.status(),
        StatusCode::PRECONDITION_FAILED,
        "MOVE overwrite (Overwrite: F) over cloud doc must return 412"
    );
    let cp_over = client
        .request(copy_m, format!("{srv_export}/dav/scratch.txt"))
        .header("Destination", format!("{srv_export}/dav/PitchDeck.url"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        cp_over.status(),
        StatusCode::FORBIDDEN,
        "COPY overwrite over shortcut must reject with 403"
    );
    assert!(mock.lock().await.nodes.contains_key("doc_init"));
    assert!(mock.lock().await.nodes.contains_key("slides_init"));

    // Recursive DELETE of a folder containing cloud docs + shortcuts + regular files must succeed
    let mkcol_m = reqwest::Method::from_bytes(b"MKCOL").unwrap();
    let mk_mixed = client
        .request(mkcol_m, format!("{srv_export}/dav/MixedFolder"))
        .send()
        .await
        .unwrap();
    assert_eq!(mk_mixed.status(), StatusCode::CREATED);
    let mixed_token = {
        let mut s = mock.lock().await;
        let tok = s
            .nodes
            .values()
            .find(|n| n.name == "MixedFolder" && n.parent_token == "fld_root")
            .unwrap()
            .token
            .clone();
        s.nodes.insert(
            "doc_in_mixed".to_string(),
            MockNode {
                token: "doc_in_mixed".to_string(),
                name: "FolderNote".to_string(),
                file_type: "docx".to_string(),
                parent_token: tok.clone(),
                content: Vec::new(),
                shortcut_target: None,
                url: Some("https://feishu.cn/docx/doc_in_mixed".to_string()),
                created_time: 1_700_000_001,
                modified_time: 1_700_000_001,
            },
        );
        s.nodes.insert(
            "url_in_mixed".to_string(),
            MockNode {
                token: "url_in_mixed".to_string(),
                name: "FolderSlides".to_string(),
                file_type: "slides".to_string(),
                parent_token: tok.clone(),
                content: Vec::new(),
                shortcut_target: None,
                url: Some("https://feishu.cn/slides/url_in_mixed".to_string()),
                created_time: 1_700_000_002,
                modified_time: 1_700_000_002,
            },
        );
        s.nodes.insert(
            "bin_in_mixed".to_string(),
            MockNode {
                token: "bin_in_mixed".to_string(),
                name: "notes.txt".to_string(),
                file_type: "file".to_string(),
                parent_token: tok.clone(),
                content: b"inside mixed folder".to_vec(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_003,
                modified_time: 1_700_000_003,
            },
        );
        tok
    };
    let del_mixed = client
        .delete(format!("{srv_export}/dav/MixedFolder"))
        .send()
        .await
        .unwrap();
    assert!(
        del_mixed.status().is_success(),
        "recursive DELETE of folder with cloud docs should succeed (got {})",
        del_mixed.status()
    );
    {
        let s = mock.lock().await;
        assert!(!s.nodes.contains_key(&mixed_token));
        assert!(!s.nodes.contains_key("doc_in_mixed"));
        assert!(!s.nodes.contains_key("url_in_mixed"));
        assert!(!s.nodes.contains_key("bin_in_mixed"));
    }

    // Verify DocMode::Url exposes docx as .url
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let dir_url = TempDir::new().unwrap();
    let (srv_url_mode, _) = spawn_test_server_with(
        &mock_base,
        &dir_url,
        TestServerOpts {
            doc_mode: DocMode::Url,
            preauth_token: Some(sample_token()),
            ..Default::default()
        },
    )
    .await;
    let pf_url = client
        .request(propfind.clone(), format!("{srv_url_mode}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(pf_url.contains("DesignSpec.url"));
    assert!(!pf_url.contains("DesignSpec.docx"));

    // Verify DocMode::FilesOnly hides cloud docs completely
    let dir_files = TempDir::new().unwrap();
    let (srv_files_mode, _) = spawn_test_server_with(
        &mock_base,
        &dir_files,
        TestServerOpts {
            doc_mode: DocMode::FilesOnly,
            preauth_token: Some(sample_token()),
            ..Default::default()
        },
    )
    .await;
    let pf_files = client
        .request(propfind, format!("{srv_files_mode}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(pf_files.contains("readme.txt"));
    assert!(pf_files.contains("SharedAlias"));
    assert!(!pf_files.contains("DesignSpec"));
    assert!(!pf_files.contains("PitchDeck"));
}

#[tokio::test]
async fn move_and_copy_onto_existing_folder_blocked_preserves_folder_and_cloud_docs() {
    let (mock_base, mock) = spawn_mock_feishu().await;
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
    let mkcol_m = reqwest::Method::from_bytes(b"MKCOL").unwrap();
    let move_m = reqwest::Method::from_bytes(b"MOVE").unwrap();
    let copy_m = reqwest::Method::from_bytes(b"COPY").unwrap();

    // Create Victim folder and populate a cloud docx, a .url slides shortcut, and a regular file
    assert_eq!(
        client
            .request(mkcol_m.clone(), format!("{srv_url}/dav/Victim"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    assert_eq!(
        client
            .request(mkcol_m, format!("{srv_url}/dav/OtherDir"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    let victim_token = {
        let mut s = mock.lock().await;
        let tok = s
            .nodes
            .values()
            .find(|n| n.name == "Victim" && n.parent_token == "fld_root")
            .unwrap()
            .token
            .clone();
        s.nodes.insert(
            "victim_doc".to_string(),
            MockNode {
                token: "victim_doc".to_string(),
                name: "VictimDoc".to_string(),
                file_type: "docx".to_string(),
                parent_token: tok.clone(),
                content: Vec::new(),
                shortcut_target: None,
                url: Some("https://feishu.cn/docx/victim_doc".to_string()),
                created_time: 1_700_000_010,
                modified_time: 1_700_000_010,
            },
        );
        s.nodes.insert(
            "victim_slides".to_string(),
            MockNode {
                token: "victim_slides".to_string(),
                name: "VictimSlides".to_string(),
                file_type: "slides".to_string(),
                parent_token: tok.clone(),
                content: Vec::new(),
                shortcut_target: None,
                url: Some("https://feishu.cn/slides/victim_slides".to_string()),
                created_time: 1_700_000_011,
                modified_time: 1_700_000_011,
            },
        );
        s.nodes.insert(
            "victim_bin".to_string(),
            MockNode {
                token: "victim_bin".to_string(),
                name: "keep.txt".to_string(),
                file_type: "file".to_string(),
                parent_token: tok.clone(),
                content: b"must-not-vanish".to_vec(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_012,
                modified_time: 1_700_000_012,
            },
        );
        tok
    };

    assert_eq!(
        client
            .put(format!("{srv_url}/dav/src.txt"))
            .body("attacker-file")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );

    // 1. MOVE file -> existing directory (default Overwrite: T) must be rejected with 403
    let mv_file_dir = client
        .request(move_m.clone(), format!("{srv_url}/dav/src.txt"))
        .header("Destination", format!("{srv_url}/dav/Victim"))
        .send()
        .await
        .unwrap();
    assert_eq!(mv_file_dir.status(), StatusCode::FORBIDDEN);

    // 2. MOVE file -> existing directory (Overwrite: F) returns 412 Precondition Failed
    let mv_file_dir_f = client
        .request(move_m.clone(), format!("{srv_url}/dav/src.txt"))
        .header("Destination", format!("{srv_url}/dav/Victim/"))
        .header("Overwrite", "F")
        .send()
        .await
        .unwrap();
    assert_eq!(mv_file_dir_f.status(), StatusCode::PRECONDITION_FAILED);

    // 3. COPY file -> existing directory (default Overwrite: T) must be rejected with 403
    let cp_file_dir = client
        .request(copy_m, format!("{srv_url}/dav/src.txt"))
        .header("Destination", format!("{srv_url}/dav/Victim"))
        .send()
        .await
        .unwrap();
    assert_eq!(cp_file_dir.status(), StatusCode::FORBIDDEN);

    // 4. MOVE directory -> existing directory must also be rejected with 403
    let mv_dir_dir = client
        .request(move_m, format!("{srv_url}/dav/OtherDir"))
        .header("Destination", format!("{srv_url}/dav/Victim"))
        .send()
        .await
        .unwrap();
    assert_eq!(mv_dir_dir.status(), StatusCode::FORBIDDEN);

    // Verify Victim directory, cloud docs, binary file inside it, and src.txt all survived intact
    {
        let s = mock.lock().await;
        assert!(s.nodes.contains_key(&victim_token));
        assert!(s.nodes.contains_key("victim_doc"));
        assert!(s.nodes.contains_key("victim_slides"));
        assert!(s.nodes.contains_key("victim_bin"));
    }
    let keep = client
        .get(format!("{srv_url}/dav/Victim/keep.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(keep.status(), StatusCode::OK);
    assert_eq!(keep.text().await.unwrap(), "must-not-vanish");

    let src = client
        .get(format!("{srv_url}/dav/src.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(src.status(), StatusCode::OK);
    assert_eq!(src.text().await.unwrap(), "attacker-file");
}

#[tokio::test]
async fn root_prefix_blocks_move_onto_existing_folder_and_cloud_doc() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            dav_prefix: "/",
            preauth_token: Some(sample_token()),
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let mkcol_m = reqwest::Method::from_bytes(b"MKCOL").unwrap();
    let move_m = reqwest::Method::from_bytes(b"MOVE").unwrap();
    let copy_m = reqwest::Method::from_bytes(b"COPY").unwrap();

    assert_eq!(
        client
            .request(mkcol_m, format!("{srv_url}/RootVictim"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );
    let victim_token = {
        let mut s = mock.lock().await;
        let tok = s
            .nodes
            .values()
            .find(|n| n.name == "RootVictim" && n.parent_token == "fld_root")
            .unwrap()
            .token
            .clone();
        s.nodes.insert(
            "doc_root_victim".to_string(),
            MockNode {
                token: "doc_root_victim".to_string(),
                name: "NestedDoc".to_string(),
                file_type: "docx".to_string(),
                parent_token: tok.clone(),
                content: Vec::new(),
                shortcut_target: None,
                url: Some("https://feishu.cn/docx/doc_root_victim".to_string()),
                created_time: 1_700_000_020,
                modified_time: 1_700_000_020,
            },
        );
        tok
    };
    assert_eq!(
        client
            .put(format!("{srv_url}/src_root.txt"))
            .body("keep-root")
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::CREATED
    );

    // MOVE onto existing folder under --dav-prefix / -> 403
    let mv_dir = client
        .request(move_m.clone(), format!("{srv_url}/src_root.txt"))
        .header("Destination", format!("{srv_url}/RootVictim"))
        .send()
        .await
        .unwrap();
    assert_eq!(mv_dir.status(), StatusCode::FORBIDDEN);

    // MOVE onto cloud doc under --dav-prefix / -> 403
    let mv_doc = client
        .request(move_m, format!("{srv_url}/src_root.txt"))
        .header("Destination", "/DesignSpec.docx")
        .send()
        .await
        .unwrap();
    assert_eq!(mv_doc.status(), StatusCode::FORBIDDEN);

    // COPY onto existing folder with Overwrite: F under --dav-prefix / -> 412
    let cp_dir_f = client
        .request(copy_m, format!("{srv_url}/src_root.txt"))
        .header("Destination", format!("{srv_url}/RootVictim"))
        .header("Overwrite", "F")
        .send()
        .await
        .unwrap();
    assert_eq!(cp_dir_f.status(), StatusCode::PRECONDITION_FAILED);

    {
        let s = mock.lock().await;
        assert!(s.nodes.contains_key(&victim_token));
        assert!(s.nodes.contains_key("doc_root_victim"));
        assert!(s.nodes.contains_key("doc_init"));
    }
    let src = client
        .get(format!("{srv_url}/src_root.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(src.status(), StatusCode::OK);
    assert_eq!(src.text().await.unwrap(), "keep-root");
}
