//! Durable, bounded signed failure notices. Relay retries reuse the same event.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{mpsc, Notify};

const MAX_NOTICES: usize = 256;
const MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_EVENT_BYTES: usize = 16 * 1024;

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Image {
    pending: BTreeMap<String, nostr::Event>,
    delivered: BTreeSet<String>,
}

fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn notice_key(batch: uuid::Uuid, event: &nostr::Event) -> String {
    format!("{batch}:{}", digest(event.content.as_bytes()))
}

struct Store {
    path: PathBuf,
    owner: nostr::PublicKey,
}

impl Store {
    fn open(path: PathBuf, owner: nostr::PublicKey) -> io::Result<Self> {
        let store = Self { path, owner };
        store.pending()?;
        Ok(store)
    }

    fn image(&self) -> io::Result<Image> {
        let metadata = match std::fs::symlink_metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Image::default()),
            Err(error) => return Err(error),
        };
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(io::Error::other("invalid or oversized notice outbox"));
        }
        let mut bytes = Vec::new();
        std::fs::File::open(&self.path)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::other("notice outbox byte cap exceeded"));
        }
        let image: Image = serde_json::from_slice(&bytes)?;
        if image.pending.len() > MAX_NOTICES || image.delivered.len() > 4000 {
            return Err(io::Error::other("notice outbox count cap exceeded"));
        }
        for event in image.pending.values() {
            if event.pubkey != self.owner
                || event.kind != nostr::Kind::Custom(9)
                || serde_json::to_vec(event)?.len() > MAX_EVENT_BYTES
                || event.verify().is_err()
            {
                return Err(io::Error::other("invalid signed notice in outbox"));
            }
        }
        Ok(image)
    }

    fn pending(&self) -> io::Result<BTreeMap<String, nostr::Event>> {
        Ok(self.image()?.pending)
    }

    fn enqueue(&self, key: String, event: nostr::Event) -> io::Result<bool> {
        if event.pubkey != self.owner
            || event.kind != nostr::Kind::Custom(9)
            || event.verify().is_err()
            || serde_json::to_vec(&event)?.len() > MAX_EVENT_BYTES
        {
            return Err(io::Error::other("invalid failure notice"));
        }
        let mut image = self.image()?;
        if image.delivered.contains(&key) {
            return Ok(true);
        }
        if image.pending.contains_key(&key) {
            return Ok(false);
        }
        if image.pending.len() >= MAX_NOTICES || image.pending.len() + image.delivered.len() >= 4000
        {
            return Err(io::Error::other(
                "notice outbox full; retain parked custody and pause admission",
            ));
        }
        image.pending.insert(key, event);
        self.write(&image)?;
        Ok(false)
    }

    fn acknowledge(&self, id: &str) -> io::Result<()> {
        let mut image = self.image()?;
        if image.pending.remove(id).is_some() {
            image.delivered.insert(id.to_owned());
        }
        self.write(&image)
    }

    fn write(&self, pending: &Image) -> io::Result<()> {
        let bytes = serde_json::to_vec(pending)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(io::Error::other("notice outbox byte cap exceeded"));
        }
        super::state_dir::write_atomic(&self.path, &bytes)
    }
}

type Ack = (
    mpsc::UnboundedSender<crate::pool::NoticeAck>,
    crate::pool::NoticeAck,
);
struct Delivery {
    store: Store,
    relay: String,
    acknowledgements: BTreeMap<String, Ack>,
}
struct Service {
    delivery: Mutex<Delivery>,
    wake: Notify,
}
static SERVICE: OnceLock<Arc<Service>> = OnceLock::new();

/// Open this relay/agent's durable outbox and resume pending delivery at startup.
/// Must be called after acquiring the reliability runtime's exclusive state lock.
pub fn initialize(dir: &Path, rest: &crate::relay::RestClient) -> Result<(), String> {
    let digest = digest(rest.base_url.as_bytes());
    let path = dir.join(format!("failure-notices-{digest}.json"));
    if let Some(service) = SERVICE.get() {
        let delivery = service
            .delivery
            .lock()
            .map_err(|_| "notice outbox lock poisoned")?;
        if delivery.store.path == path && delivery.store.owner == rest.keys.public_key() {
            return Ok(());
        }
        return Err("notice outbox already belongs to another agent or relay".into());
    }
    let store = Store::open(path, rest.keys.public_key()).map_err(|error| error.to_string())?;
    let service = Arc::new(Service {
        delivery: Mutex::new(Delivery {
            store,
            relay: rest.base_url.clone(),
            acknowledgements: BTreeMap::new(),
        }),
        wake: Notify::new(),
    });
    SERVICE
        .set(service.clone())
        .map_err(|_| "notice outbox initialized concurrently")?;
    let rest = rest.clone();
    tokio::spawn(async move {
        loop {
            if let Err(error) = drain(&service, &rest).await {
                tracing::error!(%error, "failure notices remain durable; delivery will retry");
            }
            tokio::select! {
                _ = service.wake.notified() => {},
                _ = tokio::time::sleep(Duration::from_secs(30)) => {},
            }
        }
    });
    Ok(())
}

