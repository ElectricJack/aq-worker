use super::*;
impl State {
    pub(super) fn put_archive(
        &mut self,
        name: &str,
        query: &BTreeMap<String, String>,
        body: &[u8],
    ) -> DockerResponse {
        if !self.containers.contains_key(name) {
            return error(404, "no such container");
        }
        let base = query.get("path").cloned().unwrap_or_default();
        let Some(entries) = parse_tar(body) else {
            return error(400, "invalid archive");
        };
        for (entry_path, file) in entries {
            let full = format!("{}/{}", base.trim_end_matches('/'), entry_path);
            write_file(self, name, &full, file);
        }
        empty(200)
    }

    pub(super) fn get_archive(
        &mut self,
        name: &str,
        query: &BTreeMap<String, String>,
    ) -> DockerResponse {
        let path = query.get("path").cloned().unwrap_or_default();
        match read_file(self, name, &path) {
            // A directory: every file below it, as `<dir name>/<relative path>`.
            Err(response) if response.status == 404 => {
                let files = files_under(self, name, &path);
                if files.is_empty() {
                    return response;
                }
                let base = path
                    .trim_end_matches('/')
                    .rsplit('/')
                    .next()
                    .unwrap_or_default();
                let mut body = Vec::new();
                for (rel, file) in files {
                    body.extend(tar_entry(&format!("{base}/{rel}"), &file));
                }
                body.extend_from_slice(&[0u8; 1024]);
                DockerResponse { status: 200, body }
            }
            Err(response) => response,
            Ok(file) => {
                let base = path.rsplit('/').next().unwrap_or_default();
                DockerResponse {
                    status: 200,
                    body: tar_file(base, &file),
                }
            }
        }
    }
}
