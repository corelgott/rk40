//! EGV (LHYMICRO-GL) data generation.
//!
//! Port of `egv.py` from K40 Whisperer (GPL). Generates the byte stream the
//! M2 Nano board executes: modal direction/distance codes, speed commands,
//! vector cutting and raster engraving programs.

use crate::laser_speed::{get_code_from_speed, Board};

const RIGHT: u8 = b'B'; // 66
const LEFT: u8 = b'T'; // 84
const UP: u8 = b'L'; // 76
const DOWN: u8 = b'R'; // 82
const ANGLE: u8 = b'M'; // 77
const ON: u8 = b'D'; // 68
const OFF: u8 = b'U'; // 85

/// One point of an "ecoords" polyline: (x inches, y inches, loop id).
/// Consecutive points sharing a loop id are joined with the laser on.
#[derive(Debug, Clone, Copy)]
pub struct ECoord {
    pub x: f64,
    pub y: f64,
    pub loop_id: i64,
}

pub struct Egv {
    pub data: Vec<u8>,
    /// Travel speed in mm/s for laser-off moves inside a job (K40 Whisperer's
    /// "Rapid Speed"). 0 = the board's unmoderated rapid jump at full speed,
    /// which starts and stops abruptly and can shake the workpiece.
    pub rapid_feed: f64,
    modal_dir: u8,
    modal_dist: i64,
    modal_on: bool,
    modal_ax: u8,
    modal_ay: u8,
}

impl Egv {
    pub fn new() -> Egv {
        Egv {
            data: Vec::new(),
            rapid_feed: 0.0,
            modal_dir: 0,
            modal_dist: 0,
            modal_on: false,
            modal_ax: 0,
            modal_ay: 0,
        }
    }

    fn write(&mut self, code: u8) {
        self.data.push(code);
    }

    fn move_dir(&mut self, direction: u8, distance: i64, laser_on: bool, angle_dirs: Option<(u8, u8)>) {
        let angle_dirs = angle_dirs.unwrap_or((self.modal_ax, self.modal_ay));

        if direction == self.modal_dir
            && laser_on == self.modal_on
            && angle_dirs.0 == self.modal_ax
            && angle_dirs.1 == self.modal_ay
        {
            self.modal_dist += distance;
        } else {
            self.flush(None);
            if laser_on != self.modal_on {
                self.write(if laser_on { ON } else { OFF });
                self.modal_on = laser_on;
            }
            if direction == ANGLE {
                if angle_dirs.0 != self.modal_ax {
                    self.write(angle_dirs.0);
                    self.modal_ax = angle_dirs.0;
                }
                if angle_dirs.1 != self.modal_ay {
                    self.write(angle_dirs.1);
                    self.modal_ay = angle_dirs.1;
                }
            }
            self.modal_dir = direction;
            self.modal_dist = distance;
            if direction == RIGHT || direction == LEFT {
                self.modal_ax = direction;
            }
            if direction == UP || direction == DOWN {
                self.modal_ay = direction;
            }
        }
    }

    fn flush(&mut self, laser_on: Option<bool>) {
        if self.modal_dist > 0 {
            let dir = self.modal_dir;
            self.write(dir);
            for code in make_distance(self.modal_dist) {
                self.write(code);
            }
        }
        if let Some(on) = laser_on {
            if on != self.modal_on {
                self.write(if on { ON } else { OFF });
                self.modal_on = on;
            }
        }
        self.modal_dist = 0;
    }

    fn make_dir_dist(&mut self, dxmils: i64, dymils: i64) {
        self.make_dir_dist_laser(dxmils, dymils, false)
    }

    fn make_dir_dist_laser(&mut self, dxmils: i64, dymils: i64, laser_on: bool) {
        if dymils.abs() > 0 {
            self.move_dir(if dymils > 0 { UP } else { DOWN }, dymils.abs(), laser_on, None);
        }
        if dxmils.abs() > 0 {
            self.move_dir(if dxmils > 0 { RIGHT } else { LEFT }, dxmils.abs(), laser_on, None);
        }
    }

