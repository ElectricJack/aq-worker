//! Read work files from a running sandbox or its archived snapshot.
use super::*;
impl BoatBackend {
    /// Artifact collection works after archive without resuming any writer.
    pub async fn read_work_file(
        &mut self,
        s: &SandboxIdentity,
        path: &str,
    ) -> Result<Vec<u8>, BackendError> {
        let r = self.record(s)?;
        if !valid_work_path(path) {
            return Err(BackendError::InvalidRequest("invalid work file"));
        }
        let e = self.scoped_entry(s).await?;
        if e.state == SandboxState::Archived {
            let v = self
                .api
                .request(
                    Method::GET,
                    &format!("/sandboxes/{}/snapshots/latest", s.backend_id),
                )
                .json()
                .await?;
            let id = v["snapshot"]["id"]
                .as_str()
                .filter(|s| config::safe_id(s))
                .ok_or_else(unknown)?;
            let (status, b) = self
                .api
                .request(
                    Method::GET,
                    &format!(
                        "/snapshots/{id}/files?path={}",
                        api::encode(&format!(
                            "__dockervol__/{}-work/_data/{path}",
                            r.request.name
                        ))
                    ),
                )
                .raw()
                .await?;
            if status != 200 {
                return Err(unknown());
            }
            Ok(b)
        } else {
            let out = self
                .command(
                    &s.backend_id,
                    format!(
                        "sudo docker cp {}:/aq/work/{path} - | tar -xO | base64 -w0",
                        r.request.name
                    ),
                )
                .await?;
            STANDARD.decode(out.trim()).map_err(|_| unknown())
        }
    }
}
fn valid_work_path(path: &str) -> bool {
    let valid_components = path.split('/').all(|part| !matches!(part, "" | "." | ".."));
    let valid_bytes = path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b));
    valid_components && valid_bytes
}
