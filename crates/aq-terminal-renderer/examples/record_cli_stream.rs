//! Deterministic synthetic CLI repertoire, not a provider recording.
use std::io::{self, Write};
fn main() -> io::Result<()> {
    let bytes = concat!(
        "worker: queued\r\nworker: starting\r\n",
        "\x1b]2;AQ synthetic interactive worker\x07",
        "\x1b[?1049h\x1b[?25l\x1b[?2004h\x1b[?1h\x1b[2J\x1b[H",
        "\x1b[1;38;2;48;160;255mAQ INTERACTIVE\x1b[0m\r\n",
        "\x1b(0lqqqqqqqqqqqqqqqqqqqqqqk\x1b(B\r\n",
        "\x1b[3;2Hmodel: synthetic / offline",
        "\x1b[4;2HUnicode: 雪🙂e\u{301}",
        "\x1b[5;2H\x1b[38;5;214mworking\x1b[0m",
        "\x1b[7;1Hprogress: 0%\x1b[K",
        "\x1b[7;1Hprogress: 25%\x1b[K",
        "\x1b[7;1Hprogress: 100%\x1b[K",
        "\x1b7\x1b[9;2H\x1b[3;4mstatus\x1b[0m\x1b8",
        "\x1b[2;10r\x1b[?6h\x1b[3;1Hregion\x1b[?6l\x1b[r",
        "\x1b[10;1Hone\ttwo\r\nthree",
        "\x1b[?1000h\x1b[?1006h\x1b[?1006l\x1b[?1000l",
        "\x1b[?25h\x1b[?2004l\x1b[?1l\x1b[?1049l",
        "worker: completed\r\n"
    );
    io::stdout().write_all(bytes.as_bytes())
}