/// Commit before sending; refusal must leave the caller's parked custody intact.
pub(crate) fn enqueue(
    rest: &crate::relay::RestClient,
    batch_id: uuid::Uuid,
    event: nostr::Event,
    ack: Option<Ack>,
) -> Result<(), String> {
    let service = SERVICE
        .get()
        .ok_or("notice outbox unavailable; retain parked custody")?;
    let mut delivery = service
        .delivery
        .lock()
        .map_err(|_| "notice outbox lock poisoned")?;
    if delivery.relay != rest.base_url || delivery.store.owner != rest.keys.public_key() {
        return Err("notice outbox identity mismatch".into());
    }
    let id = notice_key(batch_id, &event);
    let delivered = delivery
        .store
        .enqueue(id.clone(), event)
        .map_err(|error| error.to_string())?;
    if let Some((sender, ack)) = ack {
        if delivered {
            let _ = sender.send(ack);
        } else {
            delivery.acknowledgements.insert(id, (sender, ack));
        }
    }
    drop(delivery);
    service.wake.notify_one();
    Ok(())
}

/// Recovery recognizes any notice already committed for the same parked batch.
pub fn contains_batch(rest: &crate::relay::RestClient, batch: uuid::Uuid) -> Result<bool, String> {
    let service = SERVICE.get().ok_or("notice outbox unavailable")?;
    let delivery = service
        .delivery
        .lock()
        .map_err(|_| "notice outbox lock poisoned")?;
    if delivery.relay != rest.base_url || delivery.store.owner != rest.keys.public_key() {
        return Err("notice outbox identity mismatch".into());
    }
    let image = delivery.store.image().map_err(|error| error.to_string())?;
    let prefix = format!("{batch}:");
    Ok(image
        .pending
        .keys()
        .chain(image.delivered.iter())
        .any(|key| key.starts_with(&prefix)))
}

/// Retire deduplication receipts only after the corresponding parked custody is absent.
pub fn retain_for_parked_batches(
    rest: &crate::relay::RestClient,
    batches: &HashSet<uuid::Uuid>,
) -> Result<(), String> {
    let service = SERVICE.get().ok_or("notice outbox unavailable")?;
    let delivery = service
        .delivery
        .lock()
        .map_err(|_| "notice outbox lock poisoned")?;
    if delivery.relay != rest.base_url || delivery.store.owner != rest.keys.public_key() {
        return Err("notice outbox identity mismatch".into());
    }
    let mut image = delivery.store.image().map_err(|error| error.to_string())?;
    let previous_count = image.delivered.len();
    image.delivered.retain(|key| {
        key.split_once(':')
            .and_then(|(batch, _)| uuid::Uuid::parse_str(batch).ok())
            .is_some_and(|batch| batches.contains(&batch))
    });
    if image.delivered.len() == previous_count {
        return Ok(());
    }
    delivery
        .store
        .write(&image)
        .map_err(|error| error.to_string())
}

