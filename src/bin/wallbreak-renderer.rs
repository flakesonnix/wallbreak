use std::sync::{atomic::AtomicBool, Arc};

use anyhow::Result;
use clap::Parser;
use wallbreak::renderer::{self, Args};

fn main() -> Result<()> {
    // Parse first so --help works without a bridge socket or signal setup.
    let args = Args::parse();
    let close = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, close.clone())?;
    }
    renderer::run(args, close)
}
