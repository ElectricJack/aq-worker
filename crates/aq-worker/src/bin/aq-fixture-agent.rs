use clap::Parser;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    launch_dir: std::path::PathBuf,
    #[arg(long)]
    guardian: bool,
}
fn main() -> anyhow::Result<()> {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("guardian" | "pane")
    ) {
        return aq_session_host::helper_main();
    }
    let args = Args::parse();
    if args.guardian {
        aq_worker::guardian::run(&args.launch_dir)
    } else {
        aq_worker::fixture::run_fixture(&args.launch_dir)
    }
}
