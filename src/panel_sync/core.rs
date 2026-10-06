use super::traffic_audit::TrafficAuditPtr;
use method_name::method_name_unstable;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct PanelSyncConfig {
    pub webapi_url: Url,
    pub webapi_token: String,
    pub node_id: usize,
    pub update_interval_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct SyncUser {
    #[serde(default, deserialize_with = "deserialize_optional_uuid")]
    client_id: Option<Uuid>,
    #[serde(default = "default_true")]
    enable: bool,
}

fn default_true() -> bool {
    true
}

fn deserialize_optional_uuid<'de, D>(deserializer: D) -> Result<Option<Uuid>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(value)) => Ok(Uuid::parse_str(&value).ok()),
        _ => Ok(None),
    }
}

#[derive(Debug, Clone)]
pub struct PanelSyncClient {
    config: PanelSyncConfig,
    client: reqwest::Client,
    reported_traffic: HashMap<Uuid, (u64, u64)>,
    reporter_id: Uuid,
    next_report_sequence: u64,
    pending_traffic: Option<PendingTrafficReport>,
}

#[derive(Debug, Clone)]
struct PendingTrafficReport {
    sequence: u64,
    data: Vec<serde_json::Value>,
    traffic_snapshot: HashMap<Uuid, (u64, u64)>,
}

impl PanelSyncClient {
    pub fn new(config: PanelSyncConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .expect("valid reqwest client configuration");
        Self {
            config,
            client,
            reported_traffic: HashMap::new(),
            reporter_id: Uuid::new_v4(),
            next_report_sequence: 1,
            pending_traffic: None,
        }
    }

    pub async fn run(mut self, traffic_audit: TrafficAuditPtr, cancel_token: CancellationToken) {
        let interval_secs = self.config.update_interval_secs.max(5);
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        let mn = method_name_unstable!();
        loop {
            tokio::select! {
                _ = cancel_token.cancelled() => break,
                _ = interval.tick() => {}
            }
            let sync_result = tokio::select! {
                _ = cancel_token.cancelled() => break,
                result = self.sync_once(&traffic_audit) => result,
            };
            if let Err(error) = sync_result {
                log::warn!("{mn} -- panel sync failed: {error}");
            }
            let report_result = tokio::select! {
                _ = cancel_token.cancelled() => break,
                result = self.report_traffic_once(&traffic_audit) => result,
            };
            if let Err(error) = report_result {
                log::warn!("{mn} -- panel traffic report failed: {error}");
            }
        }
    }

    pub async fn sync_initial(&mut self, traffic_audit: &TrafficAuditPtr) -> std::io::Result<()> {
        self.sync_once(traffic_audit).await
    }

    async fn sync_once(&mut self, traffic_audit: &TrafficAuditPtr) -> std::io::Result<()> {
        let url = self.endpoint("users")?;
        let response = self.client.get(url).send().await.map_err(std::io::Error::other)?;
        let users: Vec<SyncUser> = self.parse_payload(response).await?;
        let mut seen = HashSet::new();
        let mut audit = traffic_audit.lock().await;
        for user in users {
            if let Some(client_id) = user.client_id {
                seen.insert(client_id);
                audit.sync_client(client_id, user.enable);
            } else {
                let mn = method_name_unstable!();
                log::warn!("{mn} -- ignored panel user entry with missing or invalid client_id: {user:?}");
            }
        }
        let removed = audit.remove_missing_clients(&seen.into_iter().collect::<Vec<_>>());
        for client_id in removed {
            self.reported_traffic.remove(&client_id);
        }
        Ok(())
    }

