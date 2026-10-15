//! Nuvoton NCT6779D..NCT6799D SuperIO: detection (LPC PnP space) + hardware
//! monitor register access (banked EC space via ISA index/data ports).
//! Register maps ported from LibreHardwareMonitor Nct677X.cs / LpcIO.cs (0.9.5).

use crate::driver::Driver;

/// The application's channel count — the UI, the config and the control engine
/// are built around 7 channels. The detected chip may have fewer fans: those
/// extra channels have no hardware and their registers belong to something
/// else, so writes above [`Nct::fan_count`] are refused (see `set_control_raw`).
pub const FAN_COUNT: usize = 7;

/// One chip's register map. Adding a motherboard means adding data here, not
/// code: every access in this file goes through the spec.
pub struct ChipSpec {
    pub name: &'static str,
    /// false = recognised but must not be driven: either the fan registers are
    /// unknown/different (writing them could poke something else entirely) or
    /// two chips share the same id/revision and cannot be told apart.
    pub usable: bool,
    /// Why it is not usable — shown to the user verbatim.
    pub note: &'static str,
    pub fans: usize,
    /// 13-bit tach counters (bank 4)
    rpm: &'static [u16],
    /// current PWM output duty, read back (mirror register)
    pwm_out: &'static [u16],
    /// PWM duty command while in manual mode
    pwm_cmd: &'static [u16],
    /// fan control mode register (write 0 = manual/software)
    mode: &'static [u16],
    /// temperature slots: (value_reg, half_reg, half_bit, source_reg);
    /// half_bit < 0 = the register is whole-degree only
    temps: &'static [(u16, u16, i32, u16)],
    /// voltage registers, V = 0.008 x raw
    voltages: &'static [u16],
}

// ---------- shared by the whole NCT6779D..6799D family ----------
// LHM Nct677X.cs:175-184 (rpm/command/mode/voltage) and :78-80 (pwm mirrors).
static FAM_RPM: [u16; 7] = [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC];
static FAM_CMD: [u16; 7] = [0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09];
static FAM_MODE: [u16; 7] = [0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02];
/// PWM-output mirrors, older layout (6779D..6796DR, 6701D)
static PWM_OUT_A: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0x017, 0x029];
/// PWM-output mirrors, newer layout (6796DS, 6797D, 6798D, 6799D, 5585D)
static PWM_OUT_B: [u16; 7] = [0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09];
static FAM_VOLT: [u16; 16] = [
    0x480, 0x481, 0x482, 0x483, 0x484, 0x485, 0x486, 0x487, 0x488, 0x489, 0x48A, 0x48B, 0x48C,
    0x48D, 0x48E, 0x48F,
];

// ---------- temperature tables ----------
// A slot's real probe is resolved at runtime from its SOURCE register (slots
// are routable — the probe behind a slot is not fixed).
/// Groups A/B: 6791D/6792D/6793D/6795D/6796D/6796DR/6797D/6701D.
/// LHM Nct677X.cs:196-235 (A) and :242-277 (B) — identical for our purposes.
static TEMPS_8: [(u16, u16, i32, u16); 8] = [
    (0x073, 0x074, 7, 0x100),
    (0x075, 0x076, 7, 0x200),
    (0x077, 0x078, 7, 0x300),
    (0x079, 0x07A, 7, 0x800),
    (0x07B, 0x07C, 7, 0x900),
    (0x07D, 0x07E, 7, 0xA00),
    (0x4A0, 0x49E, 6, 0xB00),
    (0x027, 0x000, -1, 0x621),
];
/// Group D adds TSENSOR: 6798D/6799D. LHM Nct677X.cs:283-320.
static TEMPS_9: [(u16, u16, i32, u16); 9] = [
    (0x073, 0x074, 7, 0x100),
    (0x075, 0x076, 7, 0x200),
    (0x077, 0x078, 7, 0x300),
    (0x079, 0x07A, 7, 0x800),
    (0x07B, 0x07C, 7, 0x900),
    (0x07D, 0x07E, 7, 0xA00),
    (0x4A0, 0x49E, 6, 0xB00),
    (0x027, 0x000, -1, 0x621),
    (0x4A2, 0x4A1, 7, 0xC00),
];
/// The `default` group — NCT6779D and NCT6792D-A. Slot order AND source
/// registers differ from the 6796 table (PECI sits in 0x027 here, CPUTIN takes
/// the PECI slot), so they cannot share it. LHM Nct677X.cs:353-365.
static TEMPS_6779: [(u16, u16, i32, u16); 7] = [
    (0x027, 0x000, -1, 0x621),
    (0x073, 0x074, 7, 0x100),
    (0x075, 0x076, 7, 0x200),
    (0x077, 0x078, 7, 0x300),
    (0x079, 0x07A, 7, 0x800),
    (0x07B, 0x07C, 7, 0x900),
    (0x150, 0x151, 7, 0x622),
];

// ---------- chips we can drive ----------
macro_rules! spec {
    ($name:expr, $fans:expr, $pwm:expr, $temps:expr) => {
        ChipSpec {
            name: $name,
            usable: true,
            note: "",
            fans: $fans,
            rpm: &FAM_RPM,
            pwm_out: $pwm,
            pwm_cmd: &FAM_CMD,
            mode: &FAM_MODE,
            temps: $temps,
            voltages: &FAM_VOLT,
        }
    };
}

static SP_6779D: ChipSpec = spec!("NCT6779D", 5, &PWM_OUT_A, &TEMPS_6779);
static SP_6791D: ChipSpec = spec!("NCT6791D", 6, &PWM_OUT_A, &TEMPS_8);
static SP_6792D: ChipSpec = spec!("NCT6792D", 6, &PWM_OUT_A, &TEMPS_8);
static SP_6792DA: ChipSpec = spec!("NCT6792D-A", 6, &PWM_OUT_A, &TEMPS_6779);
static SP_6793D: ChipSpec = spec!("NCT6793D", 6, &PWM_OUT_A, &TEMPS_8);
static SP_6795D: ChipSpec = spec!("NCT6795D", 6, &PWM_OUT_A, &TEMPS_8);
static SP_6796D: ChipSpec = spec!("NCT6796D", 6, &PWM_OUT_A, &TEMPS_8);
static SP_6796DR: ChipSpec = spec!("NCT6796D-R", 7, &PWM_OUT_A, &TEMPS_8);
static SP_6797D: ChipSpec = spec!("NCT6797D", 7, &PWM_OUT_B, &TEMPS_8);
static SP_6798D: ChipSpec = spec!("NCT6798D", 7, &PWM_OUT_B, &TEMPS_9);
static SP_6701D: ChipSpec = spec!("NCT6701D", 7, &PWM_OUT_A, &TEMPS_8);

