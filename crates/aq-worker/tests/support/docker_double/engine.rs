use super::*;
impl State {
    pub(super) fn info(&mut self) -> DockerResponse {
        let mut options = vec![json!("name=seccomp,profile=default")];
        if self.rootless {
            options.push(json!("name=rootless"));
        }
        let mut info = json!({ "SecurityOptions": options });
        if let Some(driver) = self.cgroup_driver {
            info["CgroupDriver"] = json!(driver);
        }
        if let Some(version) = self.cgroup_version {
            info["CgroupVersion"] = json!(version);
        }
        if let Some(root) = &self.root_dir {
            info["DockerRootDir"] = json!(root);
        }
        info["DriverStatus"] = json!([["Backing Filesystem", self.backing_fs]]);
        ok_json(200, info)
    }

    pub(super) fn disk_usage(&mut self) -> DockerResponse {
        let volumes: Vec<Value> = self
            .volumes
            .keys()
            .map(|name| {
                let size = self.volume_sizes.get(name).copied().unwrap_or(-1);
                json!({ "Name": name, "UsageData": { "Size": size, "RefCount": 1 } })
            })
            .collect();
        ok_json(200, json!({ "Volumes": volumes }))
    }

    pub(super) fn pull_image(&mut self, query: &BTreeMap<String, String>) -> DockerResponse {
        if let Some(image) = query.get("fromImage") {
            self.images.insert(image.clone());
        }
        empty(200)
    }
}
