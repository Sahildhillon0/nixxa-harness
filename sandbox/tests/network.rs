//! The network allowlist in the real sandbox: what commands can reach through the proxy, what
//! is refused and logged, and what stays unreachable whatever the policy.

#![cfg(target_os = "linux")]

mod common;

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use futures::StreamExt;
use nixxa_contracts::{
    DomainRule, NetRequest, NetResult, NetworkAccess, NetworkPolicy, Observation, Outcome, Sandbox, SandboxAction,
    SandboxEvent,
};
use nixxa_sandbox::bwrap::BwrapSandbox;

use common::{Scratch, config, exec, run, sandbox, sandbox_with, sh, sh_out};

fn allow(rules: &[&str]) -> NetworkPolicy {
    NetworkPolicy::Allow(rules.iter().map(|rule| DomainRule::parse(rule).unwrap()).collect())
}

/// A sandbox where `local.test` resolves to 127.0.0.1 (without asking the system resolver).
fn with_local_name(scratch: &Scratch) -> Option<BwrapSandbox> {
    let mut config = config(&scratch.workspace());
    config.hosts.insert("local.test".into(), vec![IpAddr::V4(Ipv4Addr::LOCALHOST)]);
    sandbox_with(config)
}

async fn observe(sb: &BwrapSandbox, command: &str) -> Observation {
    run(sb, exec(command)).await.expect("exec")
}

fn reason(request: &NetRequest) -> &str {
    match &request.result {
        NetResult::Blocked { reason } => reason,
        NetResult::Allowed => panic!("expected blocked: {request:?}"),
    }
}

/// `curl` through the proxy (the environment says where it is); prints the exit code.
fn curl(url: &str) -> String {
    format!("curl -sS -m 10 -o /dev/null {url}; echo exit=$?")
}

#[tokio::test]
async fn off_is_the_stage_4_sandbox() {
    let scratch = Scratch::new("net-off");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    assert_eq!(sb.capabilities().network, NetworkAccess::Controlled(NetworkPolicy::Off));
    assert_eq!(sh_out(&sb, "env | grep -ci proxy || true").await.trim(), "0", "no proxy variables while off");
    let obs = observe(&sb, &curl("https://example.com")).await;
    let Outcome::Exec { stdout, .. } = &obs.outcome else { panic!("{obs:?}") };
    assert!(!stdout.ends_with("exit=0\n"), "{stdout}");
    // Even pointing at the proxy by hand gets nowhere.
    let by_hand = sh_out(&sb, "curl -sS -m 5 -x http://127.0.0.1:3128 https://example.com; echo exit=$?").await;
    assert!(!by_hand.ends_with("exit=0\n"), "{by_hand}");
    assert!(obs.network.is_empty(), "nothing is logged while off: {:?}", obs.network);
    let quiet = tokio::time::timeout(Duration::from_millis(300), events.next()).await;
    assert!(quiet.is_err(), "no events while off: {quiet:?}");
}

#[tokio::test]
async fn a_host_that_is_not_allowed_is_blocked_and_the_observation_says_why() {
    let scratch = Scratch::new("net-blocked");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    sb.set_network(allow(&["crates.io"])).await.unwrap();
    assert_eq!(sb.capabilities().network, NetworkAccess::Controlled(allow(&["crates.io"])));
    let obs = observe(&sb, &curl("https://example.com")).await;
    let Outcome::Exec { stdout, stderr, .. } = &obs.outcome else { panic!("{obs:?}") };
    assert!(stdout.ends_with("exit=56\n") || stdout.ends_with("exit=7\n"), "{stdout}");
    assert!(stderr.contains("403"), "curl sees the proxy's refusal: {stderr}");
    let [request] = obs.network.as_slice() else { panic!("one request: {:?}", obs.network) };
    assert_eq!((request.host.as_str(), request.port, request.job.clone()), ("example.com", 443, None));
    assert_eq!(reason(request), "host example.com isn't allowed in this session; ask the user to allow it");
    match tokio::time::timeout(Duration::from_secs(5), events.next()).await {
        Ok(Some(SandboxEvent::NetRequest(event))) => assert_eq!(&event, request),
        other => panic!("the request is also an event: {other:?}"),
    }
}

