# Architecture: DMA + PIO data flow

## Tasks (single thread-mode embassy executor)

| task | role | static storage |
|---|---|---|
| `main` | WiFi join, DHCP, TCP accept loop, fills DMA blocks | 9.6 KiB (incl. 8 KiB TCP RX window) |
| `cyw43_task` | CYW43439 bus/runner (gSPI over PIO0 SM0 + DMA_CH0) | 4.3 KiB |
| `net_task` | smoltcp / embassy-net runner | 136 B |
| `injector_task` | drains blocks into PIO1 SM0 via DMA_CH1 | 72 B |

All futures are statically allocated in per-task pools — no heap.

## Peripheral allocation

| resource | user |
|---|---|
| PIO0 SM0, DMA_CH0, GP23/24/25/29 | CYW43439 WiFi (cyw43-pio) |
| PIO1 SM0, DMA_CH1, GP2/GP3/GP4 | ternary transmitter |

Separate PIO blocks keep instruction memory, IRQs and FIFOs fully independent;
WiFi traffic can never perturb the transmitter timing.

## Data path

```
TCP segment ─▶ socket.read() ─▶ Block.words (viewed as bytes, zero-copy)
                                    │  word-aligned prefix only;
                                    │  0..3 byte remainder carried over
                                    ▼
             zerocopy_channel<Block; 4>   (back-pressure → TCP window)
                                    │
                                    ▼
      injector_task: DMA_CH1  memory ─▶ PIO1 TXF0   (DREQ-paced, 32-bit)
                                    │
                                    ▼
      PIO1 SM0: TX FIFO joined (8 words)  →  OSR (autopull @ 32, shift right)
                                    │
                         out pins, 2   side 0 [1]   ; P/N change, strobe low
                         nop           side 1 [1]   ; strobe high, FPGA latches
```

1. **Reception.** `main` obtains a free `Block` (2 KiB) from the zero-copy
   channel and reads straight into it. RP2040 is little-endian, so byte *k*
   of the stream occupies bits `8·(k%4)…` of word `k/4` — exactly the order the
   right-shifting OSR emits. No re-packing pass exists.
2. **Alignment.** Only whole words are released. A 1–3 byte tail is copied to
   the head of the next block, so the trit phase never slips across TCP
   segment boundaries. On EOF the last partial word is zero-padded (trit 0).
3. **Flush policy.** A block is released when full, or early when the socket
   has nothing more queued (`recv_queue() == 0`) — low latency for small
   payloads, full 2 KiB bursts under sustained load.
4. **Back-pressure.** With all 4 blocks queued, `send()` suspends; smoltcp stops
   draining its RX buffer and the TCP window closes. Nothing is dropped.
5. **DMA.** `injector_task` issues one `dma_push` per block. The DMA channel is
   paced by the PIO TX DREQ, so the CPU is idle while 8192 trits (1.31 ms) are
   shifted out.
6. **PIO.** Autopull refills the OSR every 16 trits with zero dead cycles; the
   8-deep joined FIFO buffers 128 trits (20.5 µs) to cover DMA re-arm between
   blocks.

## Timing

```
SM cycle   │ 0   │ 1   │ 2   │ 3   │ 0   │
P/N        ╳ trit n                ╳ trit n+1
STROBE     ▁▁▁▁▁▁▁▁▁▁▁▁╱▔▔▔▔▔▔▔▔▔▔▔╲▁▁▁▁▁
                       ↑ sample: 80 ns setup, 80 ns hold
```

* `clk_sys` 125 MHz ÷ **5** (integer, no fractional jitter) = 25 MHz SM clock.
* 4 cycles/trit → **6.25 MTrit/s**, strobe 6.25 MHz, 50 % duty.
* The Tang Nano 9K samples the strobe through a 2-FF synchroniser at 27 MHz;
  each strobe phase (80 ns) spans > 2 FPGA clocks (74 ns). A `const` assert in
  `pio_logic.rs` rejects dividers that would violate this.

## Underrun behaviour

If WiFi cannot keep up (sustained PIO demand is 1.5625 MB/s), the FIFO empties
and `out` stalls **with strobe low**: the FPGA sees no edge, so no phantom
trits are injected; the stream simply pauses and resumes in phase. Rails hold
their last value during a stall; the strobe is the only qualifier.

## Not (yet) covered

* No authentication on the TCP port — trusted LAN only.
* No FPGA → Pico back-channel (ready/ack); add a GPIO + `wait` in the PIO
  program if the array needs flow control.
* Sustained throughput is WiFi-bound; measure with
  `pv weights.2b1t | nc -N <pico-ip> 7878`.
