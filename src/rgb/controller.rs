//! The ENE 6k7750 register map and its write sequences.
//!
//! Register map, the per-output tail and the frame commit are transcribed from
//! the community SignalRGB plugin (`refs/signalrgb-maxsun/`, "Default Maxsun
//! device" profile): two ARGB chains of up to 128 LEDs plus two single-colour
//! 12 V RGB headers.
//!
//! Two deliberate differences from the reference implementation, both measured
//! (see `rgb-tool bench`):
//!
//! * the per-frame "handshake" reads that the plugin author himself documents
//!   as unused ("发就对了") run once per session instead of every frame —
//!   2.28 ms per call, i.e. 41% of a frame's transfer cost;
//! * nothing is written at all unless the caller's colours changed, which is
//!   what makes a static or slow-changing effect free (the change detection
//!   lives in [`super::RgbEngine`]).

use super::transport::Device;

/// One controllable output on the controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Output {
    pub name: &'static str,
    /// LEDs the register buffer can hold (the physical chain may be shorter).
    pub leds: usize,
    pub address: u16,
    pub commit: u16,
    pub mode: u16,
    pub speed: u16,
    pub direction: u16,
    pub brightness: u16,
    /// 12 V headers take a single RGB triplet (whole-header colour).
    pub single_color: bool,
}

pub const ARGB1: Output = Output {
    name: "ARGB 通道 1",
    leds: 128,
    address: 0xe300,
    commit: 0xe02f,
    mode: 0xe021,
    speed: 0xe022,
    direction: 0xe023,
    brightness: 0xe029,
    single_color: false,
};
pub const ARGB2: Output = Output {
    name: "ARGB 通道 2",
    leds: 128,
    address: 0xe480,
    commit: 0xe03f,
    mode: 0xe031,
    speed: 0xe032,
    direction: 0xe033,
    brightness: 0xe039,
    single_color: false,
};
pub const RGB12_1: Output = Output {
    name: "12V RGB 1",
    leds: 1,
    address: 0xe600,
    commit: 0xe04f,
    mode: 0xe041,
    speed: 0xe042,
    direction: 0xe043,
    brightness: 0xe049,
    single_color: true,
};
pub const RGB12_2: Output = Output {
    name: "12V RGB 2",
    leds: 1,
    address: 0xe603,
    commit: 0xe05f,
    mode: 0xe051,
    speed: 0xe052,
    direction: 0xe053,
    brightness: 0xe059,
    single_color: true,
};

/// Everything the controller exposes on this board.
pub const OUTPUTS: [Output; 4] = [ARGB1, ARGB2, RGB12_1, RGB12_2];

/// Registers the frame commit writes `0x01` to (mode of both ARGB chains plus
/// their commits).
const FRAME_COMMIT: [u16; 4] = [0xe021, 0xe02f, 0xe031, 0xe03f];

pub struct Controller {
    dev: Device,
    ready: bool,
    /// Counters for the diagnostics panel / bench output.
    pub writes: u64,
    pub reads: u64,
}

impl Controller {
    pub fn open() -> Result<Controller, String> {
        Ok(Controller { dev: Device::open()?, ready: false, writes: 0, reads: 0 })
    }

    pub fn path(&self) -> &str {
        self.dev.path()
    }

    /// The handshake the reference implementation repeats every frame.
    fn handshake(&mut self) {
        let _ = self.dev.read(0xb500, 5);
        self.reads += 1;
        for reg in [0x4000u16, 0x4001, 0x0244, 0xe0a1, 0xe7a1] {
            let _ = self.dev.read(reg, 1);
            self.reads += 1;
        }
        for reg in [0xe0a2u16, 0xe0a4, 0xe0a6, 0xe0a8, 0xe0aa] {
            let _ = self.dev.read(reg, 2);
            self.reads += 1;
        }
    }

    /// Status registers, for diagnostics (`rgb-tool read`).
    pub fn status(&mut self) -> String {
        if !self.ready {
            self.handshake();
            self.ready = true;
        }
        let mut out = String::new();
        for (reg, len) in [(0xb500u16, 5usize), (0x4000, 1), (0x4001, 1), (0x0244, 1), (0xe0a1, 1)]
        {
            match self.dev.read(reg, len) {
                Ok(v) => out.push_str(&format!(" 0x{reg:04X}={}", hex(&v))),
                Err(e) => out.push_str(&format!(" 0x{reg:04X}=ERR({e})")),
            }
        }
        for reg in [0xe0a2u16, 0xe0a4, 0xe0a6, 0xe0a8, 0xe0aa] {
            match self.dev.read(reg, 2) {
                Ok(v) => out.push_str(&format!(" 0x{reg:04X}={}", hex(&v))),
                Err(e) => out.push_str(&format!(" 0x{reg:04X}=ERR({e})")),
            }
        }
        out.trim_start().to_string()
    }

    /// Write one output's LED data and its tail. Exactly the reference
    /// sequence (`SendSingleDevice` + `sendControlCommit` + `sendCommitPacket`).
    ///
    /// `colors` must not exceed `out.leds` entries.
    pub fn write_leds(&mut self, out: &Output, colors: &[[u8; 3]]) -> Result<(), u32> {
        if !self.ready {
            self.handshake();
            self.ready = true;
        }
        let n = colors.len().min(out.leds);
        let mut bytes = Vec::with_capacity(n * 3);
        for c in &colors[..n] {
            bytes.extend_from_slice(c);
        }
        // the plugin reads 8 bytes here before every output write
        let _ = self.dev.read(0xe0a2, 8);
        self.reads += 1;

        self.dev.write(out.address, &bytes)?;
        self.dev.write(out.mode, &[0x01])?;
        self.dev.write(out.speed, &[0x00])?;
        self.dev.write(out.direction, &[0x00])?;
        self.dev.write(out.brightness, &[0x00])?;
        self.dev.write(out.commit, &[0x01])?;
        for reg in FRAME_COMMIT {
            self.dev.write(reg, &[0x01])?;
        }
        self.writes += 1;
        Ok(())
    }

    /// Read an output's LED buffer back. The controller allows this, which is
    /// what makes every write here self-verifying.
    pub fn read_leds(&self, out: &Output, leds: usize) -> Result<Vec<[u8; 3]>, u32> {
        let raw = self.dev.read(out.address, leds.min(out.leds) * 3)?;
        Ok(raw
            .chunks(3)
            .map(|c| [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)])
            .collect())
    }

    /// Hand the lights back to the controller's own effect: leave direct mode.
    ///
    /// The reference plugin never does this (it just stops updating); writing
    /// `0x00` to the mode registers is the documented-by-omission counterpart
    /// of the `0x01` that enters direct mode. Harmless either way — if the
    /// controller ignores it, the last written colours simply stay.
    pub fn release(&mut self) -> Result<(), u32> {
        for out in OUTPUTS {
            self.dev.write(out.mode, &[0x00])?;
        }
        self.ready = false;
        Ok(())
    }
}

fn hex(v: &[u8]) -> String {
    v.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join("")
}
