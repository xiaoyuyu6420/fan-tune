//! Library surface of fan-tune.
//!
//! Only the light (RGB) subsystem lives here for now: it has to be reachable
//! from two binaries — the GUI (`fan-tune.exe`, which carries a
//! requireAdministrator manifest) and the standalone `rgb-tool.exe` used for
//! calibration, which must run *without* elevation so it can be started from a
//! normal shell. Everything else stays inside the GUI binary.

pub mod rgb;
