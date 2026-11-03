//! Scripted in-memory Docker Engine API double for docker-local tests. It
//! emulates only the endpoints the backend uses, can hang every request while
//! "paused" (WSL suspend), and can apply one-shot faults: fail before applying,
//! apply then drop the reply, or answer success without applying.
use aq_worker::backend::docker_local::{
    DockerMethod, DockerRequest, DockerResponse, DockerTransport, TransportError,
    TransportErrorKind,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::watch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    FailBefore(u16),
    DropReplyAfterApply,
    LieOkWithoutApply,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub method: DockerMethod,
    /// Full request path prefix including the API version.
    pub path_prefix: String,
    pub effect: Effect,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Runner {
    /// Writes the fixture output when material is present, then exits 0.
    Exits,
    /// Writes the fixture output when material is present and keeps running.
    StaysRunning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub data: Vec<u8>,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Debug, Clone)]
pub struct Container {
    pub id: String,
    pub body: Value,
    pub status: &'static str,
    pub layer: BTreeMap<String, File>,
}

#[derive(Debug, Clone)]
pub struct Volume {
    pub labels: Value,
    pub files: BTreeMap<String, File>,
}

struct State {
    containers: BTreeMap<String, Container>,
    volumes: BTreeMap<String, Volume>,
    requests: Vec<(DockerMethod, String)>,
    rules: Vec<Rule>,
    rootless: bool,
    runner: Runner,
    material_path: String,
    images: std::collections::BTreeSet<String>,
    next_id: u64,
}

#[derive(Clone)]
pub struct ScriptedDocker {
    state: Arc<Mutex<State>>,
    paused: Arc<watch::Sender<bool>>,
}

impl Default for ScriptedDocker {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptedDocker {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                containers: BTreeMap::new(),
                volumes: BTreeMap::new(),
                requests: Vec::new(),
                rules: Vec::new(),
                rootless: true,
                runner: Runner::Exits,
                material_path: "/aq/private/material".into(),
                images: Default::default(),
                next_id: 1,
            })),
            paused: Arc::new(watch::channel(false).0),
        }
    }
    pub fn rootless(self, rootless: bool) -> Self {
        self.state.lock().unwrap().rootless = rootless;
        self
    }
    pub fn runner(self, runner: Runner) -> Self {
        self.state.lock().unwrap().runner = runner;
        self
    }
    pub fn pause(&self) {
        self.paused.send_replace(true);
    }
    pub fn resume(&self) {
        self.paused.send_replace(false);
    }
    pub fn script(&self, rule: Rule) {
        self.state.lock().unwrap().rules.push(rule);
    }
    pub fn requests(&self) -> Vec<(DockerMethod, String)> {
        self.state.lock().unwrap().requests.clone()
    }
    pub fn clear_requests(&self) {
        self.state.lock().unwrap().requests.clear();
    }
    pub fn container(&self, name: &str) -> Option<Container> {
        self.state.lock().unwrap().containers.get(name).cloned()
    }
    pub fn volume(&self, name: &str) -> Option<Volume> {
        self.state.lock().unwrap().volumes.get(name).cloned()
    }
    pub fn container_names(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .containers
            .keys()
            .cloned()
            .collect()
    }
    pub fn volume_names(&self) -> Vec<String> {
        self.state.lock().unwrap().volumes.keys().cloned().collect()
    }
    /// Edits a stored container's labels, e.g. to plant a foreign owner.
    pub fn edit_container_labels(&self, name: &str, edit: impl FnOnce(&mut Value)) {
        let mut state = self.state.lock().unwrap();
        let container = state.containers.get_mut(name).expect("container");
        edit(&mut container.body["Labels"]);
    }
    pub fn set_status(&self, name: &str, status: &'static str) {
        self.state
            .lock()
            .unwrap()
            .containers
            .get_mut(name)
            .expect("container")
            .status = status;
    }
    /// Writes a file into the volume mounted at the path's mount target.
    pub fn put_file(&self, container: &str, path: &str, data: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        let file = File {
            data,
            mode: 0o644,
            uid: 0,
            gid: 0,
        };
        write_file(&mut state, container, path, file);
    }

    fn handle(&self, request: &DockerRequest) -> DockerResponse {
        let mut state = self.state.lock().unwrap();
        route(&mut state, request)
    }
}

