mod common;

use axum::body::{Body, to_bytes};
use cellar::{
    files::{ByteRange, InvalidByteRange},
    storage::SafeFileName,
};
use http::{HeaderMap, Method, StatusCode, header};
use serde_json::{Value, json};
use time::{OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};
use tower::ServiceExt;
use uuid::Uuid;

use common::{TestContext, authenticated_request, json_response};

fn files(project_id: Uuid) -> String {
    format!("/api/v1/projects/{project_id}/files")
}

fn download(project_id: Uuid, file_name: &str) -> String {
    format!("/api/v1/projects/{project_id}/files/{file_name}")
}

async fn response_bytes(
    response: axum::response::Response,
) -> (StatusCode, HeaderMap, bytes::Bytes) {
    let status = response.status();
    let headers = response.headers().clone();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, headers, body)
}

#[test]
fn byte_range_parser_accepts_exact_single_ranges() {
    assert_eq!(
        ByteRange::parse("bytes=10-19", 100),
        Ok(ByteRange::new(10, 19))
    );
    assert_eq!(
        ByteRange::parse("bytes=10-", 100),
        Ok(ByteRange::new(10, 99))
    );
    assert_eq!(
        ByteRange::parse("bytes=-10", 100),
        Ok(ByteRange::new(90, 99))
    );
    assert_eq!(
        ByteRange::parse("bytes=0-99", 100),
        Ok(ByteRange::new(0, 99))
    );
    assert_eq!(
        ByteRange::parse("bytes=-200", 100),
        Ok(ByteRange::new(0, 99))
    );
    assert_eq!(
        ByteRange::parse("Bytes=10-19", 100),
        Ok(ByteRange::new(10, 19))
    );
    assert_eq!(
        ByteRange::parse("BYTES=-10", 100),
        Ok(ByteRange::new(90, 99))
    );
}

#[test]
fn byte_range_parser_rejects_malformed_or_unsatisfiable_ranges() {
    for value in [
        "bytes=",
        "bytes=-",
        "bytes=1-0",
        "bytes=100-",
        "bytes=-0",
        "items=0-1",
        "bytes=0-1,2-3",
        "bytes= 0-1",
        "bytes=0 -1",
        " bytes=0-1",
        "bytes=18446744073709551616-",
        "bytes=-18446744073709551616",
    ] {
        assert_eq!(
            ByteRange::parse(value, 100),
            Err(InvalidByteRange),
            "{value}"
        );
    }
    assert_eq!(ByteRange::parse("bytes=0-0", 0), Err(InvalidByteRange));
}

