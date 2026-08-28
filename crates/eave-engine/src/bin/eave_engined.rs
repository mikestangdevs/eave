//! Standalone engine-role entry. The shipped app embeds the same role as
//! `eave --engine`; this bin exists so wire tests can spawn/kill a real
//! engine process without building GPUI.

use eave_engine::engine::run_blocking;

fn main() {
    let mut args = std::env::args().skip(1);
    let mut data_dir = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => data_dir = args.next(),
            other => {
                eprintln!("eave-engined: unknown argument {other}");
                std::process::exit(64);
            }
        }
    }
    let Some(data_dir) = data_dir else {
        eprintln!("eave-engined: --data-dir is required");
        std::process::exit(64);
    };
    run_blocking(data_dir.into());
}
