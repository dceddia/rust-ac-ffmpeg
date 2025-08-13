use std::sync::{Arc, Mutex};

use objc2_core_media::{CMVideoFormatDescription, CMVideoFormatDescriptionCreate};
use objc2_foundation::{
    ns_string, NSData, NSDictionary, NSMutableDictionary, NSObjectNSKeyValueCoding,
};
use objc2_video_toolbox::VTDecompressionSession;

use crate::{codec::VideoCodecParameters, time::TimeBase, Error};

pub struct DecodedFrame {
    pixel_buffer: CVPixelBuffer,
    pts: i64,
}

pub struct VTDecoder {
    session: VTDecompressionSession,
    format_desc: CMVideoFormatDescription,
    decoded_frames: Arc<Mutex<VecDeque<DecodedFrame>>>,
    time_base: TimeBase,
    width: u32,
    height: u32,
}

impl VTDecoder {
    pub fn is_supported(params: &VideoCodecParameters) {
        match params.decoder_name() {
            Some("h264") | Some("h265") | Some("hevc") | Some("vp9") => true,
            _ => false,
        }
    }

    pub fn new(params: &VideoCodecParameters) {
        let format_desc = Self::create_format_description(params);
        let decoder_config = Self::create_decoder_config();
        let session = VTDecompressionSession::new(format_desc.clone()).unwrap();
        let decoded_frames = Arc::new(Mutex::new(VecDeque::new()));
        let time_base = TimeBase::new(1, 1000);
        let width = params.width();
        let height = params.height();

        VTDecoder {
            session,
            format_desc,
            decoded_frames,
            time_base,
            width,
            height,
        }
    }

    fn create_format_description(
        params: &VideoCodecParameters,
    ) -> Result<CMVideoFormatDescription, Error> {
        use objc2_core_media::kCMVideoCodecType_H264;
        use objc2_core_media::kCMVideoCodecType_H265;
        use objc2_core_media::kCMVideoCodecType_HEVC;
        use objc2_core_media::kCMVideoCodecType_VP9;

        let width = params.width();
        let height = params.height();

        let codec_type = match params.decoder_name() {
            Some("h264") => kCMVideoCodecType_H264,
            Some("h265") => kCMVideoCodecType_H265,
            Some("hevc") => kCMVideoCodecType_HEVC,
            Some("vp9") => kCMVideoCodecType_VP9,
            _ => return Err(Error::new("Unsupported codec")),
        };

        let extensions = NSDictionary::new();
        if let Some(extradata) = params.extradata() {
            let data = NSData::with_bytes(extradata);
            unsafe {
                extensions.setValue_forKey(
                    &NSDictionary::from_slices(&[ns_string!("avcC")], &[data]),
                    ns_string!("SampleDescriptionExtensionAtoms"),
                );
            }
        } else {
            return Err(Error::new("Missing extradata"));
        }

        let mut format_description_out = ptr::null_mut();
        unsafe {
            CMVideoFormatDescriptionCreate(
                None,
                codec_type,
                width,
                height,
                extensions,
                &mut format_description_out,
            );
        }
        extensions
    }

    fn create_decoder_config() -> CMVideoDecoderConfiguration {
        // Implementation details for creating decoder configuration
    }
}