// ---------- recognised, but not drivable ----------
/// Two chips answer the same id/revision and only the motherboard model tells
/// them apart (LHM matches on a hard-coded board list, LpcIO.cs:374-486).
static SP_AMBIGUOUS: ChipSpec = ChipSpec {
    name: "未知型号（与其他芯片同 id/rev）",
    usable: false,
    note: "这颗芯片的 id/版本与另一颗不同芯片完全相同，只凭寄存器无法区分（需要主板型号白名单）。\
           两者风扇寄存器不同，写错可能戳到别的功能，所以这里不驱动。",
    fans: 0,
    rpm: &FAM_RPM,
    pwm_out: &PWM_OUT_A,
    pwm_cmd: &FAM_CMD,
    mode: &FAM_MODE,
    temps: &TEMPS_8,
    voltages: &FAM_VOLT,
};
static SP_EC_SPACE: ChipSpec = ChipSpec {
    name: "NCT668x（EC 空间访问）",
    usable: false,
    note: "这一代芯片的风扇/温度寄存器走 EC 空间（page/index/data，另一套读写协议），\
           且手动控速需要额外的握手时序。本程序目前只实现了 6779D..6799D 那套 bank 访问。",
    fans: 0,
    rpm: &FAM_RPM,
    pwm_out: &PWM_OUT_A,
    pwm_cmd: &FAM_CMD,
    mode: &FAM_MODE,
    temps: &TEMPS_8,
    voltages: &FAM_VOLT,
};
static SP_READONLY: ChipSpec = ChipSpec {
    name: "无风扇控制寄存器的老芯片",
    usable: false,
    note: "这颗芯片没有可写的风扇控制寄存器（只能读转速），本程序是控速工具，因此不驱动它。",
    fans: 0,
    rpm: &FAM_RPM,
    pwm_out: &PWM_OUT_A,
    pwm_cmd: &FAM_CMD,
    mode: &FAM_MODE,
    temps: &TEMPS_8,
    voltages: &FAM_VOLT,
};

/// (device id at config reg 0x20, revision at 0x21) -> spec.
/// `rev_mask` 0xFF = exact revision, otherwise `rev & mask == rev`.
/// Table taken from LHM LpcIO.cs:100-487 (`DetectWinbondFintek`).
struct ChipMatch {
    id: u8,
    rev: u8,
    rev_mask: u8,
    spec: &'static ChipSpec,
}

static CHIPS: &[ChipMatch] = &[
    ChipMatch { id: 0xB4, rev: 0x70, rev_mask: 0xF0, spec: &SP_READONLY }, // NCT6771F
    ChipMatch { id: 0xC3, rev: 0x30, rev_mask: 0xF0, spec: &SP_READONLY }, // NCT6776F
    ChipMatch { id: 0xC4, rev: 0x50, rev_mask: 0xF0, spec: &SP_READONLY }, // NCT610XD
    ChipMatch { id: 0xC5, rev: 0x60, rev_mask: 0xF0, spec: &SP_6779D },
    ChipMatch { id: 0xC7, rev: 0x32, rev_mask: 0xFF, spec: &SP_EC_SPACE }, // NCT6683D
    ChipMatch { id: 0xC8, rev: 0x03, rev_mask: 0xFF, spec: &SP_6791D },
    ChipMatch { id: 0xC9, rev: 0x11, rev_mask: 0xFF, spec: &SP_6792D },
    ChipMatch { id: 0xC9, rev: 0x13, rev_mask: 0xFF, spec: &SP_6792DA },
    ChipMatch { id: 0xD1, rev: 0x21, rev_mask: 0xFF, spec: &SP_6793D },
    ChipMatch { id: 0xD3, rev: 0x52, rev_mask: 0xFF, spec: &SP_6795D },
    ChipMatch { id: 0xD4, rev: 0x23, rev_mask: 0xFF, spec: &SP_6796D },
    ChipMatch { id: 0xD4, rev: 0x2A, rev_mask: 0xFF, spec: &SP_6796DR },
    ChipMatch { id: 0xD4, rev: 0x2B, rev_mask: 0xFF, spec: &SP_6798D },
    ChipMatch { id: 0xD4, rev: 0x51, rev_mask: 0xFF, spec: &SP_6797D },
    ChipMatch { id: 0xD4, rev: 0x40, rev_mask: 0xFF, spec: &SP_AMBIGUOUS }, // 6686D / 6796DS
    ChipMatch { id: 0xD4, rev: 0x41, rev_mask: 0xFF, spec: &SP_AMBIGUOUS },
    ChipMatch { id: 0xD5, rev: 0x92, rev_mask: 0xFF, spec: &SP_EC_SPACE },  // NCT6687D/R
    ChipMatch { id: 0xD8, rev: 0x02, rev_mask: 0xFF, spec: &SP_AMBIGUOUS }, // 6799D / 6796DS
    ChipMatch { id: 0xD8, rev: 0x06, rev_mask: 0xFF, spec: &SP_6701D },
];

fn lookup_chip(id: u8, rev: u8) -> Option<&'static ChipSpec> {
    CHIPS
        .iter()
        .find(|c| c.id == id && (c.rev_mask == 0xFF && c.rev == rev || c.rev_mask != 0xFF && rev & c.rev_mask == c.rev))
        .map(|c| c.spec)
}

/// Vendor ID registers: the same pair answers on every 6779D..6799D chip.
/// (The older 610XD and the EC-space 668x family use different ones — those
/// chips are recognised as unsupported before this is ever read.)
const VENDOR_ID_HIGH: u16 = 0x804F;
const VENDOR_ID_LOW: u16 = 0x004F;
const NUVOTON_VENDOR_ID: u16 = 0x5CA3;

