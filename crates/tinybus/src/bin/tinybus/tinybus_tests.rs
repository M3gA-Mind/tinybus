use async_trait::async_trait;
use clap::Parser;
use serde_json::Value;

use super::*;
use tinybus::module::ModuleHost;
use tinybus::name::{BusName, InterfaceName, MemberName, ObjectPath};
use tinybus::service::Interface;

const DESTINATION: &str = "ai.tinyhumans.openhuman.Echo";
const PATH: &str = "/ai/tinyhumans/openhuman/Echo";
const INTERFACE: &str = "ai.tinyhumans.openhuman.Echo";

struct Echo;

#[async_trait]
impl Interface for Echo {
    fn name(&self) -> InterfaceName {
        InterfaceName::new(INTERFACE).unwrap()
    }

    fn members(&self) -> Vec<MemberName> {
        vec![MemberName::new("Echo").unwrap()]
    }

    async fn call(&self, _member: &MemberName, args: Value) -> tinybus::Result<Value> {
        Ok(args)
    }
}

async fn broker_and_service() -> (tempfile::TempDir, PathBuf, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let address = dir.path().join("bus");
    let listener = UnixListenerAdapter::bind(&address).await.unwrap();
    Broker::new().spawn(listener);
    let service = Connection::connect(Box::new(UnixTransport::connect(&address).await.unwrap()))
        .await
        .unwrap();
    service.request_name(DESTINATION).await.unwrap();
    service
        .serve_at(ObjectPath::new(PATH).unwrap(), Echo)
        .await
        .unwrap();
    (dir, address, service)
}

async fn broker_with_module_host() -> (tempfile::TempDir, PathBuf, ModuleHost) {
    let dir = tempfile::tempdir().unwrap();
    let address = dir.path().join("bus");
    let listener = UnixListenerAdapter::bind(&address).await.unwrap();
    let broker = Broker::new();
    // The broker retains only a weak module control, so callers must keep
    // this host bound while they issue module commands.
    let host = ModuleHost::new(broker.clone());
    broker.spawn(listener);
    (dir, address, host)
}

#[test]
fn the_cli_parses_its_declared_commands_and_uses_explicit_addresses() {
    let cli = Cli::try_parse_from([
        "tinybus",
        "--address",
        "/run/user/1000/tinybus/bus",
        "--timeout",
        "7",
        "call",
        DESTINATION,
        PATH,
        INTERFACE,
        "Echo",
        "[1]",
    ])
    .unwrap();
    assert_eq!(cli.timeout, 7);
    assert_eq!(
        resolve_address(cli.address).unwrap(),
        PathBuf::from("/run/user/1000/tinybus/bus")
    );
    assert!(matches!(cli.command, Command::Call { args, .. } if args == "[1]"));
    assert!(matches!(
        Cli::try_parse_from(["tinybus", "serve"]).unwrap().command,
        Command::Serve
    ));
    assert!(matches!(
        Cli::try_parse_from(["tinybus", "list"]).unwrap().command,
        Command::List
    ));
    assert!(matches!(
        Cli::try_parse_from(["tinybus", "doctor"]).unwrap().command,
        Command::Doctor
    ));
    assert!(matches!(
        Cli::try_parse_from(["tinybus", "emit", PATH, INTERFACE, "Changed"])
            .unwrap()
            .command,
        Command::Emit { .. }
    ));
    assert!(matches!(
        Cli::try_parse_from(["tinybus", "monitor"]).unwrap().command,
        Command::Monitor { .. }
    ));
    assert!(matches!(
        Cli::try_parse_from([
            "tinybus",
            "modules",
            "reinitialize",
            "clock",
            "--config-file",
            "-"
        ])
        .unwrap()
        .command,
        Command::Modules {
            command: ModulesCommand::Reinitialize { .. }
        }
    ));
}

