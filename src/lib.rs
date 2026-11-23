//! Host-testable core of the Pico W ternary injector.
//!
//! * [`wire`]      – 2B1T dual-rail wire format (identical to `trich-codec`).
//! * [`pio_logic`] – PIO program, clock-divider math and (on ARM) the
//!   state-machine driver that streams 32-bit words onto the dual rails.
#![cfg_attr(not(test), no_std)]

pub mod pio_logic;
pub mod wire;
