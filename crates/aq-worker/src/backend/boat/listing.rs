//! Page through owned sandboxes and reconcile their journal and VM-time records.
use super::*;
impl BoatBackend {
    pub(super) async fn list_inventory(
        &mut self,
        node: Uuid,
    ) -> Result<Vec<InventoryEntry>, BackendError> {
        self.journal.check()?;
        if node != self.config.node_id {
            return Err(BackendError::Conflict);
        }
        // Deliver even if the provider inventory is currently unavailable, or
        // all reported VMs have already been destroyed.
        self.replay_vm_time().await?;
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::BTreeSet::new();
        let mut inventory = Inventory::default();
        loop {
            let mut path = format!("/sandboxes?namePrefix={}&limit=200", policy::prefix(node));
            if let Some(c) = &cursor {
                path.push_str(&format!("&cursor={}", api::encode(c)));
            }
            let page = self.api.request(Method::GET, &path).json().await?;
            self.collect_entries(node, &page, &mut inventory)?;
            let Some(next) = next_cursor(&page, &mut seen)? else {
                break;
            };
            cursor = Some(next);
        }
        self.replay_vm_time().await?;
        Ok(inventory.entries)
    }
    fn collect_entries(
        &mut self,
        node: Uuid,
        page: &Value,
        inventory: &mut Inventory,
    ) -> Result<(), BackendError> {
        for vm in page["sandboxes"].as_array().ok_or_else(unknown)? {
            if !self.in_scope(vm)
                || vm["name"]
                    .as_str()
                    .is_none_or(|s| policy::attempt(s, node).is_none())
            {
                continue;
            }
            let entry = self.entry(vm)?;
            if !inventory.ids.insert(entry.sandbox.backend_id.clone()) {
                return Err(unknown());
            }
            if let Some(mut r) = self.journal.get(&entry.name).cloned()
                && r.backend_id.is_none()
            {
                r.backend_id = Some(entry.sandbox.backend_id.clone());
                self.save(r)?;
            }
            self.observe_vm_time(vm, &entry)?;
            inventory.entries.push(entry);
        }
        Ok(())
    }
}
#[derive(Default)]
struct Inventory {
    entries: Vec<InventoryEntry>,
    ids: std::collections::BTreeSet<String>,
}
fn next_cursor(
    page: &Value,
    seen: &mut std::collections::BTreeSet<String>,
) -> Result<Option<String>, BackendError> {
    match page["pageInfo"]["hasMore"].as_bool() {
        Some(false) => Ok(None),
        Some(true) => {
            let c = page["pageInfo"]["nextCursor"]
                .as_str()
                .filter(|c| !c.is_empty())
                .ok_or_else(unknown)?
                .to_owned();
            if !seen.insert(c.clone()) || seen.len() > 10_000 {
                return Err(unknown());
            }
            Ok(Some(c))
        }
        None => Err(unknown()),
    }
}