#[test]
fn runtime_and_monitor_rendering_are_usable() {
    assert!(runtime().is_ok());
    let call = tinybus::Message::method_call(
        BusName::new(DESTINATION).unwrap(),
        ObjectPath::new(PATH).unwrap(),
        InterfaceName::new(INTERFACE).unwrap(),
        MemberName::new("Echo").unwrap(),
        serde_json::json!(["hello"]),
    );
    assert!(render(&call).starts_with("call"));
    let signal = tinybus::Message::signal(
        ObjectPath::new(PATH).unwrap(),
        InterfaceName::new(INTERFACE).unwrap(),
        MemberName::new("Changed").unwrap(),
        serde_json::json!([]),
    );
    assert!(render(&signal).starts_with("signal"));
    assert!(
        render(&tinybus::Message::method_return(&call.header, Value::Null)).starts_with("return")
    );
    assert!(
        render(&tinybus::Message::error_reply(
            &call.header,
            &Error::ConnectionClosed
        ))
        .starts_with("error")
    );
}

#[tokio::test]
async fn call_emit_list_and_doctor_use_a_running_broker() {
    let (_dir, address, service) = broker_and_service().await;

    run(Cli {
        address: Some(address.clone()),
        timeout: 1,
        command: Command::Call {
            confidential: false,
            destination: DESTINATION.into(),
            path: PATH.into(),
            interface: INTERFACE.into(),
            member: "Echo".into(),
            args: "[\"hello\"]".into(),
        },
    })
    .await
    .unwrap();
    run(Cli {
        address: Some(address.clone()),
        timeout: 1,
        command: Command::Emit {
            path: PATH.into(),
            interface: INTERFACE.into(),
            member: "Changed".into(),
            args: "[]".into(),
        },
    })
    .await
    .unwrap();
    run(Cli {
        address: Some(address.clone()),
        timeout: 1,
        command: Command::List,
    })
    .await
    .unwrap();
    run(Cli {
        address: Some(address),
        timeout: 1,
        command: Command::Doctor,
    })
    .await
    .unwrap();
    assert!(service.unique_name().is_some());
}

#[tokio::test]
async fn module_commands_report_errors_and_valid_commands_succeed() {
    let (_dir, address, _host) = broker_with_module_host().await;
    let commands = [
        ModulesCommand::Show {
            name: "missing".into(),
            json: false,
        },
        ModulesCommand::Scan {
            paths: vec![PathBuf::from("/not/a/module")],
            dry_run: true,
        },
        ModulesCommand::Load {
            path: PathBuf::from("/not/a/module"),
            config_file: None,
        },
        ModulesCommand::LoadGithub {
            release_url: "https://example.com/not-github".into(),
            asset: "module.tar.gz".into(),
            sha256: "0".repeat(64),
            config_file: None,
        },
        ModulesCommand::Reinitialize {
            name: "missing".into(),
            config_file: None,
        },
        ModulesCommand::Stop {
            name: "missing".into(),
            deadline_ms: 1_000,
        },
        ModulesCommand::Enable {
            name: "missing".into(),
        },
        ModulesCommand::Disable {
            name: "missing".into(),
        },
    ];
    for command in commands {
        assert!(
            run_modules(&address, Duration::from_secs(2), command)
                .await
                .is_err()
        );
    }

    run_modules(
        &address,
        Duration::from_secs(2),
        ModulesCommand::List {
            state: Some("ready".into()),
            json: true,
        },
    )
    .await
    .unwrap();

    let asset = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(asset.path(), b"release asset").unwrap();
    let manifest = tempfile::NamedTempFile::new().unwrap();
    run_modules(
        &address,
        Duration::from_secs(2),
        ModulesCommand::Checksum {
            paths: vec![asset.path().to_path_buf()],
            output: Some(manifest.path().to_path_buf()),
        },
    )
    .await
    .unwrap();
    assert!(
        std::fs::read_to_string(manifest.path())
            .unwrap()
            .contains("[sha256]")
    );
    run_modules(&address, Duration::from_secs(2), ModulesCommand::Doctor)
        .await
        .unwrap();
    run_modules(
        &address,
        Duration::from_secs(2),
        ModulesCommand::Scan {
            paths: Vec::new(),
            dry_run: true,
        },
    )
    .await
    .unwrap();

    let (_dir, address, _service) = broker_and_service().await;
    for command in [
        Command::Call {
            destination: DESTINATION.into(),
            path: PATH.into(),
            interface: INTERFACE.into(),
            member: "Echo".into(),
            args: "not json".into(),
            confidential: false,
        },
        Command::Emit {
            path: PATH.into(),
            interface: INTERFACE.into(),
            member: "Changed".into(),
            args: "not json".into(),
        },
    ] {
        assert!(
            run(Cli {
                address: Some(address.clone()),
                timeout: 1,
                command,
            })
            .await
            .is_err()
        );
    }
}

