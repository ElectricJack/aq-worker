use super::*;
struct Attachment {
    connection: Uuid,
    generation: u64,
    pty: Pty,
}
impl Attachment {
    async fn input(&self, value: &Value) -> Result<()> {
        let bytes: Vec<u8> = serde_json::from_value(value["bytes"].clone())?;
        ensure!(bytes.len() <= 4096, "input bound");
        self.pty.write(&bytes).await
    }
}
struct Shell<'a> {
    id: Uuid,
    exe: &'a Path,
    socket: &'a Path,
    attached: Option<Attachment>,
}
impl Shell<'_> {
    fn attach_pty(&self, (columns, rows): (u16, u16)) -> Result<Pty> {
        let mut command = clean_command(self.exe);
        command.arg("-u").arg("-S").arg(self.socket).args([
            "set-window-option",
            "-t",
            &format!("={}", name(self.id)),
            "window-size",
            "latest",
            ";",
            "attach-session",
            "-E",
            "-t",
            &format!("={}", name(self.id)),
        ]);
        Pty::command(command, columns, rows)
    }

    async fn frame(&mut self, text: &str) -> Result<Option<Message>> {
        let v: Value = serde_json::from_str(text)?;
        let connection: Uuid = serde_json::from_value(v["connection_id"].clone())?;
        let generation = v["generation"].as_u64().context("generation")?;
        if v["kind"] == "attach" {
            let geometry = size(&v)?;
            drop(self.attached.take());
            self.attached = Some(Attachment {
                connection,
                generation,
                pty: self.attach_pty(geometry)?,
            });
            return Ok(Some(Message::Text(
                json!({"kind":"ready","connection_id":connection})
                    .to_string()
                    .into(),
            )));
        }
        let Some(current) = self
            .attached
            .as_ref()
            .filter(|a| (a.connection, a.generation) == (connection, generation))
        else {
            return Ok(None);
        };
        match v["kind"].as_str() {
            Some("input") => current.input(&v).await?,
            Some("resize") => {
                let (c, r) = size(&v)?;
                current.pty.resize(c, r)?;
            }
            Some("detach") => self.attached = None,
            _ => {}
        }
        Ok(None)
    }
    async fn output(&self) -> Result<Message> {
        let Some(a) = &self.attached else {
            return std::future::pending().await;
        };
        let bytes = a.pty.read().await?;
        let mut data = a.connection.as_bytes().to_vec();
        data.extend(bytes);
        Ok(Message::Binary(data.into()))
    }
}
pub(super) async fn relay(link: &ServerLink, id: Uuid, exe: &Path, socket: &Path) -> Result<()> {
    let socket_ws = link.host_terminal_stream(id).await?;
    let (mut sink, mut source) = socket_ws.split();
    let mut shell = Shell {
        id,
        exe,
        socket,
        attached: None,
    };
    loop {
        tokio::select! {
            frame=source.next()=>match frame {
                Some(Ok(Message::Text(text)))=>{if let Some(reply)=shell.frame(&text).await? {sink.send(reply).await?;}}
                Some(Ok(Message::Ping(_)))=>sink.flush().await?,
                Some(Ok(Message::Pong(_)))=>{}
                _=>break,
            },
            output=shell.output()=>tokio::time::timeout(Duration::from_secs(2),sink.send(output?)).await??,
        }
    }
    Ok(())
}