/// Map a source-register value (low 5 bits) to the real probe name.
/// Applies to the whole 6779D..6799D family (LHM `SourceNct67Xxd`).
fn source_name(v: u8) -> Option<&'static str> {
    Some(match v & 0x1F {
        1 => "SYSTIN",
        2 => "CPUTIN",
        3 => "AUXTIN0",
        4 => "AUXTIN1",
        5 => "AUXTIN2",
        6 => "AUXTIN3",
        7 => "AUXTIN4",
        10 => "TSENSOR",
        16 => "PECI_0 (CPU)",
        17 => "PECI_1",
        20 => "PCH_CPU",
        31 => "VIRTUAL",
        _ => return None,
    })
}

/// Raw ISA port access — the only thing `Nct` needs from ring 0.
///
/// `driver::Driver` implements this on real hardware (through WinRing0 or
/// PawnIO). Tests implement it over a 256-byte-per-bank array, which is what
/// finally makes the register logic testable: before this, `Nct` held a
/// concrete `Driver`, so nothing below `detect()` could be exercised without
/// a kernel driver and administrator rights.
pub trait PortIo {
    fn read_io_port(&self, port: u16) -> u8;
    fn write_io_port(&self, port: u16, value: u8);
}

impl PortIo for Driver {
    fn read_io_port(&self, port: u16) -> u8 {
        Driver::read_io_port(self, port)
    }
    fn write_io_port(&self, port: u16, value: u8) {
        Driver::write_io_port(self, port, value)
    }
}
pub struct Nct<D: PortIo + Clone = Driver> {
    driver: D,
    /// EC base address (e.g. 0x290); index = base+5, data = base+6
    port: u16,
    /// Register map of the chip that answered detection.
    spec: &'static ChipSpec,
    /// Original (mode, pwm duty) per channel, captured before first override —
    /// used to hand control back to the BIOS.
    initial: std::sync::Mutex<[Option<(u8, u8)>; FAN_COUNT]>,
    /// Serializes whole register transactions: reads and writes both switch
    /// EC banks, so the poller thread and the control loop must not interleave.
    io: std::sync::Arc<std::sync::Mutex<()>>,
    /// Boot-static routing and slow display values (see [`SlowCache`]).
    slow: std::sync::Mutex<SlowCache>,
    /// Where this instance keeps its captured BIOS originals (see
    /// [`Nct::originals_path`]). An instance field rather than a global so
    /// tests can each use their own file — with one shared path, `boot_stamp()`
    /// is constant within a run and one test's captured originals would be
    /// picked up by the next, making the suite order-dependent.
    originals: std::path::PathBuf,
}

/// Everything that does not need re-reading every second.
///
/// The EC exposes one 256-byte page at a time through the same two ports, so
/// every register access costs "select bank + select register + read". The
/// temperature *source* registers live in banks 1..C on purpose (one per probe
/// route) — resolving them once instead of per pass removes eight bank switches
/// and eight reads from every temperature read.
#[derive(Default)]
struct SlowCache {
    /// Per slot: `None` = not resolved yet, `Some(None)` = routed to nothing.
    routes: Vec<Option<Option<&'static str>>>,
    /// Last voltage block, for the ticks that skip it.
    voltages: Vec<(String, f32)>,
}

/// One temperature slot as read this pass — including readings the `temps`
/// filter throws away, so the UI can show them greyed out.
#[derive(Clone, Debug)]
pub struct TempReading {
    pub name: String,
    pub celsius: f32,
    /// true = discarded by the filters in `read_all` (slot routed to no known
    /// probe, value out of range, or AUX/SYSTIN reading >90C).
    pub floating: bool,
}

#[derive(Default, Clone)]
pub struct NctReading {
    pub fans_rpm: [Option<f32>; FAN_COUNT],
    pub controls_pct: [Option<f32>; FAN_COUNT],
    pub temps: Vec<(String, f32)>,
    pub voltages: Vec<(String, f32)>,
    /// every temperature slot, floating (filtered-out) readings included
    pub detailed_temps: Vec<TempReading>,
}

