//! Differential test helper: emits EGV for fixed inputs so the output can be
//! compared byte-for-byte with the original Python egv.py.
//! Usage: cargo run --example dump_egv <raster|vector> <outfile>

#[path = "../src/laser_speed.rs"]
mod laser_speed;
#[path = "../src/egv.rs"]
mod egv;

use egv::{ECoord, Egv};
use laser_speed::Board;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("raster");
    let out = args.get(2).cloned().unwrap_or_else(|| "out.bin".to_string());

    let ec = |v: &[(f64, f64, i64)]| -> Vec<ECoord> {
        v.iter().map(|&(x, y, l)| ECoord { x, y, loop_id: l }).collect()
    };

    let mut e = Egv::new();
    match mode {
        "raster" | "raster_up" => {
            let ecoords = ec(&[
                (0.000, 0.010, 2), (0.100, 0.010, 2),
                (0.010, 0.008, 3), (0.050, 0.008, 3),
                (0.060, 0.008, 4), (0.090, 0.008, 4),
                (0.020, 0.006, 5), (0.080, 0.006, 5),
                (0.030, 0.000, 6), (0.070, 0.000, 6),
            ]);
            let step = if mode == "raster_up" { 2 } else { -2 };
            e.make_egv_data(&ecoords, 0.0, 0.010, 100.0, Board::M2, step).unwrap();
        }
        _ => {
            let ecoords = ec(&[
                (0.1, 0.1, 1), (0.5, 0.1, 1), (0.5, 0.4, 1), (0.1, 0.4, 1), (0.1, 0.1, 1),
                (0.7, 0.2, 2), (0.9, 0.35, 2),
            ]);
            e.make_egv_data(&ecoords, 0.0, 0.5, 10.0, Board::M2, 0).unwrap();
        }
    }
    std::fs::write(&out, &e.data).unwrap();
    eprintln!("{} {} bytes", mode, e.data.len());
}
