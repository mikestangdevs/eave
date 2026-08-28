//! `eave` — one binary, two roles (spec 01 §Topology, comet-style):
//! the GPUI shell by default, the engine with `--engine`.

use std::path::PathBuf;

use eave_app::{ui, web};

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
        .unwrap_or_else(ui::default_data_dir);
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        eprintln!("eave: cannot create data dir {}: {e}", data_dir.display());
        std::process::exit(1);
    }
    if engine {
        // The engine is headless and stays that way: it owns the tab model
        // and knows nothing about Chromium. Starting a browser here would
        // put a renderer in a process with no window to draw into.
        eave_engine::engine::run_blocking(data_dir)
    } else {
        // Slice C1, and the ORDER here is the whole point: the `NSApp`
        // subclass must be installed BEFORE gpui asks for `NSApp`, because
        // `+sharedApplication` decides that object's class once and there is
        // no second chance. `ui::run` is the first thing that asks.
        //
        // Only on this branch. The engine has no window, and Chromium's own
        // child processes are a separate executable (`eave-web-helper`), so
        // this is the one process in the tree that needs it.
        web::install_nsapp();
        ui::run(data_dir)
    }
}

// The layout rule itself lives in `ui::default_data_dir` (spec 02
// §Layout), so the friction-log fallback resolves to the same place
// without a second copy of it. `EAVE_DATA_DIR` / `--data-dir` override
// for dev loops against the fake agent.