    fn make_cut_line(&mut self, dxmils: i64, dymils: i64, spindle: bool) {
        let xcode = if dxmils < 0 { LEFT } else { RIGHT };
        let ycode = if dymils < 0 { DOWN } else { UP };

        let adx = dxmils.abs() as f64 / 1000.0;
        let ady = dymils.abs() as f64 / 1000.0;

        if dxmils == 0 {
            self.move_dir(ycode, dymils.abs(), spindle, None);
        } else if dymils == 0 {
            self.move_dir(xcode, dxmils.abs(), spindle, None);
        } else if dxmils == dymils {
            self.move_dir(ANGLE, dxmils.abs(), spindle, Some((xcode, ycode)));
        } else {
            let (slope, n, code, _code1) = if adx > ady {
                (ady / adx, dxmils.abs(), xcode, ycode)
            } else {
                (adx / ady, dymils.abs(), ycode, xcode)
            };

            let mut lh = 0.0f64;
            let mut d1 = 0i64;
            let mut d2 = 0i64;
            for i in 1..=n {
                let h = (i as f64 * slope).round();
                if h == lh {
                    d1 += 1;
                    if d2 > 0 {
                        self.move_dir(ANGLE, d2, spindle, Some((xcode, ycode)));
                        d2 = 0;
                    }
                } else {
                    d2 += 1;
                    if d1 > 0 {
                        self.move_dir(code, d1, spindle, None);
                        d1 = 0;
                    }
                }
                lh = h;
            }
            if d1 > 0 {
                self.move_dir(code, d1, spindle, None);
            }
            if d2 > 0 {
                self.move_dir(ANGLE, d2, spindle, Some((xcode, ycode)));
            }
        }
    }

    fn make_speed(&self, feed: f64, board: Board, raster_step: i64) -> Vec<u8> {
        get_code_from_speed(feed, raster_step.unsigned_abs() as i32, board)
            .into_bytes()
    }

    /// Rapid move program used for jogging the head (dx/dy in mils).
    pub fn make_move_data(&mut self, dxmils: i64, dymils: i64) {
        if dxmils.abs() + dymils.abs() > 0 {
            self.write(b'I');
            self.make_dir_dist(dxmils, dymils);
            self.flush(None);
            self.write(b'S');
            self.write(b'1');
            self.write(b'P');
        }
    }

    /// Speed-limited jog program: one laser-off move executed at `feed`
    /// instead of the board's full rapid speed ("jog compensation" — a
    /// full-speed rapid can shake the machine enough to shift the workpiece).
    /// dx/dy in mils, y positive up (EGV convention). Uses the same program
    /// structure as the vector mode of `make_egv_data`.
    pub fn make_slow_move_data(&mut self, dxmils: i64, dymils: i64, feed: f64, board: Board) {
        if dxmils.abs() + dymils.abs() == 0 {
            return;
        }
        self.write(b'I');
        let speed = self.make_speed(feed, board, 0);
        for code in speed {
            self.write(code);
        }
        self.write(b'N');
        self.write(if dymils <= 0 { DOWN } else { UP });
        self.write(if dxmils >= 0 { RIGHT } else { LEFT });
        self.write(b'S');
        self.write(b'1');
        self.write(b'E');
        self.make_cut_line(dxmils, dymils, false);
        self.flush(Some(false));
        self.write(b'F');
        self.write(b'N');
        self.write(b'S');
        self.write(b'E');
    }

    fn ecoord_adj(&self, p: &ECoord, scale: f64, flip_x_offset: f64) -> (i64, i64, i64) {
        let e0 = if flip_x_offset > 0.0 {
            ((flip_x_offset - p.x) * scale).round() as i64
        } else {
            (p.x * scale).round() as i64
        };
        let e1 = (p.y * scale).round() as i64;
        (e0, e1, p.loop_id)
    }

