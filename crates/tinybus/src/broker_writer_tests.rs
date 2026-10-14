//! A message refused for one peer must not stop the broker delivering to it.

use super::*;
use std::sync::Mutex;

/// Refuses messages whose member is `TooBig` with a protocol error and records
/// the rest; fails the link outright on `Broken`.
struct Recording {
    delivered: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl Transport for Recording {
    async fn send(&self, message: Message) -> Result<()> {
        let member = message.member_or_unknown().to_string();
        match member.as_str() {
            "TooBig" => Err(Error::protocol("frame exceeds the size cap")),
            "Broken" => Err(Error::transport("link failed")),
            _ => {
                self.delivered.lock().unwrap().push(member);
                Ok(())
            }
        }
    }

    async fn recv(&self) -> Result<Option<Message>> {
        Ok(None)
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }
}

fn signal(member: &str) -> Message {
    Message::signal(
        ObjectPath::new("/test").unwrap(),
        InterfaceName::new("test.Signals").unwrap(),
        MemberName::new(member).unwrap(),
        serde_json::json!([]),
    )
}

#[tokio::test]
async fn a_refused_message_is_dropped_and_the_next_one_is_delivered() {
    let transport = Arc::new(Recording {
        delivered: Mutex::new(Vec::new()),
    });
    let (tx, rx) = mpsc::channel(8);
    let writer = tokio::spawn(writer_task(transport.clone(), rx));
    for member in ["First", "TooBig", "Second"] {
        tx.send(signal(member)).await.unwrap();
    }
    drop(tx);
    writer.await.unwrap();
    assert_eq!(*transport.delivered.lock().unwrap(), ["First", "Second"]);
}

#[tokio::test]
async fn a_failed_link_still_ends_the_writer() {
    let transport = Arc::new(Recording {
        delivered: Mutex::new(Vec::new()),
    });
    let (tx, rx) = mpsc::channel(8);
    let writer = tokio::spawn(writer_task(transport.clone(), rx));
    for member in ["First", "Broken", "Never"] {
        tx.send(signal(member)).await.unwrap();
    }
    writer.await.unwrap();
    assert_eq!(*transport.delivered.lock().unwrap(), ["First"]);
}
