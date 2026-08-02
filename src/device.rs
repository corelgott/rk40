//! K40 (M2 Nano, CH341 USB) driver.
//!
//! Port of `nano_library.py` from K40 Whisperer. A dedicated worker thread
//! owns the USB handle; the web layer talks to it through a command channel
//! and receives status/progress through a broadcast channel.

use crate::egv::Egv;
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

const VENDOR_ID: u16 = 0x1a86;
const PRODUCT_ID: u16 = 0x5512;
const WRITE_EP: u8 = 0x02;
const READ_EP: u8 = 0x82;
const READ_LEN: usize = 168;
const TIMEOUT: Duration = Duration::from_millis(200);
const N_TIMEOUTS: u32 = 10;

// Controller status codes
const OK: u8 = 206;
const BUFFER_FULL: u8 = 238;
const CRC_ERROR: u8 = 207;
const TASK_COMPLETE: u8 = 236;
const UNKNOWN_2: u8 = 239;
const TASK_COMPLETE_M3: u8 = 204;

const HELLO: [u8; 1] = [160];
const UNLOCK: [u8; 34] = [
    166, 0, 73, 83, 50, 80, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70,
    70, 70, 70, 70, 70, 70, 70, 70, 70, 166, 15,
];
const HOME: [u8; 34] = [
    166, 0, 73, 80, 80, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70,
    70, 70, 70, 70, 70, 70, 70, 70, 70, 166, 228,
];
const ESTOP: [u8; 34] = [
    166, 0, 73, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70, 70,
    70, 70, 70, 70, 70, 70, 70, 70, 70, 166, 130,
];

fn one_wire_crc(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in data {
        let mut inbyte = b;
        for _ in 0..8 {
            let mix = (crc ^ inbyte) & 0x01;
            crc >>= 1;
            if mix != 0 {
                crc ^= 0x8C;
            }
            inbyte >>= 1;
        }
    }
    crc
}

/// Split raw EGV data into 34-byte packets, repeating for multiple passes.
/// Passes > 1 rewrites the final 'F' of intermediate passes to '@' so the
/// controller continues without re-homing (K40W "multipass" behaviour).
pub fn packetize(data: &[u8], passes: u32) -> Vec<[u8; 34]> {
    let mut data = data.to_vec();
    let mut blank = [70u8; 34];
    blank[0] = 166;
    blank[1] = 0;
    blank[32] = 166;
    blank[33] = 80;

    let mut packets: Vec<[u8; 34]> = Vec::new();
    let mut packet = blank;
    let mut cnt = 2usize;
    let len = data.len();
    for j in 0..passes.max(1) {
        let istart = if j == 0 { 0 } else { 1 };
        if passes > 1 && len >= 4 {
            data[len - 4] = if j == passes - 1 { b'F' } else { b'@' };
        }
        for i in istart..len {
            if cnt > 31 {
                packet[33] = one_wire_crc(&packet[1..32]);
                packets.push(packet);
                packet = blank;
                cnt = 2;
            }
            packet[cnt] = data[i];
            cnt += 1;
        }
    }
    packet[33] = one_wire_crc(&packet[1..32]);
    packets.push(packet);
    if cnt > 31 {
        blank[33] = one_wire_crc(&blank[1..32]);
        packets.push(blank);
    }
    packets
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Disconnected,
    Connected,
}

#[derive(Debug, Clone, Serialize)]
pub struct DeviceStatus {
    pub state: ConnState,
    pub busy: bool,
    pub paused: bool,
    pub job_name: Option<String>,
    pub progress: f64,
    pub message: String,
    /// Last job failure; stays set until the next job/connect so the UI can
    /// show it prominently.
    pub error: Option<String>,
    /// Estimated head position in mils relative to home (x right, y down)
    pub pos_x_mils: i64,
    pub pos_y_mils: i64,
}

impl Default for DeviceStatus {
    fn default() -> Self {
        DeviceStatus {
            state: ConnState::Disconnected,
            busy: false,
            paused: false,
            job_name: None,
            progress: 0.0,
            message: "Nicht verbunden".to_string(),
            error: None,
            pos_x_mils: 0,
            pos_y_mils: 0,
        }
    }
}

/// Keeps the machine awake while a job is running (K40 Whisperer does the
/// same via WindowsInhibitor): a sleeping host stops the USB stream and the
/// laser halts mid-job as soon as its small buffer drains.
struct SleepInhibitor(Option<std::process::Child>);

