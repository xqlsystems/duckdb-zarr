//! The `imagecodecs_lzw` bytes-to-bytes codec: TIFF-flavoured LZW.
//!
//! virtual-tiff names the compression of LZW TIFF strips and tiles
//! `imagecodecs_lzw` (the id of the `imagecodecs` package's decoder). zarrs has
//! no LZW codec, so this one is registered here through zarrs' plugin
//! inventory. Decoding uses `weezl`, the pure-Rust LZW used by the `tiff`
//! and `image` crates.
//!
//! The variant is the one TIFF 6.0 section 13 specifies and libtiff writes:
//! MSB-first codes, 8-bit symbols, 9-bit initial code width, and the "early
//! change" code-width switch. Pre-1988 LSB-first LZW TIFFs are not handled.
//! The codec takes no configuration.

use std::borrow::Cow;
use std::sync::Arc;

use zarrs::array::codec::api::{
    BytesToBytesCodecTraits, Codec, CodecError, CodecMetadataOptions, CodecOptions, CodecPluginV2,
    CodecPluginV3, CodecTraits, CodecTraitsV2, CodecTraitsV3, PartialDecoderCapability,
    PartialEncoderCapability, RecommendedConcurrency,
};
use zarrs::array::{ArrayBytesRaw, BytesRepresentation};
use zarrs::metadata::v2::MetadataV2;
use zarrs::metadata::v3::MetadataV3;
use zarrs::metadata::Configuration;
use zarrs::plugin::{PluginCreateError, ZarrVersion};

/// The `imagecodecs_lzw` codec.
#[derive(Clone, Debug, Default)]
pub struct LzwCodec;

const MIN_CODE_SIZE: u8 = 8;

zarrs::plugin::impl_extension_aliases!(LzwCodec,
    v3: "imagecodecs_lzw", ["lzw"],
    v2: "imagecodecs_lzw", ["lzw"]
);

inventory::submit! {
    CodecPluginV3::new::<LzwCodec>()
}

inventory::submit! {
    CodecPluginV2::new::<LzwCodec>()
}

impl CodecTraitsV3 for LzwCodec {
    fn create(_metadata: &MetadataV3) -> Result<Codec, PluginCreateError> {
        Ok(Codec::BytesToBytes(Arc::new(LzwCodec)))
    }
}

impl CodecTraitsV2 for LzwCodec {
    fn create(_metadata: &MetadataV2) -> Result<Codec, PluginCreateError> {
        Ok(Codec::BytesToBytes(Arc::new(LzwCodec)))
    }
}

impl CodecTraits for LzwCodec {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn configuration(
        &self,
        _version: ZarrVersion,
        _options: &CodecMetadataOptions,
    ) -> Option<Configuration> {
        Some(Configuration::default())
    }

    fn partial_decoder_capability(&self) -> PartialDecoderCapability {
        PartialDecoderCapability {
            partial_read: false,
            partial_decode: false,
        }
    }

    fn partial_encoder_capability(&self) -> PartialEncoderCapability {
        PartialEncoderCapability {
            partial_encode: false,
        }
    }
}

fn lzw_error(e: weezl::LzwError) -> CodecError {
    CodecError::Other(format!("lzw: {e}"))
}

impl BytesToBytesCodecTraits for LzwCodec {
    fn into_dyn(self: Arc<Self>) -> Arc<dyn BytesToBytesCodecTraits> {
        self as Arc<dyn BytesToBytesCodecTraits>
    }

    fn recommended_concurrency(
        &self,
        _decoded_representation: &BytesRepresentation,
    ) -> Result<RecommendedConcurrency, CodecError> {
        Ok(RecommendedConcurrency::new_maximum(1))
    }

    fn encode<'a>(
        &self,
        decoded_value: ArrayBytesRaw<'a>,
        _options: &CodecOptions,
    ) -> Result<ArrayBytesRaw<'a>, CodecError> {
        let mut encoder =
            weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, MIN_CODE_SIZE);
        encoder
            .encode(&decoded_value)
            .map(Cow::Owned)
            .map_err(lzw_error)
    }

    fn decode<'a>(
        &self,
        encoded_value: ArrayBytesRaw<'a>,
        _decoded_representation: &BytesRepresentation,
        _options: &CodecOptions,
    ) -> Result<ArrayBytesRaw<'a>, CodecError> {
        let mut decoder =
            weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, MIN_CODE_SIZE);
        decoder
            .decode(&encoded_value)
            .map(Cow::Owned)
            .map_err(lzw_error)
    }

    fn encoded_representation(
        &self,
        _decoded_representation: &BytesRepresentation,
    ) -> BytesRepresentation {
        // LZW can expand incompressible input by up to 12/8, and the stream
        // carries clear and end codes; no useful bound.
        BytesRepresentation::UnboundedSize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codec_options() -> CodecOptions {
        CodecOptions::default()
    }

    #[test]
    fn round_trip() {
        let data: Vec<u8> = (0..4096u32).map(|i| (i % 7 * 31) as u8).collect();
        let encoded = LzwCodec
            .encode(Cow::Borrowed(&data), &codec_options())
            .unwrap();
        assert!(encoded.len() < data.len());
        let decoded = LzwCodec
            .decode(
                encoded,
                &BytesRepresentation::FixedSize(data.len() as u64),
                &codec_options(),
            )
            .unwrap();
        assert_eq!(decoded.as_ref(), data.as_slice());
    }

    /// The bytes libtiff writes for "TOBEORNOTTOBEORTOBEORNOT" (TIFF 6.0
    /// LZW, MSB-first, early change), as produced by imagecodecs.lzw_encode.
    #[test]
    fn decodes_tiff_lzw_stream() {
        let encoded: &[u8] = &[
            0x80, 0x15, 0x09, 0xe4, 0x22, 0x29, 0x3c, 0xa4, 0x4e, 0x27, 0x95, 0x20, 0x50, 0x48,
            0x34, 0x2e, 0x0b, 0x07, 0x84, 0xc0, 0x40,
        ];
        let decoded = LzwCodec
            .decode(
                Cow::Borrowed(encoded),
                &BytesRepresentation::UnboundedSize,
                &codec_options(),
            )
            .unwrap();
        assert_eq!(decoded.as_ref(), b"TOBEORNOTTOBEORTOBEORNOT");
    }

    #[test]
    fn rejects_garbage() {
        let err = LzwCodec.decode(
            Cow::Borrowed(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff]),
            &BytesRepresentation::UnboundedSize,
            &codec_options(),
        );
        assert!(err.is_err());
    }

    #[test]
    fn registered_for_zarr_v2_under_both_ids() {
        for id in ["imagecodecs_lzw", "lzw"] {
            let metadata: MetadataV2 =
                serde_json::from_value(serde_json::json!({ "id": id })).unwrap();
            let codec = <LzwCodec as CodecTraitsV2>::create(&metadata).unwrap();
            assert!(matches!(codec, Codec::BytesToBytes(_)));
        }
    }
}
