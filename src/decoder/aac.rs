//! AAC as decoders read it: raw access units with the AudioSpecificConfig, or each access
//! unit after an ADTS header.

use super::{
    DecoderFormat, DecoderInitialization, DecoderParameters, MediaPacket, unsupported_format,
    wrong_codec,
};
use crate::error::Error;
use crate::mp4::codec::{Bits, Codec, object_type};
use crate::mp4::packet::Packet;
use bytes::Bytes;

/// The formats an AAC track converts to.
const FORMATS: [DecoderFormat; 2] = [DecoderFormat::AacRaw, DecoderFormat::AacAdts];

/// Bytes of an ADTS header without a CRC.
const ADTS_HEADER_LEN: usize = 7;

/// The largest ADTS frame, header included: its length field has 13 bits.
const ADTS_MAX_FRAME: usize = 0x1fff;

/// A converter of an AAC track's packets to [`DecoderFormat::AacRaw`] or
/// [`DecoderFormat::AacAdts`], for use without a
/// [`MediaStream`](crate::MediaStream).
#[derive(Clone, Debug)]
pub struct AacAdapter {
    /// The fields every ADTS header repeats, for [`DecoderFormat::AacAdts`].
    adts: Option<AdtsFields>,
    initialization: DecoderInitialization,
}

/// The stream fields of an ADTS header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AdtsFields {
    /// The audio object type less one: 0 to 3 for AAC Main, LC, SSR and LTP.
    profile: u8,
    /// An index into the standard sampling frequency table.
    frequency_index: u8,
    /// The channel configuration, 1 to 7.
    channels: u8,
}

impl AacAdapter {
    /// Creates an adapter converting the packets of a track with `codec` to `format`:
    /// [`DecoderFormat::AacRaw`] or [`DecoderFormat::AacAdts`].
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] if `codec` is not AAC, `format` is
    /// another format, or it is [`DecoderFormat::AacAdts`] and ADTS cannot carry the
    /// stream's configuration. Fails with [`Error::MalformedDecoderInput`] if the track
    /// has no AudioSpecificConfig, or for ADTS a truncated one.
    pub fn new(codec: &Codec, format: DecoderFormat) -> Result<Self, Error> {
        let Codec::Aac(aac) = codec else {
            return Err(wrong_codec(codec, format));
        };
        if !FORMATS.contains(&format) {
            return Err(unsupported_format(codec, format, &FORMATS));
        }
        let config = &aac.audio.config;
        if config.is_empty() {
            return Err(Error::MalformedDecoderInput {
                codec: codec.name(),
                format,
                reason: "mp4a has no AudioSpecificConfig".to_owned(),
            });
        }

        // ADTS repeats the configuration in every header, raw AAC needs it given once.
        let (adts, data) = match format {
            DecoderFormat::AacAdts => {
                let fields = adts_fields(config).map_err(|error| match error {
                    AdtsError::Truncated => Error::MalformedDecoderInput {
                        codec: codec.name(),
                        format,
                        reason: "truncated AudioSpecificConfig".to_owned(),
                    },
                    AdtsError::Unrepresentable(reason) => Error::UnsupportedDecoderInput {
                        codec: codec.name(),
                        format: Some(format),
                        reason,
                    },
                })?;
                (Some(fields), Bytes::new())
            }
            _ => (None, config.clone()),
        };
        Ok(Self {
            adts,
            initialization: DecoderInitialization {
                format,
                data,
                parameters: DecoderParameters::Audio {
                    sample_rate: aac.audio.sample_rate,
                    channels: aac.audio.channels,
                },
            },
        })
    }

    /// The initialization for a decoder of this adapter's packets.
    pub fn initialization(&self) -> &DecoderInitialization {
        &self.initialization
    }

    /// Converts `packet`, one of the track's, to the adapter's format: the access unit
    /// after an ADTS header, or as it is.
    ///
    /// Fails with [`Error::MalformedDecoderInput`] if the access unit is too long for an
    /// ADTS frame.
    pub fn convert(&self, packet: Packet) -> Result<MediaPacket, Error> {
        let Some(fields) = self.adts else {
            let data = packet.data.clone();
            return Ok(MediaPacket::from_packet(packet, data));
        };
        let len = ADTS_HEADER_LEN + packet.data.len();
        if len > ADTS_MAX_FRAME {
            return Err(Error::MalformedDecoderInput {
                codec: "AAC".to_owned(),
                format: DecoderFormat::AacAdts,
                reason: format!(
                    "a {}-byte access unit is too long for an ADTS frame",
                    packet.data.len()
                ),
            });
        }
        let mut data = Vec::with_capacity(len);
        data.extend_from_slice(&adts_header(fields, len));
        data.extend_from_slice(&packet.data);
        Ok(MediaPacket::from_packet(packet, Bytes::from(data)))
    }
}

