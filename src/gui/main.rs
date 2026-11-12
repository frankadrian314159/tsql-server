//! `tsql-gui` — the desktop application for tsql-server: browse and edit tables, design them,
//! run T-SQL, and watch the storage engine.
//!
//! ```text
//! tsql-gui [DATA_DIR] [--pool-pages N]
//! ```
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod datasheet;
mod design;
mod query;
mod sample;
mod session;
mod ui;

fn main() -> Result<(), eframe::Error> {
    let mut dir: Option<std::path::PathBuf> = None;
    let mut opts = tsql_server::StorageOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--pool-pages" => match args.next().and_then(|v| v.parse::<usize>().ok()) {
                Some(n) => opts.pool_pages = n,
                None => {
                    eprintln!("--pool-pages needs a number of 8 KiB pages");
                    std::process::exit(2);
                }
            },
            "-h" | "--help" => {
                println!("usage: tsql-gui [DATA_DIR] [--pool-pages N]\n\nWithout a data directory the database lives in memory only; File > Open data folder... opens one later.");
                return Ok(());
            }
            other if !other.starts_with('-') && dir.is_none() => dir = Some(other.into()),
            other => {
                eprintln!("unknown argument '{other}'");
                std::process::exit(2);
            }
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1200.0, 760.0]).with_min_inner_size([640.0, 420.0]).with_title("tsql-server"),
        ..Default::default()
    };
    eframe::run_native("tsql-server", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, dir, opts)))))
}

// The memory allocator of Windows is slow for the many small allocations of the engine; `--features fast-alloc` links mimalloc
// instead (about a third less time on the benchmark; see docs/performance.md). It is C code, so it is not on by default.
#[cfg(feature = "fast-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
