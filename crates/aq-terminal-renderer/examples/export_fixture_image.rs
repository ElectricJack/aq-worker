//! Deterministic abstract-model vector; never a physical terminal attestation.
use aq_session_protocol::{Digest, OutputFrame, SessionIdentity, TerminalIdentity};
use aq_terminal_renderer::{AssumedBlank, Renderer};
use std::io::{self, Write};
use uuid::Uuid;
fn main() -> io::Result<()> {
    let terminal = TerminalIdentity {
        session: SessionIdentity::Central {
            authority_id: Uuid::from_u128(1),
            session_id: Uuid::from_u128(2),
            generation: 1,
        },
        terminal_id: Uuid::from_u128(3),
        epoch: 1,
    };
    let mut renderer = Renderer::from_assumed_blank(AssumedBlank {
        terminal: terminal.clone(),
        columns: 48,
        rows: 12,
    })
    .unwrap();
    let stream = include_bytes!("../tests/fixtures/synthetic-cli.ansi");
    let through = stream
        .windows(b"\x1b[?25h".len())
        .position(|p| p == b"\x1b[?25h")
        .unwrap();
    for (index, bytes) in [&stream[..through], b"\x1b[38;2;4;".as_slice()]
        .into_iter()
        .enumerate()
    {
        let sequence = index as u64 + 1;
        renderer
            .bytes(
                sequence,
                &OutputFrame {
                    terminal: terminal.clone(),
                    sequence,
                    byte_length: bytes.len() as u32,
                    bytes: bytes.to_vec(),
                    digest: Digest::of(bytes),
                },
            )
            .unwrap();
    }
    io::stdout().write_all(renderer.image().unwrap().bytes())
}
