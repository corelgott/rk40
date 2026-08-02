//! Speed-code generation for LHYMICRO-GL boards (M2 Nano and friends).
//!
//! Port of `LaserSpeed.py` from K40 Whisperer (MIT licensed section).
//! The controller encodes speed as a stepper-tick period mapped through a
//! per-board linear equation and a "gear" selected from the speed range.

const DEFAULT_D_RATIO: f64 = 0.261199033289;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // all board variants kept for completeness; rk40 defaults to M2
pub enum Board {
    A,
    B,
    B1,
    B2,
    M,
    M1,
    M2,
}

impl Board {
    #[allow(dead_code)]
    pub fn from_name(name: &str) -> Board {
        // Accepts "LASER-M2" or plain "M2"
        let code = name.rsplit('-').next().unwrap_or("M2");
        match code {
            "A" => Board::A,
            "B" => Board::B,
            "B1" => Board::B1,
            "B2" => Board::B2,
            "M" => Board::M,
            "M1" => Board::M1,
            _ => Board::M2,
        }
    }

    fn is_m_series(self) -> bool {
        matches!(self, Board::M | Board::M1 | Board::M2)
    }
}

fn get_gear_for_speed(mm_per_second: f64, uses_raster_step: bool) -> u8 {
    if mm_per_second <= 25.4 {
        return 1;
    }
    if mm_per_second <= 60.0 {
        return 2;
    }
    if !uses_raster_step {
        if mm_per_second < 127.0 {
            3
        } else {
            4
        }
    } else if mm_per_second < 127.0 {
        2
    } else if mm_per_second <= 320.0 {
        3
    } else {
        4
    }
}

/// Returns (b, m, gear)
fn get_gearing(board: Board, mm_per_second: f64, uses_raster_step: bool) -> (f64, f64, u8) {
    let gear = get_gear_for_speed(mm_per_second, uses_raster_step);
    let b_values: [f64; 4] = if board.is_m_series() {
        [60416.0, 60416.0, 59904.0, 59392.0]
    } else {
        [64752.0, 64752.0, 64640.0, 64512.0]
    };
    let mut m = if board.is_m_series() { -12120.0 } else { -2000.0 };
    if board == Board::B2 {
        m = -24240.0;
    }
    // gear 0 (C-suffix) special cases for low speeds
    match board {
        Board::B2 => {
            if mm_per_second < 7.0 {
                return (b_values[0], m / 12.0, if uses_raster_step { 1 } else { 0 });
            }
        }
        Board::M => {
            if mm_per_second < 6.0 {
                return (b_values[0], m, 0);
            }
        }
        Board::M1 => {
            if mm_per_second < 6.0 || (!uses_raster_step && mm_per_second < 7.0) {
                return (b_values[0], m, 0);
            }
        }
        Board::M2 if mm_per_second < 7.0 => {
            return (65528.0, m / 12.0, 0);
        }
        _ => {}
    }
    (b_values[(gear - 1) as usize], m, gear)
}

fn get_value_from_speed(mm_per_second: f64, b: f64, m: f64) -> f64 {
    if mm_per_second == 0.0 {
        return b;
    }
    let frequency_khz = mm_per_second / 25.4;
    let period_in_ms = 1.0 / frequency_khz;
    m * period_in_ms + b
}

fn encode_value(value: f64) -> String {
    let value = value as i64; // truncate toward zero, like Python int()
    let b0 = value & 255;
    let b1 = (value >> 8) & 0xFFFFFF; // emulates the controller's bugged negative form
    format!("{:03}{:03}", b1, b0)
}

/// Generate the speed code string for a given speed in mm/s.
/// `raster_step` is the raster step in mils (0 for vector mode).
pub fn get_code_from_speed(mm_per_second: f64, raster_step: i32, board: Board) -> String {
    let mut mm_per_second = mm_per_second;
    if mm_per_second > 240.0 && raster_step == 0 {
        mm_per_second = 19.05; // out-of-range fallback, as in the original
    }
    let uses_raster_step = raster_step != 0;
    let (b, m, mut gear) = get_gearing(board, mm_per_second, uses_raster_step);

    let mut speed_value = get_value_from_speed(mm_per_second, b, m);
    if speed_value - speed_value.round() > 0.005 {
        speed_value = speed_value.ceil();
    }
    let speed_value = speed_value.round();
    let encoded_speed = encode_value(speed_value);

    if raster_step != 0 {
        if gear == 0 {
            gear = 1; // no C-suffix notation in raster mode
        }
        return format!("V{}{}G{:03}", encoded_speed, gear, raster_step);
    }

    let d_ratio = DEFAULT_D_RATIO;
    if matches!(board, Board::A | Board::B | Board::M) {
        // no diagonal correction code
        if gear == 0 {
            format!("CV{}1C", encoded_speed)
        } else {
            format!("CV{}{}", encoded_speed, gear)
        }
    } else {
        let step_value = ((mm_per_second.floor() as i64) + 1).min(128);
        let frequency_khz = mm_per_second / 25.4;
        let period_in_ms = if frequency_khz != 0.0 { 1.0 / frequency_khz } else { 0.0 };
        let d_value = d_ratio * -m * period_in_ms / step_value as f64;
        let encoded_diagonal = encode_value(d_value);
        if gear == 0 {
            format!("CV{}1{:03}{}C", encoded_speed, step_value, encoded_diagonal)
        } else {
            format!("CV{}{}{:03}{}", encoded_speed, gear, step_value, encoded_diagonal)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Reference values generated with the original Python LaserSpeed
    // (board M2, d_ratio default).
    #[test]
    fn vector_codes_match_reference() {
        assert_eq!(get_code_from_speed(0.5, 0, Board::M2), "CV0551401001052089C");
        assert_eq!(get_code_from_speed(5.0, 0, Board::M2), "CV2352381006000223C");
        assert_eq!(get_code_from_speed(10.0, 0, Board::M2), "CV1151921011002218");
        assert_eq!(get_code_from_speed(30.0, 0, Board::M2), "CV1952352031000086");
        assert_eq!(get_code_from_speed(50.0, 0, Board::M2), "CV2112442051000031");
        assert_eq!(get_code_from_speed(100.0, 0, Board::M2), "CV2212503101000007");
        assert_eq!(get_code_from_speed(180.0, 0, Board::M2), "CV2250824128000003");
        assert_eq!(get_code_from_speed(240.0, 0, Board::M2), "CV2262544128000002");
    }

    #[test]
    fn raster_codes_match_reference() {
        assert_eq!(get_code_from_speed(100.0, 2, Board::M2), "V2232502G002");
        assert_eq!(get_code_from_speed(180.0, 2, Board::M2), "V2270823G002");
        assert_eq!(get_code_from_speed(250.0, 2, Board::M2), "V2290493G002");
        assert_eq!(get_code_from_speed(50.0, 5, Board::M2), "V2112442G005");
        assert_eq!(get_code_from_speed(10.0, 1, Board::M2), "V1151921G001");
    }
}
