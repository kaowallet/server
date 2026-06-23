#![forbid(unsafe_code)]

mod check;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("check") => check::check(&workspace_root()?),
        _ => {
            eprintln!("usage: cargo xtask <check>");
            std::process::exit(1);
        }
    }
}

fn workspace_root() -> Result<PathBuf> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").context("CARGO_MANIFEST_DIR not set")?;
    Path::new(&manifest)
        .parent()
        .map(Path::to_path_buf)
        .context("xtask must live one level below the workspace root")
}