#[tokio::test]
async fn listing_reflects_current_safe_regular_disk_files() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Disk files").await;
    let directory = context.project_path(project_id).join("files");
    std::fs::write(directory.join("Alpha.TXT"), b"one").unwrap();
    std::fs::write(directory.join("alpha2.txt"), b"twenty").unwrap();
    std::fs::write(directory.join("beta.bin"), b"1234567").unwrap();
    std::fs::create_dir(directory.join("nested")).unwrap();
    let symlink_created =
        create_test_file_symlink(directory.join("beta.bin"), directory.join("linked.bin"));

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &files(project_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, StatusCode::OK);
    let listed = body.as_array().unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(
        listed
            .iter()
            .map(|file| file["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["Alpha.TXT", "alpha2.txt", "beta.bin"]
    );
    assert_eq!(
        listed
            .iter()
            .map(|file| file["size"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["3", "6", "7"]
    );
    for file in listed {
        assert_eq!(file.as_object().unwrap().len(), 3);
        let timestamp = file["modifiedAt"].as_str().unwrap();
        let parsed = OffsetDateTime::parse(timestamp, &Rfc3339).unwrap();
        assert_eq!(parsed.offset(), UtcOffset::UTC);
        assert!(timestamp.ends_with('Z'));
    }

    if symlink_created {
        let response = context
            .app()
            .oneshot(
                authenticated_request("GET", &download(project_id, "linked.bin"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, _, body) = response_bytes(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_ne!(&body[..], b"1234567");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "file_not_found");
    }

    std::fs::write(directory.join("Alpha.TXT"), b"changed").unwrap();
    std::fs::remove_file(directory.join("beta.bin")).unwrap();
    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &files(project_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (_, _, body) = json_response(response).await;
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(body[0]["size"], "7");
    context.close().await;
}

#[tokio::test]
async fn listing_requires_a_committed_database_project() {
    let context = TestContext::new().await;
    let missing = Uuid::now_v7();
    std::fs::create_dir_all(context.project_path(missing).join("files")).unwrap();
    std::fs::write(
        context.project_path(missing).join("files/orphan.txt"),
        b"orphan",
    )
    .unwrap();

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &files(missing))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, request_id, body) = json_response(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "project_not_found");
    assert_eq!(body["error"]["requestId"], request_id);

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &download(missing, "orphan.txt"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "project_not_found");
    context.close().await;
}

#[tokio::test]
async fn full_get_and_head_return_exact_download_headers() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Downloads").await;
    std::fs::write(
        context.project_path(project_id).join("files/report.txt"),
        b"hello world",
    )
    .unwrap();

    let mut get_headers = None;
    for method in [Method::GET, Method::HEAD] {
        let response = context
            .app()
            .oneshot(
                authenticated_request(method.as_str(), &download(project_id, "report.txt"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, headers, body) = response_bytes(response).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
        assert_eq!(headers[header::CONTENT_LENGTH], "11");
        assert_eq!(headers[header::CONTENT_TYPE], "text/plain");
        assert_eq!(
            headers[header::CONTENT_DISPOSITION],
            "attachment; filename=\"report.txt\"; filename*=UTF-8''report.txt"
        );
        if method == Method::GET {
            assert_eq!(&body[..], b"hello world");
            get_headers = Some(headers);
        } else {
            assert!(body.is_empty());
            let get_headers = get_headers.as_ref().unwrap();
            for name in [
                header::ACCEPT_RANGES,
                header::CONTENT_LENGTH,
                header::CONTENT_TYPE,
                header::CONTENT_DISPOSITION,
            ] {
                assert_eq!(headers.get(&name), get_headers.get(&name));
            }
        }
    }
    context.close().await;
}

#[tokio::test]
async fn zero_byte_and_unknown_extension_downloads_are_safe() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Empty").await;
    std::fs::write(
        context
            .project_path(project_id)
            .join("files/empty.unknownext"),
        [],
    )
    .unwrap();
    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &download(project_id, "empty.unknownext"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers, body) = response_bytes(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_LENGTH], "0");
    assert_eq!(headers[header::CONTENT_TYPE], "application/octet-stream");
    assert!(body.is_empty());

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &download(project_id, "empty.unknownext"))
                .header(header::RANGE, "bytes=0-0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers, _) = response_bytes(response).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes */0");
    context.close().await;
}

#[tokio::test]
async fn get_and_head_support_single_bounded_open_and_suffix_ranges() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Ranges").await;
    let contents = b"0123456789abcdefghijklmnopqrstuvwxyz";
    std::fs::write(
        context.project_path(project_id).join("files/data.bin"),
        contents,
    )
    .unwrap();
    let cases = [
        ("bytes=0-0", 0, 0),
        ("bytes=35-35", 35, 35),
        ("bytes=10-19", 10, 19),
        ("bytes=10-", 10, 35),
        ("bytes=-10", 26, 35),
        ("bytes=0-35", 0, 35),
        ("Bytes=1-2", 1, 2),
        ("BYTES=-2", 34, 35),
    ];
    for (range, start, end) in cases {
        for method in [Method::GET, Method::HEAD] {
            let response = context
                .app()
                .oneshot(
                    authenticated_request(method.as_str(), &download(project_id, "data.bin"))
                        .header(header::RANGE, range)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let (status, headers, body) = response_bytes(response).await;
            assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{method} {range}");
            assert_eq!(
                headers[header::CONTENT_RANGE],
                format!("bytes {start}-{end}/36")
            );
            assert_eq!(
                headers[header::CONTENT_LENGTH],
                (end - start + 1).to_string()
            );
            assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
            if method == Method::GET {
                assert_eq!(&body[..], &contents[start as usize..=end as usize]);
            } else {
                assert!(body.is_empty());
            }
        }
    }
    context.close().await;
}

#[tokio::test]
async fn malformed_duplicate_and_unsatisfiable_ranges_are_416() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Bad ranges").await;
    std::fs::write(
        context.project_path(project_id).join("files/data.bin"),
        b"0123456789",
    )
    .unwrap();
    for value in [
        "bytes=",
        "bytes=10-9",
        "bytes=10-",
        "bytes=-0",
        "items=0-1",
        "bytes=0-1,3-4",
        "bytes= 0-1",
        "bytes=18446744073709551616-",
    ] {
        let response = context
            .app()
            .oneshot(
                authenticated_request("GET", &download(project_id, "data.bin"))
                    .header(header::RANGE, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, headers, body) = response_bytes(response).await;
        assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE, "{value}");
        assert_eq!(headers[header::CONTENT_RANGE], "bytes */10");
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "range_not_satisfiable");
    }

    let request = authenticated_request("GET", &download(project_id, "data.bin"))
        .header(header::RANGE, "bytes=0-1")
        .header(header::RANGE, "bytes=2-3")
        .body(Body::empty())
        .unwrap();
    let response = context.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");

    let response = context
        .app()
        .oneshot(
            authenticated_request("HEAD", &download(project_id, "data.bin"))
                .header(header::RANGE, "bytes=10-")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers, body) = response_bytes(response).await;
    assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(headers[header::CONTENT_RANGE], "bytes */10");
    assert!(body.is_empty());
    context.close().await;
}

