//! Fixed, disposable DockerLocal PostgreSQL service.
pub const ROOT: &str = "/run/aq-postgres";
pub const TMPFS: &str = "rw,nosuid,nodev,noexec,mode=1777,size=256m";
pub const ENV: [(&str, &str); 5] = [
    ("PGHOST", "/run/aq-postgres/socket"),
    ("PGPORT", "5432"),
    ("PGUSER", "postgres"),
    ("PGDATABASE", "postgres"),
    (
        "DATABASE_URL",
        "postgresql://postgres@localhost/postgres?host=/run/aq-postgres/socket&port=5432",
    ),
];
pub const NOTE: &str = "\nTask PostgreSQL is ready before the harness starts. Connect with `psql \"$DATABASE_URL\"` or use the PGHOST, PGPORT, PGUSER and PGDATABASE variables. Its data is local to this attempt and is discarded when the sandbox stops.\n";

pub fn command(runner: &str) -> Vec<String> {
    [
        "/bin/bash",
        "-c",
        include_str!("task-postgres-gate.sh"),
        "aq-task-postgres",
        runner,
    ]
    .map(str::to_owned)
    .into()
}

pub fn startup_failure(code: i32) -> Option<&'static str> {
    match code {
        95 => {
            Some("Required PostgreSQL tools or the unprivileged database account are unavailable.")
        }
        96 => Some("Required task PostgreSQL did not become ready before launch."),
        _ => None,
    }
}

/// Only the DockerLocal backend invokes this fixed setup, as an unprivileged user.
pub const BOOTSTRAP: &str = include_str!("task-postgres.sh");