impl Nct<Driver> {
    /// Probe both LPC entry ports (0x2E primary, 0x4E secondary) for a Nuvoton
    /// SuperIO chip we have a register map for, and return the configured
    /// instance. A chip we recognise but cannot drive is reported by name and
    /// reason instead of being poked with the wrong registers.
    pub fn detect(driver: Driver) -> Result<Nct<Driver>, String> {
        let mut seen: Vec<String> = Vec::new();
        for reg_port in [0x2Eu16, 0x4E] {
            let driver = driver.clone();
            let val_port = reg_port + 1;
            // PawnIO's LpcIO module has to be told which config port pair we
            // talk to (0x2E -> slot 0, 0x4E -> slot 1). It resets the module,
            // so it belongs before this port's config-mode entry.
            driver.pawnio_select_slot(if reg_port == 0x2E { 0 } else { 1 });
            // enter configuration mode (Winbond/Nuvoton 0x87 x2)
            driver.write_io_port(reg_port, 0x87);
            driver.write_io_port(reg_port, 0x87);

            // read chip id + revision from config space (LDN-independent)
            driver.write_io_port(reg_port, 0x20);
            let id = driver.read_io_port(val_port);
            driver.write_io_port(reg_port, 0x21);
            let rev = driver.read_io_port(val_port);

            let Some(spec) = lookup_chip(id, rev) else {
                driver.write_io_port(reg_port, 0xAA); // exit
                seen.push(format!("id 0x{id:02X} rev 0x{rev:02X}"));
                continue;
            };
            if !spec.usable {
                driver.write_io_port(reg_port, 0xAA);
                return Err(format!(
                    "检测到 {}（id 0x{id:02X} rev 0x{rev:02X}），但本程序不能驱动它：\n{}",
                    spec.name, spec.note
                ));
            }
            // The chip answered and we have a map for it: let the PawnIO module
            // discover the I/O BARs. Until it has, the module denies every
            // access outside the config port pair — including the EC register
            // window we are about to use (no-op on the WinRing0 backend).
            driver.pawnio_find_bars();
            // select logical device 0x0B (environment controller)
            driver.write_io_port(reg_port, 0x07);
            driver.write_io_port(val_port, 0x0B);
            // activate the logical device (register 0x30 bit0 = 1)
            driver.write_io_port(reg_port, 0x30);
            let activate = driver.read_io_port(val_port);
            eprintln!("[debug] LDN 0x0B activate register = 0x{activate:02X}");
            if activate & 1 == 0 {
                driver.write_io_port(val_port, activate | 0x01);
                eprintln!("[debug] activated LDN (wrote 0x{:02X})", activate | 0x01);
            }
            // read base address (BAR) twice and compare
            driver.write_io_port(reg_port, 0x60);
            let bar_lo = driver.read_io_port(val_port);
            driver.write_io_port(reg_port, 0x61);
            let bar_hi = driver.read_io_port(val_port);
            let bar = ((bar_hi as u16) << 8) | bar_lo as u16;
            // also try alternate BAR registers 0x62/0x63
            driver.write_io_port(reg_port, 0x62);
            let bar2_lo = driver.read_io_port(val_port);
            driver.write_io_port(reg_port, 0x63);
            let bar2_hi = driver.read_io_port(val_port);
            let bar2 = ((bar2_hi as u16) << 8) | bar2_lo as u16;
            eprintln!("[debug] BAR0(0x60/61)=0x{bar:04X}  BAR1(0x62/63)=0x{bar2:04X}");
            driver.write_io_port(reg_port, 0x60);
            let bar_lo = driver.read_io_port(val_port);
            driver.write_io_port(reg_port, 0x61);
            let bar_hi = driver.read_io_port(val_port);
            let bar = ((bar_hi as u16) << 8) | bar_lo as u16;
            std::thread::sleep(std::time::Duration::from_millis(1));
            driver.write_io_port(reg_port, 0x60);
            let bar_lo2 = driver.read_io_port(val_port);
            driver.write_io_port(reg_port, 0x61);
            let bar_hi2 = driver.read_io_port(val_port);
            let verify = ((bar_hi2 as u16) << 8) | bar_lo2 as u16;
            // disable the IO space lock (register 0x28 bit 4)
            driver.write_io_port(reg_port, 0x28);
            let options = driver.read_io_port(val_port);
            if options & 0x10 != 0 {
                driver.write_io_port(val_port, options & !0x10);
            }
            // exit configuration mode
            driver.write_io_port(reg_port, 0xAA);

            if bar != verify {
                return Err(format!(
                    "BAR verification failed: 0x{bar:04X} != 0x{verify:04X}"
                ));
            }
            let port = if bar & 0x07 == 0x05 { bar & 0xFFF8 } else { bar };
            eprintln!("[debug] reg_port=0x{reg_port:02X} id=0x{id:02X} rev=0x{rev:02X} bar=0x{bar:04X} port=0x{port:04X}");
            let nct = Nct { driver: driver.clone(), port, spec, initial: std::sync::Mutex::new(std::array::from_fn(|_| None)), io: std::sync::Arc::new(std::sync::Mutex::new(())), slow: Default::default(), originals: std::path::PathBuf::new() };
            let vid_hi = nct.read_reg(VENDOR_ID_HIGH);
            let vid_lo = nct.read_reg(VENDOR_ID_LOW);
            let vid = (vid_hi as u16) << 8 | vid_lo as u16;
            eprintln!("[debug] vendor hi=0x{vid_hi:02X} lo=0x{vid_lo:02X} -> 0x{vid:04X}");
            if nct.vendor_ok() {
                return Ok(nct);
            }
            // BAR didn't work — try common Nuvoton EC base addresses
            eprintln!("[debug] BAR-based vendor check failed, trying common bases...");
            for try_port in [0x290u16, 0xA00, 0x800, 0x500, 0x250, 0xA20, 0x228, 0xFF00] {
                let nct2 = Nct { driver: driver.clone(), port: try_port, spec, initial: std::sync::Mutex::new(std::array::from_fn(|_| None)), io: std::sync::Arc::new(std::sync::Mutex::new(())), slow: Default::default(), originals: std::path::PathBuf::new() };
                let vid_hi = nct2.read_reg(VENDOR_ID_HIGH);
                let vid_lo = nct2.read_reg(VENDOR_ID_LOW);
                let vid = (vid_hi as u16) << 8 | vid_lo as u16;
                eprintln!("[debug]   try 0x{try_port:04X}: vid=0x{vid:04X}");
                if vid == NUVOTON_VENDOR_ID {
                    eprintln!("[debug]   FOUND at 0x{try_port:04X}");
                    return Ok(nct2);
                }
            }
            // try bar2 too
            let port2 = if bar2 & 0x07 == 0x05 { bar2 & 0xFFF8 } else { bar2 };
            if port2 != port && port2 != 0 {
                let nct3 = Nct { driver, port: port2, spec, initial: std::sync::Mutex::new(std::array::from_fn(|_| None)), io: std::sync::Arc::new(std::sync::Mutex::new(())), slow: Default::default(), originals: std::path::PathBuf::new() };
                if nct3.vendor_ok() {
                    return Ok(nct3);
                }
            }
        }
        Err(format!(
            "0x2E/0x4E 上都没有找到支持的 Nuvoton 芯片。读到的 id/rev：{}",
            if seen.is_empty() {
                "无响应".to_string()
            } else {
                seen.join(" / ")
            }
        ))
    }

}

impl<D: PortIo + Clone> Nct<D> {    /// Register read with a bank cache.
    ///
    /// Selecting the bank is half of what one register costs, so callers that
    /// touch many registers keep a cache and walk their accesses in bank order
    /// (see `read_all`). Single accesses pass `&mut None` and pay the full
    /// price, which is what the old code did for every register.
    #[inline]
    fn read_reg_cached(&self, addr: u16, bank: &mut Option<u8>) -> u8 {
        let idx = self.port + 5;
        let dat = self.port + 6;
        let want = (addr >> 8) as u8;
        if *bank != Some(want) {
            self.driver.write_io_port(idx, 0x4E);
            self.driver.write_io_port(dat, want);
            *bank = Some(want);
        }
        self.driver.write_io_port(idx, (addr & 0xFF) as u8);
        self.driver.read_io_port(dat)
    }

    #[inline]
    fn write_reg_cached(&self, addr: u16, value: u8, bank: &mut Option<u8>) {
        let idx = self.port + 5;
        let dat = self.port + 6;
        let want = (addr >> 8) as u8;
        if *bank != Some(want) {
            self.driver.write_io_port(idx, 0x4E);
            self.driver.write_io_port(dat, want);
            *bank = Some(want);
        }
        self.driver.write_io_port(idx, (addr & 0xFF) as u8);
        self.driver.write_io_port(dat, value);
    }

    #[inline]
    fn read_reg(&self, addr: u16) -> u8 {
        self.read_reg_cached(addr, &mut None)
    }

    #[inline]
    fn write_reg(&self, addr: u16, value: u8) {
        self.write_reg_cached(addr, value, &mut None)
    }

