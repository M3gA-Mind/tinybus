use super::*;
use crate::module::failure::{FailureReport, FailureReporter, FailureSink};
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

/// A host without the loader: it knows its registry and refuses every load.
struct Loaderless {
    enabled: bool,
    reporter: FailureReporter,
}

impl ModuleAccess for Loaderless {
    fn find(&self, id: &str) -> Option<&'static ReleaseRecord> {
        (id == RECORD.id).then_some(&RECORD)
    }

    fn enabled(&self) -> bool {
        self.enabled
    }

    fn reporter(&self) -> &FailureReporter {
        &self.reporter
    }

    fn ensure_loaded<'a>(&'a self, id: &'a str) -> AccessFuture<'a, Result<(), String>> {
        let _ = id;
        Box::pin(async {
            self.reporter()
                .report(RECORD.id, RECORD.version, Reason::LoaderDisabled);
            Err("this build has no module loader".to_string())
        })
    }

    fn proxy<'a>(
        &'a self,
        _bus_name: &'a str,
        _object_path: &'a str,
    ) -> AccessFuture<'a, Result<Proxy, ProxyFailure>> {
        Box::pin(async { Err(ProxyFailure::Transport) })
    }
}

fn resolver(enabled: bool) -> (Arc<Recording>, Loaderless) {
    let sink = Arc::new(Recording::default());
    let access = Loaderless {
        enabled,
        reporter: FailureReporter::new(sink.clone()),
    };
    (sink, access)
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

struct OwnerFixture(&'static str);
#[async_trait::async_trait]
impl crate::Interface for OwnerFixture {
    fn name(&self) -> crate::InterfaceName {
        crate::InterfaceName::new(RECORD.bus_name).unwrap()
    }
    fn members(&self) -> Vec<crate::MemberName> {
        ["Who", "Block"]
            .map(|name| crate::MemberName::new(name).unwrap())
            .to_vec()
    }
    async fn call(
        &self,
        member: &crate::MemberName,
        _: serde_json::Value,
    ) -> crate::Result<serde_json::Value> {
        if member.as_str() == "Block" {
            std::future::pending::<()>().await;
        }
        Ok(serde_json::json!(self.0))
    }
}

#[tokio::test]
async fn pinned_objects_and_derived_children_keep_the_original_peer() {
    let (_, access) = resolver(true);
    let bus = crate::transport::memory::MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let original = crate::Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let replacement = crate::Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    for (service, label) in [(&original, "original"), (&replacement, "replacement")] {
        for path in [RECORD.object_path, "/child"] {
            service
                .serve_at(crate::ObjectPath::new(path).unwrap(), OwnerFixture(label))
                .await
                .unwrap();
        }
    }
    original.request_name(RECORD.bus_name).await.unwrap();
    let caller = crate::Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let client = ModuleClient::new(access).with_fixture(
        caller
            .proxy(RECORD.bus_name, RECORD.object_path, RECORD.bus_name)
            .unwrap(),
    );
    let root = client
        .object_client(RECORD.id, RECORD.object_path)
        .await
        .unwrap()
        .pin_owner()
        .await
        .unwrap();
    original.release_name(RECORD.bus_name).await.unwrap();
    replacement.request_name(RECORD.bus_name).await.unwrap();
    assert_eq!(root.call::<String>("Who", ()).await.unwrap(), "original");
    let child = root.object_at("/child").unwrap();
    assert_eq!(child.call::<String>("Who", ()).await.unwrap(), "original");
    assert_eq!(
        child.call_confidential::<String>("Who", ()).await,
        Err(ModuleCallError::TransportFailed)
    );
    assert!(root.object_at("invalid path").is_err());
}

#[tokio::test(start_paused = true)]
async fn derived_objects_keep_the_configured_deadline_and_report_failures() {
    let (sink, access) = resolver(true);
    let bus = crate::transport::memory::MemoryBus::new();
    crate::broker::Broker::new().spawn(bus.clone());
    let service = crate::Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    service.request_name(RECORD.bus_name).await.unwrap();
    service
        .serve_at(
            crate::ObjectPath::new("/child").unwrap(),
            OwnerFixture("original"),
        )
        .await
        .unwrap();
    let caller = crate::Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let client = ModuleClient::new(access)
        .with_fixture(
            caller
                .proxy(RECORD.bus_name, RECORD.object_path, RECORD.bus_name)
                .unwrap(),
        )
        .with_timeout(Duration::from_millis(50));
    let root = client
        .object_client(RECORD.id, RECORD.object_path)
        .await
        .unwrap()
        .pin_owner()
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    assert_eq!(
        root.object_at("/child")
            .unwrap()
            .call::<String>("Block", ())
            .await,
        Err(ModuleCallError::TransportFailed)
    );
    assert!(start.elapsed() < Duration::from_secs(1));
    assert_eq!(
        sink.0.lock().unwrap().last().unwrap().1,
        Reason::TransportFailed
    );
    let missing = client
        .object_client(RECORD.id, RECORD.object_path)
        .await
        .unwrap();
    service.release_name(RECORD.bus_name).await.unwrap();
    assert!(matches!(
        missing.pin_owner().await,
        Err(ModuleCallError::Unavailable)
    ));
}