    /// Laser-off travel inside moderated mode. With a rapid feed set, the
    /// travel runs at that speed (speed change there and back); otherwise at
    /// the current work speed. Tiny hops never switch speed.
    fn rapid_move_slow(&mut self, dx: i64, dy: i64, feed: f64, board: Board, min_rapid: i64) {
        let tiny = dx.abs() < min_rapid && dy.abs() < min_rapid;
        if self.rapid_feed > 0.0 && !tiny && self.rapid_feed != feed {
            self.change_speed(self.rapid_feed, board, false, true, 0);
            self.make_dir_dist(dx, dy);
            self.change_speed(feed, board, false, true, 0);
        } else {
            self.make_dir_dist(dx, dy);
        }
    }

    /// Raster variant: travel at the rapid feed, then switch back to the
    /// raster speed code and finish with one tiny unmoderated step.
    fn raster_rapid_move_slow(&mut self, dx: i64, dy: i64, raster_step: i64, feed: f64, board: Board) {
        let tiny_step = raster_step.signum();
        self.change_speed(self.rapid_feed, board, false, false, 0);
        self.make_dir_dist(dx, dy - tiny_step);
        self.flush(Some(false));
        self.change_speed(feed, board, false, false, raster_step);
        self.write(b'N');
        self.make_dir_dist(0, tiny_step);
        self.flush(Some(false));
        self.write(b'S');
        self.write(b'E');
    }

    /// Standalone moderated move at `feed` that ends with '@NSE' so the main
    /// program can follow (K40W `make_egv_rapid` with finish=False).
    fn make_egv_rapid_prefix(&mut self, dx: i64, dy: i64, feed: f64, board: Board) {
        for code in self.make_speed(feed, board, 0) {
            self.write(code);
        }
        self.flush(Some(false));
        for &c in b"NRBS1E" {
            self.write(c);
        }
        self.make_cut_line(dx, dy, false);
        self.flush(Some(false));
        for &c in b"@NSE" {
            self.write(c);
        }
    }

    fn rapid_move_fast(&mut self, dx: i64, dy: i64) {
        let mut pad = 3i64;
        if pad == -dx {
            pad += 3;
        }
        self.make_dir_dist(-pad, 0); // "T" move
        self.make_dir_dist(0, pad); // "L" move
        self.flush(Some(false));
        if dx + pad < 0 {
            self.write(b'B');
        } else {
            self.write(b'T');
        }
        self.write(b'N');
        self.make_dir_dist(dx + pad, dy - pad);
        self.flush(Some(false));
        self.write(b'S');
        self.write(b'E');
    }