impl DockerTransport for ScriptedDocker {
    async fn send(&self, request: DockerRequest) -> Result<DockerResponse, TransportError> {
        self.state
            .lock()
            .unwrap()
            .requests
            .push((request.method, request.path.clone()));
        let mut paused = self.paused.subscribe();
        let _ = paused.wait_for(|paused| !*paused).await;
        let effect = {
            let mut state = self.state.lock().unwrap();
            let index = state.rules.iter().position(|rule| {
                rule.method == request.method && request.path.starts_with(&rule.path_prefix)
            });
            index.map(|index| state.rules.remove(index).effect)
        };
        let response = match effect {
            Some(Effect::FailBefore(status)) => error(status, "scripted failure"),
            Some(Effect::LieOkWithoutApply) => DockerResponse {
                status: match request.method {
                    DockerMethod::Get | DockerMethod::Put => 200,
                    DockerMethod::Post if request.path.contains("/create") => 201,
                    DockerMethod::Post | DockerMethod::Delete => 204,
                },
                body: b"{}".to_vec(),
            },
            Some(Effect::DropReplyAfterApply) => {
                self.handle(&request);
                return Err(TransportError::io("connection reset after apply"));
            }
            None => self.handle(&request),
        };
        if response.body.len() > request.max_response_bytes {
            return Err(TransportError {
                kind: TransportErrorKind::TooLarge,
                message: "response exceeds limit".into(),
            });
        }
        Ok(response)
    }
}

fn error(status: u16, message: &str) -> DockerResponse {
    DockerResponse {
        status,
        body: serde_json::to_vec(&json!({ "message": message })).unwrap(),
    }
}
fn ok_json(status: u16, value: Value) -> DockerResponse {
    DockerResponse {
        status,
        body: serde_json::to_vec(&value).unwrap(),
    }
}
fn empty(status: u16) -> DockerResponse {
    DockerResponse {
        status,
        body: Vec::new(),
    }
}

