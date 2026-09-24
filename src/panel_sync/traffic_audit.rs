use std::collections::HashMap;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
struct ClientTraffic {
    enabled: bool,
    upstream: u64,
    downstream: u64,
}

pub type TrafficAuditPtr = std::sync::Arc<tokio::sync::Mutex<TrafficAudit>>;

#[derive(Debug, Clone, Default)]
pub struct TrafficAudit {
    clients: HashMap<Uuid, ClientTraffic>,
}

impl TrafficAudit {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn client_ids(&self) -> Vec<Uuid> {
        self.clients.keys().copied().collect()
    }

    pub fn sync_client(&mut self, client_id: Uuid, enabled: bool) {
        self.clients.entry(client_id).or_default().enabled = enabled;
    }

    pub fn remove_missing_clients(&mut self, seen: &[Uuid]) -> Vec<Uuid> {
        let removed: Vec<_> = self.clients.keys().filter(|id| !seen.contains(id)).copied().collect();
        for client_id in &removed {
            self.clients.remove(client_id);
        }
        removed
    }

    pub fn is_enabled(&self, client_id: &Uuid) -> bool {
        self.clients.get(client_id).is_some_and(|client| client.enabled)
    }

    pub fn add_upstream(&mut self, client_id: &Uuid, bytes: u64) {
        if let Some(client) = self.clients.get_mut(client_id) {
            client.upstream = client.upstream.saturating_add(bytes);
        }
    }

    pub fn add_downstream(&mut self, client_id: &Uuid, bytes: u64) {
        if let Some(client) = self.clients.get_mut(client_id) {
            client.downstream = client.downstream.saturating_add(bytes);
        }
    }

    pub fn traffic(&self, client_id: &Uuid) -> (u64, u64) {
        self.clients
            .get(client_id)
            .map_or((0, 0), |client| (client.upstream, client.downstream))
    }
}

#[cfg(test)]
mod tests {
    use super::TrafficAudit;
    use uuid::Uuid;

    #[test]
    fn tracks_enable_state_and_directional_traffic() {
        let client_id = Uuid::parse_str("f2d46ca2-8d6d-4c5c-ae77-80c902ce68d7").unwrap();
        let mut audit = TrafficAudit::new();
        audit.sync_client(client_id, true);
        audit.add_upstream(&client_id, 12);
        audit.add_downstream(&client_id, 34);

        assert!(audit.is_enabled(&client_id));
        assert_eq!(audit.traffic(&client_id), (12, 34));
        assert_eq!(audit.remove_missing_clients(&[]), vec![client_id]);
        assert!(!audit.is_enabled(&client_id));
    }
}