    /// Probe behind a temperature slot, cached after the first read: the route
    /// registers are set by the BIOS at boot and never change while we run.
    fn probe_of(&self, slot: usize, src_reg: u16, bank: &mut Option<u8>) -> Option<&'static str> {
        if let Ok(c) = self.slow.lock() {
            if let Some(known) = c.routes.get(slot).copied().flatten() {
                return known;
            }
        }
        let resolved = source_name(self.read_reg_cached(src_reg, bank));
        if let Ok(mut c) = self.slow.lock() {
            if c.routes.len() <= slot {
                c.routes.resize(slot + 1, None);
            }
            c.routes[slot] = Some(resolved);
        }
        resolved
    }

    /// The temperature block, in bank order so the page changes at most once.
    fn read_temp_block(&self, bank: &mut Option<u8>) -> (Vec<(String, f32)>, Vec<TempReading>) {
        let mut temps: Vec<(String, f32)> = Vec::new();
        let mut detailed: Vec<TempReading> = Vec::new();
        // dedupe by probe: first slot wins
        let mut seen: Vec<&str> = Vec::new();
        // display list dedupes independently: two slots routed to the same
        // probe (dead AUX/SYS pins) must not render twice
        let mut seen_disp: Vec<String> = Vec::new();

        let mut order: Vec<usize> = (0..self.spec.temps.len()).collect();
        order.sort_by_key(|&i| self.spec.temps[i].0 >> 8);

        for slot in order {
            let (reg, half_reg, half_bit, src_reg) = self.spec.temps[slot];
            let raw = (self.read_reg_cached(reg, bank) as i8 as i32) << 1;
            let mut value = raw;
            if half_bit > 0 && half_reg != 0 {
                value |= ((self.read_reg_cached(half_reg, bank) as i32) >> half_bit) & 1;
            }
            let t = 0.5 * value as f32;
            let in_range = (-55.0..=125.0).contains(&t);
            let probe = self.probe_of(slot, src_reg, bank);
            // diagnostic row: name unknown routes explicitly instead of
            // dropping them silently
            let name = match probe {
                Some(n) => n.to_string(),
                None => format!("未知路由 #{slot}"),
            };
            // floating pins (unpopulated AUX/SYS probes on budget boards) read
            // bogus "legal" values; >90C on an ambient probe is impossible.
            let floating = probe.is_none()
                || !in_range
                || matches!(probe, Some(n) if (n.starts_with("AUXTIN") || n == "SYSTIN") && t > 90.0);
            if !seen_disp.contains(&name) {
                seen_disp.push(name.clone());
                detailed.push(TempReading {
                    name,
                    celsius: t,
                    floating,
                });
            }
            if !in_range {
                continue;
            }
            let name = match probe {
                Some(n) => n,
                None => continue, // slot routed to unused/reserved source
            };
            if seen.contains(&name) {
                continue; // same probe already taken from an earlier slot
            }
            if (name.starts_with("AUXTIN") || name == "SYSTIN") && t > 90.0 {
                continue;
            }
            seen.push(name);
            temps.push((name.to_string(), t));
        }
        (temps, detailed)
    }

    /// Temperatures only — what the control loop needs. Skips the PWM mirrors,
    /// the tachometers and the voltages, which is most of the port traffic.
    pub fn read_temps(&self) -> Vec<(String, f32)> {
        let _io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        let mut bank: Option<u8> = None;
        self.read_temp_block(&mut bank).0
    }

    /// Bulk read for the panel. `slow` also refreshes the voltage block, which
    /// is display-only and a third of the reads.
    pub fn read_all(&self, slow: bool) -> NctReading {
        let _io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        let mut r = NctReading::default();
        let mut bank: Option<u8> = None;

        // bank 0: the PWM mirrors
        for i in 0..self.spec.fans {
            r.controls_pct[i] = Some(self.read_reg_cached(self.spec.pwm_out[i], &mut bank) as f32 / 2.55);
        }

        // temperatures (bank-ordered inside)
        let (temps, detailed) = self.read_temp_block(&mut bank);
        r.temps = temps;
        r.detailed_temps = detailed;

        // bank 4: 13-bit tachometers
        for i in 0..self.spec.fans {
            let reg = self.spec.rpm[i];
            let high = self.read_reg_cached(reg, &mut bank) as u32;
            let low = self.read_reg_cached(reg + 1, &mut bank) as u32;
            let count = (high << 5) | (low & 0x1F);
            r.fans_rpm[i] = if count >= 0x1FFF {
                Some(0.0)
            } else if count >= 0x15 {
                Some(1_350_000.0 / count as f32)
            } else {
                None
            };
        }

        // bank 4: voltages — refreshed on the slow tick, reused in between
        if slow {
            let mut v = Vec::with_capacity(self.spec.voltages.len());
            for (i, reg) in self.spec.voltages.iter().enumerate() {
                let volts = 0.008 * self.read_reg_cached(*reg, &mut bank) as f32;
                if volts > 0.0 {
                    v.push((format!("Vin #{i}"), volts));
                }
            }
            if let Ok(mut c) = self.slow.lock() {
                c.voltages = v.clone();
            }
            r.voltages = v;
        } else if let Ok(c) = self.slow.lock() {
            r.voltages = c.voltages.clone();
        }

        r
    }

    pub fn vendor_ok(&self) -> bool {
        ((self.read_reg(VENDOR_ID_HIGH) as u16) << 8 | self.read_reg(VENDOR_ID_LOW) as u16)
            == NUVOTON_VENDOR_ID
    }

    pub fn base_port(&self) -> u16 {
        self.port
    }

    /// How many fans the detected chip actually has. Channels at or above this
    /// index have no hardware: their registers belong to other functions.
    pub fn fan_count(&self) -> usize {
        self.spec.fans
    }

    /// Chip name of the detected spec (for the UI / logs).
    pub fn chip_name(&self) -> &'static str {
        self.spec.name
    }

    /// A second handle to the same kernel driver. `detect` consumes the caller's
    /// `Driver`; readers that are not SuperIO transactions (e.g.
    /// `cpu::read_core_temps`) need one of their own.
    pub fn driver(&self) -> D {
        self.driver.clone()
    }

    /// Switch channel to manual (software) mode and set duty 0-255.
    /// Captures the BIOS's original control registers on first override so
    /// `restore_auto` can hand the channel back exactly as it was.
    pub fn set_control_raw(&self, index: usize, duty: u8) -> Result<(), String> {
        if index >= self.spec.fans {
            // Refusing here is a safety property, not an error path: above the
            // chip's fan count those registers belong to other functions.
            return Err(format!(
                "通道 {index} 在 {} 上不存在（本机共 {} 个风扇通道）",
                self.spec.name, self.spec.fans
            ));
        }
        let _io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        // Prefer a persisted original from an earlier process: this channel may
        // already be in software mode (previous run, previous build), in which
        // case capturing now would record our own duty as the "BIOS original".
        self.load_originals();
        {
            let mut g = self.initial.lock().unwrap_or_else(|e| e.into_inner());
            if g[index].is_none() {
                let m = self.read_reg(self.spec.mode[index]);
                let p = self.read_reg(self.spec.pwm_cmd[index]);
                g[index] = Some((m, p));
                let snapshot = *g;
                self.persist_originals(&snapshot);
            }
        }
        self.write_reg(self.spec.mode[index], 0);
        self.write_reg(self.spec.pwm_cmd[index], duty);
        Ok(())
    }

    /// Hand the channel back to BIOS control by restoring the registers that
    /// were active before our first override. Returns false if we never
    /// touched this channel (nothing to restore).
    pub fn restore_auto(&self, index: usize) -> bool {
        if index >= self.spec.fans {
            return false;
        }
        let _io = self.io.lock().unwrap_or_else(|e| e.into_inner());
        self.load_originals();
        let mut g = self.initial.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((m, p)) = g[index] {
            self.write_reg(self.spec.mode[index], m);
            self.write_reg(self.spec.pwm_cmd[index], p);
            g[index] = None;
            let snapshot = *g;
            // Keep the sidecar in sync, so a later process does not "restore" a
            // channel that is already back on the BIOS.
            self.persist_originals(&snapshot);
            true
        } else {
            false
        }
    }

    pub fn set_duty_pct(&self, index: usize, pct: f32) -> Result<(), String> {
        let duty = ((pct.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u8;
        self.set_control_raw(index, duty)
    }

    /// Where the captured BIOS originals live. Next to the exe like the config:
    /// the in-memory copy dies with the process while the chip stays in software
    /// mode, so without this file a restart (or the successor instance) could no
    /// longer hand a channel back to the BIOS.
    fn originals_path(&self) -> std::path::PathBuf {
        if !self.originals.as_os_str().is_empty() {
            return self.originals.clone();
        }
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.join("fan-tune-originals.json")))
            .unwrap_or_else(|| std::path::PathBuf::from("fan-tune-originals.json"))
    }

    /// Fill cache slots that are still empty from the sidecar. Best effort: a
    /// missing, unreadable or stale file just means "no originals known yet".
    fn load_originals(&self) {
        let Ok(text) = std::fs::read_to_string(self.originals_path()) else {
            return;
        };
        let Ok(saved) = serde_json::from_str::<OriginalsFile>(&text) else {
            return;
        };
        // Recorded in an earlier boot session: the BIOS has re-initialized the
        // chip since, so the live registers are a better source than a record
        // that may have caught our own leftover software mode.
        if saved.boot != boot_stamp() {
            return;
        }
        let Ok(mut g) = self.initial.lock() else { return };
        for (i, e) in saved.orig.into_iter().enumerate().take(FAN_COUNT) {
            if g[i].is_none() {
                g[i] = e;
            }
        }
    }

    fn persist_originals(&self, g: &[Option<(u8, u8)>; FAN_COUNT]) {
        let file = OriginalsFile {
            boot: boot_stamp(),
            orig: g.to_vec(),
        };
        if let Ok(text) = serde_json::to_string(&file) {
            let _ = std::fs::write(self.originals_path(), text);
        }
    }
}