#[tokio::test]
async fn an_allowed_name_that_resolves_to_this_machine_is_refused() {
    let scratch = Scratch::new("net-local");
    let Some(sb) = with_local_name(&scratch) else { return };
    sb.set_network(allow(&["local.test"])).await.unwrap();
    for url in ["https://local.test", "http://local.test/"] {
        let obs = observe(&sb, &curl(url)).await;
        let [request] = obs.network.as_slice() else { panic!("{url}: {:?}", obs.network) };
        assert_eq!(reason(request), "local.test resolves to 127.0.0.1, a local or private address", "{url}");
    }
    sb.set_network(NetworkPolicy::Open).await.unwrap();
    let obs = observe(&sb, &curl("https://local.test")).await;
    assert!(reason(&obs.network[0]).contains("local or private"), "Open doesn't reach it either");
}

#[tokio::test]
async fn ip_literals_and_other_ports_are_refused() {
    let scratch = Scratch::new("net-ip");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    sb.set_network(NetworkPolicy::Open).await.unwrap();
    let obs = observe(&sb, &curl("https://1.1.1.1")).await;
    assert!(reason(&obs.network[0]).contains("IP address"), "{:?}", obs.network);
    let obs = observe(&sb, &curl("https://example.com:22")).await;
    assert!(reason(&obs.network[0]).starts_with("port 22 isn't allowed"), "{:?}", obs.network);
}

#[tokio::test]
async fn raw_tcp_and_udp_dns_still_fail_with_the_network_open() {
    let scratch = Scratch::new("net-raw");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    sb.set_network(NetworkPolicy::Open).await.unwrap();
    // Around the proxy: the namespace has only loopback.
    let direct = sh_out(&sb, "curl -sS -m 5 --noproxy '*' -o /dev/null https://1.1.1.1; echo exit=$?").await;
    assert!(!direct.ends_with("exit=0\n"), "{direct}");
    let (code, out, err) = sh(
        &sb,
        "python3 -c 'import socket\n\
         s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)\n\
         s.settimeout(3)\n\
         s.sendto(bytes.fromhex(\"12340100000100000000000007\") + b\"example\\x03com\\x00\\x00\\x01\\x00\\x01\", (\"1.1.1.1\", 53))\n\
         print(s.recvfrom(512))'",
    )
    .await;
    assert_ne!(code, Some(0), "a UDP DNS query got an answer: {out}{err}");
    let tcp =
        sh_out(&sb, "python3 -c 'import socket; socket.create_connection((\"1.1.1.1\", 443), 3)' 2>&1; echo exit=$?")
            .await;
    assert!(!tcp.ends_with("exit=0\n"), "{tcp}");
}

#[tokio::test]
async fn the_proxy_directory_can_only_be_connected_to() {
    let scratch = Scratch::new("net-dir");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let (code, _, _) = sh(&sb, "ls /run/nixxa").await;
    assert_ne!(code, Some(0), "listing it is refused");
    let (code, _, _) = sh(&sb, "touch /run/nixxa/x").await;
    assert_ne!(code, Some(0), "writing in it is refused");
}

#[tokio::test]
async fn a_background_jobs_requests_carry_its_id() {
    let scratch = Scratch::new("net-job");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    let mut events = sb.subscribe();
    sb.set_network(allow(&["crates.io"])).await.unwrap();
    let action =
        SandboxAction::Exec { command: curl("https://example.com"), cwd: None, timeout_ms: None, background: true };
    let Outcome::Started { job } = run(&sb, action).await.unwrap().outcome else { panic!("not started") };
    let request = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(SandboxEvent::NetRequest(request)) = events.next().await {
                return request;
            }
        }
    })
    .await
    .expect("the job's request is logged");
    assert_eq!(request.job, Some(job));
}

