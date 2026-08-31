use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Output};
use std::thread;

use serde_json::Value;

#[test]
fn price_sync_status_and_immutable_version_work_through_the_binary() {
    let root = tempfile::tempdir().expect("test root");
    let state = root.path().join("state");
    let mut catalog = embedded_catalog();
    catalog["version"] = Value::String("2026-08-28".to_string());
    let first_server = CatalogServer::start(serde_json::to_vec(&catalog).unwrap());
    let first_url = first_server.url();

    let first = price_sync(&state, &first_url);
    first_server.finish();
    assert_success(&first);
    assert!(stdout(&first).contains("deepseek-official-usd@2026-08-28 · updated"));

    let status = Command::new(agul_bin())
        .args(["price", "status", "--state-dir"])
        .arg(&state)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run price status");
    assert_success(&status);
    let status_text = stdout(&status);
    assert!(status_text.contains("deepseek-official-usd@2026-08-28"));
    assert!(status_text.contains(&format!("source {first_url}")));
    assert!(status_text.contains("checked "));
    assert!(status_text.contains("synced "));
    assert!(!status_text.contains("last error"));

    let glm_status = Command::new(agul_bin())
        .args(["price", "status", "--provider", "glm", "--state-dir"])
        .arg(&state)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run isolated GLM price status");
    assert_success(&glm_status);
    let glm_status_text = stdout(&glm_status);
    assert!(glm_status_text.contains("glm-official-cny@2026-08-29 · embedded"));
    assert!(glm_status_text.contains("source not configured"));

    let mut mutation = catalog.clone();
    mutation["cards"][0]["bands"][0]["rates"]["cache_miss_input_nanos_per_million"] =
        Value::from(220_000_001_u64);
    let second_server = CatalogServer::start(serde_json::to_vec(&mutation).unwrap());
    let second_url = second_server.url();
    let second = price_sync(&state, &second_url);
    second_server.finish();
    assert_eq!(second.status.code(), Some(1));
    assert!(
        stderr(&second).contains("changed without a new version"),
        "stderr={}",
        stderr(&second)
    );

    let after_failure = Command::new(agul_bin())
        .args(["price", "status", "--state-dir"])
        .arg(&state)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run price status after rejected sync");
    assert_success(&after_failure);
    let status_text = stdout(&after_failure);
    assert!(status_text.contains("deepseek-official-usd@2026-08-28"));
    assert!(status_text.contains(&format!("source {second_url}")));
    assert!(status_text.contains("last error"));
    assert!(status_text.contains("changed without a new version"));

    let price_root = state.join("prices").join("deepseek");
    let cached: Value = serde_json::from_slice(
        &fs::read(price_root.join("catalog-deepseek-official-usd-2026-08-28.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        cached, catalog,
        "rejected content must not replace the cache"
    );
    let mut files = fs::read_dir(price_root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(
        files,
        ["catalog-deepseek-official-usd-2026-08-28.json", "sync.json",]
    );
}

#[test]
fn price_status_is_local_and_uses_the_embedded_card_without_a_source() {
    let root = tempfile::tempdir().expect("test root");
    let state = root.path().join("state");

    let output = Command::new(agul_bin())
        .args(["price", "status", "--state-dir"])
        .arg(&state)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run local price status");

    assert_success(&output);
    let text = stdout(&output);
    assert!(text.contains("deepseek-official-usd@2026-08-27.1 · embedded"));
    assert!(text.contains("source not configured"));
    assert!(!state.join("prices").exists());
}

#[test]
fn price_status_selects_the_embedded_glm_catalog() {
    let root = tempfile::tempdir().expect("test root");
    let state = root.path().join("state");

    let output = Command::new(agul_bin())
        .args(["price", "status", "--provider", "glm", "--state-dir"])
        .arg(&state)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run GLM price status");

    assert_success(&output);
    assert!(stdout(&output).contains("glm-official-cny@2026-08-29 · embedded"));
    assert!(stdout(&output).contains(&format!(
        "cache {}",
        state.join("prices").join("glm").display()
    )));
}

#[test]
fn price_sync_rejects_a_catalog_for_another_provider() {
    let root = tempfile::tempdir().expect("test root");
    let state = root.path().join("state");
    let catalog: Value =
        serde_json::from_str(include_str!("../src/runtime/billing/glm-2026-08-29.json")).unwrap();
    let server = CatalogServer::start(serde_json::to_vec(&catalog).unwrap());

    let output = price_sync(&state, &server.url());
    server.finish();

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("does not match the selected provider, origin, and model target")
    );
    let files = fs::read_dir(state.join("prices").join("deepseek"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(files, ["sync.json"]);
}

fn price_sync(state: &std::path::Path, url: &str) -> Output {
    Command::new(agul_bin())
        .args(["price", "sync", "--url", url, "--state-dir"])
        .arg(state)
        .env_remove("AGUL_PRICE_CATALOG_URL")
        .env_remove("AGUL_PROVIDER")
        .output()
        .expect("run price sync")
}

fn embedded_catalog() -> Value {
    serde_json::from_str(include_str!(
        "../src/runtime/billing/deepseek-2026-08-27.json"
    ))
    .unwrap()
}

fn agul_bin() -> &'static str {
    env!("CARGO_BIN_EXE_agul")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={} stderr={}",
        stdout(output),
        stderr(output)
    );
}

struct CatalogServer {
    url: String,
    handle: thread::JoinHandle<()>,
}

impl CatalogServer {
    fn start(body: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind catalog server");
        let url = format!("http://{}/catalog.json", listener.local_addr().unwrap());
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("catalog request");
            let request = read_headers(&mut stream);
            assert!(request.starts_with("GET /catalog.json HTTP/1.1\r\n"));
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(response.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
            stream.flush().unwrap();
        });
        Self { url, handle }
    }

    fn url(&self) -> String {
        self.url.clone()
    }

    fn finish(self) {
        self.handle.join().expect("catalog server");
    }
}

fn read_headers(stream: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 512];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..count]);
    }
    String::from_utf8(bytes).expect("UTF-8 request")
}