/// The captured BIOS fan registers plus the boot they were captured in.
#[derive(serde::Serialize, serde::Deserialize)]
struct OriginalsFile {
    /// Boot instant (unix seconds). The BIOS re-initializes the chip at POST, so
    /// anything recorded before this boot describes a state that no longer
    /// exists — and it may well be our own leftover software mode, captured
    /// after a previous run had already taken the channel over.
    boot: u64,
    orig: Vec<Option<(u8, u8)>>,
}

/// Unix seconds of the last boot = now − uptime.
fn boot_stamp() -> u64 {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetTickCount64() -> u64;
    }
    let uptime = unsafe { GetTickCount64() } / 1000;
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(uptime))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tables this file hard-coded before it learned other chips. The
    /// NCT6796D-R profile must still reproduce them byte for byte — that is the
    /// evidence that adding chip profiles did not change behaviour on the
    /// machine this was developed on.
    #[test]
    fn nct6796dr_profile_matches_the_original_tables() {
        let s = &SP_6796DR;
        assert!(s.usable);
        assert_eq!(s.name, "NCT6796D-R");
        assert_eq!(s.fans, 7);
        assert_eq!(
            s.rpm,
            [0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC]
        );
        assert_eq!(
            s.pwm_out,
            [0x001, 0x003, 0x011, 0x013, 0x015, 0x017, 0x029]
        );
        assert_eq!(
            s.pwm_cmd,
            [0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09]
        );
        assert_eq!(
            s.mode,
            [0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02]
        );
        assert_eq!(
            s.temps,
            [
                (0x073, 0x074, 7, 0x100),
                (0x075, 0x076, 7, 0x200),
                (0x077, 0x078, 7, 0x300),
                (0x079, 0x07A, 7, 0x800),
                (0x07B, 0x07C, 7, 0x900),
                (0x07D, 0x07E, 7, 0xA00),
                (0x4A0, 0x49E, 6, 0xB00),
                (0x027, 0x000, -1, 0x621),
            ]
        );
        assert_eq!(s.voltages.len(), 16);
        assert_eq!(s.voltages[0], 0x480);
        assert_eq!(s.voltages[15], 0x48F);
    }

    /// Every drivable profile must have a register for each of its fans,
    /// otherwise a write would index past the table (or, worse, hit the wrong
    /// register). Also: an unusable chip must explain itself.
    #[test]
    fn profiles_are_internally_consistent() {
        for m in CHIPS {
            let s = m.spec;
            assert!(!s.name.is_empty());
            if !s.usable {
                assert!(
                    !s.note.is_empty(),
                    "{} is not usable but has no explanation",
                    s.name
                );
                continue;
            }
            assert!(s.note.is_empty());
            assert!(s.fans > 0 && s.fans <= FAN_COUNT);
            for (what, len) in [
                ("rpm", s.rpm.len()),
                ("pwm_out", s.pwm_out.len()),
                ("pwm_cmd", s.pwm_cmd.len()),
                ("mode", s.mode.len()),
            ] {
                assert!(
                    len >= s.fans,
                    "{}: {what} has {len} registers for {} fans",
                    s.name,
                    s.fans
                );
            }
            assert!(s.temps.len() >= 7, "{}: too few temperature slots", s.name);
            assert_eq!(s.voltages.len(), 16);
        }
    }

    /// Someone else's machine: the chip is not in the table at all. `detect`
    /// must return None (so it reports "no supported chip" and exits) rather
    /// than falling back to some default register map — writing a real chip's
    /// registers to unknown hardware is the one failure this must never have.
    #[test]
    fn unknown_chips_are_not_guessed() {
        // ids we have never heard of, and a known id with an unknown revision
        for (id, rev) in [(0x00u8, 0x00u8), (0xFF, 0xFF), (0xA0, 0x11), (0xD4, 0x99)] {
            assert!(
                lookup_chip(id, rev).is_none(),
                "id 0x{id:02X} rev 0x{rev:02X} must not resolve to a register map"
            );
        }
    }

    /// The chips we recognise but refuse to drive must say why, in the user's
    /// language, because `detect` surfaces `note` verbatim in a dialog.
    #[test]
    fn refused_chips_explain_themselves() {
        for (id, rev, why) in [
            (0xD4u8, 0x40u8, "ambiguous / same id-rev as another chip"),
            (0xD5, 0x92, "NCT668x EC-space access"),
            (0xD8, 0x02, "ambiguous / same id-rev as another chip"),
            (0xC7, 0x32, "NCT6683D EC-space access"),
            (0xB4, 0x70, "read-only, no writable fan registers"),
        ] {
            let s = lookup_chip(id, rev).unwrap_or_else(|| panic!("{id:02X}/{rev:02X} unresolved"));
            assert!(!s.usable, "{why} should be refused");
            assert!(!s.note.is_empty(), "{why} must explain itself to the user");
            assert_eq!(s.fans, 0, "{why} must expose no fans to drive");
        }
    }
    /// Id/revision matching, including the masked revisions (NCT6779D answers
    /// with a whole 0x6x range) and the two chips we deliberately refuse.
    #[test]
    fn chip_matching_by_id_and_revision() {
        assert_eq!(lookup_chip(0xD4, 0x2A).unwrap().name, "NCT6796D-R");
        assert_eq!(lookup_chip(0xD4, 0x23).unwrap().name, "NCT6796D");
        assert_eq!(lookup_chip(0xD4, 0x2B).unwrap().name, "NCT6798D");
        assert_eq!(lookup_chip(0xD4, 0x51).unwrap().name, "NCT6797D");
        assert_eq!(lookup_chip(0xC5, 0x6F).unwrap().name, "NCT6779D");
        assert_eq!(lookup_chip(0xC5, 0x70).map(|s| s.name), None);
        assert_eq!(lookup_chip(0xD8, 0x06).unwrap().name, "NCT6701D");
        // same id/rev as another chip: recognised, refused with a reason
        assert!(!lookup_chip(0xD4, 0x40).unwrap().usable);
        assert!(!lookup_chip(0xD5, 0x92).unwrap().usable);
        assert!(!lookup_chip(0xD8, 0x02).unwrap().usable);
        // a fan count of 5 or 6 keeps the unused channels out of reach
        assert_eq!(lookup_chip(0xC5, 0x60).unwrap().fans, 5);
        assert_eq!(lookup_chip(0xC8, 0x03).unwrap().fans, 6);
        // nothing at all
        assert!(lookup_chip(0x00, 0x00).is_none());
        assert!(lookup_chip(0xFF, 0xFF).is_none());
    }
}

