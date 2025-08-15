use std::ffi::{c_int, c_void};
use std::ptr::NonNull;
use std::sync::Weak;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use objc2_core_foundation::{
    CFBoolean, CFData, CFMutableDictionary, CFNumber, CFNumberType, CFRetained, CFString, CFType,
};
use objc2_core_media::{
    kCMVideoCodecType_H264, CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo,
    CMTime, CMVideoFormatDescription, CMVideoFormatDescriptionCreate,
};
use objc2_core_video::{
    kCVPixelBufferHeightKey, kCVPixelBufferMetalCompatibilityKey, kCVPixelBufferPixelFormatTypeKey,
    kCVPixelBufferWidthKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVImageBuffer,
    CVPixelBuffer,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
    VTDecompressionSession,
};

use crate::codec::video::{VideoFrame, VideoFrameMut};
use crate::codec::CodecError;
use crate::packet::Packet;
use crate::time::{TimeBase, Timestamp};
use crate::{codec::VideoCodecParameters, Error};

pub struct DecodedFrame {
    pixel_buffer: *mut CVPixelBuffer,
    presentation_timestamp: CMTime,
    duration: CMTime,
}

pub struct CallbackContext {
    decoded_frames: Weak<Mutex<VecDeque<DecodedFrame>>>,
}

pub struct VTDecoder {
    session: NonNull<*mut VTDecompressionSession>,
    format_description: NonNull<*const CMFormatDescription>,
    decoded_frames: Arc<Mutex<VecDeque<DecodedFrame>>>,
    width: u32,
    height: u32,
    time_base: TimeBase,
}

extern "C" {
    pub(crate) fn ffw_frame_new_videotoolbox(
        pixel_buffer: *const c_void,
        width: c_int,
        height: c_int,
        duration_ts: i64,
    ) -> *mut c_void;
}

impl VTDecoder {
    pub fn is_supported(params: &VideoCodecParameters) -> bool {
        match params.decoder_name() {
            Some("h264") | Some("h265") | Some("hevc") | Some("vp9") => true,
            _ => false,
        }
    }

    pub fn new(
        width: u32,
        height: u32,
        extradata: &[u8],
        time_base: TimeBase,
    ) -> Result<VTDecoder, Error> {
        let par = CFMutableDictionary::empty();
        let atoms = CFMutableDictionary::empty();
        let extensions: CFRetained<CFMutableDictionary<_, CFType>> = CFMutableDictionary::empty();

        par.set(&*CFString::from_str("HorizontalSpacing"), &*cf_i32(0)?);
        par.set(&*CFString::from_str("VerticalSpacing"), &*cf_i32(0)?);

        let extradata = CFData::from_bytes(extradata);
        atoms.set(&*CFString::from_str("avcC"), &*extradata);

        extensions.set(
            &*CFString::from_str("CVImageBufferChromaLocationBottomField"),
            &*CFString::from_str("left"),
        );
        extensions.set(
            &*CFString::from_str("CVImageBufferChromaLocationTopField"),
            &*CFString::from_str("left"),
        );
        extensions.set(
            &*CFString::from_str("FullRangeVideo"),
            &*CFBoolean::new(false),
        );
        extensions.set(&*CFString::from_str("CVPixelAspectRatio"), &*par);
        extensions.set(
            &*CFString::from_str("SampleDescriptionExtensionAtoms"),
            &*atoms,
        );

        // Allocate space for the output pointer
        let mut format_description_ptr: *const CMVideoFormatDescription = std::ptr::null();

        // Create a NonNull pointing to the location of our pointer variable
        let format_description =
            NonNull::new(&mut format_description_ptr as *mut *const CMVideoFormatDescription)
                .expect("pointer to stack variable should never be null");

        let status = unsafe {
            CMVideoFormatDescriptionCreate(
                None,
                kCMVideoCodecType_H264,
                width as i32,
                height as i32,
                Some(&*extensions.as_opaque()),
                format_description,
            )
        };

        if status != 0 {
            return Err(Error::new(format!(
                "Failed to create format description. Error: {}",
                status
            )));
        }

        // Create destination pixel buffer attributes
        let pixbuf_attrs: CFRetained<CFMutableDictionary<_, CFType>> = CFMutableDictionary::empty();
        unsafe {
            pixbuf_attrs.set(
                kCVPixelBufferPixelFormatTypeKey,
                &*cf_i32(kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange as i32)?,
            );
            pixbuf_attrs.set(kCVPixelBufferWidthKey, &*cf_i32(width as i32)?);
            pixbuf_attrs.set(kCVPixelBufferHeightKey, &*cf_i32(height as i32)?);
            pixbuf_attrs.set(kCVPixelBufferMetalCompatibilityKey, CFBoolean::new(true));
        }

        // Setup output callback
        let decoded_frame_queue = Arc::new(Mutex::new(VecDeque::<DecodedFrame>::new()));
        let ctx = CallbackContext {
            decoded_frames: Arc::downgrade(&decoded_frame_queue),
        };
        let output_callback = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(decode_completion_callback),
            decompressionOutputRefCon: &ctx as *const _ as *mut c_void,
        };

        let mut decompress_session_ptr: *mut VTDecompressionSession = std::ptr::null_mut();
        let decompress_session =
            NonNull::new(&mut decompress_session_ptr as *mut *mut VTDecompressionSession)
                .expect("pointer to stack variable should never be null");
        let status = unsafe {
            VTDecompressionSession::create(
                None,
                &**format_description.as_ref(),
                None,
                Some(&*pixbuf_attrs.as_opaque()),
                &output_callback,
                decompress_session,
            )
        };

        if status != 0 {
            return Err(Error::new(format!(
                "Failed to create decompression session. Error: {}",
                status
            )));
        }