#[tokio::test]
async fn a_module_stop_deadline_cannot_outlive_the_call_deadline() {
    let (_dir, address, _service) = broker_and_service().await;
    let error = run_modules(
        &address,
        Duration::from_millis(10),
        ModulesCommand::Stop {
            name: "missing".into(),
            deadline_ms: 10,
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("shorter than the RPC timeout"));
}

#[tokio::test]
#[ignore = "requires TINYBUS_TEST_MODULE to point at the built cdylib"]
async fn module_cli_controls_a_real_dynamic_module_lifecycle() {
    let path = PathBuf::from(std::env::var_os("TINYBUS_TEST_MODULE").unwrap());
    let (dir, address, _host) = broker_with_module_host().await;
    let timeout = Duration::from_secs(2);
    let config = dir.path().join("config.json");
    std::fs::write(&config, r#"{"prefix":"cli:"}"#).unwrap();

    run_modules(
        &address,
        timeout,
        ModulesCommand::Load {
            path,
            config_file: Some(config),
        },
    )
    .await
    .unwrap();
    let client = connect(&address).await.unwrap();
    let clock = client
        .proxy(
            "ai.tinyhumans.openhuman.Clock",
            "/ai/tinyhumans/openhuman/Clock",
            "ai.tinyhumans.openhuman.Clock",
        )
        .unwrap();
    let before: String = clock.call("Now", ()).await.unwrap();
    assert!(before.starts_with("cli:"));
    let replacement = dir.path().join("replacement.json");
    std::fs::write(&replacement, r#"{"prefix":"reinit:"}"#).unwrap();
    run_modules(
        &address,
        timeout,
        ModulesCommand::Reinitialize {
            name: "tinybus".into(),
            config_file: Some(replacement),
        },
    )
    .await
    .unwrap();
    let after: String = clock.call("Now", ()).await.unwrap();
    assert!(after.starts_with("reinit:"));
    run_modules(
        &address,
        timeout,
        ModulesCommand::List {
            state: None,
            json: true,
        },
    )
    .await
    .unwrap();
    run_modules(
        &address,
        timeout,
        ModulesCommand::List {
            state: None,
            json: false,
        },
    )
    .await
    .unwrap();
    run_modules(
        &address,
        timeout,
        ModulesCommand::Show {
            name: "tinybus".into(),
            json: false,
        },
    )
    .await
    .unwrap();
    run_modules(
        &address,
        timeout,
        ModulesCommand::Show {
            name: "tinybus".into(),
            json: true,
        },
    )
    .await
    .unwrap();
    run_modules(&address, timeout, ModulesCommand::Doctor)
        .await
        .unwrap();
    run_modules(
        &address,
        timeout,
        ModulesCommand::Enable {
            name: "tinybus".into(),
        },
    )
    .await
    .unwrap();
    run_modules(
        &address,
        timeout,
        ModulesCommand::Stop {
            name: "tinybus".into(),
            deadline_ms: 500,
        },
    )
    .await
    .unwrap();
    assert!(
        run_modules(
            &address,
            timeout,
            ModulesCommand::Disable {
                name: "tinybus".into(),
            },
        )
        .await
        .is_err()
    );
}
