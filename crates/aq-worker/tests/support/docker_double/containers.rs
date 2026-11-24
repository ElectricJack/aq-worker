use super::*;
impl State {
    pub(super) fn list_containers(&mut self, query: &BTreeMap<String, String>) -> DockerResponse {
        let labels = filter_labels(query);
        let list: Vec<Value> = self
            .containers
            .iter()
            .filter(|(_, c)| labels_match(&c.body["Labels"], &labels))
            .map(|(name, c)| {
                let mut entry = json!({
                    "Id": c.id,
                    "Names": [format!("/{name}")],
                    "Image": c.body["Image"],
                    "Labels": c.body["Labels"],
                    "State": c.status,
                });
                if query.get("size").is_some_and(|size| size == "true") {
                    entry["SizeRw"] = json!(self.container_sizes.get(name).copied().unwrap_or(0));
                }
                entry
            })
            .collect();
        ok_json(200, Value::Array(list))
    }

    pub(super) fn create_container(
        &mut self,
        query: &BTreeMap<String, String>,
        body: &[u8],
    ) -> DockerResponse {
        let Some(name) = query.get("name").cloned() else {
            return error(400, "name required");
        };
        if self.containers.contains_key(&name) {
            return error(409, "container name already in use");
        }
        let Ok(body) = serde_json::from_slice::<Value>(body) else {
            return error(400, "invalid body");
        };
        for mount in mounts(&body) {
            if mount["Type"] == "volume" {
                let source = mount["Source"].as_str().unwrap_or_default().to_string();
                self.volumes.entry(source).or_insert(Volume {
                    labels: json!({}),
                    files: BTreeMap::new(),
                });
            }
        }
        let id = format!("{:064x}", self.next_id);
        self.next_id += 1;
        self.containers.insert(
            name,
            Container {
                id: id.clone(),
                body,
                status: "created",
                layer: BTreeMap::new(),
                exit_code: 0,
            },
        );
        ok_json(201, json!({ "Id": id, "Warnings": [] }))
    }

    pub(super) fn inspect_container(
        &mut self,
        name: &str,
        query: &BTreeMap<String, String>,
    ) -> DockerResponse {
        match self.containers.get(name) {
            None => error(404, "no such container"),
            Some(c) => {
                let running = c.status == "running";
                let size_rw = (query.get("size").map(String::as_str) == Some("true"))
                    .then(|| self.container_sizes.get(name).copied());
                let mut body = json!({
                    "Id": c.id,
                    "Name": format!("/{name}"),
                    "Config": {
                        "Image": c.body["Image"],
                        "Labels": c.body["Labels"],
                        "Cmd": c.body["Cmd"],
                        "Env": c.body["Env"],
                        "WorkingDir": c.body["WorkingDir"],
                    },
                    "HostConfig": c.body["HostConfig"],
                    "State": {
                        "Status": c.status,
                        "Running": running,
                        "Paused": false,
                        "Pid": if running { 4242 } else { 0 },
                        "ExitCode": c.exit_code,
                    },
                });
                if let Some(Some(size)) = size_rw {
                    body["SizeRw"] = json!(size);
                }
                ok_json(200, body)
            }
        }
    }

    pub(super) fn start_container(&mut self, name: &str) -> DockerResponse {
        let runner = self.runner;
        let material_path = self.material_path.clone();
        let Some(container) = self.containers.get_mut(name) else {
            return error(404, "no such container");
        };
        if container.status == "running" {
            return empty(304);
        }
        let has_material = container
            .layer
            .get(&material_path)
            .is_some_and(|file| !file.data.is_empty());
        container.status = match runner {
            Runner::Exits | Runner::ExitsWith(_) => "exited",
            Runner::StaysRunning => "running",
        };
        container.exit_code = match runner {
            Runner::ExitsWith(code) => code,
            _ => 0,
        };
        if has_material {
            let output = File {
                data: b"fixture-complete\n".to_vec(),
                mode: 0o644,
                uid: 0,
                gid: 0,
            };
            write_file(self, name, "/aq/work/fixture-output", output);
            for (path, data) in self.start_files.clone() {
                let file = File {
                    data,
                    mode: 0o644,
                    uid: 0,
                    gid: 0,
                };
                write_file(self, name, &path, file);
            }
        }
        empty(204)
    }

    pub(super) fn stop_container(&mut self, name: &str) -> DockerResponse {
        match self.containers.get_mut(name) {
            None => error(404, "no such container"),
            Some(c) if c.status != "running" => empty(304),
            Some(c) => {
                c.status = "exited";
                empty(204)
            }
        }
    }

    pub(super) fn delete_container(&mut self, name: &str) -> DockerResponse {
        let Some(container) = self.containers.get(name) else {
            return error(404, "no such container");
        };
        if container.status == "running" {
            return error(409, "container is running");
        }
        self.containers.remove(name);
        empty(204)
    }
}