fn route(state: &mut State, request: &DockerRequest) -> DockerResponse {
    let Some(rest) = request.path.strip_prefix("/v1.44/") else {
        return error(400, "unsupported API version");
    };
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let query: BTreeMap<String, String> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect();
    let segments: Vec<&str> = path.split('/').collect();
    use DockerMethod::*;
    // Image references contain '/', so image inspect is matched by shape.
    if let Some(image) = path
        .strip_prefix("images/")
        .and_then(|rest| rest.strip_suffix("/json"))
        .filter(|_| request.method == Get)
    {
        return match state.images.contains(image) {
            true => ok_json(200, json!({ "Id": "sha256:image", "RepoDigests": [image] })),
            false => error(404, "no such image"),
        };
    }
    match (request.method, segments.as_slice()) {
        (Get, ["info"]) => {
            let mut options = vec![json!("name=seccomp,profile=default")];
            if state.rootless {
                options.push(json!("name=rootless"));
            }
            ok_json(200, json!({ "SecurityOptions": options }))
        }
        (Post, ["images", "create"]) => {
            if let Some(image) = query.get("fromImage") {
                state.images.insert(image.clone());
            }
            empty(200)
        }
        (Get, ["containers", "json"]) => {
            let labels = filter_labels(&query);
            let list: Vec<Value> = state
                .containers
                .iter()
                .filter(|(_, c)| labels_match(&c.body["Labels"], &labels))
                .map(|(name, c)| {
                    json!({
                        "Id": c.id,
                        "Names": [format!("/{name}")],
                        "Image": c.body["Image"],
                        "Labels": c.body["Labels"],
                        "State": c.status,
                    })
                })
                .collect();
            ok_json(200, Value::Array(list))
        }
        (Post, ["containers", "create"]) => {
            let Some(name) = query.get("name").cloned() else {
                return error(400, "name required");
            };
            if state.containers.contains_key(&name) {
                return error(409, "container name already in use");
            }
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
                return error(400, "invalid body");
            };
            for mount in mounts(&body) {
                if mount["Type"] == "volume" {
                    let source = mount["Source"].as_str().unwrap_or_default().to_string();
                    state.volumes.entry(source).or_insert(Volume {
                        labels: json!({}),
                        files: BTreeMap::new(),
                    });
                }
            }
            let id = format!("{:064x}", state.next_id);
            state.next_id += 1;
            state.containers.insert(
                name,
                Container {
                    id: id.clone(),
                    body,
                    status: "created",
                    layer: BTreeMap::new(),
                },
            );
            ok_json(201, json!({ "Id": id, "Warnings": [] }))
        }
        (Get, ["containers", name, "json"]) => match state.containers.get(*name) {
            None => error(404, "no such container"),
            Some(c) => {
                let running = c.status == "running";
                ok_json(
                    200,
                    json!({
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
                            "ExitCode": 0,
                        },
                    }),
                )
            }
        },
        (Put, ["containers", name, "archive"]) => {
            if !state.containers.contains_key(*name) {
                return error(404, "no such container");
            }
            let base = query.get("path").cloned().unwrap_or_default();
            let Some(entries) = parse_tar(&request.body) else {
                return error(400, "invalid archive");
            };
            for (entry_path, file) in entries {
                let full = format!("{}/{}", base.trim_end_matches('/'), entry_path);
                write_file(state, name, &full, file);
            }
            empty(200)
        }
        (Get, ["containers", name, "archive"]) => {
            let path = query.get("path").cloned().unwrap_or_default();
            match read_file(state, name, &path) {
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
        (Post, ["containers", name, "start"]) => {
            let runner = state.runner;
            let material_path = state.material_path.clone();
            let Some(container) = state.containers.get_mut(*name) else {
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
                Runner::Exits => "exited",
                Runner::StaysRunning => "running",
            };
            if has_material {
                let output = File {
                    data: b"fixture-complete\n".to_vec(),
                    mode: 0o644,
                    uid: 0,
                    gid: 0,
                };
                write_file(state, name, "/aq/work/fixture-output", output);
            }
            empty(204)
        }
        (Post, ["containers", name, "stop"]) => match state.containers.get_mut(*name) {
            None => error(404, "no such container"),
            Some(c) if c.status != "running" => empty(304),
            Some(c) => {
                c.status = "exited";
                empty(204)
            }
        },
        (Delete, ["containers", name]) => match state.containers.get(*name) {
            None => error(404, "no such container"),
            Some(c) if c.status == "running" => error(409, "container is running"),
            Some(_) => {
                state.containers.remove(*name);
                empty(204)
            }
        },
        (Get, ["volumes"]) => {
            let labels = filter_labels(&query);
            let list: Vec<Value> = state
                .volumes
                .iter()
                .filter(|(_, v)| labels_match(&v.labels, &labels))
                .map(|(name, v)| json!({ "Name": name, "Labels": v.labels, "Driver": "local" }))
                .collect();
            ok_json(200, json!({ "Volumes": list, "Warnings": [] }))
        }
        (Post, ["volumes", "create"]) => {
            let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
                return error(400, "invalid body");
            };
            let name = body["Name"].as_str().unwrap_or_default().to_string();
            if state.volumes.contains_key(&name) {
                return error(409, "volume already exists");
            }
            let labels = body.get("Labels").cloned().unwrap_or(json!({}));
            state.volumes.insert(
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
        (Get, ["volumes", name]) => match state.volumes.get(*name) {
            None => error(404, "no such volume"),
            Some(v) => ok_json(
                200,
                json!({ "Name": name, "Labels": v.labels, "Driver": "local" }),
            ),
        },
        (Delete, ["volumes", name]) => {
            if !state.volumes.contains_key(*name) {
                return error(404, "no such volume");
            }
            let in_use = state.containers.values().any(|c| {
                mounts(&c.body)
                    .iter()
                    .any(|m| m["Type"] == "volume" && m["Source"] == *name)
            });
            if in_use {
                return error(409, "volume is in use");
            }
            state.volumes.remove(*name);
            empty(204)
        }
        _ => error(404, "page not found"),
    }
}

fn mounts(body: &Value) -> Vec<Value> {
    body["HostConfig"]["Mounts"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// The volume mount covering `path`, as (volume name, path inside the volume).
fn volume_for(state: &State, container: &str, path: &str) -> Option<(String, String)> {
    let c = state.containers.get(container)?;
    mounts(&c.body).iter().find_map(|m| {
        let target = m["Target"].as_str()?;
        let inner = path.strip_prefix(target)?;
        (m["Type"] == "volume" && (inner.is_empty() || inner.starts_with('/'))).then(|| {
            (
                m["Source"].as_str().unwrap_or_default().to_string(),
                inner.to_string(),
            )
        })
    })
}

fn write_file(state: &mut State, container: &str, path: &str, file: File) {
    match volume_for(state, container, path) {
        Some((volume, inner)) => {
            state
                .volumes
                .get_mut(&volume)
                .expect("mounted volume")
                .files
                .insert(inner, file);
        }
        None => {
            state
                .containers
                .get_mut(container)
                .expect("container")
                .layer
                .insert(path.to_string(), file);
        }
    }
}

fn read_file(state: &State, container: &str, path: &str) -> Result<File, DockerResponse> {
    let c = state
        .containers
        .get(container)
        .ok_or_else(|| error(404, "no such container"))?;
    let found = match volume_for(state, container, path) {
        Some((volume, inner)) => state.volumes.get(&volume).and_then(|v| v.files.get(&inner)),
        None => c.layer.get(path),
    };
    found.cloned().ok_or_else(|| error(404, "no such file"))
}

fn filter_labels(query: &BTreeMap<String, String>) -> Vec<String> {
    query
        .get("filters")
        .and_then(|filters| serde_json::from_str::<Value>(filters).ok())
        .and_then(|filters| filters.get("label").cloned())
        .and_then(|labels| serde_json::from_value::<Vec<String>>(labels).ok())
        .unwrap_or_default()
}

fn labels_match(labels: &Value, wanted: &[String]) -> bool {
    wanted.iter().all(|filter| match filter.split_once('=') {
        Some((key, value)) => labels[key] == value,
        None => labels.get(filter).is_some(),
    })
}

fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = value
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

fn octal(field: &[u8]) -> Option<u64> {
    let text = std::str::from_utf8(field).ok()?;
    let text = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if text.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(text, 8).ok()
}

fn parse_tar(archive: &[u8]) -> Option<Vec<(String, File)>> {
    let mut entries = Vec::new();
    let mut offset = 0;
    while offset + 512 <= archive.len() {
        let header = &archive[offset..offset + 512];
        if header.iter().all(|b| *b == 0) {
            return Some(entries);
        }
        let name = std::str::from_utf8(&header[..100])
            .ok()?
            .trim_end_matches('\0');
        let prefix = std::str::from_utf8(&header[345..500])
            .ok()?
            .trim_end_matches('\0');
        let path = if prefix.is_empty() {
            name.to_string()
        } else {
            format!("{prefix}/{name}")
        };
        let size = octal(&header[124..136])? as usize;
        let file = File {
            data: archive.get(offset + 512..offset + 512 + size)?.to_vec(),
            mode: octal(&header[100..108])? as u32,
            uid: octal(&header[108..116])? as u32,
            gid: octal(&header[116..124])? as u32,
        };
        if matches!(header[156], b'0' | 0) {
            entries.push((path.trim_start_matches("./").to_string(), file));
        }
        offset += 512 + size.div_ceil(512) * 512;
    }
    None
}

pub fn tar_file(name: &str, file: &File) -> Vec<u8> {
    let mut header = [0u8; 512];
    header[..name.len()].copy_from_slice(name.as_bytes());
    let fields: [(usize, usize, u64); 5] = [
        (100, 8, file.mode as u64),
        (108, 8, file.uid as u64),
        (116, 8, file.gid as u64),
        (124, 12, file.data.len() as u64),
        (136, 12, 0),
    ];
    for (start, len, value) in fields {
        let text = format!("{value:0width$o}\0", width = len - 1);
        header[start..start + len].copy_from_slice(text.as_bytes());
    }
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    header[148..156].copy_from_slice(b"        ");
    let sum: u32 = header.iter().map(|b| *b as u32).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    let mut out = header.to_vec();
    out.extend_from_slice(&file.data);
    out.resize(out.len().div_ceil(512) * 512, 0);
    out.extend_from_slice(&[0u8; 1024]);
    out
}
