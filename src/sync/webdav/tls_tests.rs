use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::thread;
use std::time::Duration;

use age::secrecy::SecretString;
use base64::Engine as _;

use super::*;

/// Test PKI for the loopback TLS server below.
///
/// The server certificate is a LEAF issued by this CA — not the CA itself. That distinction is
/// load-bearing. The previous fixture presented the self-signed CA as the server certificate,
/// with `CA:TRUE`, no `extendedKeyUsage`, and a ten-year validity. OpenSSL accepts that, so the
/// test passed on Linux, while Security.framework on macOS and SChannel on Windows both rejected
/// it — making a working product path look like a custom-CA bug on two of three platforms.
///
/// A replacement must keep all four properties the platform verifiers require of a server
/// certificate: `CA:FALSE`, `extendedKeyUsage=serverAuth`, a subjectAltName (macOS ignores CN),
/// and a validity of 825 days or fewer (Apple's limit for certificates issued after 2020-09-01).
///
/// The pinned leaf expires **2028-10-23**; the CA expires 2036-07-23. When the leaf lapses this
/// test starts failing with a certificate error that looks exactly like a TLS regression, so
/// regenerate both with:
///
/// ```sh
/// openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes -keyout ca.key -out ca.crt \
///   -subj "/CN=Yututui WebDAV Test CA" \
///   -addext "basicConstraints=critical,CA:TRUE" \
///   -addext "keyUsage=critical,keyCertSign,cRLSign"
/// openssl req -newkey rsa:2048 -sha256 -nodes -keyout leaf.key -out leaf.csr -subj "/CN=localhost"
/// cat > leaf.ext <<'EXT'
/// basicConstraints=critical,CA:FALSE
/// keyUsage=critical,digitalSignature,keyEncipherment
/// extendedKeyUsage=serverAuth
/// subjectAltName=IP:127.0.0.1,DNS:localhost
/// EXT
/// openssl x509 -req -in leaf.csr -CA ca.crt -CAkey ca.key -CAcreateserial -out leaf.crt \
///   -days 820 -sha256 -extfile leaf.ext
/// openssl pkcs12 -export -out id.p12 -inkey leaf.key -in leaf.crt -certfile ca.crt \
///   -passout pass:yututui-test -legacy -macalg sha1 \
///   -keypbe PBE-SHA1-3DES -certpbe PBE-SHA1-3DES
/// ```
///
/// `-legacy` and the SHA-1 MAC/PBE are required: `native-tls` reads the identity through the
/// platform PKCS#12 parser, which does not accept OpenSSL 3's AES-256/PBKDF2 default.
pub(crate) const TEST_CA_PEM: &[u8] = br#"
-----BEGIN CERTIFICATE-----
MIIDMzCCAhugAwIBAgIUJAshzf3omUZUBEdLf8H+87tiuFQwDQYJKoZIhvcNAQEL
BQAwITEfMB0GA1UEAwwWWXV0dXR1aSBXZWJEQVYgVGVzdCBDQTAeFw0yNjA3MjYx
NTMyMjlaFw0zNjA3MjMxNTMyMjlaMCExHzAdBgNVBAMMFll1dHV0dWkgV2ViREFW
IFRlc3QgQ0EwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQD4WyRghC42
WhkbLi/jedkbd7qH0AyjOQwVOao/9o9gDNbCirBQXMojOMgg/cKokg7QP8WsOr0m
bgCYLcojamvSZGMf2JpNZ8IY+90ZM7GqeIqHzwU7AiBIhacClqEtZ7X5j5E7tdPG
vmD22/9aTUlAp+LWiWyjzvA0S3m7wkCSYXIPvZ+3SOGrZifuL3gw+ubkplTjgTpB
UVcAzlVBukDfW3MuOx3oeKrYSf7x8O1ki4w1/3dGO7HXa3mp/EtJ5gKFHqa1EnsC
od7+BcetjYofiVGjGKtoDX65OSyl7/vJZsESn8J6bPYuSQL/fijw2W5whQZ3IcWy
gx773Hymz609AgMBAAGjYzBhMB0GA1UdDgQWBBRDVBj0wcDObY7B2GgX9eOCowJ7
+DAfBgNVHSMEGDAWgBRDVBj0wcDObY7B2GgX9eOCowJ7+DAPBgNVHRMBAf8EBTAD
AQH/MA4GA1UdDwEB/wQEAwIBBjANBgkqhkiG9w0BAQsFAAOCAQEAemXRpbCgFUJs
Db3iHytnfwnC4XVDo0Kvf3xoH+vR3ozrajPCwYkXXbnnZQD9WzAe3YbMceLcObEl
jGd3ZJ1C+qCUyisNW9qObNi4P/a2b1qH2PU06KKajrVECTV2mwFExUdHrYEv3IJR
SN+VRsJo7kgL2pj/Kt8er7Z5OcfON31TYj/994Vle/5cIz+X6US37h457Ig51AVY
4WlOc7CprtjbUtGL2L2wHwIEMKNE4Ppp+jWuubYbsGQHDPfj1OVfDizTScxkzF9C
qnLeZFYm7hyqDmCwQjhXF5XYenUyHMhYbuoihiM+KsUbPYqjH2Lm80MafZe0VKXK
ntr8u/fUdw==
-----END CERTIFICATE-----
"#;

const TEST_IDENTITY_P12_B64: &str = concat!(
    "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",
    "EDuyhePZiCrpTfCGIkvvHbECAggA",
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