    /// Generate a full EGV program from ecoords.
    ///
    /// Coordinates are in inches, y positive up, with the laser head assumed
    /// to sit at (start_x, start_y) when the program starts. `raster_step` in
    /// mils: 0 = vector mode, negative = raster engraving top-down.
    #[allow(unused_assignments)] // faithful port of egv.py state machine
    pub fn make_egv_data(
        &mut self,
        ecoords: &[ECoord],
        start_x: f64,
        start_y: f64,
        feed: f64,
        board: Board,
        raster_step: i64,
    ) -> Result<(), String> {
        if ecoords.is_empty() {
            return Err("no coordinates".to_string());
        }
        let scale = 1000.0; // inches -> mils
        let flip_x_offset = 0.0;
        let start_x = (start_x * scale).round() as i64;
        let start_y = (start_y * scale).round() as i64;

        let speed = self.make_speed(feed, board, raster_step);

        if raster_step == 0 {
            //////////////////// Vector mode ////////////////////
            for &code in &speed {
                self.write(code);
            }
            let rapid = self.rapid_feed > 0.0;
            let (mut lastx, mut lasty, mut last_loop) =
                self.ecoord_adj(&ecoords[0], scale, flip_x_offset);
            if !rapid {
                self.make_dir_dist(lastx - start_x, lasty - start_y);
            }
            self.flush(Some(false));
            self.write(b'N');
            self.write(if lasty - start_y <= 0 { DOWN } else { UP });
            self.write(if lastx - start_x >= 0 { RIGHT } else { LEFT });
            self.write(b'S');
            self.write(b'1');
            self.write(b'E');

            let mut laser = false;
            let min_rapid = 5i64;
            if rapid {
                self.rapid_move_slow(lastx - start_x, lasty - start_y, feed, board, min_rapid);
            }
            for p in ecoords.iter().skip(1) {
                let (e0, e1, e2) = self.ecoord_adj(p, scale, flip_x_offset);
                if e2 == last_loop && !laser {
                    laser = true;
                } else if e2 != last_loop && laser {
                    laser = false;
                }
                let dx = e0 - lastx;
                let dy = e1 - lasty;
                if dx.abs() + dy.abs() > 0 {
                    if laser {
                        self.make_cut_line(dx, dy, true);
                    } else if (dx.abs() < min_rapid && dy.abs() < min_rapid) || rapid {
                        self.rapid_move_slow(dx, dy, feed, board, min_rapid);
                    } else {
                        self.rapid_move_fast(dx, dy);
                    }
                }
                lastx = e0;
                lasty = e1;
                last_loop = e2;
            }

            let dx = start_x - lastx;
            let dy = start_y - lasty;
            if (dx.abs() < min_rapid && dy.abs() < min_rapid) || rapid {
                self.rapid_move_slow(dx, dy, feed, board, min_rapid);
            } else {
                self.rapid_move_fast(dx, dy);
            }
        } else {
            //////////////////// Raster mode ////////////////////
            // Group points into scanlines (one group per distinct y).
            let mut scanline: Vec<Vec<ECoord>> = Vec::new();
            let mut scanline_y: Option<f64> = None;
            let indices: Vec<usize> = if raster_step < 0 {
                (0..ecoords.len()).collect()
            } else {
                (0..ecoords.len()).rev().collect()
            };
            for i in indices {
                let p = ecoords[i];
                if Some(p.y) != scanline_y {
                    scanline.push(vec![p]);
                    scanline_y = Some(p.y);
                } else {
                    // flip_x_offset = 0, so: insert at front when raster_step > 0
                    let last = scanline.last_mut().unwrap();
                    if raster_step > 0 {
                        last.insert(0, p);
                    } else {
                        last.push(p);
                    }
                }
            }

            let (mut lastx, mut lasty, _) = self.ecoord_adj(&scanline[0][0], scale, flip_x_offset);
            let dx_start = lastx - start_x;
            let dy_start = lasty - start_y;
            let rapid = self.rapid_feed > 0.0;

            if rapid {
                self.make_egv_rapid_prefix(dx_start, dy_start, self.rapid_feed, board);
            }
            for &code in &speed {
                self.write(code);
            }
            if !rapid {
                self.make_dir_dist(dx_start, dy_start);
            }

            self.flush(Some(false));
            self.write(b'N');
            self.write(if raster_step < 0 { b'R' } else { b'L' });
            self.write(b'B');
            self.write(b'S');
            self.write(b'1');
            self.write(b'E');

            let pad = 2i64;
            let mut sign = -1i64;
            let mut rapid_flag = true;

            for scan_raw in &scanline {
                let scan: Vec<(i64, i64, i64)> = scan_raw
                    .iter()
                    .map(|p| self.ecoord_adj(p, scale, flip_x_offset))
                    .collect();

                // Flip direction and reset loop
                sign = -sign;
                let mut last_loop: Option<i64> = None;
                let y = scan[0].1;
                let dy = y - lasty;
                let mut xr = if sign == 1 { scan[0].0 } else { scan[scan.len() - 1].0 };
                let mut dxr = xr - lastx;

                // Rapid move between separated scanlines
                if (dy - raster_step) != 0 && !rapid_flag {
                    let yoffset = if dxr * sign < 0 { -raster_step * 3 } else { -raster_step };
                    let yoff_sign = if yoffset >= 0 { 1 } else { -1 };
                    if (dy + yoffset) * yoff_sign < 0 {
                        self.flush(Some(false));
                        if rapid {
                            self.raster_rapid_move_slow(0, dy + yoffset, raster_step, feed, board);
                        } else {
                            self.write(b'N');
                            self.make_dir_dist(0, dy + yoffset);
                            self.flush(Some(false));
                            self.write(b'S');
                            self.write(b'E');
                        }
                        rapid_flag = true;
                    } else {
                        let adj_steps = dy / raster_step; // truncating division
                        for _ in 1..adj_steps {
                            let adj_dist = 5i64;
                            self.make_dir_dist(sign * adj_dist, 0);
                            lastx += sign * adj_dist;
                            sign = -sign;
                            xr = if sign == 1 { scan[0].0 } else { scan[scan.len() - 1].0 };
                            dxr = xr - lastx;
                        }
                    }
                    lasty = y;
                }

                // Pad row end if needed
                if dxr * sign <= 0 {
                    if !rapid_flag {
                        self.make_dir_dist(-sign * pad, 0);
                        self.make_dir_dist(dxr, 0);
                        self.make_dir_dist(sign * pad, 0);
                    } else {
                        self.make_dir_dist(dxr, 0);
                    }
                    lastx += dxr;
                }
                rapid_flag = false;

                let rng: Vec<usize> = if sign == 1 {
                    (0..scan.len()).collect()
                } else {
                    (0..scan.len()).rev().collect()
                };
                for j in rng {
                    let x = scan[j].0;
                    let dx = x - lastx;
                    let loop_id = scan[j].2;
                    if Some(loop_id) == last_loop {
                        self.make_cut_line(dx, 0, true);
                    } else if dx * sign > 0 {
                        self.make_dir_dist(dx, 0);
                    }
                    lastx = x;
                    last_loop = Some(loop_id);
                }
                lasty = y;
            }

            // Final move to ensure the last move is to the right
            self.make_dir_dist(pad, 0);
            lastx += pad;
            if sign < 0 {
                lasty += raster_step;
            }
            self.flush(Some(false));

            let dx_final = start_x - lastx;
            let dy_final = if raster_step < 0 {
                (start_y - lasty) + raster_step
            } else {
                (start_y - lasty) - raster_step
            };

            let max_return_feed = 50.0;
            let final_feed = if rapid {
                self.rapid_feed
            } else if feed > max_return_feed {
                max_return_feed
            } else {
                0.0
            };
            if final_feed > 0.0 {
                self.change_speed(final_feed, board, false, false, 0);
                let dy_final = dy_final + raster_step.abs();
                self.make_dir_dist(dx_final, dy_final);
            } else {
                self.write(b'N');
                self.make_dir_dist(dx_final, dy_final);
                self.flush(Some(false));
                self.write(b'S');
                self.write(b'E');
            }
        }

        // Footer
        self.flush(Some(false));
        self.write(b'F');
        self.write(b'N');
        self.write(b'S');
        self.write(b'E');
        Ok(())
    }

