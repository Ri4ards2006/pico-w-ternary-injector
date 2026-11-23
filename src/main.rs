//! Pico W ternary injector firmware.
//!
//! WiFi (CYW43439 on PIO0) -> embassy-net TCP server -> zero-copy block queue
//! -> DMA -> PIO1/SM0 -> dual-rail 2B1T signals for the Tang Nano 9K.
#![no_std]
#![no_main]

use cyw43::{JoinOptions, aligned_bytes};
use cyw43_pio::{DEFAULT_CLOCK_DIVIDER, PioSpi};
use defmt::{info, unwrap, warn};
use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, StackResources};
use embassy_rp::clocks::RoscRng;
use embassy_rp::gpio::{Level, Output};
use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, PIO0, PIO1};
use embassy_rp::pio::{InterruptHandler, Pio};
use embassy_rp::{bind_interrupts, dma};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::zerocopy_channel::{Channel, Receiver, Sender};
use embassy_time::Duration;
use pico_w_ternary_injector::pio_logic::{self, TernaryInjector};
use pico_w_ternary_injector::wire::{self, BYTES_PER_WORD};
use static_cell::{ConstStaticCell, StaticCell};
use {defmt_rtt as _, panic_probe as _};

bind_interrupts!(struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    PIO1_IRQ_0 => InterruptHandler<PIO1>;
    DMA_IRQ_0 => dma::InterruptHandler<DMA_CH0>, dma::InterruptHandler<DMA_CH1>;
});

// Injected by build.rs from the environment / git-ignored `.env`.
const WIFI_SSID: &str = env!("WIFI_SSID");
const WIFI_PSK: &str = env!("WIFI_PSK");
const TCP_PORT: u16 = parse_port(env!("TCP_PORT"));

/// Words per DMA block: 512 x 32 bit = 2 KiB = 8192 trits = 1.31 ms of PIO output.
const BLOCK_WORDS: usize = 512;
const BLOCK_BYTES: usize = BLOCK_WORDS * BYTES_PER_WORD;
/// Blocks in flight between TCP and PIO (8 KiB total) to absorb WiFi jitter.
const BLOCKS: usize = 4;
/// TCP receive window; larger window = fewer stalls on the 802.11n link.
const TCP_RX_BUF: usize = 8 * 1024;

type Mutex = CriticalSectionRawMutex;

struct Block {
    words: [u32; BLOCK_WORDS],
    len: usize, // valid words
}

impl Block {
    const EMPTY: Self = Self { words: [0; BLOCK_WORDS], len: 0 };

    fn bytes_mut(&mut self) -> &mut [u8; BLOCK_BYTES] {
        // SAFETY: [u32; N] and [u8; 4N] have identical size, u8 has weaker
        // alignment, and every bit pattern is a valid u32. RP2040 is little
        // endian, so byte k of the stream lands in bits 8*(k%4).. of word k/4 —
        // exactly the LSB-first order the PIO shifts out.
        unsafe { &mut *(self.words.as_mut_ptr() as *mut [u8; BLOCK_BYTES]) }
    }
}

const fn parse_port(s: &str) -> u16 {
    let b = s.as_bytes();
    let mut i = 0;
    let mut v: u32 = 0;
    while i < b.len() {
        assert!(b[i].is_ascii_digit());
        v = v * 10 + (b[i] - b'0') as u32;
        assert!(v <= u16::MAX as u32);
        i += 1;
    }
    v as u16
}

#[embassy_executor::task]
async fn cyw43_task(runner: cyw43::Runner<'static, cyw43::SpiBus<Output<'static>, PioSpi<'static, PIO0, 0>>>) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, cyw43::NetDriver<'static>>) -> ! {
    runner.run().await
}

