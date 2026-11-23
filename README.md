# pico-w-ternary-injector

Wireless high-speed bridge for the **Tang Nano 9K** ternary accelerator.
A **Raspberry Pi Pico W** joins the local WiFi, accepts a TCP stream of
2B1T-packed neural-network weights and replays them — deterministically,
via DMA and a **PIO state machine** — as **dual-rail ternary signals**
(P rail, N rail, strobe) into the FPGA.

```
 host (trich-codec)        Pico W (RP2040 + CYW43439)                    Tang Nano 9K
┌──────────────┐  WiFi  ┌─────────┐  ┌──────────┐  DMA  ┌───────────┐ P ┌───────────┐
│ pack 2B1T +  │──TCP──▶│ cyw43 + │─▶│ 4×2 KiB  │──────▶│ PIO1 SM0  │──▶│ 2-FF sync │
│ sync / CRC   │ :7878  │ emb-net │  │ blk queue│       │ out pins,2│ N │ + systolic│
└──────────────┘        └─────────┘  └──────────┘       │ side strb │──▶│   array   │
                                                        └───────────┘ S └───────────┘
```

* `no_std` Rust, embassy async executor
* `embassy-rp 0.10` (HAL, PIO, DMA) · `embassy-net 0.9` (smoltcp, DHCPv4) ·
  `cyw43 0.7` + `cyw43-pio 0.10` (WiFi) · `pio 0.3` (`pio_asm!` assembler)
* Transparent bridge: the byte stream is emitted unchanged, so framing
  (`0xFFFF` tile sync, header, CRC-16) stays identical to the Pi-Zero SPI path
  in `trich-spi-orchestrator` / `trich-codec`.

## 2B1T wire format

| trit | bits | P rail | N rail |
|:----:|:----:|:------:|:------:|
| 0    | `00` | 0 | 0 |
| +1   | `01` | 1 | 0 |
| −1   | `10` | 0 | 1 |
| rsv  | `11` | 1 | 1 | ← only in the `0xFFFF` tile sync word |

4 trits per byte, LSB-first; 16 trits per little-endian 32-bit PIO word.

## Timing

| parameter | value |
|---|---|
| `clk_sys` | 125 MHz |
| PIO clock divider | **5** (integer → jitter-free) |
| SM clock | 25 MHz (40 ns/cycle) |
| cycles per trit | 4 |
| strobe / trit rate | **6.25 MHz / 6.25 MTrit/s** (1.5625 MB/s payload) |
| setup / hold around rising strobe edge | 80 ns / 80 ns |

Change `PIO_CLOCK_DIVIDER` in `src/pio_logic.rs`; a compile-time assert
guards the 2-FF-synchroniser limit of the FPGA's 27 MHz domain.

## Wiring matrix (Pico W ↔ Tang Nano 9K)

Both sides are 3.3 V LVCMOS — no level shifter. Keep leads < 10 cm, twist
each signal with a GND wire if longer.

| Signal | Pico W GPIO | Pico W pin | Direction | Tang Nano 9K pin¹ | `.cst` name |
|---|---|---|---|---|---|
| P rail (+1) | GP2 | 4 | Pico → FPGA | 25 | `tern_p` |
| N rail (−1) | GP3 | 5 | Pico → FPGA | 26 | `tern_n` |
| Strobe (sample on ↑) | GP4 | 6 | Pico → FPGA | 27 | `tern_strb` |
| GND | GND | 3 (or 8) | common | GND | — |

¹ Suggested free 3.3 V header pins — adapt to your bitstream's constraints:

```
IO_LOC "tern_p" 25;    IO_PORT "tern_p"    IO_TYPE=LVCMOS33 PULL_MODE=DOWN;
IO_LOC "tern_n" 26;    IO_PORT "tern_n"    IO_TYPE=LVCMOS33 PULL_MODE=DOWN;
IO_LOC "tern_strb" 27; IO_PORT "tern_strb" IO_TYPE=LVCMOS33 PULL_MODE=DOWN;
```

GP2/GP3 **must stay consecutive** (PIO `OUT` pin base / base+1).
GP23/24/25/29 are reserved for the on-board CYW43439.

## Build & flash

Requirements: Rust ≥ 1.85 with `thumbv6m-none-eabi`, `probe-rs`, a SWD probe
(Raspberry Pi Debug Probe or a second Pico running picoprobe).

```sh
rustup target add thumbv6m-none-eabi
cargo install probe-rs-tools --locked

cp .env.example .env        # fill in WIFI_SSID / WIFI_PSK (git-ignored)
cargo run --release         # build, flash over SWD, stream defmt logs via RTT
```

The DHCP lease and port are printed over RTT. Stream a packed weight file:

```sh
nc -N <pico-ip> 7878 < weights.2b1t
```

Host-side unit tests (wire model, PIO machine code, divider math):

```sh
cargo test-host
```

## Layout

```
src/main.rs        WiFi, DHCP, TCP server, block queue, task wiring
src/pio_logic.rs   PIO program, timing constants, state-machine driver
src/wire.rs        2B1T reference model + alignment helpers (host-tested)
build.rs           linker scripts + compile-time config from env / .env
cyw43-firmware/    CYW43439 firmware/CLM/NVRAM (Infineon permissive licence)
docs/ARCHITECTURE.md  DMA + PIO data flow
```

## OpSec

No SSIDs, passphrases, IPs or hostnames live in the tree. Credentials are
injected by `build.rs` from the environment or the git-ignored `.env` and end
up only in the firmware image under `target/` (also git-ignored).
The TCP endpoint is unauthenticated — run it only on a trusted LAN segment.
