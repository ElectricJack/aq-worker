use super::*;
impl State {
    pub(super) fn list_volumes(&mut self, query: &BTreeMap<String, String>) -> DockerResponse {
        let labels = filter_labels(query);
        let list: Vec<Value> = self
            .volumes
            .iter()
            .filter(|(_, v)| labels_match(&v.labels, &labels))
            .map(|(name, v)| json!({ "Name": name, "Labels": v.labels, "Driver": "local" }))
            .collect();
        ok_json(200, json!({ "Volumes": list, "Warnings": [] }))
    }

    pub(super) fn create_volume(&mut self, body: &[u8]) -> DockerResponse {
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return error(400, "invalid body");
        };
        let name = body["Name"].as_str().unwrap_or_default().to_string();
        if self.volumes.contains_key(&name) {
            return error(409, "volume already exists");
        }
        let labels = body.get("Labels").cloned().unwrap_or(json!({}));
        if let Some(opts) = body.get("DriverOpts") {
            self.volume_opts.insert(name.clone(), opts.clone());
        }
        self.volumes.insert(
            name.clone(),
            Volume {
                labels: labels.clone(),
                files: BTreeMap::new(),
            },
        );
        ok_json(
            201,
            json!({ "Name": name, "Labels": labels, "Driver": "local" }),
        )
    }

    pub(super) fn inspect_volume(&mut self, name: &str) -> DockerResponse {
        match self.volumes.get(name) {
            None => error(404, "no such volume"),
            Some(v) => ok_json(
                200,
                json!({ "Name": name, "Labels": v.labels, "Driver": "local" }),
            ),
        }
    }

    pub(super) fn delete_volume(&mut self, name: &str) -> DockerResponse {
        if !self.volumes.contains_key(name) {
            return error(404, "no such volume");
        }
        let in_use = self.containers.values().any(|c| {
            mounts(&c.body)
                .iter()
                .any(|m| m["Type"] == "volume" && m["Source"] == name)
        });
        if in_use {
            return error(409, "volume is in use");
        }
        self.volumes.remove(name);
        empty(204)
    }
}
