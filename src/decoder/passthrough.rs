//! Codecs whose MP4 samples are already decoder-ready, needing only their configuration
//! put in decoder form.

use super::{DecoderFormat, DecoderInitialization, DecoderPacket, DecoderParameters};
use super::{config_error, flac, opus};
use crate::error::Error;
use crate::mp4::codec::{AudioConfig, Codec};
use crate::mp4::packet::Packet;
use bytes::Bytes;

/// A converter of the packets of a track whose samples decoders read as they are: MP3,
/// AC-3, E-AC-3, FLAC, Opus, VP9 and PCM. Packets keep their bytes, and the
/// initialization is the codec's configuration in the form each [`DecoderFormat`]
/// describes. For use without a [`DecoderPacketStream`](crate::DecoderPacketStream).
#[derive(Clone, Debug)]
pub struct PassthroughAdapter {
    initialization: DecoderInitialization,
}

impl PassthroughAdapter {
    /// Creates an adapter for the packets of a track with `codec`.
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] for another codec, or a FLAC or Opus
    /// configuration of a version other than 0, and with [`Error::MalformedDecoderInput`]
    /// for a damaged FLAC or Opus configuration.
    pub fn new(codec: &Codec) -> Result<Self, Error> {
        let audio = |format, audio: &AudioConfig, data| DecoderInitialization {
            format,
            data,
            parameters: DecoderParameters::Audio {
                sample_rate: audio.sample_rate,
                channels: audio.channels,
            },
        };
        let initialization = match codec {
            Codec::Mp3(mp3) => audio(DecoderFormat::Mp3, mp3, mp3.config.clone()),
            Codec::Ac3(ac3) => audio(DecoderFormat::Ac3, ac3, ac3.config.clone()),
            Codec::Eac3(eac3) => audio(DecoderFormat::Eac3, eac3, eac3.config.clone()),
            Codec::Flac(config) => {
                let (header, parameters) = flac::initialization(&config.config)
                    .map_err(|error| config_error(codec, DecoderFormat::Flac, error))?;
                DecoderInitialization {
                    format: DecoderFormat::Flac,
                    data: header,
                    parameters,
                }
            }
            Codec::Opus(config) => {
                let (head, parameters) = opus::initialization(&config.audio.config)
                    .map_err(|error| config_error(codec, DecoderFormat::OpusPackets, error))?;
                DecoderInitialization {
                    format: DecoderFormat::OpusPackets,
                    data: head,
                    parameters,
                }
            }
            Codec::Pcm(pcm) => DecoderInitialization {
                format: DecoderFormat::Pcm,
                data: Bytes::new(),
                parameters: DecoderParameters::Pcm {
                    sample_rate: pcm.audio.sample_rate,
                    channels: pcm.audio.channels,
                    bits_per_sample: pcm.bits_per_sample,
                    float: pcm.float,
                    little_endian: pcm.little_endian,
                },
            },
            Codec::Vp9(video) => DecoderInitialization {
                format: DecoderFormat::Vp9Frames,
                data: video.config.clone(),
                parameters: super::video(video.width, video.height),
            },
            _ => {
                return Err(Error::UnsupportedDecoderInput {
                    codec: codec.name(),
                    format: None,
                    reason: format!("{} packets are not decoder-ready as stored", codec.name()),
                });
            }
        };
        Ok(Self { initialization })
    }

    /// The initialization for a decoder of this adapter's packets.
    pub fn initialization(&self) -> &DecoderInitialization {
        &self.initialization
    }

    /// Converts `packet`, one of the track's, keeping its bytes.
    pub fn convert(&self, packet: Packet) -> DecoderPacket {
        let data = packet.data.clone();
        DecoderPacket::from_packet(packet, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::codec::{AacConfig, OpusConfig, PcmConfig, VideoConfig};

    /// An AAC-LC codec with `config` as its AudioSpecificConfig.
    fn aac(config: &[u8]) -> Codec {
        Codec::Aac(AacConfig {
            audio: audio(44_100, 2, config),
            object_type: 2,
            sbr: false,
        })
    }

    fn audio(sample_rate: u32, channels: u16, config: &[u8]) -> AudioConfig {
        AudioConfig {
            sample_rate,
            channels,
            config: Bytes::copy_from_slice(config),
        }
    }

    fn initialization(codec: Codec) -> DecoderInitialization {
        PassthroughAdapter::new(&codec)
            .unwrap()
            .initialization()
            .clone()
    }

    #[test]
    fn each_codec_has_its_initialization() {
        let ac3 = initialization(Codec::Ac3(audio(48_000, 6, &[0x10, 0x3d, 0xc0])));
        assert_eq!(
            (ac3.format, &ac3.data[..]),
            (DecoderFormat::Ac3, &[0x10, 0x3d, 0xc0][..])
        );
        let mp3 = initialization(Codec::Mp3(audio(44_100, 2, &[])));
        assert_eq!((mp3.format, mp3.data.len()), (DecoderFormat::Mp3, 0));

        let mut dfla = vec![0, 0, 0, 0, 0x80, 0, 0, 34];
        dfla.extend([0; 34]);
        let flac = initialization(Codec::Flac(audio(22_050, 1, &dfla)));
        assert_eq!(flac.format, DecoderFormat::Flac);
        assert_eq!(&flac.data[..8], b"fLaC\x80\0\0\x22");

        let opus = initialization(Codec::Opus(OpusConfig {
            audio: audio(48_000, 1, &[0, 1, 0x01, 0x38, 0, 0, 0xbb, 0x80, 0, 0, 0]),
            pre_skip: 312,
        }));
        assert_eq!(opus.format, DecoderFormat::OpusPackets);
        assert!(opus.data.starts_with(b"OpusHead"));
        assert!(matches!(
            opus.parameters,
            DecoderParameters::Opus {
                channels: 1,
                pre_skip: 312,
                ..
            }
        ));

        let pcm = initialization(Codec::Pcm(PcmConfig {
            audio: audio(8_000, 1, &[]),
            bits_per_sample: 16,
            float: false,
            little_endian: true,
        }));
        assert_eq!(
            pcm.parameters,
            DecoderParameters::Pcm {
                sample_rate: 8_000,
                channels: 1,
                bits_per_sample: 16,
                float: false,
                little_endian: true
            }
        );

        let vpcc: &[u8] = &[1, 0, 0, 0, 0, 10, 0x80, 2, 2, 2, 0, 0, 0, 0];
        let vp9 = initialization(Codec::Vp9(VideoConfig {
            width: 96,
            height: 64,
            config: Bytes::from_static(vpcc),
        }));
        assert_eq!(
            (vp9.format, &vp9.data[..], vp9.parameters),
            (
                DecoderFormat::Vp9Frames,
                vpcc,
                crate::decoder::video(96, 64)
            )
        );
    }

    #[test]
    fn packets_keep_their_bytes() {
        let adapter = PassthroughAdapter::new(&Codec::Mp3(audio(44_100, 2, &[]))).unwrap();
        let packet = Packet {
            data: Bytes::from_static(&[0xff, 0xfb, 0x90]),
            pts: -1_105,
            dts: -1_105,
            duration: 1_152,
            timescale: 44_100,
            keyframe: true,
            trim_start: 1_105,
            trim_end: 0,
        };
        let converted = adapter.convert(packet.clone());
        assert_eq!(converted.data, packet.data);
        assert_eq!(converted.trim_start, 1_105);
    }

    #[test]
    fn other_codecs_and_bad_configurations_are_refused() {
        assert!(matches!(
            PassthroughAdapter::new(&aac(&[0x12, 0x10])),
            Err(Error::UnsupportedDecoderInput { reason, .. })
                if reason == "AAC packets are not decoder-ready as stored"
        ));
        assert!(matches!(
            PassthroughAdapter::new(&Codec::Flac(audio(22_050, 1, &[0, 0, 0, 0, 1]))),
            Err(Error::MalformedDecoderInput {
                format: DecoderFormat::Flac,
                ..
            })
        ));
        let h264 = Codec::H264(VideoConfig {
            width: 1,
            height: 1,
            config: Bytes::new(),
        });
        assert!(matches!(
            PassthroughAdapter::new(&h264),
            Err(Error::UnsupportedDecoderInput { format: None, reason, .. })
                if reason == "H.264 packets are not decoder-ready as stored"
        ));
    }
}