    async fn report_traffic_once(&mut self, traffic_audit: &TrafficAuditPtr) -> std::io::Result<()> {
        if self.pending_traffic.is_none() {
            let snapshot = {
                let audit = traffic_audit.lock().await;
                audit
                    .client_ids()
                    .into_iter()
                    .map(|id| (id, audit.traffic(&id)))
                    .collect::<Vec<_>>()
            };
            let mut data = Vec::new();
            let mut traffic_snapshot = HashMap::new();
            for (client_id, (upstream, downstream)) in snapshot {
                let previous = self.reported_traffic.get(&client_id).copied().unwrap_or_default();
                let delta_upstream = upstream.saturating_sub(previous.0);
                let delta_downstream = downstream.saturating_sub(previous.1);
                if delta_upstream == 0 && delta_downstream == 0 {
                    continue;
                }
                data.push(serde_json::json!({
                    "client_id": client_id,
                    "u": delta_upstream,
                    "d": delta_downstream,
                }));
                traffic_snapshot.insert(client_id, (upstream, downstream));
            }
            if data.is_empty() {
                return Ok(());
            }

            let sequence = self.next_report_sequence;
            self.next_report_sequence = sequence
                .checked_add(1)
                .ok_or_else(|| std::io::Error::other("panel traffic report sequence exhausted"))?;
            self.pending_traffic = Some(PendingTrafficReport {
                sequence,
                data,
                traffic_snapshot,
            });
        }
        let pending = self.pending_traffic.as_ref().expect("pending report was created").clone();

        let url = self.endpoint("users/traffic")?;
        let response = self
            .client
            .post(url)
            .json(&serde_json::json!({
                "reporter_id": self.reporter_id,
                "sequence": pending.sequence,
                "data": pending.data,
            }))
            .send()
            .await
            .map_err(std::io::Error::other)?;
        let result: serde_json::Value = self.parse_payload(response).await?;
        log::trace!("{} -- panel traffic report response: {result:?}", method_name_unstable!());
        self.reported_traffic.extend(pending.traffic_snapshot);
        self.pending_traffic = None;
        Ok(())
    }

    fn endpoint(&self, action: &str) -> std::io::Result<Url> {
        let mut url = self.config.webapi_url.clone();
        let path = format!("{}/mod_mu/{action}", url.path().trim_end_matches('/'));
        url.set_path(&path);
        url.set_fragment(None);
        url.query_pairs_mut()
            .append_pair("key", &self.config.webapi_token)
            .append_pair("node_id", &self.config.node_id.to_string());
        Ok(url)
    }

    async fn parse_payload<T: for<'de> Deserialize<'de>>(&self, response: reqwest::Response) -> std::io::Result<T> {
        if response.status() != reqwest::StatusCode::OK {
            return Err(std::io::Error::other(format!("panel API returned {}", response.status())));
        }
        let value = response.json::<serde_json::Value>().await.map_err(std::io::Error::other)?;
        if value.get("ret").and_then(serde_json::Value::as_i64).unwrap_or_default() == 0 {
            return Err(std::io::Error::other(format!("panel API rejected request: {value:?}")));
        }
        let data = value.get("data").cloned().unwrap_or(value);
        serde_json::from_value(data).map_err(std::io::Error::other)
    }
}

#[cfg(test)]
mod tests {
    use super::{PanelSyncClient, PanelSyncConfig, SyncUser};
    use crate::panel_sync::TrafficAudit;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    use tokio_util::sync::CancellationToken;
    use url::Url;
    use uuid::Uuid;

    #[test]
    fn parses_panel_users_and_defaults_enable_to_true() {
        let users: Vec<SyncUser> =
            serde_json::from_str(r#"[{"client_id":"f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7"},{"client_id":"invalid","enable":false}]"#)
                .unwrap();
        assert!(users[0].enable);
        assert!(users[0].client_id.is_some());
        assert!(!users[1].enable);
        assert!(users[1].client_id.is_none());
    }

    #[test]
    fn builds_panel_users_url_with_configured_port_and_query() {
        let client = PanelSyncClient::new(PanelSyncConfig {
            webapi_url: Url::parse("https://example.com:5432").unwrap(),
            webapi_token: "test-token".to_string(),
            node_id: 7,
            update_interval_secs: 10,
        });

        assert_eq!(
            client.endpoint("users").unwrap().as_str(),
            "https://example.com:5432/mod_mu/users?key=test-token&node_id=7"
        );
    }

    #[tokio::test]
    async fn fetches_panel_users_from_configured_node() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let panel_server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let request = String::from_utf8(request).unwrap();
            assert!(request.starts_with("GET /panel/mod_mu/users?key=test-token&node_id=7 HTTP/1.1\r\n"));

            let body = r#"{"ret":1,"data":[{"client_id":"f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7","enable":true}]}"#;
            respond(&mut stream, body).await;

            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_request(&mut stream).await;
            let header_end = request.windows(4).position(|window| window == b"\r\n\r\n").unwrap() + 4;
            let request_line = String::from_utf8_lossy(&request[..header_end]);
            assert!(request_line.starts_with("POST /panel/mod_mu/users/traffic?key=test-token&node_id=7 HTTP/1.1\r\n"));
            let payload: serde_json::Value = serde_json::from_slice(&request[header_end..]).unwrap();
            assert_eq!(payload["data"][0]["client_id"], "f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7");
            assert_eq!(payload["data"][0]["u"], 11);
            assert_eq!(payload["data"][0]["d"], 22);
            respond(&mut stream, r#"{"ret":1,"data":true}"#).await;
        });

