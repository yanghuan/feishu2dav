mod common;

use std::sync::atomic::Ordering;

use reqwest::{StatusCode, header};
use serde_json::json;
use tempfile::TempDir;

use common::{MockNode, TestServerOpts, sample_token, spawn_mock_feishu, spawn_test_server_with};
use feishu2dav::config::DocMode;

#[tokio::test]
async fn markdown_mode_exposes_exact_size_reads_edits_in_place_and_uploads_new_md_as_file() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            doc_mode: DocMode::Markdown,
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();

    // 1. Initial GET returns rendered Markdown
    let initial_md = "# Design Spec\n\nInitial spec body\n";
    let get_resp = client
        .get(format!("{srv_url}/dav/DesignSpec.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    assert_eq!(get_resp.text().await.unwrap(), initial_md);

    // 2. PROPFIND reports exact non-zero content length for .md (never 0 KB)
    let list_resp = client
        .request(propfind.clone(), format!("{srv_url}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status(), StatusCode::MULTI_STATUS);
    let list_xml = list_resp.text().await.unwrap();
    assert!(list_xml.contains("DesignSpec.md"));
    assert!(!list_xml.contains("DesignSpec.docx"));
    assert!(list_xml.contains("PitchDeck.url"));
    let expected_len_tag = format!("<D:getcontentlength>{}</D:getcontentlength>", initial_md.len());
    assert!(
        list_xml.contains(&expected_len_tag),
        "expected exact non-zero size {expected_len_tag} in PROPFIND XML, got: {list_xml}"
    );

    // 3. Editing existing .md via PUT updates Feishu docx IN-PLACE (keeps doc_init token)
    let updated_md = "\
# Updated Spec

Paragraph with **bold** and *italic* and `inline` and [Docs](https://open.feishu.cn/docs).

- Parent item
  - Nested child
- [x] Ship Markdown mode
- [ ] Drink tea

1. Step one
2. Step two

> Important note

```rust
fn hello() {}
```

| Key | Value |
| --- | --- |
| mode | markdown |

---
";
    let put_edit = client
        .put(format!("{srv_url}/dav/DesignSpec.md"))
        .body(updated_md)
        .send()
        .await
        .unwrap();
    assert!(
        put_edit.status().is_success(),
        "expected PUT overwrite of editable docx to succeed, got {}",
        put_edit.status()
    );

    {
        let s = mock.lock().await;
        assert!(
            s.nodes.contains_key("doc_init"),
            "doc_init token must remain in-place"
        );
    }

    let get_after = client
        .get(format!("{srv_url}/dav/DesignSpec.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(get_after.status(), StatusCode::OK);
    let body_after = get_after.text().await.unwrap();
    assert!(body_after.contains("# Updated Spec"));
    assert!(body_after.contains("**bold**"));
    assert!(body_after.contains("- Parent item\n  - Nested child"));
    assert!(body_after.contains("- [x] Ship Markdown mode\n- [ ] Drink tea"));
    assert!(body_after.contains("1. Step one\n2. Step two"));
    assert!(body_after.contains("> Important note"));
    assert!(body_after.contains("```rust\nfn hello() {}"));
    assert!(body_after.contains("| Key | Value |"));
    assert!(body_after.contains("---"));

    // Single-file HEAD/PROPFIND Depth: 0 reflects updated size
    let depth0 = client
        .request(propfind, format!("{srv_url}/dav/DesignSpec.md"))
        .header("Depth", "0")
        .send()
        .await
        .unwrap();
    assert_eq!(depth0.status(), StatusCode::MULTI_STATUS);
    let depth0_xml = depth0.text().await.unwrap();
    assert!(depth0_xml.contains(&format!(
        "<D:getcontentlength>{}</D:getcontentlength>",
        body_after.len()
    )));

    // Clearing existing .md document to empty text updates in-place instead of uploading duplicate binary
    let put_empty = client
        .put(format!("{srv_url}/dav/DesignSpec.md"))
        .body("")
        .send()
        .await
        .unwrap();
    assert!(put_empty.status().is_success());
    assert_eq!(
        client
            .get(format!("{srv_url}/dav/DesignSpec.md"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        ""
    );

    // 4. Creating a brand-new .md uploads as ordinary binary file
    let put_new = client
        .put(format!("{srv_url}/dav/brand_new.md"))
        .body("# Standalone File\n")
        .send()
        .await
        .unwrap();
    assert_eq!(put_new.status(), StatusCode::CREATED);
    assert_eq!(
        client
            .get(format!("{srv_url}/dav/brand_new.md"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap(),
        "# Standalone File\n"
    );

    let s = mock.lock().await;
    assert!(s.nodes.values().any(|n| {
        n.name == "brand_new.md" && n.file_type == "file" && n.content == b"# Standalone File\n"
    }));
}

#[tokio::test]
async fn markdown_mode_blocks_rich_media_overwrite_with_friendly_403_and_blocks_move_overwrite() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    {
        let mut s = mock.lock().await;
        s.nodes.insert(
            "doc_rich".to_string(),
            MockNode {
                token: "doc_rich".to_string(),
                name: "RichMedia".to_string(),
                file_type: "docx".to_string(),
                parent_token: "fld_root".to_string(),
                content: Vec::new(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_000,
                modified_time: 1_700_000_100,
            },
        );
        s.docx_blocks.insert(
            "doc_rich".to_string(),
            vec![
                json!({
                    "block_id": "doc_rich",
                    "block_type": 1,
                    "parent_id": "",
                    "children": ["b_text", "b_img", "b_board"]
                }),
                json!({
                    "block_id": "b_text",
                    "block_type": 2,
                    "parent_id": "doc_rich",
                    "children": [],
                    "text": { "elements": [{ "text_run": { "content": "Doc with assets" } }] }
                }),
                json!({
                    "block_id": "b_img",
                    "block_type": 27,
                    "parent_id": "doc_rich",
                    "children": [],
                    "image": { "token": "img_asset_1" }
                }),
                json!({
                    "block_id": "b_board",
                    "block_type": 43,
                    "parent_id": "doc_rich",
                    "children": [],
                    "board": { "token": "brd_asset_1" }
                }),
            ],
        );

        // Plain doc that gains an image remotely after cache was warmed (TOCTOU test)
        s.nodes.insert(
            "doc_late".to_string(),
            MockNode {
                token: "doc_late".to_string(),
                name: "LateImage".to_string(),
                file_type: "docx".to_string(),
                parent_token: "fld_root".to_string(),
                content: Vec::new(),
                shortcut_target: None,
                url: None,
                created_time: 1_700_000_000,
                modified_time: 1_700_000_100,
            },
        );
        s.docx_blocks.insert(
            "doc_late".to_string(),
            vec![
                json!({
                    "block_id": "doc_late",
                    "block_type": 1,
                    "parent_id": "",
                    "children": ["l_p"]
                }),
                json!({
                    "block_id": "l_p",
                    "block_type": 2,
                    "parent_id": "doc_late",
                    "children": [],
                    "text": { "elements": [{ "text_run": { "content": "Initially plain" } }] }
                }),
            ],
        );
    }

    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            doc_mode: DocMode::Markdown,
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let move_m = reqwest::Method::from_bytes(b"MOVE").unwrap();

    // 1. Rich-media document is still readable as Markdown
    let rich_get = client
        .get(format!("{srv_url}/dav/RichMedia.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(rich_get.status(), StatusCode::OK);
    let rich_md = rich_get.text().await.unwrap();
    assert!(rich_md.contains("Doc with assets"));
    assert!(rich_md.contains("![image](feishu-image://img_asset_1)"));

    // 2. Overwriting rich-media .md returns 403 with friendly UTF-8 Chinese explanation
    let rich_put = client
        .put(format!("{srv_url}/dav/RichMedia.md"))
        .body("# Attempted overwrite")
        .send()
        .await
        .unwrap();
    assert_eq!(rich_put.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        rich_put
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; charset=utf-8")
    );
    let err_body = rich_put.text().await.unwrap();
    assert!(
        err_body.contains("禁止覆盖写回") && err_body.contains("图片 (image)"),
        "expected friendly Chinese 403 message, got: {err_body}"
    );

    // 3. Warm cache for LateImage.md, then add an image remotely in Feishu before PUT flush:
    // flush() re-checks live blocks and refuses overwrite without deleting existing blocks!
    assert_eq!(
        client
            .get(format!("{srv_url}/dav/LateImage.md"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    {
        let mut s = mock.lock().await;
        s.docx_blocks.insert(
            "doc_late".to_string(),
            vec![
                json!({
                    "block_id": "doc_late",
                    "block_type": 1,
                    "parent_id": "",
                    "children": ["l_p", "l_img"]
                }),
                json!({
                    "block_id": "l_p",
                    "block_type": 2,
                    "parent_id": "doc_late",
                    "children": [],
                    "text": { "elements": [{ "text_run": { "content": "Initially plain" } }] }
                }),
                json!({
                    "block_id": "l_img",
                    "block_type": 27,
                    "parent_id": "doc_late",
                    "children": [],
                    "image": { "token": "img_new_live" }
                }),
            ],
        );
    }
    let late_put = client
        .put(format!("{srv_url}/dav/LateImage.md"))
        .body("# Clobber attempt")
        .send()
        .await
        .unwrap();
    assert_eq!(late_put.status(), StatusCode::FORBIDDEN);
    let late_body = late_put.text().await.unwrap();
    assert!(late_body.contains("禁止覆盖写回") && late_body.contains("图片 (image)"));
    assert_eq!(
        mock.lock().await.docx_blocks["doc_late"].len(),
        3,
        "live image and text blocks must remain untouched"
    );

    // 4. MOVE overwriting an existing .md cloud doc stays blocked (403)
    let move_over_doc = client
        .request(move_m, format!("{srv_url}/dav/readme.txt"))
        .header("Destination", format!("{srv_url}/dav/DesignSpec.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(move_over_doc.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn markdown_save_failure_preserves_original_document_and_rolls_back_partial_batches() {
    let (mock_base, mock) = spawn_mock_feishu().await;
    let dir = TempDir::new().unwrap();
    let (srv_url, _) = spawn_test_server_with(
        &mock_base,
        &dir,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            doc_mode: DocMode::Markdown,
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let original_md = "# Design Spec\n\nInitial spec body\n";

    // Warm markdown cache first
    let initial_get = client
        .get(format!("{srv_url}/dav/DesignSpec.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(initial_get.status(), StatusCode::OK);
    assert_eq!(initial_get.text().await.unwrap(), original_md);

    // 1. Single-batch descendant failure leaves original blocks untouched and invalidates cache safely
    mock.lock().await.fail_docx_descendant_after = Some(0);
    let fail_single = client
        .put(format!("{srv_url}/dav/DesignSpec.md"))
        .body("# Replacement That Fails\n")
        .send()
        .await
        .unwrap();
    assert!(!fail_single.status().is_success());

    {
        let s = mock.lock().await;
        assert_eq!(
            s.docx_blocks["doc_init"].len(),
            3,
            "original blocks must stay untouched when batch 0 insert fails"
        );
    }
    let after_single_fail = client
        .get(format!("{srv_url}/dav/DesignSpec.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(after_single_fail.status(), StatusCode::OK);
    assert_eq!(after_single_fail.text().await.unwrap(), original_md);

    // 2. Multi-batch (>900 blocks -> 2 batches) failure on batch 2 rolls back batch 1 cleanly
    let mut big_md = String::new();
    for i in 0..950 {
        big_md.push_str(&format!("Paragraph {i}\n\n"));
    }
    {
        let mut s = mock.lock().await;
        s.fail_docx_descendant_after = Some(1);
        s.docx_descendant_calls.store(0, Ordering::Relaxed);
    }
    let fail_multi = client
        .put(format!("{srv_url}/dav/DesignSpec.md"))
        .body(big_md)
        .send()
        .await
        .unwrap();
    assert!(!fail_multi.status().is_success());

    {
        let s = mock.lock().await;
        assert_eq!(
            s.docx_descendant_calls.load(Ordering::Relaxed),
            2,
            "expected first batch to succeed and second batch to fail"
        );
        assert_eq!(
            s.docx_blocks["doc_init"].len(),
            3,
            "partial tail batch must be rolled back so original document remains pristine"
        );
    }
    let after_multi_fail = client
        .get(format!("{srv_url}/dav/DesignSpec.md"))
        .send()
        .await
        .unwrap();
    assert_eq!(after_multi_fail.status(), StatusCode::OK);
    assert_eq!(after_multi_fail.text().await.unwrap(), original_md);
}

#[tokio::test]
async fn markdown_directory_probe_respects_per_dir_limit_and_disable_flag() {
    let seed_extra_docs = |s: &mut common::MockFeishuState| {
        for i in 0..8 {
            let tok = format!("doc_bulk_{i}");
            let blk = format!("blk_bulk_{i}");
            s.nodes.insert(
                tok.clone(),
                MockNode {
                    token: tok.clone(),
                    name: format!("BulkDoc_{i}"),
                    file_type: "docx".to_string(),
                    parent_token: "fld_root".to_string(),
                    content: Vec::new(),
                    shortcut_target: None,
                    url: None,
                    created_time: 1_700_000_000,
                    modified_time: 1_700_000_100,
                },
            );
            s.docx_blocks.insert(
                tok.clone(),
                vec![
                    json!({
                        "block_id": tok,
                        "block_type": 1,
                        "parent_id": "",
                        "children": [blk]
                    }),
                    json!({
                        "block_id": blk,
                        "block_type": 2,
                        "parent_id": tok,
                        "children": [],
                        "text": { "elements": [{ "text_run": { "content": format!("Body {i}") } }] }
                    }),
                ],
            );
        }
    };

    // 1. Default probe_markdown_size=true probes at most 5 uncached Markdown docs per PROPFIND
    let (mock_base_cap, mock_cap) = spawn_mock_feishu().await;
    seed_extra_docs(&mut *mock_cap.lock().await);
    let dir_cap = TempDir::new().unwrap();
    let (srv_cap, _) = spawn_test_server_with(
        &mock_base_cap,
        &dir_cap,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            doc_mode: DocMode::Markdown,
            probe_markdown_size: true,
            ..Default::default()
        },
    )
    .await;

    let client = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();
    let list_cap = client
        .request(propfind.clone(), format!("{srv_cap}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(list_cap.status(), StatusCode::MULTI_STATUS);
    let _list_cap_xml = list_cap.text().await.unwrap();
    assert_eq!(
        mock_cap.lock().await.docx_blocks_calls.load(Ordering::Relaxed),
        5,
        "PROPFIND should cap uncached Markdown conversions at 5 per directory"
    );

    // 2. probe_markdown_size=false triggers 0 blocks calls on PROPFIND, then populates after GET
    let (mock_base_off, mock_off) = spawn_mock_feishu().await;
    seed_extra_docs(&mut *mock_off.lock().await);
    let dir_off = TempDir::new().unwrap();
    let (srv_off, _) = spawn_test_server_with(
        &mock_base_off,
        &dir_off,
        TestServerOpts {
            preauth_token: Some(sample_token()),
            doc_mode: DocMode::Markdown,
            probe_markdown_size: false,
            cache_ttl_secs: 0,
            ..Default::default()
        },
    )
    .await;

    let list_off = client
        .request(propfind.clone(), format!("{srv_off}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(list_off.status(), StatusCode::MULTI_STATUS);
    let _list_off_xml = list_off.text().await.unwrap();
    assert_eq!(
        mock_off.lock().await.docx_blocks_calls.load(Ordering::Relaxed),
        0,
        "probe_markdown_size=false must not fetch blocks during PROPFIND"
    );

    let design_md = client
        .get(format!("{srv_off}/dav/DesignSpec.md"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        mock_off.lock().await.docx_blocks_calls.load(Ordering::Relaxed),
        1
    );

    let list_after_get = client
        .request(propfind, format!("{srv_off}/dav/"))
        .header("Depth", "1")
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_eq!(
        mock_off.lock().await.docx_blocks_calls.load(Ordering::Relaxed),
        1,
        "subsequent PROPFIND should use cached size without extra blocks API calls"
    );
    assert!(list_after_get.contains(&format!(
        "<D:getcontentlength>{}</D:getcontentlength>",
        design_md.len()
    )));
}
