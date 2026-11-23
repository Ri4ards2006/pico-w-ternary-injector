//! PIO dual-rail ternary transmitter.
//!
//! One state machine pulls 32-bit words from its (joined, 8-deep) TX FIFO,
//! shifts them out right (LSB first) two bits at a time onto two consecutive
//! GPIOs (P rail, N rail) and generates a strobe on a side-set pin:
//!
//! ```text
//! cycle   0     1     2     3   | 0 ...
//! P/N   ==X=========================X== (changes on falling strobe edge)
//! STRB  ____________/‾‾‾‾‾‾‾‾‾‾\____    (FPGA samples on rising edge)
//! ```
//!
//! 4 SM cycles per trit: 2 cycles setup, 2 cycles hold around the rising edge.
//! When the FIFO runs dry, `out` stalls with STRB low, so the FPGA sees no
//! spurious edges and no zero-trits are injected between DMA blocks.

/// RP2040 system clock as configured by `embassy_rp::init(Default::default())`.
pub const SYS_CLK_HZ: u32 = 125_000_000;
/// State-machine cycles per emitted trit (2 instructions x (1 + 1 delay)).
pub const CYCLES_PER_TRIT: u32 = 4;
/// Integer clock divider: no fractional part -> zero cycle-to-cycle jitter.
/// 125 MHz / 5 = 25 MHz SM clock -> 6.25 MHz strobe / 6.25 MTrit/s.
pub const PIO_CLOCK_DIVIDER: u16 = 5;

/// SM clock for a given integer divider.
pub const fn sm_clock_hz(div: u16) -> u32 {
    SYS_CLK_HZ / div as u32
}

/// Trit (= strobe) rate for a given integer divider.
pub const fn trit_rate_hz(div: u16) -> u32 {
    sm_clock_hz(div) / CYCLES_PER_TRIT
}

/// Duration of one SM cycle in picoseconds.
pub const fn sm_cycle_ps(div: u16) -> u32 {
    (1_000_000_000_000u64 / sm_clock_hz(div) as u64) as u32
}

// The Tang Nano 9K samples the strobe with its 27 MHz crystal clock through a
// 2-FF synchroniser; each strobe phase must last >= 2 FPGA clocks (74 ns).
const _: () = assert!(2 * sm_cycle_ps(PIO_CLOCK_DIVIDER) >= 2 * 37_037);

/// Assemble the dual-rail program.
pub fn program() -> pio::Program<32> {
    pio::pio_asm!(
        ".side_set 1",
        ".wrap_target",
        "    out pins, 2   side 0 [1]", // present next trit, strobe low (2 cycles)
        "    nop           side 1 [1]", // strobe high: FPGA latches P/N (2 cycles)
        ".wrap",
    )
    .program
}

#[cfg(target_arch = "arm")]
pub use driver::TernaryInjector;

#[cfg(target_arch = "arm")]
mod driver {
    use embassy_rp::Peri;
    use embassy_rp::dma;
    use embassy_rp::gpio::Level;
    use embassy_rp::pio::{
        Common, Config, Direction, FifoJoin, Instance, PioPin, ShiftConfig, ShiftDirection, StateMachine,
    };

    use super::{PIO_CLOCK_DIVIDER, program};

    /// Owns one PIO state machine configured as a dual-rail ternary transmitter.
    pub struct TernaryInjector<'d, PIO: Instance, const SM: usize> {
        sm: StateMachine<'d, PIO, SM>,
    }

    impl<'d, PIO: Instance, const SM: usize> TernaryInjector<'d, PIO, SM> {
        /// `p_rail` and `n_rail` must be consecutive GPIOs (OUT base, base + 1).
        pub fn new(
            common: &mut Common<'d, PIO>,
            mut sm: StateMachine<'d, PIO, SM>,
            p_rail: Peri<'d, impl PioPin>,
            n_rail: Peri<'d, impl PioPin>,
            strobe: Peri<'d, impl PioPin>,
        ) -> Self {
            let prg = program();
            let loaded = common.load_program(&prg);
            let p = common.make_pio_pin(p_rail);
            let n = common.make_pio_pin(n_rail);
            let s = common.make_pio_pin(strobe);

            let mut cfg = Config::default();
            cfg.use_program(&loaded, &[&s]);
            cfg.set_out_pins(&[&p, &n]);
            cfg.clock_divider = PIO_CLOCK_DIVIDER.into();
            cfg.fifo_join = FifoJoin::TxOnly;
            cfg.shift_out = ShiftConfig {
                auto_fill: true,
                threshold: 32,
                direction: ShiftDirection::Right,
            };

            sm.set_config(&cfg);
            // Rails and strobe idle low (trit 0, no edge) before the SM starts.
            sm.set_pins(Level::Low, &[&p, &n, &s]);
            sm.set_pin_dirs(Direction::Out, &[&p, &n, &s]);
            sm.set_enable(true);
            Self { sm }
        }

        /// Stream `words` into the TX FIFO via DMA; resolves once the last word
        /// has been accepted by the FIFO (up to 8 words may still be shifting).
        pub async fn push(&mut self, dma: &mut dma::Channel<'_>, words: &[u32]) {
            self.sm.tx().dma_push(dma, words, false).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divider_yields_documented_rates() {
        assert_eq!(sm_clock_hz(PIO_CLOCK_DIVIDER), 25_000_000);
        assert_eq!(trit_rate_hz(PIO_CLOCK_DIVIDER), 6_250_000);
        assert_eq!(sm_cycle_ps(PIO_CLOCK_DIVIDER), 40_000);
    }

    #[test]
    fn program_is_two_instructions_with_one_sideset_bit() {
        let p = program();
        assert_eq!(p.code.len(), 2);
        assert_eq!(p.side_set.bits(), 1);
        assert!(!p.side_set.optional());
        assert_eq!(p.wrap.target, 0);
        assert_eq!(p.wrap.source, 1);
    }

    #[test]
    fn program_machine_code_matches_hand_assembly() {
        // OUT  = 011 | side(1b) delay(4b) | dest PINS=000 | bitcount=00010
        // NOP  = MOV Y,Y = 101 | side delay | dest Y=010 | op 00 | src Y=010
        let p = program();
        assert_eq!(p.code[0], 0b011_0_0001_000_00010); // 0x6102
        assert_eq!(p.code[1], 0b101_1_0001_010_00_010); // 0xB142
    }
}