/// Why an AudioSpecificConfig gives no ADTS header.
enum AdtsError {
    Truncated,
    /// ADTS cannot express the configuration; the reason says how.
    Unrepresentable(String),
}

/// The ADTS header fields for `config`, an AudioSpecificConfig (ISO/IEC 14496-3
/// §1.6.2.1). For HE-AAC they describe the core, as ADTS does.
fn adts_fields(config: &[u8]) -> Result<AdtsFields, AdtsError> {
    let mut bits = Bits::new(config);
    let mut object = object_type(&mut bits).ok_or(AdtsError::Truncated)?;
    let frequency_index = bits.read(4).ok_or(AdtsError::Truncated)?;
    if frequency_index == 15 {
        return Err(AdtsError::Unrepresentable(
            "ADTS cannot carry an explicit sampling frequency".to_owned(),
        ));
    }
    let channels = bits.read(4).ok_or(AdtsError::Truncated)?;

    // Explicit SBR or parametric stereo: the output frequency, then the core's object type.
    if matches!(object, 5 | 29) {
        if bits.read(4).ok_or(AdtsError::Truncated)? == 15 {
            bits.read(24).ok_or(AdtsError::Truncated)?;
        }
        object = object_type(&mut bits).ok_or(AdtsError::Truncated)?;
    }

    if !(1..=4).contains(&object) {
        return Err(AdtsError::Unrepresentable(format!(
            "ADTS cannot carry audio object type {object}"
        )));
    }
    if !(1..=7).contains(&channels) {
        return Err(AdtsError::Unrepresentable(format!(
            "ADTS cannot carry channel configuration {channels}"
        )));
    }
    Ok(AdtsFields {
        profile: object - 1,
        frequency_index: frequency_index as u8,
        channels: channels as u8,
    })
}