#[tokio::test]
async fn unsafe_names_are_400_while_safe_missing_and_nonregular_are_404() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Names").await;
    std::fs::create_dir(context.project_path(project_id).join("files/nested")).unwrap();
    for encoded in [
        "..%2Fsecret",
        "%2E%2E",
        "a%5Cb",
        "CON",
        "bad.",
        "bad%00name",
    ] {
        let response = context
            .app()
            .oneshot(
                authenticated_request("GET", &download(project_id, encoded))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, request_id, body) = json_response(response).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{encoded}");
        assert_eq!(body["error"]["code"], "invalid_file_name");
        assert_eq!(body["error"]["requestId"], request_id);
        assert!(!body.to_string().contains(encoded));
    }
    for name in ["missing.bin", "nested"] {
        let response = context
            .app()
            .oneshot(
                authenticated_request("GET", &download(project_id, name))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, _, body) = json_response(response).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"]["code"], "file_not_found");
    }
    context.close().await;
}

#[tokio::test]
async fn project_id_must_be_canonical_lowercase_uuid() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Canonical").await;
    for id in [
        project_id.to_string().to_uppercase(),
        project_id.simple().to_string(),
        "not-a-uuid".to_owned(),
    ] {
        for uri in [
            format!("/api/v1/projects/{id}/files"),
            format!("/api/v1/projects/{id}/files/missing.bin"),
        ] {
            let response = context
                .app()
                .oneshot(
                    authenticated_request("GET", &uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let (status, _, body) = json_response(response).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert_eq!(body["error"]["code"], "invalid_request");
        }
    }
    context.close().await;
}

#[tokio::test]
async fn unicode_download_name_has_ascii_fallback_and_utf8_disposition() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Unicode").await;
    std::fs::write(
        context
            .project_path(project_id)
            .join("files/résumé 2026.pdf"),
        b"pdf",
    )
    .unwrap();
    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &download(project_id, "r%C3%A9sum%C3%A9%202026.pdf"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers()[header::CONTENT_DISPOSITION],
        "attachment; filename=\"r_sum_ 2026.pdf\"; filename*=UTF-8''r%C3%A9sum%C3%A9%202026.pdf"
    );
    context.close().await;
}

#[tokio::test]
async fn path_is_percent_decoded_exactly_once() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Decode once").await;
    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &download(project_id, "%252E%252E"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "file_not_found");
    context.close().await;
}

