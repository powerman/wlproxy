use clap::Parser;
use wlproxy::{run, Args};

fn main() -> Result<(), String> {
    run(Args::parse())
}
