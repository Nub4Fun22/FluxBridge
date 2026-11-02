//! Stream decode loop.
//!
//! Pulls [`PacketType::VideoFrame`] packets from the connection channel, feeds
//! their payloads to a [`VideoDecoder`] trait object, and pushes the resulting
//! [`DecodedFrame`] buffers onto a frame sink (an `mpsc` channel the UI thread
//! drains to update the video surface).
//!
//! The decoder is a trait object so the real backend (FFmpeg with DXVA2 /
//! D3D11VA on Windows, VA-API / VDPAU on Linux) can be dropped in without
//! touching this loop. This module ships [`StubDecoder`], a pure-Rust software
//! decoder that fabricates a frame from the encoded bytes. It exists purely so
//! the scaffold runs end-to-end on a host with no FFmpeg; it is NOT a real
//! codec and must be replaced before shipping.

use std::sync::mpsc::{Receiver, Sender};

use fluxbridge_core::protocol::{Packet, PacketType};
use fluxbridge_core::{CoreResult, DecodedFrame, VideoConfig, VideoDecoder};

/// A stand-in software decoder.
///
/// It does not actually decode H.264/HEVC; it wraps each encoded payload in a
/// [`DecodedFrame`] sized from its configuration so the pipeline (connection ->
/// decode -> sink -> UI) can be exercised without a hardware codec. The real
/// FFmpeg-backed `VideoDecoder` replaces this; see `docs/ARCHITECTURE.md`.
#[derive(Debug, Default)]
pub struct StubDecoder {
    width: u32,
    height: u32,
    configured: bool,
}

impl StubDecoder {
    /// Create an unconfigured stub decoder.
    pub fn new() -> Self {
        StubDecoder {
            width: 0,
            height: 0,
            configured: false,
        }
    }
}

impl VideoDecoder for StubDecoder {
    fn configure(&mut self, config: &VideoConfig) -> CoreResult<()> {
        self.width = config.width;
        self.height = config.height;
        self.configured = true;
        Ok(())
    }

    fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
        // Produce a deterministic placeholder frame. Real decoders return the
        // actual decoded pixels; here we echo the input length into a tiny
        // buffer so tests can assert data flowed through.
        Ok(DecodedFrame {
            width: self.width,
            height: self.height,
            pixel_format: "stub".to_string(),
            data: encoded.to_vec(),
        })
    }

    fn flush(&mut self) -> CoreResult<()> {
        Ok(())
    }
}

/// Run the decode loop until the packet channel closes.
///
/// For every packet received:
/// * [`PacketType::VideoConfig`] payloads (re)configure the decoder. In this
///   scaffold the config bytes are not parsed; a fixed [`VideoConfig`] is used
///   so the stub has sane dimensions. The real loop will decode the config
///   payload defined in `docs/PROTOCOL.md`.
/// * [`PacketType::VideoFrame`] payloads are decoded and the resulting
///   [`DecodedFrame`] is pushed to `frame_sink`.
/// * All other packet types are ignored here (handled elsewhere).
///
/// Returns when the `packets` sender is dropped (connection closed) or the
/// `frame_sink` receiver is gone (UI shut down).
pub fn run_decode_loop(
    packets: &Receiver<Packet>,
    decoder: &mut dyn VideoDecoder,
    frame_sink: &Sender<DecodedFrame>,
) -> CoreResult<()> {
    // Ensure the decoder has a configuration even if the server never sends an
    // explicit VideoConfig in this scaffold.
    let default_config = VideoConfig {
        codec: "h264".to_string(),
        width: 1080,
        height: 1920,
        extra_data: Vec::new(),
    };
    decoder.configure(&default_config)?;

    while let Ok(packet) = packets.recv() {
        match packet.packet_type {
            PacketType::VideoConfig => {
                // Real impl: parse the codec/resolution from packet.payload.
                decoder.configure(&default_config)?;
            }
            PacketType::VideoFrame => {
                let frame = decoder.decode(&packet.payload)?;
                if frame_sink.send(frame).is_err() {
                    break; // UI gone
                }
            }
            _ => {
                // Non-video packets are routed by other consumers.
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    /// A fake decoder that records how many frames it decoded and tags output.
    #[derive(Default)]
    struct FakeDecoder {
        decoded: usize,
        configured: bool,
    }

    impl VideoDecoder for FakeDecoder {
        fn configure(&mut self, _config: &VideoConfig) -> CoreResult<()> {
            self.configured = true;
            Ok(())
        }

        fn decode(&mut self, encoded: &[u8]) -> CoreResult<DecodedFrame> {
            self.decoded += 1;
            Ok(DecodedFrame {
                width: 4,
                height: 4,
                pixel_format: "fake".to_string(),
                data: encoded.to_vec(),
            })
        }

        fn flush(&mut self) -> CoreResult<()> {
            Ok(())
        }
    }

    #[test]
    fn drives_fake_frames_through_fake_decoder() {
        let (pkt_tx, pkt_rx) = mpsc::channel();
        let (frame_tx, frame_rx) = mpsc::channel();

        // Queue a config packet and three video frames, then close the sender.
        pkt_tx
            .send(Packet::new(PacketType::VideoConfig, vec![]))
            .unwrap();
        for i in 0..3u8 {
            pkt_tx
                .send(Packet::new(PacketType::VideoFrame, vec![i, i + 1, i + 2]))
                .unwrap();
        }
        // A non-video packet that must be ignored by the loop.
        pkt_tx
            .send(Packet::new(PacketType::Heartbeat, vec![]))
            .unwrap();
        drop(pkt_tx);

        let mut decoder = FakeDecoder::default();
        run_decode_loop(&pkt_rx, &mut decoder, &frame_tx).unwrap();

        assert!(decoder.configured, "decoder should be configured");
        assert_eq!(decoder.decoded, 3, "three video frames should be decoded");

        let frames: Vec<DecodedFrame> = frame_rx.try_iter().collect();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].data, vec![0, 1, 2]);
        assert_eq!(frames[2].data, vec![2, 3, 4]);
        assert_eq!(frames[0].pixel_format, "fake");
    }

    #[test]
    fn stub_decoder_echoes_payload() {
        let mut decoder = StubDecoder::new();
        decoder
            .configure(&VideoConfig {
                codec: "h264".into(),
                width: 100,
                height: 200,
                extra_data: vec![],
            })
            .unwrap();
        let frame = decoder.decode(&[9, 8, 7]).unwrap();
        assert_eq!(frame.width, 100);
        assert_eq!(frame.height, 200);
        assert_eq!(frame.data, vec![9, 8, 7]);
    }
}
