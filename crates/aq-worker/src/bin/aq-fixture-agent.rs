use clap::Parser;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    launch_dir: std::path::PathBuf,
}
fn main() -> anyhow::Result<()> {
    aq_worker::fixture::run_fixture(&Args::parse().launch_dir)
}