impl SleepInhibitor {
    fn new() -> SleepInhibitor {
        use std::process::{Command, Stdio};
        #[cfg(target_os = "macos")]
        let child = Command::new("caffeinate")
            .args(["-i", "-m"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        #[cfg(target_os = "linux")]
        let child = Command::new("systemd-inhibit")
            .args([
                "--what=sleep:idle",
                "--who=rk40",
                "--why=Laserjob läuft",
                "sleep",
                "infinity",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok();
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let child: Option<std::process::Child> = None;
        SleepInhibitor(child)
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        if let Some(c) = &mut self.0 {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

pub enum Command {
    Connect,
    Disconnect,
    Home,
    Unlock,
    PauseToggle,
    /// dx, dy in mils (x right, y down in machine/user terms)
    RapidMove(i64, i64),
    /// name, single-pass EGV data, passes
    RunJob(String, Vec<u8>, u32),
}

#[derive(Clone)]
pub struct DeviceHandle {
    tx: mpsc::Sender<Command>,
    pub status: Arc<Mutex<DeviceStatus>>,
    pub events: tokio::sync::broadcast::Sender<String>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
}

impl DeviceHandle {
    pub fn send(&self, cmd: Command) -> Result<(), String> {
        self.tx.send(cmd).map_err(|_| "Gerätethread beendet".to_string())
    }

    /// Emergency stop: takes effect between packets; also sent by the worker.
    pub fn request_stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
    }

    pub fn request_pause_toggle(&self) {
        self.pause_flag
            .store(!self.pause_flag.load(Ordering::SeqCst), Ordering::SeqCst);
        // wake worker if idle so a pause outside a job is a no-op
    }

    pub fn status_snapshot(&self) -> DeviceStatus {
        self.status.lock().unwrap().clone()
    }
}

pub fn spawn_device_thread() -> DeviceHandle {
    let (tx, rx) = mpsc::channel::<Command>();
    let status = Arc::new(Mutex::new(DeviceStatus::default()));
    let (events, _) = tokio::sync::broadcast::channel::<String>(64);
    let stop_flag = Arc::new(AtomicBool::new(false));
    let pause_flag = Arc::new(AtomicBool::new(false));

    let handle = DeviceHandle {
        tx,
        status: status.clone(),
        events: events.clone(),
        stop_flag: stop_flag.clone(),
        pause_flag: pause_flag.clone(),
    };

    std::thread::spawn(move || {
        let mut worker = Worker {
            usb: None,
            status,
            events,
            stop_flag,
            pause_flag,
            needs_drain: false,
        };
        while let Ok(cmd) = rx.recv() {
            worker.handle(cmd);
        }
    });

    handle
}

struct Worker {
    usb: Option<rusb::DeviceHandle<rusb::GlobalContext>>,
    status: Arc<Mutex<DeviceStatus>>,
    events: tokio::sync::broadcast::Sender<String>,
    stop_flag: Arc<AtomicBool>,
    pause_flag: Arc<AtomicBool>,
    /// A status read timed out: its late response may still be queued on the
    /// IN endpoint and must be discarded before the next hello, otherwise all
    /// following status reads are off by one.
    needs_drain: bool,
}

fn log(msg: &str) {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    eprintln!("[rk40 {t}] {msg}");
}

impl Worker {
    fn set_status(&self, f: impl FnOnce(&mut DeviceStatus)) {
        let snapshot = {
            let mut s = self.status.lock().unwrap();
            f(&mut s);
            s.clone()
        };
        let _ = self
            .events
            .send(serde_json::to_string(&snapshot).unwrap_or_default());
    }

    fn handle(&mut self, cmd: Command) {
        // A stop request applies only to the job that was running when it was
        // issued. Without this reset, the next jog/job would immediately see
        // the stale flag, fire another e-stop and freeze the board again —
        // making the laser appear dead until power-cycle + server restart.
        self.stop_flag.store(false, Ordering::SeqCst);
        match cmd {
            Command::Connect => match self.connect() {
                Ok(()) => self.set_status(|s| {
                    s.state = ConnState::Connected;
                    s.message = "Verbunden".into();
                }),
                Err(e) => {
                    self.usb = None;
                    self.set_status(|s| {
                        s.state = ConnState::Disconnected;
                        s.message = format!("Verbindung fehlgeschlagen: {e}");
                    });
                }
            },
            Command::Disconnect => {
                self.usb = None;
                self.set_status(|s| {
                    s.state = ConnState::Disconnected;
                    s.message = "Getrennt".into();
                });
            }
            Command::Home => {
                if self.send_raw(&HOME).is_ok() {
                    self.set_status(|s| {
                        s.message = "Home-Position wird angefahren".into();
                        s.pos_x_mils = 0;
                        s.pos_y_mils = 0;
                    });
                }
            }
            Command::Unlock => {
                // unfreeze first: after an e-stop the board ignores motion
                // until it gets an FNSE (K40 Whisperer's "unfreeze")
                self.send_unfreeze();
                if self.send_raw(&UNLOCK).is_ok() {
                    self.set_status(|s| s.message = "Achsen entriegelt".into());
                }
            }
            Command::PauseToggle => {
                // handled via pause_flag inside job loop; outside a job just send P N
                let data = vec![b'P', b'N'];
                let _ = self.send_data(&data, 1, None);
            }
            Command::RapidMove(dx, dy) => {
                // machine y up is positive in EGV; user dy>0 means down
                let mut egv = Egv::new();
                egv.make_move_data(dx, -dy);
                if egv.data.is_empty() {
                    return;
                }
                let data = egv.data.clone();
                if self.send_data(&data, 1, None).is_ok() {
                    self.set_status(|s| {
                        s.pos_x_mils += dx;
                        s.pos_y_mils += dy;
                        s.message = "Bewegt".into();
                    });
                }
            }
            Command::RunJob(name, data, passes) => {
                self.stop_flag.store(false, Ordering::SeqCst);
                self.pause_flag.store(false, Ordering::SeqCst);
                self.set_status(|s| {
                    s.busy = true;
                    s.paused = false;
                    s.progress = 0.0;
                    s.error = None;
                    s.job_name = Some(name.clone());
                    s.message = format!("Sende \"{name}\" an den Laser…");
                });
                log(&format!("Job start: \"{name}\" ({} bytes, {passes} passes)", data.len()));
                // keep the machine awake for the whole job (send + execution)
                let inhibitor = SleepInhibitor::new();
                let result = self.send_data(&data, passes, Some(&name));
                drop(inhibitor);
                match result {
                    Ok(()) => {
                        log(&format!("Job fertig: \"{name}\""));
                        self.set_status(|s| {
                            s.busy = false;
                            s.paused = false;
                            s.progress = 1.0;
                            s.job_name = None;
                            s.message = "Job abgeschlossen".into();
                        })
                    }
                    Err(e) => {
                        log(&format!("Job abgebrochen: \"{name}\": {e}"));
                        self.set_status(|s| {
                            s.busy = false;
                            s.paused = false;
                            s.job_name = None;
                            s.message = format!("Job abgebrochen: {e}");
                            s.error = Some(format!("Job „{name}“ abgebrochen: {e}"));
                        })
                    }
                }
            }
        }
    }

    fn connect(&mut self) -> Result<(), String> {
        self.usb = None;
        self.needs_drain = false;
        let devices = rusb::devices().map_err(|e| e.to_string())?;
        for dev in devices.iter() {
            let desc = match dev.device_descriptor() {
                Ok(d) => d,
                Err(_) => continue,
            };
            if desc.vendor_id() != VENDOR_ID || desc.product_id() != PRODUCT_ID {
                continue;
            }
            // `mut` only used on Linux (kernel driver detach)
            #[allow(unused_mut)]
            let mut handle = dev.open().map_err(|e| format!("USB open: {e}"))?;
            #[cfg(target_os = "linux")]
            {
                let _ = handle.set_auto_detach_kernel_driver(true);
            }
            // first configuration active, then claim interface 0
            let _ = handle.set_active_configuration(1);
            handle
                .claim_interface(0)
                .map_err(|e| format!("USB claim: {e}"))?;
            // CH341 init control transfer, as in K40 Whisperer
            let _ = handle.write_control(0x40, 177, 0x0102, 0, &[], Duration::from_millis(2000));
            self.usb = Some(handle);
            if self.say_hello().is_some() {
                return Ok(());
            }
            self.usb = None;
        }
        Err("Kein K40 gefunden (USB 1a86:5512). Läuft der Laser und ist er angeschlossen?".to_string())
    }

    /// Try to recover a dropped USB connection mid-job (K40s are notorious
    /// for EMI-induced USB drops; K40 Whisperer re-initializes the device
    /// after repeated timeouts and continues).
    fn reconnect(&mut self) -> bool {
        log("USB-Verbindung wird neu aufgebaut…");
        match self.connect() {
            Ok(()) => {
                log("USB neu verbunden");
                true
            }
            Err(e) => {
                log(&format!("USB-Reconnect fehlgeschlagen: {e}"));
                false
            }
        }
    }

    /// Emergency stop + unfreeze, so the board accepts motion again afterwards.
    fn do_estop(&mut self) {
        let _ = self.write_packet(&ESTOP);
        self.send_unfreeze();
    }

    /// K40 Whisperer's "unfreeze": after an e-stop the controller ignores
    /// motion commands until it receives an FNSE program end. Best effort,
    /// no handshake — the board may be in a confused state.
    fn send_unfreeze(&mut self) {
        std::thread::sleep(Duration::from_millis(100));
        for p in packetize(&[b'F', b'N', b'S', b'E'], 1) {
            let _ = self.write_packet(&p);
        }
    }

    fn write_packet(&self, packet: &[u8]) -> Result<(), rusb::Error> {
        let usb = self.usb.as_ref().ok_or(rusb::Error::NoDevice)?;
        usb.write_bulk(WRITE_EP, packet, TIMEOUT)?;
        Ok(())
    }

    fn say_hello(&mut self) -> Option<u8> {
        self.usb.as_ref()?;
        if self.needs_drain {
            // discard a late response from an earlier timed-out read so the
            // request/response pairing stays in sync
            let mut buf = [0u8; READ_LEN];
            if let Some(usb) = self.usb.as_ref() {
                let _ = usb.read_bulk(READ_EP, &mut buf, Duration::from_millis(50));
            }
            self.needs_drain = false;
        }
        let mut sent = false;
        for _ in 0..N_TIMEOUTS {
            if self.write_packet(&HELLO).is_ok() {
                sent = true;
                break;
            }
        }
        if !sent {
            return None;
        }
        let usb = self.usb.as_ref()?;
        let mut buf = [0u8; READ_LEN];
        for _ in 0..N_TIMEOUTS {
            match usb.read_bulk(READ_EP, &mut buf, TIMEOUT) {
                Ok(n) if n >= 2 => {
                    let r = buf[1];
                    return if r == OK
                        || r == BUFFER_FULL
                        || r == CRC_ERROR
                        || r == TASK_COMPLETE
                        || r == TASK_COMPLETE_M3
                        || r == UNKNOWN_2
                    {
                        Some(r)
                    } else {
                        Some(255) // unknown response
                    };
                }
                _ => continue,
            }
        }
        self.needs_drain = true;
        None
    }

    fn send_raw(&mut self, packet: &[u8]) -> Result<(), String> {
        if self.usb.is_none() {
            self.set_status(|s| s.message = "Nicht verbunden".into());
            return Err("Nicht verbunden".to_string());
        }
        self.write_packet(packet).map_err(|e| {
            let msg = format!("USB-Fehler: {e}");
            msg
        })?;
        Ok(())
    }

    fn check_pause_and_stop(&mut self) -> Result<(), String> {
        if self.stop_flag.load(Ordering::SeqCst) {
            let _ = self.write_packet(&ESTOP);
            self.set_status(|s| {
                s.paused = false;
                s.message = "Not-Stopp ausgeführt".into();
            });
            return Err("Von Benutzer gestoppt".to_string());
        }
        if self.pause_flag.load(Ordering::SeqCst) {
            // send pause (P N as its own mini transmission)
            let pn = packetize(&[b'P', b'N'], 1);
            for p in &pn {
                let _ = self.send_packet_checked(p);
            }
            self.set_status(|s| {
                s.paused = true;
                s.message = "Pausiert".into();
            });
            while self.pause_flag.load(Ordering::SeqCst) {
                if self.stop_flag.load(Ordering::SeqCst) {
                    self.do_estop();
                    self.set_status(|s| {
                        s.paused = false;
                        s.message = "Not-Stopp ausgeführt".into();
                    });
                    return Err("Von Benutzer gestoppt".to_string());
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            // resume
            for p in &pn {
                let _ = self.send_packet_checked(p);
            }
            self.set_status(|s| {
                s.paused = false;
                s.message = "Fortgesetzt".into();
            });
        }
        Ok(())
    }

    fn send_packet_checked(&mut self, packet: &[u8; 34]) -> Result<(), String> {
        let mut timeout_cnt = 0u32;
        let mut crc_cnt = 1u32;
        loop {
            // wait for buffer space
            let mut response = self.say_hello();
            while response == Some(BUFFER_FULL) {
                if self.stop_flag.load(Ordering::SeqCst) {
                    self.do_estop();
                    return Err("Von Benutzer gestoppt".to_string());
                }
                response = self.say_hello();
            }
            if self.write_packet(packet).is_err() {
                timeout_cnt += 1;
                log(&format!("USB-Timeout #{timeout_cnt} beim Senden"));
                self.set_status(|s| {
                    s.message = format!("USB-Timeout #{timeout_cnt} – versuche weiter…");
                });
                if self.stop_flag.load(Ordering::SeqCst) {
                    self.do_estop();
                    return Err("Von Benutzer gestoppt".to_string());
                }
                if timeout_cnt >= 120 {
                    // ~2 minutes of retries incl. reconnects: give up
                    return Err(format!(
                        "Laser antwortet nicht mehr ({timeout_cnt} Versuche, inkl. USB-Reconnects)"
                    ));
                }
                // like K40 Whisperer: after repeated timeouts re-initialize
                // the USB device and keep going (EMI drops, hub hiccups, …)
                if timeout_cnt % 20 == 0 {
                    self.set_status(|s| {
                        s.message = "USB-Verbindung wird neu aufgebaut…".into();
                    });
                    self.reconnect();
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            match self.say_hello() {
                Some(CRC_ERROR) => {
                    crc_cnt += 1;
                    log(&format!("CRC-Fehler #{crc_cnt}, Paket wird wiederholt"));
                    if crc_cnt >= N_TIMEOUTS {
                        return Err(format!("Zu viele Übertragungsfehler (CRC, {crc_cnt})"));
                    }
                    continue;
                }
                _ => return Ok(()), // OK, None or other -> next packet
            }
        }
    }

    fn send_data(&mut self, data: &[u8], passes: u32, job_name: Option<&str>) -> Result<(), String> {
        if self.usb.is_none() {
            self.set_status(|s| s.message = "Nicht verbunden".into());
            return Err("Nicht verbunden".to_string());
        }
        let packets = packetize(data, passes);
        let total = packets.len().max(1);
        let mut last_pct = -1i64;
        for (i, packet) in packets.iter().enumerate() {
            self.check_pause_and_stop()?;
            self.send_packet_checked(packet)?;
            let pct = (i as f64 / total as f64 * 100.0) as i64;
            if pct != last_pct && job_name.is_some() {
                last_pct = pct;
                self.set_status(|s| {
                    s.progress = i as f64 / total as f64;
                    s.message = format!("Übertrage… {pct} %");
                });
            }
        }
        if job_name.is_some() {
            self.set_status(|s| {
                s.progress = 1.0;
                s.message = "Übertragung fertig – Laser arbeitet".into();
            });
            self.wait_for_laser()?;
        }
        Ok(())
    }

    fn wait_for_laser(&mut self) -> Result<(), String> {
        loop {
            if self.stop_flag.load(Ordering::SeqCst) {
                self.do_estop();
                return Err("Von Benutzer gestoppt".to_string());
            }
            match self.say_hello() {
                Some(TASK_COMPLETE) | Some(TASK_COMPLETE_M3) => return Ok(()),
                None => {
                    // controller stopped reporting; assume finished
                    return Ok(());
                }
                _ => {
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_known_packets() {
        // The constant packets embed their own CRC as the last byte.
        for p in [&UNLOCK, &HOME, &ESTOP] {
            assert_eq!(one_wire_crc(&p[1..32]), p[33], "CRC mismatch");
        }
    }

    #[test]
    fn packetize_pads_and_crcs() {
        let data = vec![b'I', b'B', b'a', b'F', b'N', b'S', b'E'];
        let packets = packetize(&data, 1);
        assert_eq!(packets.len(), 1);
        let p = packets[0];
        assert_eq!(p[0], 166);
        assert_eq!(p[1], 0);
        assert_eq!(p[2], b'I');
        assert_eq!(p[8], b'E');
        assert_eq!(p[9], 70); // padding
        assert_eq!(p[32], 166);
        assert_eq!(p[33], one_wire_crc(&p[1..32]));
    }

    #[test]
    fn packetize_multipass_rewrites_footer() {
        // data ends with F N S E; two passes -> first pass footer '@'
        let data = vec![b'I', b'B', b'a', b'F', b'N', b'S', b'E'];
        let packets = packetize(&data, 2);
        assert_eq!(packets.len(), 1);
        // pass 1: I B a @ N S E ; pass 2 (skips leading I): B a F N S E
        assert_eq!(&packets[0][2..15], b"IBa@NSEBaFNSE");
    }
}