/// Drains filled blocks into the PIO TX FIFO via DMA.
#[embassy_executor::task]
async fn injector_task(
    mut rx: Receiver<'static, Mutex, Block>,
    mut injector: TernaryInjector<'static, PIO1, 0>,
    mut dma: dma::Channel<'static>,
) -> ! {
    loop {
        let block = rx.receive().await;
        injector.push(&mut dma, &block.words[..block.len]).await;
        rx.receive_done();
    }
}

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());
    info!(
        "pico-w-ternary-injector: PIO div {} -> {} trit/s",
        pio_logic::PIO_CLOCK_DIVIDER,
        pio_logic::trit_rate_hz(pio_logic::PIO_CLOCK_DIVIDER)
    );

    // --- Ternary PIO transmitter on PIO1 (PIO0 belongs to the CYW43 SPI) ---
    let Pio { mut common, sm0, .. } = Pio::new(p.PIO1, Irqs);
    // GP2 = P rail, GP3 = N rail (must be consecutive), GP4 = strobe.
    let injector = TernaryInjector::new(&mut common, sm0, p.PIN_2, p.PIN_3, p.PIN_4);

    static BLOCK_POOL: ConstStaticCell<[Block; BLOCKS]> = ConstStaticCell::new([Block::EMPTY; BLOCKS]);
    static CHANNEL: StaticCell<Channel<'static, Mutex, Block>> = StaticCell::new();
    let (tx, rx) = CHANNEL.init(Channel::new(BLOCK_POOL.take())).split();
    spawner.spawn(unwrap!(injector_task(rx, injector, dma::Channel::new(p.DMA_CH1, Irqs))));

    // --- CYW43439 WiFi ---
    let fw = aligned_bytes!("../cyw43-firmware/43439A0.bin");
    let clm = aligned_bytes!("../cyw43-firmware/43439A0_clm.bin");
    let nvram = aligned_bytes!("../cyw43-firmware/nvram_rp2040.bin");

    let pwr = Output::new(p.PIN_23, Level::Low);
    let cs = Output::new(p.PIN_25, Level::High);
    let mut pio0 = Pio::new(p.PIO0, Irqs);
    let spi = PioSpi::new(
        &mut pio0.common,
        pio0.sm0,
        DEFAULT_CLOCK_DIVIDER,
        pio0.irq0,
        cs,
        p.PIN_24,
        p.PIN_29,
        dma::Channel::new(p.DMA_CH0, Irqs),
    );

    static STATE: StaticCell<cyw43::State> = StaticCell::new();
    let (net_device, mut control, runner) = cyw43::new(STATE.init(cyw43::State::new()), pwr, spi, fw, nvram).await;
    spawner.spawn(unwrap!(cyw43_task(runner)));

    control.init(clm).await;
    // Throughput over battery life: PowerSave adds ~100 ms wake-up latency per burst.
    control.set_power_management(cyw43::PowerManagementMode::Performance).await;

    static RESOURCES: StaticCell<StackResources<3>> = StaticCell::new();
    let (stack, runner) = embassy_net::new(
        net_device,
        Config::dhcpv4(Default::default()),
        RESOURCES.init(StackResources::new()),
        RoscRng.next_u64(),
    );
    spawner.spawn(unwrap!(net_task(runner)));

    let join_opts = if WIFI_PSK.is_empty() {
        JoinOptions::new_open()
    } else {
        JoinOptions::new(WIFI_PSK.as_bytes())
    };
    while let Err(err) = control.join(WIFI_SSID, join_opts.clone()).await {
        warn!("join failed: {:?}", err);
    }
    stack.wait_link_up().await;
    stack.wait_config_up().await;
    if let Some(cfg) = stack.config_v4() {
        info!("DHCP lease {} - listening on TCP {}", cfg.address, TCP_PORT);
    }

    // --- TCP server: one sender at a time ---
    let mut rx_buf = [0u8; TCP_RX_BUF];
    let mut tx_buf = [0u8; 256];
    let mut tx = tx;
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(Duration::from_secs(10)));
        control.gpio_set(0, false).await; // onboard LED: idle
        if let Err(e) = socket.accept(TCP_PORT).await {
            warn!("accept: {:?}", e);
            continue;
        }
        info!("client {:?}", socket.remote_endpoint());
        control.gpio_set(0, true).await; // onboard LED: streaming

        let total = stream_connection(&mut socket, &mut tx).await;
        info!("client done: {} bytes = {} trits", total, total * 4);
        socket.abort();
        let _ = socket.flush().await;
    }
}

/// Copy one TCP connection into DMA blocks. Only word-aligned prefixes are
/// handed to the PIO; the 0..3 byte remainder is carried into the next block,
/// so trit alignment never breaks. On EOF the final partial word is zero-padded
/// (trit 0 = 0b00). Returns bytes received.
async fn stream_connection(socket: &mut TcpSocket<'_>, tx: &mut Sender<'static, Mutex, Block>) -> usize {
    let mut total = 0usize;
    let mut carry = [0u8; BYTES_PER_WORD];
    let mut carry_len = 0usize;

    loop {
        let block = tx.send().await; // back-pressure: waits for a free block
        let bytes = block.bytes_mut();
        bytes[..carry_len].copy_from_slice(&carry[..carry_len]);
        let mut filled = carry_len;
        let mut eof = false;

        // Fill until the block is full or the socket has nothing queued.
        while filled < BLOCK_BYTES {
            match socket.read(&mut bytes[filled..]).await {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(n) => {
                    filled += n;
                    total += n;
                    if socket.recv_queue() == 0 {
                        break; // flush early: low latency for small payloads
                    }
                }
                Err(e) => {
                    warn!("read: {:?}", e);
                    eof = true;
                    break;
                }
            }
        }

        let send_len = if eof {
            carry_len = 0;
            wire::pad_to_word(bytes, filled)
        } else {
            let aligned = wire::aligned_len(filled);
            carry_len = filled - aligned;
            carry[..carry_len].copy_from_slice(&bytes[aligned..filled]);
            aligned
        };

        if send_len > 0 {
            block.len = send_len / BYTES_PER_WORD;
            tx.send_done();
        }
        if eof {
            return total;
        }
    }
}