    fn change_speed(&mut self, feed: f64, board: Board, laser_on: bool, pad: bool, raster_step: i64) {
        let cspad = 5i64;
        if laser_on {
            self.write(OFF);
        }
        if pad {
            self.make_dir_dist(-cspad, -cspad);
        }
        self.flush(Some(false));

        self.write(b'@');
        self.write(b'N');
        self.write(b'S');
        self.write(b'E');
        let speed = self.make_speed(feed, board, raster_step);
        for code in speed {
            self.write(code);
        }
        self.write(b'N');
        self.write(b'R');
        self.write(b'B');
        self.write(b'S');
        self.write(b'1');
        self.write(b'E');
        self.write(b'U');

        if pad {
            self.make_dir_dist(cspad, cspad);
        }
        self.flush(Some(false));

        if laser_on {
            self.write(ON);
        }
    }
}

/// Strip modal direction codes repeated without effect (raster optimization).
pub fn strip_redundant_codes(egv_data: &[u8]) -> Vec<u8> {
    let e = b'E';
    let mut out = Vec::with_capacity(egv_data.len());
    let mut modal_value: i32 = -1;
    for &code in egv_data {
        if code as i32 == modal_value && code != e {
            continue;
        }
        if code == RIGHT || code == LEFT || code == UP || code == DOWN || code == ANGLE || code == e {
            modal_value = code as i32;
        }
        out.push(code);
    }
    out
}

