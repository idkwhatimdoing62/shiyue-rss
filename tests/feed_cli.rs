use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_shiyue-cli"))
        .args(args)
        .env("SHIYUE_TEST_ROOT", root)
        .env("SHIYUE_TEST_FEED_FIXTURE", "loopback-v1")
        .output()
        .unwrap()
}

fn rss_server(expected_requests: usize) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0"><channel><title>Lifecycle Feed</title><link>https://example.test/</link>
<description>fixture</description><item><guid>one</guid><title>First</title>
<link>https://example.test/one</link></item></channel></rss>"#
        .as_bytes()
        .to_vec();
    let join = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut served = 0;
        while served < expected_requests && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    let mut request = Vec::with_capacity(2048);
                    loop {
                        let mut chunk = [0_u8; 1024];
                        match stream.read(&mut chunk) {
                            Ok(0) => break,
                            Ok(read) => {
                                request.extend_from_slice(&chunk[..read]);
                                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                                    break;
                                }
                                assert!(request.len() <= 16 * 1024, "fixture request too large");
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                panic!("fixture request headers were incomplete")
                            }
                            Err(error) => panic!("fixture request read failed: {error}"),
                        }
                    }
                    let headers = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/rss+xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    stream.write_all(headers.as_bytes()).unwrap();
                    stream.write_all(&body).unwrap();
                    served += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("fixture server failed: {error}"),
            }
        }
        assert_eq!(served, expected_requests);
    });
    (format!("http://{address}/feed"), join)
}

#[test]
fn feed_cli_routes_the_subscription_lifecycle_through_one_consistent_seam() {
    let root = std::env::temp_dir().join(format!(
        "shiyue-feed-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(".shiyue-loopback-fixture"), b"fixture").unwrap();
    let (url, server) = rss_server(2);

    let added = run(&root, &["add", &url]);
    assert_eq!(
        added.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&added.stdout),
        String::from_utf8_lossy(&added.stderr)
    );
    assert!(String::from_utf8_lossy(&added.stdout).contains("Lifecycle Feed"));

    assert_eq!(run(&root, &["disable", "1"]).status.code(), Some(0));
    let disabled = run(&root, &["list"]);
    assert!(String::from_utf8_lossy(&disabled.stdout).contains("[已禁用]"));

    assert_eq!(
        run(&root, &["set-interval", "1", "2h"]).status.code(),
        Some(0)
    );
    let enabled = run(&root, &["enable", "1"]);
    assert_eq!(
        enabled.status.code(),
        Some(0),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&enabled.stdout),
        String::from_utf8_lossy(&enabled.stderr)
    );
    server.join().unwrap();

    let removed = run(&root, &["rm", "1"]);
    assert_eq!(removed.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&removed.stdout).contains("已删除"));
    let empty = run(&root, &["list"]);
    assert!(String::from_utf8_lossy(&empty.stdout).contains("还没有订阅源"));

    std::fs::remove_dir_all(root).unwrap();
}
