use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use super::*;
use crate::broker::Broker;
use crate::message::Message;
use crate::service::Interface;
use crate::transport::memory::MemoryBus;

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

    async fn call(&self, _member: &MemberName, args: Value) -> Result<Value> {
        Ok(args)
    }
}

async fn peers() -> (Connection, Connection) {
    let bus = MemoryBus::new();
    Broker::new().spawn(bus.clone());
    let service = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    let client = Connection::connect(bus.connect().await.unwrap())
        .await
        .unwrap();
    (service, client)
}

fn proxy(connection: Connection) -> Proxy {
    Proxy::new(
        connection,
        BusName::new(DESTINATION).unwrap(),
        ObjectPath::new(PATH).unwrap(),
        InterfaceName::new(INTERFACE).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn a_proxy_calls_its_service_and_exposes_its_address() {
    let (service, client) = peers().await;
    service.request_name(DESTINATION).await.unwrap();
    service
        .serve_at(ObjectPath::new(PATH).unwrap(), Echo)
        .await
        .unwrap();

    let proxy = proxy(client).with_timeout(Duration::from_millis(250));
    assert_eq!(proxy.destination().as_str(), DESTINATION);
    assert_eq!(proxy.path().as_str(), PATH);
    assert_eq!(proxy.interface().as_str(), INTERFACE);
    assert!(proxy.is_available().await.unwrap());
    assert_eq!(
        proxy.call::<Value>("Echo", ("hello",)).await.unwrap(),
        serde_json::json!(["hello"])
    );
    assert!(format!("{proxy:?}").contains(DESTINATION));
    assert!(proxy.connection().unique_name().is_some());
}

#[tokio::test]
async fn a_proxy_reports_a_missing_destination_and_filters_its_signals() {
    let (service, client) = peers().await;
    let proxy = proxy(client);
    assert!(!proxy.is_available().await.unwrap());

    service
        .emit(
            ObjectPath::new(PATH).unwrap(),
            InterfaceName::new(INTERFACE).unwrap(),
            MemberName::new("Changed").unwrap(),
            serde_json::json!([]),
        )
        .await
        .unwrap();
    let mut signals = proxy.receive_signal("Changed").await.unwrap();

    service
        .emit(
            ObjectPath::new("/ai/tinyhumans/openhuman/Other").unwrap(),
            InterfaceName::new(INTERFACE).unwrap(),
            MemberName::new("Changed").unwrap(),
            serde_json::json!(["outside"]),
        )
        .await
        .unwrap();
    service
        .emit(
            ObjectPath::new(PATH).unwrap(),
            InterfaceName::new(INTERFACE).unwrap(),
            MemberName::new("Changed").unwrap(),
            serde_json::json!(["inside"]),
        )
        .await
        .unwrap();

    let message: Message = tokio::time::timeout(Duration::from_secs(5), signals.recv())
        .await
        .expect("matching signal arrives")
        .expect("connection stays live");
    assert_eq!(message.body, serde_json::json!(["inside"]));
}