fn make_distance(dist_mils: i64) -> Vec<u8> {
    let mut code = Vec::new();
    let mut rest = dist_mils;
    while rest >= 255 {
        code.push(122); // 'z' = 255 mils
        rest -= 255;
    }
    if rest == 0 {
        // nothing
    } else if rest < 26 {
        code.push((96 + rest) as u8); // 'a'..'y'
    } else if rest < 52 {
        code.push(124); // '|'
        code.push((96 + rest - 25) as u8);
    } else {
        let s = format!("{:03}", rest);
        code.extend_from_slice(s.as_bytes());
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_encoding() {
        assert_eq!(make_distance(1), vec![b'a']);
        assert_eq!(make_distance(25), vec![b'y']);
        assert_eq!(make_distance(26), vec![b'|', b'a']);
        assert_eq!(make_distance(51), vec![b'|', b'z']);
        assert_eq!(make_distance(52), b"052".to_vec());
        assert_eq!(make_distance(254), b"254".to_vec());
        assert_eq!(make_distance(255), vec![122]);
        assert_eq!(make_distance(256), vec![122, b'a']);
        assert_eq!(make_distance(510), vec![122, 122]);
    }

    #[test]
    fn jog_move_program() {
        let mut e = Egv::new();
        e.make_move_data(100, -50);
        // I, R(down)+dist, B(right)+dist, S, 1, P
        let s: Vec<u8> = e.data;
        assert_eq!(s[0], b'I');
        assert_eq!(*s.last().unwrap(), b'P');
    }

    #[test]
    fn slow_jog_program() {
        let mut e = Egv::new();
        e.make_slow_move_data(1000, -500, 50.0, Board::M2);
        let s = e.data;
        assert_eq!(s[0], b'I');
        // vector speed code follows the init byte
        assert_eq!(s[1], b'C');
        assert_eq!(s[2], b'V');
        // program ends like a job, head stays at the target
        assert!(s.ends_with(b"FNSE"));
        // laser must never turn on during a jog
        assert!(!s.contains(&ON));

        // zero move produces no program
        let mut e2 = Egv::new();
        e2.make_slow_move_data(0, 0, 50.0, Board::M2);
        assert!(e2.data.is_empty());
    }

    /// Minimal EGV interpreter for tests: returns the laser-on segments (in
    /// mils, relative to the start) and the final head position.
    #[allow(clippy::type_complexity)]
    fn trace(data: &[u8]) -> (Vec<((i64, i64), (i64, i64))>, (i64, i64)) {
        let (mut x, mut y) = (0i64, 0i64);
        let (mut sx, mut sy) = (1i64, 1i64);
        let mut axis = b'B';
        let mut on = false;
        let mut segs = Vec::new();
        let mut i = 0;
        while i < data.len() {
            let c = data[i];
            i += 1;
            let n = match c {
                b'C' if data.get(i) == Some(&b'V') => {
                    i += 1;
                    while i < data.len() && data[i].is_ascii_digit() {
                        i += 1;
                    }
                    if data.get(i) == Some(&b'C') {
                        i += 1;
                    }
                    continue;
                }
                b'V' => {
                    // raster speed code: V<digits>G<3 digits>
                    while i < data.len() && data[i].is_ascii_digit() {
                        i += 1;
                    }
                    if data.get(i) == Some(&b'G') {
                        i += 4;
                    }
                    continue;
                }
                b'S' => {
                    if data.get(i) == Some(&b'1') {
                        i += 1;
                    }
                    continue;
                }
                b'I' | b'N' | b'E' | b'F' | b'P' | b'@' => continue,
                RIGHT | LEFT => {
                    sx = if c == RIGHT { 1 } else { -1 };
                    axis = c;
                    continue;
                }
                UP | DOWN => {
                    sy = if c == UP { 1 } else { -1 };
                    axis = c;
                    continue;
                }
                ANGLE => {
                    axis = c;
                    continue;
                }
                ON | OFF => {
                    on = c == ON;
                    continue;
                }
                b'z' => 255,
                b'a'..=b'y' => (c - 96) as i64,
                b'|' => {
                    let v = 25 + (data[i] - 96) as i64;
                    i += 1;
                    v
                }
                b'0'..=b'9' => {
                    let v: i64 = std::str::from_utf8(&data[i - 1..i + 2]).unwrap().parse().unwrap();
                    i += 2;
                    v
                }
                other => panic!("unexpected code {:?}", other as char),
            };
            let from = (x, y);
            match axis {
                RIGHT | LEFT => x += sx * n,
                UP | DOWN => y += sy * n,
                _ => {
                    x += sx * n;
                    y += sy * n;
                }
            }
            if on {
                segs.push((from, (x, y)));
            }
        }
        (segs, (x, y))
    }

    fn two_squares() -> Vec<ECoord> {
        let mut pts = Vec::new();
        for (id, ox, oy) in [(1, 0.5, 0.5), (2, 3.0, 1.5)] {
            for (dx, dy) in [(0.0, 0.0), (0.4, 0.0), (0.4, 0.4), (0.0, 0.4), (0.0, 0.0)] {
                pts.push(ECoord { x: ox + dx, y: oy + dy, loop_id: id });
            }
        }
        pts
    }

    #[test]
    fn rapid_feed_keeps_cut_geometry() {
        let pts = two_squares();
        let mut fast = Egv::new();
        fast.make_egv_data(&pts, 0.0, 0.0, 10.0, Board::M2, 0).unwrap();
        let mut slow = Egv::new();
        slow.rapid_feed = 50.0;
        slow.make_egv_data(&pts, 0.0, 0.0, 10.0, Board::M2, 0).unwrap();

        let (cuts_fast, end_fast) = trace(&fast.data);
        let (cuts_slow, end_slow) = trace(&slow.data);
        assert_eq!(end_fast, (0, 0), "fast program returns to start");
        assert_eq!(end_slow, (0, 0), "slow program returns to start");
        assert_eq!(cuts_fast, cuts_slow, "identical laser-on path");
        assert_eq!(cuts_slow.first().unwrap().0, (500, 500));

        // without rapid feed: no speed change inside the program; with it:
        // a switch to the travel speed and back around every travel
        let speed_changes = |d: &[u8]| d.windows(4).filter(|w| w == b"@NSE").count();
        assert_eq!(speed_changes(&fast.data), 0);
        assert_eq!(speed_changes(&slow.data), 6); // to start, between, back home
        // the unmoderated rapid jump ("SE" without "S1E") is gone
        let jumps = |d: &[u8]| {
            d.windows(2).filter(|w| w == b"SE").count() - d.windows(3).filter(|w| w == b"NSE").count()
        };
        assert!(jumps(&fast.data) > 0);
        assert_eq!(jumps(&slow.data), 0);
    }

    #[test]
    fn rapid_feed_raster_returns_home() {
        // two separated bands of scanlines force a rapid between them
        let mut pts = Vec::new();
        for (id, row) in (0..5).chain(40..45).enumerate() {
            let id = id as i64 + 1;
            let y = 1.0 - row as f64 * 0.002;
            pts.push(ECoord { x: 0.5, y, loop_id: id });
            pts.push(ECoord { x: 0.9, y, loop_id: id });
        }
        let run = |rapid: f64| {
            let mut e = Egv::new();
            e.rapid_feed = rapid;
            e.make_egv_data(&pts, 0.0, 1.2, 100.0, Board::M2, -2).unwrap();
            assert!(e.data.ends_with(b"FNSE"));
            trace(&e.data)
        };
        let (cuts_fast, end_fast) = run(0.0);
        let (cuts_slow, end_slow) = run(50.0);
        assert!(!cuts_fast.is_empty());
        assert_eq!(cuts_fast, cuts_slow, "identical laser-on path");
        assert_eq!(end_fast, end_slow, "same final position");
    }
}