        Ok(VTDecoder {
            session: decompress_session,
            decoded_frames: decoded_frame_queue,
            format_description,
            width,
            height,
            time_base,
        })
    }

    pub fn try_push(&mut self, packet: Packet) -> Result<(), CodecError> {
        self.decode_frame(
            packet.data(),
            packet.pts(),
            packet.dts(),
            packet.duration(),
            packet.is_discard(),
        )
    }

    pub fn try_flush(&mut self) -> Result<(), CodecError> {
        let t0 = Timestamp::new(0, TimeBase::MICROSECONDS);
        // Push an empty packet
        self.decode_frame(&[], t0, t0, t0, false)
    }

    fn decode_frame(
        &mut self,
        data: &[u8],
        pts: Timestamp,
        dts: Timestamp,
        duration: Timestamp,
        is_discard: bool,
    ) -> Result<(), CodecError> {
        // Create the CMBlockBuffer first
        let mut block_buffer_ptr: *mut CMBlockBuffer = std::ptr::null_mut();
        let block_buffer = NonNull::new(&mut block_buffer_ptr as *mut *mut CMBlockBuffer)
            .expect("pointer to stack variable should never be null");
        let status = unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                data as *const _ as _,
                data.len(),
                None,
                std::ptr::null(),
                0,
                data.len(),
                0,
                block_buffer,
            )
        };
        if status != 0 {
            return Err(CodecError::error(format!(
                "Failed to create block buffer: {}",
                status
            )));
        }

        // Create timing info
        let timing_info = CMSampleTimingInfo {
            presentationTimeStamp: pts.into(),
            decodeTimeStamp: dts.into(),
            duration: duration.into(),
        };

        // Create a sample buffer from the block buffer
        let mut sample_buffer_ptr: *mut CMSampleBuffer = std::ptr::null_mut();
        let sample_buffer = NonNull::new(&mut sample_buffer_ptr as *mut *mut CMSampleBuffer)
            .expect("pointer to stack variable should never be null");
        let status = unsafe {
            CMSampleBuffer::create(
                None,
                Some(&**block_buffer.as_ref()),
                true,
                None,
                std::ptr::null_mut(),
                Some(&**self.format_description.as_ptr()),
                1,
                1,
                &timing_info as *const _,
                0,
                std::ptr::null(),
                sample_buffer,
            )
        };
        if status != 0 {
            return Err(CodecError::error(format!(
                "Failed to create sample buffer: {}",
                status
            )));
        }

        let mut decode_flags = VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression;
        if is_discard {
            decode_flags = decode_flags | VTDecodeFrameFlags::Frame_DoNotOutputFrame;
        }
        let mut info_flags = VTDecodeInfoFlags::empty();

        let status = unsafe {
            VTDecompressionSession::decode_frame(
                &**self.session.as_ref(),
                &**sample_buffer.as_ref(),
                decode_flags,
                std::ptr::null_mut(),
                &mut info_flags as *mut _,
            )
        };
        if status != 0 {
            return Err(CodecError::error(format!(
                "Failed to decode frame: {}",
                status
            )));
        }

        Ok(())
    }

    pub fn take(&mut self) -> Result<Option<VideoFrame>, Error> {
        let mut frames = self.decoded_frames.lock().unwrap();
        let width = self.width;
        let height = self.height;
        Ok(frames.pop_front().map(|frame| {
            let pixel_buffer = unsafe { &*frame.pixel_buffer };
            let pts = cmtime_to_timestamp(frame.presentation_timestamp);

            VideoFrameMut::from_videotoolbox(pixel_buffer, width, height, frame.duration.value)
                .with_time_base(self.time_base)
                .with_pts(pts)
                .freeze()
        }))
    }

    pub fn flush_buffers(&mut self) {
        unsafe { VTDecompressionSession::wait_for_asynchronous_frames(&**self.session.as_ref()) };
    }
}

impl From<Timestamp> for CMTime {
    fn from(value: Timestamp) -> Self {
        timestamp_to_cmtime(value)
    }
}

impl From<CMTime> for Timestamp {
    fn from(value: CMTime) -> Self {
        cmtime_to_timestamp(value)
    }
}

fn cmtime_to_timestamp(time: CMTime) -> Timestamp {
    Timestamp::new(time.value, TimeBase::new(1, time.timescale as u32))
}

fn timestamp_to_cmtime(time: Timestamp) -> CMTime {
    unsafe {
        CMTime::new(
            time.timestamp() * time.time_base().num() as i64,
            time.time_base().den() as i32,
        )
    }
}

unsafe extern "C-unwind" fn decode_completion_callback(
    output_callback_refcon: *mut c_void,
    _source_frame_refcon: *mut c_void,
    _status: i32, // OSStatus is pub(crate)
    _flags: VTDecodeInfoFlags,
    image_buffer: *mut CVImageBuffer,
    presentation_timestamp: CMTime,
    duration: CMTime,
) {
    // Skip this frame if it's null. We'll get these for frames marked DoNotDisplay
    if image_buffer.is_null() {
        return;
    }

    let ctx = &*(output_callback_refcon as *const CallbackContext);

    // Save the decoded frame to the queue
    if let Some(decoded_frames) = ctx.decoded_frames.upgrade() {
        let decoded_frame = DecodedFrame {
            pixel_buffer: image_buffer,
            presentation_timestamp,
            duration,
        };
        decoded_frames.lock().unwrap().push_back(decoded_frame);
    }
}

fn cf_i32(value: i32) -> Result<CFRetained<CFNumber>, crate::Error> {
    unsafe {
        CFNumber::new(None, CFNumberType::SInt32Type, &value as *const i32 as _)
            .ok_or_else(|| Error::new("Could not create CFNumber"))
    }
}