/// An ADTS header without a CRC for a frame of `len` bytes, header included.
fn adts_header(fields: AdtsFields, len: usize) -> [u8; ADTS_HEADER_LEN] {
    let AdtsFields {
        profile,
        frequency_index,
        channels,
    } = fields;
    [
        // Sync word, MPEG-4, layer 0, no CRC.
        0xff,
        0xf1,
        profile << 6 | frequency_index << 2 | channels >> 2,
        (channels & 0x03) << 6 | (len >> 11) as u8,
        (len >> 3) as u8,
        // The frame length's last bits, then a buffer fullness of 0x7ff for VBR and one
        // raw data block.
        ((len & 0x07) as u8) << 5 | 0x1f,
        0xfc,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::codec::{AacConfig, AudioConfig};

    fn aac(config: &[u8]) -> Codec {
        Codec::Aac(AacConfig {
            audio: AudioConfig {
                sample_rate: 44_100,
                channels: 2,
                config: Bytes::copy_from_slice(config),
            },
            object_type: 2,
            sbr: false,
        })
    }

    fn packet(data: Vec<u8>) -> Packet {
        Packet {
            data: Bytes::from(data),
            pts: -1_024,
            dts: -1_024,
            duration: 1_024,
            timescale: 44_100,
            keyframe: true,
            trim_start: 1_024,
            trim_end: 0,
        }
    }

    #[test]
    fn raw_is_the_access_units_with_the_audio_specific_config() {
        let adapter = AacAdapter::new(&aac(&[0x12, 0x10]), DecoderFormat::AacRaw).unwrap();
        assert_eq!(
            adapter.initialization(),
            &DecoderInitialization {
                format: DecoderFormat::AacRaw,
                data: Bytes::from_static(&[0x12, 0x10]),
                parameters: DecoderParameters::Audio {
                    sample_rate: 44_100,
                    channels: 2
                },
            }
        );
        let converted = adapter.convert(packet(vec![0x21, 0x1a])).unwrap();
        assert_eq!(
            (&converted.data[..], converted.trim_start),
            (&[0x21, 0x1a][..], 1_024)
        );
    }

    #[test]
    fn adts_puts_a_header_on_every_access_unit() {
        // AAC-LC, 44.1 kHz (index 4), stereo.
        let adapter = AacAdapter::new(&aac(&[0x12, 0x10]), DecoderFormat::AacAdts).unwrap();
        assert_eq!(adapter.initialization().format, DecoderFormat::AacAdts);
        assert!(adapter.initialization().data.is_empty());

        let converted = adapter.convert(packet(vec![0xaa; 300])).unwrap();
        // Profile 1, index 4, channels 2, and a frame length of 307.
        assert_eq!(
            converted.data[..7],
            [0xff, 0xf1, 0x50, 0x80, 0x26, 0x7f, 0xfc]
        );
        assert_eq!(converted.data[7..], [0xaa; 300]);
        assert_eq!(converted.trim_start, 1_024);

        assert!(matches!(
            adapter.convert(packet(vec![0; ADTS_MAX_FRAME])),
            Err(Error::MalformedDecoderInput { reason, .. })
                if reason == "a 8191-byte access unit is too long for an ADTS frame"
        ));
    }

    #[test]
    fn adts_describes_the_core_of_he_aac() {
        // Explicit HE-AAC: SBR, 24 kHz core (index 6), stereo, 48 kHz output, LC core.
        let adapter =
            AacAdapter::new(&aac(&[0x2b, 0x11, 0x88, 0x00]), DecoderFormat::AacAdts).unwrap();
        assert_eq!(
            adapter.adts,
            Some(AdtsFields {
                profile: 1,
                frequency_index: 6,
                channels: 2
            })
        );
    }

    #[test]
    fn adts_refuses_what_it_cannot_carry() {
        let reason = |config: &[u8]| match AacAdapter::new(&aac(config), DecoderFormat::AacAdts) {
            Err(Error::UnsupportedDecoderInput { reason, .. }) => reason,
            Err(Error::MalformedDecoderInput { reason, .. }) => reason,
            other => panic!("{other:?}"),
        };
        // ER AAC-LD, object type 23.
        assert_eq!(
            reason(&[0xb9, 0x90]),
            "ADTS cannot carry audio object type 23"
        );
        // A program config element's layout, channel configuration 0.
        assert_eq!(
            reason(&[0x12, 0x00]),
            "ADTS cannot carry channel configuration 0"
        );
        // An explicit frequency, index 15.
        assert_eq!(
            reason(&[0x17, 0x80, 0x00, 0x00, 0x10]),
            "ADTS cannot carry an explicit sampling frequency"
        );
        assert_eq!(reason(&[0x12]), "truncated AudioSpecificConfig");

        let raw = AacAdapter::new(&aac(&[]), DecoderFormat::AacRaw);
        assert!(matches!(
            raw,
            Err(Error::MalformedDecoderInput { reason, .. })
                if reason == "mp4a has no AudioSpecificConfig"
        ));
        assert!(matches!(
            AacAdapter::new(&aac(&[0x12, 0x10]), DecoderFormat::Mp3),
            Err(Error::UnsupportedDecoderInput { reason, .. })
                if reason == "AAC converts only to raw AAC or ADTS AAC"
        ));
    }
}
