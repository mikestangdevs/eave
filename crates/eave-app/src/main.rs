//! `eave` — one binary, two roles (spec 01 §Topology, comet-style):
//! the GPUI shell by default, the engine with `--engine`.

use std::path::PathBuf;

mod ui;

fn main() {
    let mut engine = false;
    let mut data_dir: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--engine" => engine = true,
            "--data-dir" => data_dir = args.next().map(Into::into),
            other => {
                eprintln!("eave: unknown argument {other}");
                std::process::exit(64);
            }
        }
    }
    let data_dir = data_dir
        .or_else(|| std::env::var_os("EAVE_DATA_DIR").map(Into::into))
        .unwrap_or_else(default_data_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("eave: cannot create data dir {}: {e}", data_dir.display());
        std::process::exit(1);
    }
    if engine {
        eave_engine::engine::run_blocking(data_dir)
    } else {
        ui::run(data_dir)
    }
}

/// Data dir per spec 02 §Layout. `EAVE_DATA_DIR` / `--data-dir` override
/// for dev loops against the fake agent.
fn default_data_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME is set");
    PathBuf::from(home).join("Library/Application Support/eave")
}