        let client = PanelSyncClient::new(PanelSyncConfig {
            webapi_url: Url::parse(&format!("http://{address}/panel/")).unwrap(),
            webapi_token: "test-token".to_string(),
            node_id: 7,
            update_interval_secs: 10,
        });
        let client_id = Uuid::parse_str("f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7").unwrap();
        let audit = Arc::new(tokio::sync::Mutex::new(TrafficAudit::new()));
        let mut client = client;
        client.sync_initial(&audit).await.unwrap();
        assert!(audit.lock().await.is_enabled(&client_id));
        {
            let mut audit = audit.lock().await;
            audit.add_upstream(&client_id, 11);
            audit.add_downstream(&client_id, 22);
        }
        client.report_traffic_once(&audit).await.unwrap();
        assert_eq!(client.reported_traffic.get(&client_id), Some(&(11, 22)));
        panel_server.await.unwrap();
    }

    #[tokio::test]
    async fn retries_the_same_traffic_report_after_a_lost_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let panel_server = tokio::spawn(async move {
            let (mut first_stream, _) = listener.accept().await.unwrap();
            let first_request = read_request(&mut first_stream).await;
            let first_body_start = first_request.windows(4).position(|window| window == b"\r\n\r\n").unwrap() + 4;
            let first_payload: serde_json::Value = serde_json::from_slice(&first_request[first_body_start..]).unwrap();
            first_stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{")
                .await
                .unwrap();
            drop(first_stream);

            let (mut retry_stream, _) = listener.accept().await.unwrap();
            let retry_request = read_request(&mut retry_stream).await;
            let retry_body_start = retry_request.windows(4).position(|window| window == b"\r\n\r\n").unwrap() + 4;
            let retry_payload: serde_json::Value = serde_json::from_slice(&retry_request[retry_body_start..]).unwrap();
            assert_eq!(retry_payload, first_payload);
            assert_eq!(retry_payload["sequence"], 1);
            assert_eq!(retry_payload["data"][0]["u"], 11);
            assert_eq!(retry_payload["data"][0]["d"], 22);
            respond(&mut retry_stream, r#"{"ret":1,"data":true}"#).await;
        });

        let mut client = PanelSyncClient::new(PanelSyncConfig {
            webapi_url: Url::parse(&format!("http://{address}/panel/")).unwrap(),
            webapi_token: "test-token".to_string(),
            node_id: 7,
            update_interval_secs: 10,
        });
        let client_id = Uuid::parse_str("f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7").unwrap();
        let audit = Arc::new(tokio::sync::Mutex::new(TrafficAudit::new()));
        {
            let mut audit = audit.lock().await;
            audit.sync_client(client_id, true);
            audit.add_upstream(&client_id, 11);
            audit.add_downstream(&client_id, 22);
        }

        assert!(client.report_traffic_once(&audit).await.is_err());
        assert!(client.reported_traffic.is_empty());
        client.report_traffic_once(&audit).await.unwrap();
        assert_eq!(client.reported_traffic.get(&client_id), Some(&(11, 22)));
        assert!(client.pending_traffic.is_none());
        panel_server.await.unwrap();
    }

    #[tokio::test]
    async fn panel_sync_loop_stops_when_cancelled() {
        let client = PanelSyncClient::new(PanelSyncConfig {
            webapi_url: Url::parse("http://127.0.0.1:1/").unwrap(),
            webapi_token: "test-token".to_string(),
            node_id: 7,
            update_interval_secs: 60,
        });
        let audit = Arc::new(tokio::sync::Mutex::new(TrafficAudit::new()));
        let cancel_token = CancellationToken::new();
        let task = tokio::spawn(client.run(audit, cancel_token.clone()));

        cancel_token.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("panel sync loop should stop promptly after cancellation")
            .unwrap();
    }

    async fn read_request(stream: &mut tokio::net::TcpStream) -> Vec<u8> {
        let mut request = Vec::new();
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
        }
        let headers = String::from_utf8_lossy(&request);
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or_default();
        let header_end = request.len();
        request.resize(header_end + content_length, 0);
        stream.read_exact(&mut request[header_end..]).await.unwrap();
        request
    }

    async fn respond(stream: &mut tokio::net::TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}
