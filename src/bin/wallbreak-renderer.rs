use std::sync::{Arc, atomic::AtomicBool};

use clap::Parser;
use wallbreak::{
    log,
    renderer::{self, Args},
};

fn main() {
    // Parse first so --help works without a bridge socket or signal setup.
    let args = Args::parse();
    log::init_from_env();
    if let Err(error) = run(args) {
        eprintln!("error: {error}");
        for cause in error.chain().skip(1) {
            eprintln!("  caused by: {cause}");
        }
        std::process::exit(1);
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let close = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, close.clone())?;
    }
    renderer::run(args, close)
}
