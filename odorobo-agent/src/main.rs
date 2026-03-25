mod state;
use stable_eyre::Result;
use std::fs::{self};

fn main() -> Result<()> {
    stable_eyre::install()?;

    println!("Hello, world!");

    Ok(())
}