#[tokio::test]
async fn turning_the_network_off_again_takes_effect_for_the_next_command() {
    let scratch = Scratch::new("net-toggle");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    sh(&sb, "true").await; // the daemon is running before the change
    sb.set_network(allow(&["crates.io"])).await.unwrap();
    assert_ne!(sh_out(&sb, "env | grep -c '^https_proxy=' || true").await.trim(), "0");
    sb.set_network(NetworkPolicy::Off).await.unwrap();
    assert_eq!(sh_out(&sb, "env | grep -ci proxy || true").await.trim(), "0");
    let obs = observe(&sb, "curl -sS -m 5 -x http://127.0.0.1:3128 https://crates.io; echo exit=$?").await;
    assert!(obs.network.is_empty(), "{:?}", obs.network);
}

/// The Stage 6 exit criterion: `cargo test` on a fresh project with a crates.io dependency,
/// with only crates.io allowed, and another host blocked and logged.
#[tokio::test]
#[ignore = "needs the internet (crates.io)"]
async fn needs_internet_cargo_test_with_only_crates_io_allowed() {
    let scratch = Scratch::new("net-cargo");
    let ws = scratch.workspace();
    std::fs::create_dir_all(ws.join("demo/src")).unwrap();
    std::fs::write(
        ws.join("demo/Cargo.toml"),
        "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nitoa = \"1\"\n",
    )
    .unwrap();
    std::fs::write(
        ws.join("demo/src/lib.rs"),
        "#[test]\nfn formats() { assert_eq!(itoa::Buffer::new().format(42), \"42\"); }\n",
    )
    .unwrap();
    let mut config = config(&ws);
    config.default_timeout = Duration::from_secs(300);
    // The production process limit: the test default (64) is too low for the linker's threads.
    config.pids_max = nixxa_sandbox::bwrap::BwrapConfig::new(&ws, "unused").pids_max;
    let Some(sb) = sandbox_with(config) else { return };
    let crates = nixxa_sandbox::network::PRESETS.iter().find(|p| p.name == "crates.io").unwrap();
    sb.set_network(NetworkPolicy::Allow(crates.rules())).await.unwrap();
    let action = SandboxAction::Exec {
        command: "cd demo && cargo test --lib 2>&1 | tail -5".into(),
        cwd: None,
        timeout_ms: Some(300_000),
        background: false,
    };
    let obs = run(&sb, action).await.unwrap();
    let Outcome::Exec { stdout, .. } = &obs.outcome else { panic!("{obs:?}") };
    assert!(stdout.contains("test result: ok. 1 passed"), "{stdout}");
    assert!(obs.network.iter().all(|r| r.result == NetResult::Allowed), "{:?}", obs.network);
    assert!(obs.network.iter().any(|r| r.host == "static.crates.io"), "{:?}", obs.network);
    let blocked = observe(&sb, &curl("https://example.com")).await;
    assert_eq!(reason(&blocked.network[0]), "host example.com isn't allowed in this session; ask the user to allow it");
}

/// A tunnel opened to an allowed host can't carry a TLS session for another name.
#[tokio::test]
#[ignore = "needs the internet (example.com)"]
async fn needs_internet_a_tunnel_for_another_server_name_is_refused() {
    let scratch = Scratch::new("net-sni");
    let Some(sb) = sandbox(&scratch.workspace()) else { return };
    sb.set_network(allow(&["example.com"])).await.unwrap();
    let ok = observe(&sb, &curl("https://example.com")).await;
    assert_eq!(ok.network[0].result, NetResult::Allowed, "{:?}", ok.network);
    // curl asks the proxy for example.com:443 but says `evil.example` in its ClientHello.
    let obs = observe(&sb, &curl("--connect-to evil.example:443:example.com:443 https://evil.example")).await;
    let [request] = obs.network.as_slice() else { panic!("{:?}", obs.network) };
    assert_eq!(request.host, "example.com");
    assert_eq!(reason(request), "the TLS server name `evil.example` doesn't match example.com");
}
