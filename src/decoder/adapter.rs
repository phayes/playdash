//! The codec-specific adapter a decoder packet stream converts packets with, chosen once
//! from the track's codec.

use super::{
    AacAdapter, Av1Adapter, DecoderFormat, DecoderInitialization, DecoderPacket, DecoderSeeked,
    DecoderStyle, H264Adapter, HevcAdapter, PassthroughAdapter, unsupported_format,
};
use crate::error::Error;
use crate::mp4::codec::Codec;
use crate::mp4::packet::{Packet, Seeked};

/// What a decoder packet stream was asked to produce.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Choice {
    /// The format of this style for the track's codec.
    Style(DecoderStyle),
    /// Exactly this format.
    Format(DecoderFormat),
}

/// One track's packet converter, for whichever codec it has.
#[derive(Clone, Debug)]
pub(crate) enum Adapter {
    H264(H264Adapter),
    Hevc(HevcAdapter),
    Av1(Av1Adapter),
    Aac(AacAdapter),
    Passthrough(PassthroughAdapter),
}

impl Adapter {
    /// The adapter for `codec` producing what `choice` asks for.
    ///
    /// Fails with [`Error::UnsupportedDecoderInput`] if no adapter reads `codec` or its
    /// adapter cannot produce the format asked for, and with the adapter's own errors.
    pub(crate) fn new(codec: &Codec, choice: Choice) -> Result<Self, Error> {
        // The format asked for, or the one of the asked style among a codec's two.
        let pick = |elementary, config_record| match choice {
            Choice::Format(format) => format,
            Choice::Style(DecoderStyle::Elementary) => elementary,
            Choice::Style(DecoderStyle::ConfigRecord) => config_record,
        };
        let adapter = match codec {
            Codec::H264(_) => Adapter::H264(H264Adapter::new(
                codec,
                pick(DecoderFormat::H264AnnexB, DecoderFormat::H264LengthPrefixed),
            )?),
            Codec::Hevc(_) => Adapter::Hevc(HevcAdapter::new(
                codec,
                pick(DecoderFormat::HevcAnnexB, DecoderFormat::HevcLengthPrefixed),
            )?),
            Codec::Av1(_) => Adapter::Av1(Av1Adapter::new(
                codec,
                pick(DecoderFormat::Av1Obu, DecoderFormat::Av1Samples),
            )?),
            // The AudioSpecificConfig is both AAC's in-band form and its record.
            Codec::Aac(_) => Adapter::Aac(AacAdapter::new(
                codec,
                pick(DecoderFormat::AacRaw, DecoderFormat::AacRaw),
            )?),
            Codec::Unknown(_) => {
                return Err(Error::UnsupportedDecoderInput {
                    codec: codec.name(),
                    format: match choice {
                        Choice::Format(format) => Some(format),
                        Choice::Style(_) => None,
                    },
                    reason: "no decoder-input adapter reads this codec".to_owned(),
                });
            }
            // One format serves both styles.
            _ => Adapter::Passthrough(PassthroughAdapter::new(codec)?),
        };
        let produced = adapter.initialization().format;
        if let Choice::Format(format) = choice
            && format != produced
        {
            return Err(unsupported_format(codec, format, &[produced]));
        }
        Ok(adapter)
    }

    pub(crate) fn initialization(&self) -> &DecoderInitialization {
        match self {
            Adapter::H264(adapter) => adapter.initialization(),
            Adapter::Hevc(adapter) => adapter.initialization(),
            Adapter::Av1(adapter) => adapter.initialization(),
            Adapter::Aac(adapter) => adapter.initialization(),
            Adapter::Passthrough(adapter) => adapter.initialization(),
        }
    }

    pub(crate) fn convert(&self, packet: Packet) -> Result<DecoderPacket, Error> {
        match self {
            Adapter::H264(adapter) => adapter.convert(packet),
            Adapter::Hevc(adapter) => adapter.convert(packet),
            Adapter::Av1(adapter) => adapter.convert(packet),
            Adapter::Aac(adapter) => adapter.convert(packet),
            Adapter::Passthrough(adapter) => Ok(adapter.convert(packet)),
        }
    }

    /// `seeked` with its pre-roll converted.
    pub(crate) fn seeked(&self, seeked: Seeked) -> Result<DecoderSeeked, Error> {
        let Seeked {
            requested,
            landed,
            discard_before,
            preroll,
        } = seeked;
        let preroll = preroll
            .into_iter()
            .map(|packet| self.convert(packet))
            .collect::<Result<_, _>>()?;
        Ok(DecoderSeeked {
            requested,
            landed,
            discard_before,
            preroll,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mp4::codec::UnknownCodec;
    use bytes::Bytes;

    #[test]
    fn unknown_codecs_are_refused_by_name() {
        let codec = Codec::Unknown(UnknownCodec {
            fourcc: *b"mp4v",
            sample_entry: Bytes::new(),
        });
        let error = Adapter::new(&codec, Choice::Style(DecoderStyle::Elementary)).unwrap_err();
        assert!(matches!(
            &error,
            Error::UnsupportedDecoderInput { codec, format: None, .. } if codec == "unknown codec mp4v"
        ));
        assert_eq!(
            error.to_string(),
            "unknown codec mp4v cannot be decoder input in any format: no decoder-input adapter \
             reads this codec"
        );
    }
}
