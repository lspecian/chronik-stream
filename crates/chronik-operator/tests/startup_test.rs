//! The operator binary starts: it gets as far as contacting the Kubernetes API.
//!
//! 0.4.0 panicked before that: the tree links two rustls crypto providers
//! (`ring` through kube, `aws-lc-rs` through the Kafka TLS client) and nothing
//! chose one, so building the kube client aborted the process. Every unit test
//! passed, because none of them ran `chronik-operator start`.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn start_reaches_the_kubernetes_api_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    // An API server nothing listens on: the operator fails to connect, which
    // is the point this test needs it to reach.
    std::fs::write(
        &kubeconfig,
        "apiVersion: v1\nkind: Config\nclusters:\n- cluster: {server: \"https://127.0.0.1:1\", insecure-skip-tls-verify: true}\n  name: t\ncontexts:\n- context: {cluster: t, user: t}\n  name: t\ncurrent-context: t\nusers:\n- name: t\n  user: {token: t}\n",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_chronik-operator"))
        .arg("start")
        .env("KUBECONFIG", &kubeconfig)
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // Give it time to build the client and try the API, then stop it.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();

    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert!(
        !out.contains("panicked"),
        "the operator panicked at startup:\n{out}"
    );
    assert!(
        out.contains("Starting chronik-operator"),
        "the operator did not start:\n{out}"
    );
}
