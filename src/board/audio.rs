//! Audio output over I2S. BCLK G41, LRCK G43, DATA G42 on both models.
//!
//! v1.1 feeds an NS4168 I2S amplifier, which needs no setup. Adv feeds an ES8311 codec, which must be
//! configured over I2C before this starts ([`crate::board::adv::codec`]).
//!
//! Streams 16kHz 16-bit stereo (same sound on both channels) continuously via DMA. Plays silence when idle.
//!
//! The DMA stream buffer ends the transfer itself if it runs empty because refilling fell behind.
//! This happens when other tasks hog the CPU (e.g. SD initialization), so recreate it and resume when it ends.

use core::sync::atomic::{AtomicU32, Ordering};

use esp_hal::dma::DmaTxStreamBuf;
use esp_hal::dma_tx_stream_buffer;
use esp_hal::i2s::master::{Channels, DataFormat, I2s, TdmConfig};
use esp_hal::peripherals::{DMA_CH0, GPIO41, GPIO42, GPIO43, I2S0};
use esp_hal::time::Rate;

use crate::sound::SAMPLE_RATE;

pub struct AudioPins {
    pub i2s: I2S0<'static>,
    pub dma: DMA_CH0<'static>,
    pub bclk: GPIO41<'static>,
    pub ws: GPIO43<'static>,
    pub dout: GPIO42<'static>,
}

/// Number of frames synthesized at once.
const FRAMES_PER_FILL: usize = 128;

/// Number of times the transfer stopped because refilling fell behind and was resumed (diagnostics).
pub static UNDERRUNS: AtomicU32 = AtomicU32::new(0);

/// Fill the buffer with silence.
fn fill_silence(buf: &mut DmaTxStreamBuf) {
    while buf.push_with(|b| {
        b.fill(0);
        b.len()
    }) > 0
    {}
}

/// Keep streaming sound. `fill` fills the given region with mono samples.
pub async fn run(pins: AudioPins, mut fill: impl FnMut(&mut [i16])) -> ! {
    // 16KB = 4096 frames ≈ 256ms
    let mut buf = dma_tx_stream_buffer!(16384, 1024);
    let config = TdmConfig::new_tdm_philips()
        .with_sample_rate(Rate::from_hz(SAMPLE_RATE))
        .with_data_format(DataFormat::Data16Channel16)
        .with_channels(Channels::STEREO);
    let i2s = I2s::new(pins.i2s, pins.dma, config)
        .expect("Failed to configure I2S")
        .into_async();
    let tx = i2s
        .i2s_tx
        .with_bclk(pins.bclk)
        .with_ws(pins.ws)
        .with_dout(pins.dout)
        .build();

    // Fill with silence before starting
    fill_silence(&mut buf);
    let mut transfer = match tx.write(buf) {
        Ok(t) => t,
        Err(_) => panic!("Cannot start I2S DMA"),
    };

    let mut mono = [0i16; FRAMES_PER_FILL];
    loop {
        let result = transfer.wait_for_available_async().await;
        if result.is_err() || transfer.is_done() {
            // Ran empty and the transfer ended. Recreate the buffer and resume from silence
            let n = UNDERRUNS.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 3 || n.is_power_of_two() {
                log::info!("Audio transfer ran dry; restarting (count {})", n);
            }
            let (tx, buf) = transfer.stop();
            let (descriptors, buffer) = buf.split();
            let mut buf = DmaTxStreamBuf::new(descriptors, buffer).expect("Cannot recreate DMA buffer");
            fill_silence(&mut buf);
            transfer = match tx.write(buf) {
                Ok(t) => t,
                Err(_) => panic!("Cannot restart I2S DMA"),
            };
            continue;
        }
        loop {
            let pushed = transfer.push_with(|b| {
                let frames = (b.len() / 4).min(FRAMES_PER_FILL);
                fill(&mut mono[..frames]);
                for (i, s) in mono[..frames].iter().enumerate() {
                    let bytes = s.to_le_bytes();
                    b[i * 4..i * 4 + 2].copy_from_slice(&bytes);
                    b[i * 4 + 2..i * 4 + 4].copy_from_slice(&bytes);
                }
                frames * 4
            });
            if pushed == 0 {
                break;
            }
        }
    }
}
