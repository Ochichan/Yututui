use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use age::secrecy::SecretString;

use super::{VaultCredential, WebDavClient};

const IMPLICIT_PROXY_CHILD_ENDPOINT: &str = "YUTUTUI_TEST_WEBDAV_DIRECT_ENDPOINT";

fn credential() -> VaultCredential {
    VaultCredential::password("sync-user", SecretString::from("sync-password")).unwrap()
}

#[test]
fn implicit_proxy_child_probe() {
    let Ok(endpoint) = std::env::var(IMPLICIT_PROXY_CHILD_ENDPOINT) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    let capabilities = runtime
        .block_on(WebDavClient::new(&endpoint).unwrap().options(&credential()))
        .unwrap();
    assert!(capabilities.options);
    assert!(capabilities.dav_class_1);
}

#[test]
#[cfg_attr(
    windows,
    ignore = "GitHub Windows loopback can abort or stall this raw-socket fixture"
)]
fn loopback_http_bypasses_implicit_system_proxy() {
    let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    proxy.set_nonblocking(true).unwrap();
    let target_endpoint = format!("http://{}/vault", target.local_addr().unwrap());
    let proxy_endpoint = format!("http://{}", proxy.local_addr().unwrap());
    let done = Arc::new(AtomicBool::new(false));
    let target_seen = Arc::new(AtomicBool::new(false));
    let proxy_seen = Arc::new(AtomicBool::new(false));

    let target_server = {
        let done = Arc::clone(&done);
        let target_seen = Arc::clone(&target_seen);
        std::thread::spawn(move || {
            let Some(mut stream) = accept_until_done(&target, &done) else {
                return;
            };
            target_seen.store(true, Ordering::Release);
            let request = read_headers(&mut stream);
            let request = String::from_utf8_lossy(&request).to_ascii_lowercase();
            assert!(request.starts_with("options /vault/ http/1.1\r\n"));
            assert!(request.contains("\r\nauthorization: basic "));
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        })
    };
    let proxy_server = {
        let done = Arc::clone(&done);
        let proxy_seen = Arc::clone(&proxy_seen);
        std::thread::spawn(move || {
            let Some(mut stream) = accept_until_done(&proxy, &done) else {
                return;
            };
            proxy_seen.store(true, Ordering::Release);
            stream
                .write_all(
                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
        })
    };

    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("implicit_proxy_child_probe")
        .arg("--test-threads=1")
        .env(IMPLICIT_PROXY_CHILD_ENDPOINT, target_endpoint)
        .env("HTTP_PROXY", &proxy_endpoint)
        .env("http_proxy", &proxy_endpoint)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy")
        .env_remove("ALL_PROXY")
        .env_remove("all_proxy")
        .env_remove("REQUEST_METHOD")
        .status()
        .unwrap();
    done.store(true, Ordering::Release);
    target_server.join().unwrap();
    proxy_server.join().unwrap();

    assert!(status.success(), "direct WebDAV child probe failed");
    assert!(
        target_seen.load(Ordering::Acquire),
        "the configured loopback WebDAV origin did not receive the request"
    );
    assert!(
        !proxy_seen.load(Ordering::Acquire),
        "the credential-owning WebDAV client used an implicit proxy"
    );
}

fn accept_until_done(
    listener: &std::net::TcpListener,
    done: &AtomicBool,
) -> Option<std::net::TcpStream> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match listener.accept() {
            Ok((stream, _)) => return Some(stream),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if done.load(Ordering::Acquire) || std::time::Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("test listener failed: {error}"),
        }
    }
}

fn read_headers(stream: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read as _;

    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !request.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut buffer).unwrap();
        assert!(read > 0, "request ended before its headers");
        request.extend_from_slice(&buffer[..read]);
        assert!(request.len() <= 16 * 1024, "test request exceeded cap");
    }
    request
}