async fn drain(service: &Service, rest: &crate::relay::RestClient) -> Result<(), String> {
    let pending = service
        .delivery
        .lock()
        .map_err(|_| "notice outbox lock poisoned")?
        .store
        .pending()
        .map_err(|error| error.to_string())?;
    for (id, event) in pending {
        // Failure retains the exact signed event on disk for the next round/restart.
        match tokio::time::timeout(Duration::from_secs(5), rest.submit_event(&event)).await {
            Ok(Ok(_)) => {
                let mut delivery = service
                    .delivery
                    .lock()
                    .map_err(|_| "notice outbox lock poisoned")?;
                delivery
                    .store
                    .acknowledge(&id)
                    .map_err(|error| error.to_string())?;
                if let Some((sender, ack)) = delivery.acknowledgements.remove(&id) {
                    let _ = sender.send(ack);
                }
            }
            _ => return Err("relay has not acknowledged failure notice".into()),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(keys: &nostr::Keys, text: &str) -> nostr::Event {
        nostr::EventBuilder::new(nostr::Kind::Custom(9), text)
            .sign_with_keys(keys)
            .unwrap()
    }

    #[test]
    fn notice_outbox_survives_restart_and_retries_identical_event() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        let keys = nostr::Keys::generate();
        let event = notice(&keys, "notice");
        let id = event.id.to_hex();
        Store::open(path.clone(), keys.public_key())
            .unwrap()
            .enqueue(id.clone(), event.clone())
            .unwrap();
        let reopened = Store::open(path, keys.public_key()).unwrap();
        assert_eq!(reopened.pending().unwrap().get(&id), Some(&event));
        // A failed network attempt performs no acknowledgement: next read is identical.
        assert_eq!(reopened.pending().unwrap().get(&id), Some(&event));
        reopened.acknowledge(&id).unwrap();
        assert!(reopened.pending().unwrap().is_empty());
    }

    #[test]
    fn notice_outbox_receipt_closes_enqueue_marker_crash_gap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        let keys = nostr::Keys::generate();
        let batch = uuid::Uuid::new_v4();
        let event = notice(&keys, "parked");
        let key = notice_key(batch, &event);
        let store = Store::open(path.clone(), keys.public_key()).unwrap();
        assert!(!store.enqueue(key.clone(), event).unwrap());
        store.acknowledge(&key).unwrap();
        // Delivery completed but the park marker was never updated before crash.
        let recovered = Store::open(path, keys.public_key()).unwrap();
        let rebuilt = nostr::EventBuilder::new(nostr::Kind::Custom(9), "parked")
            .custom_created_at(nostr::Timestamp::from_secs(1))
            .sign_with_keys(&keys)
            .unwrap();
        assert!(recovered
            .enqueue(notice_key(batch, &rebuilt), rebuilt)
            .unwrap());
        assert!(recovered.pending().unwrap().is_empty());
        assert!(recovered.image().unwrap().delivered.contains(&key));
    }

    #[tokio::test]
    async fn notice_outbox_failed_delivery_restarts_with_identical_signed_event() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for status in ["500 Internal Server Error", "200 OK"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buffer = vec![0; 32768];
                let received = socket.read(&mut buffer).await.unwrap();
                assert!(received > 0, "client must send its notice request");
                let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: 4\r\nConnection: close\r\n\r\nnull");
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let keys = nostr::Keys::generate();
        let rest = crate::relay::RestClient {
            http: reqwest::Client::new(),
            base_url: base_url.clone(),
            keys: keys.clone(),
            auth_tag_json: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        let event = notice(&keys, "retry me");
        let key = notice_key(uuid::Uuid::new_v4(), &event);
        let make_service = || Service {
            delivery: Mutex::new(Delivery {
                store: Store::open(path.clone(), keys.public_key()).unwrap(),
                relay: base_url.clone(),
                acknowledgements: BTreeMap::new(),
            }),
            wake: Notify::new(),
        };
        let first = make_service();
        first
            .delivery
            .lock()
            .unwrap()
            .store
            .enqueue(key.clone(), event.clone())
            .unwrap();
        assert!(drain(&first, &rest).await.is_err());
        drop(first);
        let restarted = make_service();
        assert_eq!(
            restarted
                .delivery
                .lock()
                .unwrap()
                .store
                .pending()
                .unwrap()
                .get(&key),
            Some(&event)
        );
        drain(&restarted, &rest).await.unwrap();
        assert!(restarted
            .delivery
            .lock()
            .unwrap()
            .store
            .pending()
            .unwrap()
            .is_empty());
        server.await.unwrap();
    }

    #[test]
    fn notice_outbox_corruption_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notices.json");
        std::fs::write(&path, b"corrupt pending notices").unwrap();
        assert!(Store::open(path.clone(), nostr::Keys::generate().public_key()).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"corrupt pending notices");
    }

    #[test]
    fn notice_outbox_capacity_refuses_without_evicting() {
        let dir = tempfile::tempdir().unwrap();
        let keys = nostr::Keys::generate();
        let store = Store::open(dir.path().join("notices.json"), keys.public_key()).unwrap();
        for index in 0..MAX_NOTICES {
            store
                .enqueue(index.to_string(), notice(&keys, &index.to_string()))
                .unwrap();
        }
        assert!(store
            .enqueue("overflow".into(), notice(&keys, "overflow"))
            .is_err());
        assert_eq!(store.pending().unwrap().len(), MAX_NOTICES);
    }
}
