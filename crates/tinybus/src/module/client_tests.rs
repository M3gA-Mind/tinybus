use super::*;
use crate::module::failure::{FailureReport, FailureReporter, FailureSink};
use crate::module::resolve::{ResolverInputs, RuntimeProvider};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Recording(Mutex<Vec<(String, Reason)>>);

impl FailureSink for Recording {
    fn capture(&self, report: &FailureReport<'_>) {
        self.0
            .lock()
            .unwrap()
            .push((report.module.to_string(), report.reason));
    }
}

static RECORD: ReleaseRecord = ReleaseRecord {
    id: "client-test-module",
    bus_name: "ai.tinyhumans.test.ClientModule",
    object_path: "/ai/tinyhumans/test/ClientModule",
    version: "0.0.0",
    release_url: "https://github.com/tinyhumansai/tinybus/releases/tag/v0.0.0",
    assets: &[],
};

fn resolver(enabled: bool) -> (Arc<Recording>, Resolver) {
    let sink = Arc::new(Recording::default());
    let runtime: RuntimeProvider = Arc::new(|| {
        Box::pin(async { Err(Error::Transport("no runtime in this test".to_string())) })
    });
    let resolver = Resolver::new(ResolverInputs {
        enabled,
        install_dir: None,
        allow_download: false,
        bundled_dir: None,
        sources: HashMap::new(),
        linked: HashMap::new(),
        search_skip: Vec::new(),
        gate: None,
        on_resolved: None,
        records: vec![&RECORD],
        terminal_marker: String::new(),
        runtime,
        module_config: Arc::new(|_| serde_json::json!({})),
        reporter: Arc::new(FailureReporter::new(sink.clone())),
    });
    (sink, resolver)
}

fn method_failed(name: &str) -> Error {
    Error::MethodFailed {
        name: name.to_string(),
        message: String::new(),
    }
}

#[test]
fn bus_errors_classify_to_closed_outcomes() {
    let cases = [
        (
            Error::ModuleUnavailable {
                module: "m".to_string(),
                state: String::new(),
                detail: String::new(),
            },
            ModuleCallError::Unavailable,
            Reason::ModuleUnavailable,
        ),
        (
            method_failed("ai.tinyhumans.tinybus.Error.ModuleUnavailable"),
            ModuleCallError::Unavailable,
            Reason::ModuleUnavailable,
        ),
        (
            method_failed(Error::UNKNOWN_METHOD),
            ModuleCallError::IncompatibleContract,
            Reason::IncompatibleContract,
        ),
        (
            method_failed(Error::NOT_ATTESTED),
            ModuleCallError::TransportFailed,
            Reason::TransportFailed,
        ),
        (
            method_failed("com.example.Whatever"),
            ModuleCallError::ModuleFault,
            Reason::ModuleFault,
        ),
        (
            Error::Transport("down".to_string()),
            ModuleCallError::TransportFailed,
            Reason::TransportFailed,
        ),
    ];
    for (error, outcome, reason) in cases {
        assert_eq!(classify(&error), (outcome, reason), "{error:?}");
    }
}

#[test]
fn call_errors_render_the_already_reported_prefix() {
    for error in [
        ModuleCallError::Unavailable,
        ModuleCallError::IncompatibleContract,
        ModuleCallError::TransportFailed,
        ModuleCallError::ModuleFault,
    ] {
        assert!(
            error
                .to_string()
                .starts_with("MODULE_CALL_REPORTED: module ")
        );
    }
}

#[tokio::test]
async fn an_unknown_module_is_unavailable_and_reported_anonymously() {
    let (sink, resolver) = resolver(true);
    let client = ModuleClient::new(resolver);
    let result: Result<String, _> = client.call("not-registered", "Member", ()).await;
    assert_eq!(result, Err(ModuleCallError::Unavailable));
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        &[("unregistered".to_string(), Reason::UnknownModule)]
    );
}

#[tokio::test]
async fn a_disabled_registry_refuses_without_starting_anything() {
    let (sink, resolver) = resolver(false);
    let client = ModuleClient::new(resolver);
    let result: Result<String, _> = client.call(RECORD.id, "Member", ()).await;
    assert_eq!(result, Err(ModuleCallError::Unavailable));
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        &[(RECORD.id.to_string(), Reason::Disabled)]
    );
}

