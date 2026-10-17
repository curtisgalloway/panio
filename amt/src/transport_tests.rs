// SPDX-FileCopyrightText: 2026 Curtis Galloway
// SPDX-License-Identifier: Apache-2.0

use super::*;
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver};
use std::thread;

struct Fixture {
    ca: tempfile::NamedTempFile,
    config: Arc<rustls::ServerConfig>,
}

fn fixture(name: &str, validity: Option<(i32, i32)>) -> Fixture {
    fixture_versions(name, validity, rustls::DEFAULT_VERSIONS)
}

fn fixture_versions(
    name: &str,
    validity: Option<(i32, i32)>,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> Fixture {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let key = KeyPair::generate().unwrap();
    let mut params = CertificateParams::new(vec![name.to_string()]).unwrap();
    if let Some((start, end)) = validity {
        params.not_before = rcgen::date_time_ymd(start, 1, 1);
        params.not_after = rcgen::date_time_ymd(end, 1, 1);
    }
    let cert = params.signed_by(&key, &ca, &ca_key).unwrap();
    let config = rustls::ServerConfig::builder_with_protocol_versions(versions)
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        )
        .unwrap();
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(ca.pem().as_bytes()).unwrap();
    Fixture {
        ca: file,
        config: Arc::new(config),
    }
}

fn response(code: u16, headers: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {code} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

fn challenge() -> String {
    response(
        401,
        "WWW-Authenticate: Digest realm=\"test\", nonce=\"nonce\", qop=\"auth\"\r\n",
        "",
    )
}

fn request(stream: &mut (impl Read + Write)) -> io::Result<String> {
    let mut buf = Vec::new();
    let mut byte = [0];
    while !buf.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte)?;
        buf.push(byte[0]);
        assert!(buf.len() < 64 * 1024);
    }
    let header = String::from_utf8(buf.clone()).unwrap();
    let len: usize = header
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse().unwrap())
        })
        .unwrap_or(0);
    let mut body = vec![0; len];
    stream.read_exact(&mut body)?;
    buf.extend(body);
    Ok(String::from_utf8(buf).unwrap())
}

fn accept(listener: &TcpListener) -> TcpStream {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                return stream;
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < end => {
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("test server accept: {e}"),
        }
    }
}

fn server(
    config: Option<Arc<rustls::ServerConfig>>,
    replies: Vec<String>,
) -> (u16, Receiver<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut received = Vec::new();
        for reply in replies {
            let socket = accept(&listener);
            let mut stream: Box<dyn ReadWrite> = if let Some(config) = &config {
                Box::new(rustls::StreamOwned::new(
                    rustls::ServerConnection::new(config.clone()).unwrap(),
                    socket,
                ))
            } else {
                Box::new(socket)
            };
            match request(&mut stream) {
                Ok(req) => {
                    received.push(req);
                    stream.write_all(reply.as_bytes()).unwrap();
                    stream.flush().unwrap();
                }
                Err(_) => break,
            }
        }
        tx.send(received).unwrap();
    });
    (port, rx)
}

trait ReadWrite: Read + Write {}
impl<T: Read + Write> ReadWrite for T {}

fn received(rx: Receiver<Vec<String>>) -> Vec<String> {
    rx.recv_timeout(Duration::from_secs(6)).unwrap()
}

fn client(port: u16, ca: Option<&Path>) -> Client {
    Client::new(
        Connection::new(&format!("https://localhost:{port}"), false, ca).unwrap(),
        "admin",
        "test-password",
    )
}

#[test]
fn endpoint_policy_and_explicit_ports() {
    for (device, insecure, expected) in [
        (
            "amt-host.example",
            false,
            "https://amt-host.example:16993/wsman",
        ),
        (
            "amt-host.example:16992",
            false,
            "https://amt-host.example:16992/wsman",
        ),
        (
            "https://amt-host.example:18443",
            false,
            "https://amt-host.example:18443/wsman",
        ),
        (
            "amt-host.example",
            true,
            "http://amt-host.example:16992/wsman",
        ),
        (
            "http://amt-host.example:18080",
            true,
            "http://amt-host.example:18080/wsman",
        ),
        (
            "http://[2001:db8::1]",
            true,
            "http://[2001:db8::1]:16992/wsman",
        ),
    ] {
        assert_eq!(
            Connection::new(device, insecure, None).unwrap().url,
            expected
        );
    }
    for (device, insecure, ca) in [
        ("http://localhost", false, None),
        ("https://localhost", true, None),
        ("localhost", true, Some(Path::new("unused.pem"))),
        ("ftp://localhost", false, None),
    ] {
        assert!(Connection::new(device, insecure, ca).is_err());
    }
}

