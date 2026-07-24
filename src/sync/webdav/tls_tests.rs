use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use age::secrecy::SecretString;
use base64::Engine as _;

use super::*;

pub(crate) const TEST_CA_PEM: &[u8] = br#"-----BEGIN CERTIFICATE-----
MIIDPzCCAiegAwIBAgIUKGG2gqPOpWoVM6pSyPG7XiRVakswDQYJKoZIhvcNAQEL
BQAwITEfMB0GA1UEAwwWWXV0dXR1aSBXZWJEQVYgVGVzdCBDQTAeFw0yNjA3MjQx
NzMwNDFaFw0zNjA3MjExNzMwNDFaMCExHzAdBgNVBAMMFll1dHV0dWkgV2ViREFW
IFRlc3QgQ0EwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQC8oPKemgzH
0d1vDoobLOSHE/gKS2mIG2eAFVbIJGWlSyKVsMca0VUCvlJQwldk4DnmmQizLF3Y
bjnGB/2+2wBhg/BzGN+gR27KsCrCJS52pfOFkaOqfcWM3QJvfQbLmRZvyWNhy9q8
FI26+cyqD4Hy9HvroiSmkQUmsNofu/gvrhB9G17eVtKdVLlKGku8njR+ufnSc6X+
ZYFi715ecqORk3cc7TGZJBwqW5x/npNzEbPFQwWM02Hw/kctDnZGoC9dYKqFlk67
I/hCEoojrFBBF+gB7pVGVdrw2R5R1KnFbZ/gLF+BaG73MLUwFfxTPV0UMoE1xfq7
GQ0q8UoJNtxFAgMBAAGjbzBtMB0GA1UdDgQWBBQ4tEQ5cKWqwMOM7HVO/R9GYadK
MDAfBgNVHSMEGDAWgBQ4tEQ5cKWqwMOM7HVO/R9GYadKMDAaBgNVHREEEzARhwR/
AAABgglsb2NhbGhvc3QwDwYDVR0TAQH/BAUwAwEB/zANBgkqhkiG9w0BAQsFAAOC
AQEAGgF/DjvE5/YB8efdorH5CLZxBVQUObCU6wRKHcAU0xnAeI/Kwoz3fFB8duww
rpsmjcFjZ3dn2k5jj4pkwmV3luzFf3L6IvQbCTQVQNOCGUVetZbVRicN5ksnnAGB
OPMLheVNlKwMqAOBfG4xXTroycdGaOxFradaF2CCjYC7qlNz0btY+b82bjAFc1Dd
3Thkrtr/narY1WiHpKoRycmowGEr5TgeYVGuuojsDP4Z21c2AOTxBlRp0t4teyZj
CPgnDFOFsAJFyCozkgGosm8YZtBU/Tl9/ecL7r+/X6bAEoqzjyhNDVuvJP/aSrow
KC23wFROeGlz9YHrMW/dbXm5xA==
-----END CERTIFICATE-----
"#;

const TEST_IDENTITY_P12_B64: &str = concat!(
    "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",
    "XgV/3DhWtAICCAA=",
);

fn credential() -> VaultCredential {
    VaultCredential::bearer_token(SecretString::from("tls-test-token")).unwrap()
}

fn spawn_tls_server(expect_handshake: bool) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let identity_bytes = base64::engine::general_purpose::STANDARD
        .decode(TEST_IDENTITY_P12_B64)
        .unwrap();
    let identity = native_tls::Identity::from_pkcs12(&identity_bytes, "yututui-test").unwrap();
    let acceptor = native_tls::TlsAcceptor::new(identity).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let result = acceptor.accept(stream);
        if !expect_handshake {
            assert!(
                result.is_err(),
                "untrusted client unexpectedly completed TLS"
            );
            return;
        }
        let mut stream = result.expect("trusted client completes TLS");
        let mut request = Vec::new();
        let mut byte = [0_u8; 1];
        while !request.ends_with(b"\r\n\r\n") {
            assert!(request.len() < 16 * 1024);
            assert_eq!(stream.read(&mut byte).unwrap(), 1);
            request.push(byte[0]);
        }
        assert!(String::from_utf8_lossy(&request).starts_with("OPTIONS /vault/ HTTP/1.1\r\n"));
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nDAV: 1\r\nAllow: OPTIONS\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        stream.flush().unwrap();
    });
    (format!("https://{address}/vault"), server)
}

#[tokio::test]
async fn custom_ca_succeeds_and_untrusted_certificate_is_typed() {
    assert_eq!(
        WebDavClient::with_custom_ca("https://127.0.0.1/", Some(b"not a certificate"))
            .err()
            .expect("invalid custom CA must fail"),
        WebDavError::CertificateFailed
    );

    let (endpoint, server) = spawn_tls_server(false);
    let error = WebDavClient::new(&endpoint)
        .unwrap()
        .options(&credential())
        .await
        .unwrap_err();
    assert_eq!(error, WebDavError::CertificateFailed);
    server.join().unwrap();

    let (endpoint, server) = spawn_tls_server(true);
    let capabilities = WebDavClient::with_custom_ca(&endpoint, Some(TEST_CA_PEM))
        .unwrap()
        .options(&credential())
        .await
        .unwrap();
    assert!(capabilities.options);
    assert!(capabilities.dav_class_1);
    server.join().unwrap();
}
