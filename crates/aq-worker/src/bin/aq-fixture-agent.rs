use clap::Parser;
#[derive(Parser)]
struct Args {
    #[arg(long)]
    launch_dir: std::path::PathBuf,
    #[arg(long)]
    guardian: bool,
}
fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    if args.guardian {
        aq_worker::guardian::run(&args.launch_dir)
    } else {
        aq_worker::fixture::run_fixture(&args.launch_dir)
    }
}
