use super::*;
use aq_node_service::link::TerminalStream;

struct Viewer {
    connection: Uuid,
    generation: u64,
    pty: pty::Pty,
}

struct Relay<'a> {
    target: Attachment<'a>,
    viewer: Option<Viewer>,
    read_only: bool,
    redactor: redact::Redactor,
}

impl Relay<'_> {
    async fn handle(&mut self, message: &Value) -> Result<Option<Message>> {
        let connection = serde_json::from_value(message["connection_id"].clone())?;
        let generation = message["generation"]
            .as_u64()
            .context("terminal generation")?;
        let current = self.viewer.as_ref().is_some_and(|viewer| {
            viewer.connection == connection && viewer.generation == generation
        });
        let fixed = matches!(self.target, Attachment::Attempt { .. });
        let kind = message["kind"].as_str();
        let repaint = kind == Some("repaint") && fixed && current;
        if kind == Some("attach") || repaint {
            return self.attach(message, connection, generation).await.map(Some);
        }
        if current && let Some(kind) = kind {
            self.control(kind, message).await?;
        }
        Ok(None)
    }

    async fn attach(
        &mut self,
        message: &Value,
        connection: Uuid,
        generation: u64,
    ) -> Result<Message> {
        let dimensions = match self.target {
            Attachment::Manual { .. } => Some(size(message)?),
            Attachment::Attempt { .. } => {
                if message["kind"] == "attach" {
                    self.read_only = message["read_only"]
                        .as_bool()
                        .context("attempt viewer mode")?;
                }
                None
            }
        };
        self.viewer = None;
        self.redactor = redact::Redactor::default();
        let (pty, (columns, rows)) = self.target.open(dimensions, self.read_only).await?;
        self.viewer = Some(Viewer {
            connection,
            generation,
            pty,
        });
        let ready =
            json!({"kind":"ready", "connection_id":connection, "columns":columns, "rows":rows});
        Ok(Message::Text(ready.to_string().into()))
    }

    async fn control(&mut self, kind: &str, message: &Value) -> Result<()> {
        let pty = &self.viewer.as_ref().expect("current viewer").pty;
        let fixed = matches!(self.target, Attachment::Attempt { .. });
        match kind {
            "input" if !self.read_only => {
                let bytes = input_bytes(message)?;
                ensure!(
                    !fixed || complete_terminal_input(&bytes, false),
                    "incomplete terminal input"
                );
                pty.write(&bytes).await?;
            }
            "escape" if fixed && !self.read_only => pty.write(b"\x1b").await?,
            "resize" => match &self.target {
                Attachment::Manual { .. } => {
                    let (c, r) = size(message)?;
                    pty.resize(c, r)?;
                }
                Attachment::Attempt { attempt, tmux } if !self.read_only => {
                    let (c, r) = size(message)?;
                    let (c, r) = attempt.resize_pane(tmux, c, r).await?;
                    pty.resize(c, r)?;
                }
                _ => {}
            },
            "detach" => self.viewer = None,
            _ => {}
        }
        Ok(())
    }

    async fn read(&self) -> Result<(Uuid, Vec<u8>)> {
        match &self.viewer {
            Some(viewer) => Ok((viewer.connection, viewer.pty.read().await?)),
            None => std::future::pending().await,
        }
    }
}

fn input_bytes(message: &Value) -> Result<Vec<u8>> {
    let bytes = message["bytes"].as_array().context("terminal bytes")?;
    ensure!(
        bytes.len() <= aq_session_protocol::MAX_INPUT_BYTES,
        "terminal input bound"
    );
    bytes
        .iter()
        .map(|b| {
            b.as_u64()
                .and_then(|n| u8::try_from(n).ok())
                .context("terminal byte")
        })
        .collect()
}

pub(super) async fn relay(socket: TerminalStream, id: Uuid, target: Attachment<'_>) -> Result<()> {
    tracing::info!(session_id=%id,"manual terminal relay connected");
    let (mut sink, mut source) = socket.split();
    let mut relay = Relay {
        target,
        viewer: None,
        redactor: redact::Redactor::default(),
        read_only: false,
    };
    loop {
        tokio::select! {
            frame = source.next() => match frame {
                Some(Ok(Message::Text(text))) => {
                    if let Some(reply) = relay.handle(&serde_json::from_str(&text)?).await? {
                        sink.send(reply).await?;
                    }
                }
                Some(Ok(Message::Ping(_))) => sink.flush().await?,
                Some(Ok(Message::Pong(_))) => {}
                _ => break,
            },
            output = relay.read() => {
                let (connection, bytes) = output?;
                let mut data = connection.as_bytes().to_vec();
                data.extend_from_slice(&relay.redactor.frame(&bytes));
                tokio::time::timeout(Duration::from_secs(2), sink.send(Message::Binary(data.into()))).await??;
            }
        }
    }
    Ok(())
}