#[cfg(test)]
mod hw_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    /// A simulated NCT6796D-R EC space: 16 banks of 256 bytes, reached
    /// through the index/data port pair exactly like the real chip.
    ///
    /// This is what `PortIo` bought: the bank cache, the BIOS hand-back and
    /// the out-of-range guard can now be exercised without a kernel driver.
    struct FakeEc {
        banks: Arc<Mutex<HashMap<u8, [u8; 256]>>>,
        cur_bank: Arc<Mutex<Option<u8>>>,
        idx_reg: Arc<Mutex<u8>>,
        /// counts every real port operation, so a cache regression shows up
        /// as a number, not as a guess
        ops: Arc<AtomicUsize>,
        base: u16,
    }

    impl FakeEc {
        fn new(base: u16) -> Self {
            FakeEc {
                banks: Arc::new(Mutex::new(HashMap::new())),
                cur_bank: Arc::new(Mutex::new(None)),
                idx_reg: Arc::new(Mutex::new(0)),
                ops: Arc::new(AtomicUsize::new(0)),
                base,
            }
        }
        fn set(&self, addr: u16, v: u8) {
            let mut b = self.banks.lock().unwrap();
            b.entry((addr >> 8) as u8).or_insert([0; 256])[(addr & 0xFF) as usize] = v;
        }
        fn get(&self, addr: u16) -> u8 {
            let b = self.banks.lock().unwrap();
            b.get(&((addr >> 8) as u8))
                .map(|p| p[(addr & 0xFF) as usize])
                .unwrap_or(0)
        }
    }

    impl Clone for FakeEc {
        fn clone(&self) -> Self {
            FakeEc {
                banks: self.banks.clone(),
                cur_bank: self.cur_bank.clone(),
                idx_reg: self.idx_reg.clone(),
                ops: self.ops.clone(),
                base: self.base,
            }
        }
    }

    impl PortIo for FakeEc {
        fn read_io_port(&self, port: u16) -> u8 {
            self.ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if port == self.base + 6 {
                let bank = *self.cur_bank.lock().unwrap();
                let idx = *self.idx_reg.lock().unwrap();
                let b = self.banks.lock().unwrap();
                b.get(&bank.unwrap_or(0)).map(|p| p[idx as usize]).unwrap_or(0)
            } else {
                0
            }
        }
        fn write_io_port(&self, port: u16, value: u8) {
            self.ops.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if port == self.base + 5 {
                *self.idx_reg.lock().unwrap() = value;
            } else if port == self.base + 6 {
                let idx = *self.idx_reg.lock().unwrap();
                if idx == 0x4E {
                    *self.cur_bank.lock().unwrap() = Some(value);
                } else {
                    let bank = *self.cur_bank.lock().unwrap();
                    let mut b = self.banks.lock().unwrap();
                    b.entry(bank.unwrap_or(0)).or_insert([0; 256])[idx as usize] = value;
                }
            }
        }
    }

    /// `Nct` can only be built by `detect`, which needs a real chip. These
    /// tests go through the private constructor the same way `detect` does.
    fn nct_on(ec: FakeEc, base: u16) -> Nct<FakeEc> {
        // Each instance gets its own sidecar, so no test can observe another's
        // captured originals.
        let path = std::env::temp_dir().join(format!(
            "fantune-originals-{}-{}.json",
            std::process::id(),
            // One number per instance, drawn from a process-wide sequence.
            {
                static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            }
        ));
        let _ = std::fs::remove_file(&path);
        Nct {
            driver: ec,
            port: base,
            spec: &SP_6796DR,
            initial: std::sync::Mutex::new(std::array::from_fn(|_| None)),
            io: std::sync::Arc::new(std::sync::Mutex::new(())),
            slow: Default::default(),
            originals: path,
        }
    }

    /// The bank cache is the single biggest port-traffic saving in the file.
    /// Two registers in the same bank must cost one bank switch, not two.
    #[test]
    fn consecutive_reads_in_one_bank_switch_the_bank_once() {
        let ec = FakeEc::new(0x290);
        ec.set(0x073, 60);
        ec.set(0x075, 70);
        let nct = nct_on(ec.clone(), 0x290);
        let ops_before = ec.ops.load(std::sync::atomic::Ordering::Relaxed);
        let mut bank = None;
        let a = nct.read_reg_cached(0x073, &mut bank);
        let b = nct.read_reg_cached(0x075, &mut bank);
        assert_eq!((a, b), (60, 70), "both registers must read back");
        // 2 reads x (idx + data) + 1 bank select (idx + data) = 6
        let used = ec.ops.load(std::sync::atomic::Ordering::Relaxed) - ops_before;
        assert_eq!(used, 6, "one bank switch for two reads in the same bank: {used}");
    }

    /// Crossing a bank must switch it again — the cache must not lie.
    #[test]
    fn crossing_banks_switches_again() {
        let ec = FakeEc::new(0x290);
        ec.set(0x073, 60); // bank 0
        ec.set(0x4B0, 12); // bank 4
        let nct = nct_on(ec.clone(), 0x290);
        let mut bank = None;
        assert_eq!(nct.read_reg_cached(0x073, &mut bank), 60);
        assert_eq!(nct.read_reg_cached(0x4B0, &mut bank), 12);
    }

    /// The safety property that matters most: a channel above the chip's fan
    /// count must be refused, because those registers belong to something else.
    #[test]
    fn writes_above_the_fan_count_are_refused() {
        let ec = FakeEc::new(0x290);
        let nct = nct_on(ec, 0x290);
        assert_eq!(SP_6796DR.fans, 7);
        assert!(nct.set_control_raw(7, 128).is_err());
        assert!(nct.set_control_raw(0, 128).is_ok());
    }

    /// A 5-fan chip (NCT6779D) must refuse channels 5 and 6 even though the
    /// config always carries 7. `default_with_7` builds seven channels what-
    /// ever the chip is, so the guard is the only thing keeping a write away
    /// from registers that belong to another function.
    #[test]
    fn a_five_fan_chip_refuses_channels_5_and_6() {
        let ec = FakeEc::new(0x290);
        let mut nct = Nct {
            driver: ec.clone(),
            port: 0x290,
            spec: &SP_6779D,
            initial: std::sync::Mutex::new(std::array::from_fn(|_| None)),
            io: std::sync::Arc::new(std::sync::Mutex::new(())),
            slow: Default::default(),
            originals: std::path::PathBuf::new(),
        };
        nct.originals = std::env::temp_dir().join(format!(
            "fantune-originals-5fan-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&nct.originals);
        assert_eq!(SP_6779D.fans, 5);
        assert_eq!(nct.fan_count(), 5);
        for i in 0..5 {
            assert!(nct.set_control_raw(i, 128).is_ok(), "channel {i} is real");
        }
        for i in 5..7 {
            assert!(nct.set_control_raw(i, 128).is_err(), "channel {i} has no hardware");
            assert!(!nct.restore_auto(i), "nothing to hand back on channel {i}");
        }
        // and the registers behind the phantom channels were never touched
        assert_eq!(ec.get(FAM_MODE[5]), 0);
        assert_eq!(ec.get(FAM_CMD[6]), 0);
    }
    /// Taking over a channel and handing it back must leave the registers
    /// exactly as the BIOS had them — this is the "don't leave the chip in
    /// software mode" promise the README makes.
    #[test]
    fn restore_auto_writes_back_the_captured_registers() {
        let ec = FakeEc::new(0x290);
        // BIOS state: mode register = 0x81 (automatic), duty = 0x33
        ec.set(FAM_MODE[0], 0x81);
        ec.set(FAM_CMD[0], 0x33);
        let nct = nct_on(ec.clone(), 0x290);
        assert!(nct.set_control_raw(0, 200).is_ok());
        // software mode + our duty
        assert_eq!(ec.get(FAM_MODE[0]), 0);
        assert_eq!(ec.get(FAM_CMD[0]), 200);
        assert!(nct.restore_auto(0));
        assert_eq!(ec.get(FAM_MODE[0]), 0x81, "BIOS mode restored");
        assert_eq!(ec.get(FAM_CMD[0]), 0x33, "BIOS duty restored");
    }

    /// Handing back a channel we never touched must be a no-op, not a write
    /// of zeros into the control registers.
    #[test]
    fn restore_auto_on_an_untouched_channel_does_nothing() {
        let ec = FakeEc::new(0x290);
        ec.set(FAM_MODE[0], 0x81);
        ec.set(FAM_CMD[0], 0x33);
        let nct = nct_on(ec.clone(), 0x290);
        assert!(!nct.restore_auto(0));
        assert_eq!(ec.get(FAM_MODE[0]), 0x81);
        assert_eq!(ec.get(FAM_CMD[0]), 0x33);
    }
}