#[test]
fn ca_files_reject_missing_empty_malformed_and_invalid_der() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ca.pem");
    assert!(Connection::new("localhost", false, Some(&path)).is_err());
    for bytes in [
        "",
        "garbage",
        "-----BEGIN CERTIFICATE-----\n!\n-----END CERTIFICATE-----",
        "-----BEGIN CERTIFICATE-----\nYQ==\n-----END CERTIFICATE-----",
    ] {
        std::fs::write(&path, bytes).unwrap();
        assert!(Connection::new("localhost", false, Some(&path)).is_err());
    }
}

#[test]
fn verified_tls_digest_preserves_destination_and_uri() {
    let fixture = fixture("localhost", None);
    let (port, rx) = server(
        Some(fixture.config),
        vec![challenge(), response(200, "", "<PowerState>2</PowerState>")],
    );
    assert_eq!(
        client(port, Some(fixture.ca.path())).power_state().unwrap(),
        PS_ON
    );
    let requests = received(rx);
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].contains("Authorization:"));
    assert!(requests[1].contains("Authorization: Digest"));
    assert!(requests[1].contains("uri=\"/wsman\""));
    assert!(requests[1].contains(&format!("https://localhost:{port}/wsman")));
    assert!(!requests[1].contains("test-password"));
}

#[test]
fn certificate_failures_are_terminal_before_any_soap() {
    for (name, dates, trusted) in [
        ("localhost", None, false),
        ("wrong.example", None, true),
        ("localhost", Some((2000, 2001)), true),
        ("localhost", Some((4090, 4091)), true),
    ] {
        let fixture = fixture(name, dates);
        let (port, rx) = server(
            Some(fixture.config),
            vec![response(200, "", "<PowerState>2</PowerState>")],
        );
        let ca = trusted.then_some(fixture.ca.path());
        let err = client(port, ca).power_state().unwrap_err();
        assert!(!is_transient(&err), "{err:?}");
        assert!(err.to_string().contains("TLS"), "{err:?}");
        assert!(received(rx).is_empty());
    }
}