#[test]
fn malformed_json_is_a_module_fault_reported_by_identity_only() {
    let (sink, resolver) = resolver(true);
    let client = ModuleClient::new(resolver);
    let decoded: Result<serde_json::Value, _> = client.decode_json(RECORD.id, "not json {");
    assert_eq!(decoded, Err(ModuleCallError::ModuleFault));
    let decoded: Result<serde_json::Value, _> = client.decode_json(RECORD.id, "{\"a\":1}");
    assert!(decoded.is_ok());
    assert_eq!(
        sink.0.lock().unwrap().as_slice(),
        &[(RECORD.id.to_string(), Reason::ModuleFault)]
    );
}

#[tokio::test]
async fn a_fixture_proxy_is_classified_and_reported_when_the_call_fails() {
    use crate::broker::Broker;
    use crate::transport::memory::MemoryBus;

    let (sink, resolver) = resolver(true);
    let transport = MemoryBus::new();
    Broker::new().spawn(transport.clone());
    let connection = crate::Connection::connect(transport.connect().await.unwrap())
        .await
        .unwrap();
    let proxy = connection
        .proxy(RECORD.bus_name, RECORD.object_path, RECORD.bus_name)
        .unwrap();
    let client = ModuleClient::new(resolver)
        .with_fixture(proxy)
        .with_timeout(Duration::from_millis(500));
    assert!(client.fixture_proxy().is_some());
    let result: Result<String, _> = client.call(RECORD.id, "Member", ()).await;
    assert!(result.is_err());
    assert_eq!(sink.0.lock().unwrap().len(), 1);
}

struct JsonFixture;
#[async_trait::async_trait]
impl crate::Interface for JsonFixture {
    fn name(&self) -> crate::InterfaceName {
        crate::InterfaceName::new(RECORD.bus_name).unwrap()
    }
    fn members(&self) -> Vec<crate::MemberName> {
        vec![crate::MemberName::new("Json").unwrap()]
    }
    async fn call(
        &self,
        _: &crate::MemberName,
        _: serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        Ok(serde_json::json!("{\"answer\":42}"))
    }
}

#[tokio::test]
async fn fixture_json_and_object_calls_preserve_deadlines_and_confidential_refusals() {
    let (sink, resolver) = resolver(true);
    let bus = crate::transport::memory::MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let service = crate::Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    service.request_name(RECORD.bus_name).await.unwrap();
    for path in [RECORD.object_path, "/child"] {
        service
            .serve_at(crate::ObjectPath::new(path).unwrap(), JsonFixture)
            .await
            .unwrap();
    }
    let caller = crate::Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let proxy = caller
        .proxy(RECORD.bus_name, RECORD.object_path, RECORD.bus_name)
        .unwrap();
    let client = ModuleClient::new(resolver)
        .with_fixture(proxy)
        .with_timeout(Duration::from_secs(1));
    let reply: serde_json::Value = client.call_json(RECORD.id, "Json", ()).await.unwrap();
    assert_eq!(reply, serde_json::json!({"answer":42}));
    let object = client.object_client(RECORD.id, "/child").await.unwrap();
    let reply: String = object.call("Json", ()).await.unwrap();
    assert_eq!(reply, "{\"answer\":42}");
    assert!(
        client
            .object_client(RECORD.id, "invalid path")
            .await
            .is_err()
    );
    let reply: Result<String, _> = object.call_confidential("Json", ()).await;
    assert_eq!(reply, Err(ModuleCallError::TransportFailed));
    let reply: Result<String, _> = client.call_confidential(RECORD.id, "Json", ()).await;
    assert_eq!(reply, Err(ModuleCallError::TransportFailed));
    let reply: Result<serde_json::Value, _> =
        client.call_confidential_json(RECORD.id, "Json", ()).await;
    assert_eq!(reply, Err(ModuleCallError::TransportFailed));
    assert!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .all(|(_, reason)| *reason == Reason::TransportFailed)
    );
}

#[tokio::test]
async fn unavailable_loads_and_explicit_reports_use_only_registered_identity() {
    let (sink, resolver) = resolver(true);
    let client = ModuleClient::new(resolver);
    let reply: Result<String, _> = client.call(RECORD.id, "Json", ()).await;
    assert_eq!(reply, Err(ModuleCallError::Unavailable));
    assert!(client.object_client(RECORD.id, "/child").await.is_err());
    client.report_incompatible_contract(RECORD.id);
    client.report_unavailable(RECORD.id);
    client.report_malformed_reply("unknown");
    client.report_incompatible_contract("unknown");
    client.report_unavailable("unknown");
    assert!(
        sink.0
            .lock()
            .unwrap()
            .iter()
            .all(|(module, _)| module == RECORD.id)
    );
}