#[tokio::test]
async fn database_and_managed_storage_failures_are_safe_503s() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Unavailable").await;
    std::fs::remove_dir(context.project_path(project_id).join("files")).unwrap();
    for uri in [files(project_id), download(project_id, "missing.bin")] {
        let response = context
            .app()
            .oneshot(
                authenticated_request("GET", &uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let (status, request_id, body) = json_response(response).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["code"], "files_unavailable");
        assert_eq!(body["error"]["requestId"], request_id);
        assert!(!body.to_string().contains("missing.bin"));
        assert!(
            !body
                .to_string()
                .contains(context.temp.path().to_string_lossy().as_ref())
        );
    }

    context.database.clone().close().await;
    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &files(project_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["code"], "files_unavailable");
    context.close().await;
}

#[tokio::test]
async fn large_range_stream_is_bounded_and_truncation_never_overreads() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Streaming").await;
    let path = context.project_path(project_id).join("files/large.bin");
    let mut contents = vec![0_u8; 2 * 1024 * 1024];
    for (index, byte) in contents.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    std::fs::write(&path, &contents).unwrap();

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &download(project_id, "large.bin"))
                .header(header::RANGE, "bytes=1048576-1048591")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, headers, body) = response_bytes(response).await;
    assert_eq!(status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(headers[header::CONTENT_LENGTH], "16");
    assert_eq!(&body[..], &contents[1_048_576..1_048_592]);

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &download(project_id, "large.bin"))
                .header(header::RANGE, "bytes=0-1023")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(8)
        .unwrap();
    let (_, headers, body) = response_bytes(response).await;
    assert_eq!(headers[header::CONTENT_LENGTH], "1024");
    assert!(body.len() <= 1024);
    context.close().await;
}

#[tokio::test]
async fn downloads_require_authentication_but_not_origin() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Security").await;
    std::fs::write(
        context.project_path(project_id).join("files/data.bin"),
        b"data",
    )
    .unwrap();

    for method in [Method::GET, Method::HEAD] {
        let uri = download(project_id, "data.bin");
        let unauthenticated = http::Request::builder()
            .method(method.clone())
            .uri(&uri)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            context
                .app()
                .oneshot(unauthenticated)
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        let authenticated = authenticated_request(method.as_str(), &uri)
            .body(Body::empty())
            .unwrap();
        assert!(
            context
                .app()
                .oneshot(authenticated)
                .await
                .unwrap()
                .status()
                .is_success()
        );
    }
    context.close().await;
}

#[tokio::test]
async fn listing_ignores_unsafe_host_filename_instead_of_failing() {
    let context = TestContext::new().await;
    let project_id = context.create_project("Unsafe host entry").await;
    let directory = context.project_path(project_id).join("files");
    std::fs::write(directory.join("safe.bin"), b"safe").unwrap();
    #[cfg(not(windows))]
    std::fs::write(directory.join("bad:name"), b"unsafe").unwrap();

    let response = context
        .app()
        .oneshot(
            authenticated_request("GET", &files(project_id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, _, body) = json_response(response).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([{"name":"safe.bin","size":"4","modifiedAt":body[0]["modifiedAt"]}])
    );
    context.close().await;
}

#[test]
fn safe_filename_fixture_covers_download_validation_contract() {
    assert!(SafeFileName::parse("résumé 2026.pdf").is_ok());
    for value in ["../x", "a/b", "a\\b", "NUL.txt", "trailing.", "bad\nname"] {
        assert!(SafeFileName::parse(value).is_err());
    }
}

fn create_test_file_symlink(
    target: impl AsRef<std::path::Path>,
    link: impl AsRef<std::path::Path>,
) -> bool {
    #[cfg(unix)]
    let result = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let result = std::os::windows::fs::symlink_file(target, link);

    match result {
        Ok(()) => true,
        #[cfg(windows)]
        Err(error) if matches!(error.raw_os_error(), Some(1 | 50 | 1314)) => {
            eprintln!(
                "skipping symlink assertions: Windows privilege or symlink support unavailable ({:?})",
                error.raw_os_error()
            );
            false
        }
        Err(error) => panic!("unexpected test symlink creation failure: {error}"),
    }
}