#[test]
fn redirects_on_either_digest_leg_never_contact_destination() {
    for authenticated in [false, true] {
        for code in [301, 302, 303, 307, 308] {
            let fixture = fixture("localhost", None);
            let destination = TcpListener::bind("127.0.0.1:0").unwrap();
            destination.set_nonblocking(true).unwrap();
            let redirect = response(
                code,
                &format!(
                    "Location: http://localhost:{}/wsman\r\n",
                    destination.local_addr().unwrap().port()
                ),
                "",
            );
            let mut replies = Vec::new();
            if authenticated {
                replies.push(challenge());
            }
            replies.push(redirect);
            let (port, rx) = server(Some(fixture.config), replies);
            let err = client(port, Some(fixture.ca.path()))
                .power_state()
                .unwrap_err();
            assert!(err.to_string().contains("redirect"), "{err:?}");
            assert!(!is_transient(&err));
            assert_eq!(received(rx).len(), if authenticated { 2 } else { 1 });
            assert_eq!(
                destination.accept().unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
        }
    }
}

#[test]
fn second_401_is_terminal_and_http_requires_opt_in() {
    let (port, rx) = server(None, vec![challenge(), challenge()]);
    let connection = Connection::new(&format!("http://localhost:{port}"), true, None).unwrap();
    let err = Client::new(connection, "admin", "pw")
        .power_state()
        .unwrap_err();
    assert!(err.to_string().contains("authentication failed"));
    assert!(!is_transient(&err));
    assert_eq!(received(rx).len(), 2);
}

#[test]
fn tls_protocol_errors_are_terminal_but_connection_refusal_can_retry() {
    let fixture = fixture_versions("localhost", None, &[&rustls::version::TLS12]);
    let (port, rx) = server(Some(fixture.config), vec![response(200, "", "")]);
    let mut connection = Connection::new(
        &format!("https://localhost:{port}"),
        false,
        Some(fixture.ca.path()),
    )
    .unwrap();
    let roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .with_root_certificates(roots)
        .with_no_client_auth();
    connection.agent = ureq::AgentBuilder::new().tls_config(Arc::new(tls)).build();
    let err = Client::new(connection, "admin", "pw")
        .power_state()
        .unwrap_err();
    assert!(!is_transient(&err));
    assert!(err.to_string().contains("ProtocolVersion"), "{err:?}");
    assert!(received(rx).is_empty());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let err = client(port, None).power_state().unwrap_err();
    assert!(is_transient(&err), "{err:?}");
}

#[test]
fn tls12_and_kvm_settings_use_the_same_verified_transport() {
    let fixture = fixture_versions("localhost", None, &[&rustls::version::TLS12]);
    let (port, rx) = server(
        Some(fixture.config),
        vec![
            challenge(),
            response(
                200,
                "",
                "<Is5900PortEnabled>true</Is5900PortEnabled><EnabledByMEBx>true</EnabledByMEBx>",
            ),
        ],
    );
    let settings = client(port, Some(fixture.ca.path()))
        .kvm_settings()
        .unwrap();
    assert!(settings.port_5900_enabled);
    assert!(settings.enabled_by_mebx);
    assert!(received(rx)[1].contains("IPS_KVMRedirectionSettingData"));
}

#[test]
fn self_signed_leaf_uses_standard_verifier_and_still_checks_name() {
    let key = KeyPair::generate().unwrap();
    let cert = CertificateParams::new(vec!["localhost".to_owned()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let config = Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap(),
    );
    let mut file = tempfile::NamedTempFile::new().unwrap();
    file.write_all(cert.pem().as_bytes()).unwrap();
    let (port, rx) = server(
        Some(config.clone()),
        vec![response(200, "", "<PowerState>2</PowerState>")],
    );
    assert_eq!(
        client(port, Some(file.path())).power_state().unwrap(),
        PS_ON
    );
    assert_eq!(received(rx).len(), 1);
    let (port, rx) = server(Some(config), vec![response(200, "", "")]);
    let connection = Connection::new(
        &format!("https://127.0.0.1:{port}"),
        false,
        Some(file.path()),
    )
    .unwrap();
    let err = Client::new(connection, "admin", "pw")
        .power_state()
        .unwrap_err();
    assert!(!is_transient(&err));
    assert!(received(rx).is_empty());
}

#[test]
fn slow_tls_handshake_obeys_total_io_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let mut stream = accept(&listener);
        let mut hello = [0; 4096];
        assert!(stream.read(&mut hello).unwrap() > 0);
        stream.write_all(&[22, 3, 3, 3, 232]).unwrap();
        for _ in 0..400 {
            if stream.write_all(&[0]).is_err() {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
    });
    // Build the client outside the timed window: construction (root-store
    // setup, CA parsing) is not part of the I/O deadline under test, and on
    // a loaded runner it can eat the whole elapsed budget before the call.
    // The budget is seconds, not milliseconds, for the same reason: the
    // Windows CI runner executes this suite ~50x slower than a dev machine,
    // and a sub-second budget flakes there without testing anything more.
    let amt_client = client(port, None);
    let start = Instant::now();
    let err = amt_client
        .power_state_within(Duration::from_secs(5))
        .unwrap_err();
    assert!(
        start.elapsed() < Duration::from_secs(8),
        "deadline was exceeded: {err:?}"
    );
    assert!(is_transient(&err), "{err:?}");
    server.join().unwrap();
}

#[test]
fn response_body_timeout_is_transient_but_tls_corruption_is_terminal() {
    for corrupt in [false, true] {
        let fixture = fixture("localhost", None);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || {
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(fixture.config).unwrap(),
                accept(&listener),
            );
            request(&mut stream).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            if corrupt {
                stream.sock.write_all(&[23, 3, 3, 0, 17]).unwrap();
                stream.sock.write_all(&[0; 17]).unwrap();
            } else {
                for _ in 0..400 {
                    if stream.write_all(b"x").and_then(|_| stream.flush()).is_err() {
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
            }
        });
        let amt_client = client(port, Some(fixture.ca.path()));
        let start = Instant::now();
        let err = amt_client
            .power_state_within(Duration::from_secs(5))
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(8), "{err:?}");
        assert_eq!(is_transient(&err), !corrupt, "{err:?}");
        if corrupt {
            assert!(format!("{err:?}").contains("TLS"));
        }
        server.join().unwrap();
    }
}

#[test]
fn truncated_response_body_is_retryable() {
    let fixture = fixture("localhost", None);
    let (port, rx) = server(
        Some(fixture.config),
        vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n<Power".to_owned(),
        ],
    );
    let err = client(port, Some(fixture.ca.path()))
        .power_state()
        .unwrap_err();
    assert!(is_transient(&err), "{err:?}");
    assert_eq!(received(rx).len(), 1);
}
