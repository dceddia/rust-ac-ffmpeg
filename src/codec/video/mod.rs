//! Video decoder/encoder.

pub mod color;
pub mod frame;
mod frame_order;
#[allow(non_camel_case_types)]
pub mod pixel;
pub mod scaler;

#[cfg(target_os = "macos")]
pub mod videotoolbox;

use log::{debug, info, warn};
use std::{
    env,
    ffi::{CStr, CString},
    os::raw::c_void,
    ptr,
    sync::OnceLock,
};

use crate::{
    codec::{CodecError, CodecParameters, Decoder, Encoder, VideoCodecParameters},
    format::stream::Stream,
    packet::Packet,
    time::{TimeBase, Timestamp},
    Error,
};

use self::frame_order::FrameOrder;

pub use self::{
    frame::{
        Chromaticity, ContentLightLevel, DolbyVisionMetadata, HdrMetadata,
        MasteringDisplayMetadata, PixelFormat, SampleAspectRatio, VideoFrame, VideoFrameMut,
    },
    scaler::{VideoFrameScaler, VideoFrameScalerBuilder},
};

use super::ThreadType;

pub use self::color::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HardwareAccel {
    Disabled,
    Optional,
    Required,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeBackend {
    Software,
    #[cfg(target_os = "macos")]
    VideoToolbox,
    FfmpegHardware,
}

impl DecodeBackend {
    pub fn is_hardware_accelerated(self) -> bool {
        !matches!(self, Self::Software)
    }
}

/// The device hardware decoders decode on from now on (D3D11VA: a DXGI adapter's index, as
/// `IDXGIFactory1::EnumAdapters1` lists them), `None` for the system's default. The decoders
/// share one device, made when the first needs it after this changes.
#[cfg(target_os = "windows")]
pub fn set_hardware_device(name: Option<&str>) {
    let name = name.map(|name| CString::new(name).expect("invalid device name"));
    unsafe {
        super::ffw_set_hwaccel_device(name.as_ref().map_or(ptr::null(), |name| name.as_ptr()))
    };
}

fn reorder_debug_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| env::var_os("RECUT_DEBUG_REORDER").is_some())
}

/// Builder for the video decoder.
pub struct VideoDecoderBuilder {
    ptr: *mut c_void,
    time_base: TimeBase,
    rotation: f64,
    hardware_accel: HardwareAccel,

    enable_reordering: bool,
    decode_backend: DecodeBackend,
    thread_count: Option<i32>,

    #[cfg(target_os = "macos")]
    vt_decoder: Option<videotoolbox::VTDecoder>,
}

impl VideoDecoderBuilder {
    /// Create a new decoder builder from a given raw representation.
    unsafe fn from_raw_ptr(ptr: *mut c_void) -> Self {
        let time_base = TimeBase::MICROSECONDS;

        super::ffw_decoder_set_pkt_timebase(ptr, time_base.num() as _, time_base.den() as _);

        Self {
            ptr,
            time_base,
            rotation: 0.0,
            hardware_accel: HardwareAccel::Disabled,
            enable_reordering: false,
            decode_backend: DecodeBackend::Software,
            thread_count: None,
            #[cfg(target_os = "macos")]
            vt_decoder: None,
        }
    }

    /// Create a new builder for a given codec.
    fn new(codec: &str) -> Result<Self, Error> {
        let codec_str = CString::new(codec).expect("invalid codec name");

        let ptr = unsafe { super::ffw_decoder_new(codec_str.as_ptr() as _) };

        if ptr.is_null() {
            return Err(Error::new(format!("unknown codec {}", codec)));
        }

        unsafe { Ok(Self::from_raw_ptr(ptr)) }
    }

    /// Create a new builder from given codec parameters.
    fn from_codec_parameters(codec_parameters: &VideoCodecParameters) -> Result<Self, Error> {
        let ptr = unsafe { super::ffw_decoder_from_codec_parameters(codec_parameters.as_ptr()) };

        if ptr.is_null() {
            return Err(Error::new("unable to create a decoder"));
        }

        unsafe { Ok(Self::from_raw_ptr(ptr)) }
    }

    /// Set a decoder option.
    pub fn set_option<V>(self, name: &str, value: V) -> Self
    where
        V: ToString,
    {
        let name = CString::new(name).expect("invalid option name");
        let value = CString::new(value.to_string()).expect("invalid option value");

        let ret = unsafe {
            super::ffw_decoder_set_initial_option(self.ptr, name.as_ptr() as _, value.as_ptr() as _)
        };

        if ret < 0 {
            panic!("unable to allocate an option");
        }

        self
    }

    /// Set decoder time base (all input packets will be rescaled into this
    /// time base). The default time base is in microseconds.
    pub fn time_base(mut self, time_base: TimeBase) -> Self {
        self.time_base = time_base;

        unsafe {
            super::ffw_decoder_set_pkt_timebase(
                self.ptr,
                time_base.num() as _,
                time_base.den() as _,
            );
        }

        self
    }

    /// Set codec extradata.
    pub fn extradata<T>(self, data: Option<T>) -> Self
    where
        T: AsRef<[u8]>,
    {
        let data = data.as_ref().map(|d| d.as_ref());

        let ptr;
        let size;

        if let Some(data) = data {
            ptr = data.as_ptr();
            size = data.len();
        } else {
            ptr = ptr::null();
            size = 0;
        }

        let res = unsafe { super::ffw_decoder_set_extradata(self.ptr, ptr, size as _) };

        if res < 0 {
            panic!("unable to allocate extradata");
        }

        self
    }

    /// Set the number of threads to use while decoding
    /// Default is 1. Set to 0 to let FFmpeg decide how many threads to use.
    pub fn with_thread_count(mut self, count: i32) -> Self {
        self.thread_count = Some(count);
        let _ = unsafe { super::ffw_decoder_set_thread_count(self.ptr, count) };

        self
    }

    /// The thread type to use, either Frame (one thread per frame) or Slice
    /// (break each frame into slices, and use one thread per slice).
    ///
    /// Note: using the Frame type will cause decoded frames to be delayed by
    /// N-1, where N is the number of threads.
    pub fn with_thread_type(self, thread_type: ThreadType) -> Self {
        let thread_type = match thread_type {
            ThreadType::Frame => 1,
            ThreadType::Slice => 2,
        };

        let _ = unsafe { super::ffw_decoder_set_thread_type(self.ptr, thread_type) };

        self
    }

    pub fn with_hardware_accel(mut self, hardware_accel: HardwareAccel) -> Self {
        self.hardware_accel = hardware_accel;
        self
    }

    /// Get the codec parameters.
    pub fn codec_parameters(&self) -> VideoCodecParameters {
        let ptr = unsafe { super::ffw_decoder_get_codec_parameters(self.ptr) };

        if ptr.is_null() {
            panic!("unable to allocate codec parameters");
        }

        let params = unsafe { CodecParameters::from_raw_ptr(ptr) };

        params.into_video_codec_parameters().unwrap()
    }

    /// Set the rotation (in degrees)
    pub fn rotation(mut self, degrees: f64) -> Self {
        self.rotation = degrees;

        self
    }

    /// Give frames in display order, each as soon as no frame shown before it can still come
    /// (see `frame_order`): VideoToolbox gives them in decode order.
    pub fn enable_reordering(mut self) -> Self {
        self.enable_reordering = true;

        self
    }

    /// Disable frame reordering (default behavior).
    pub fn disable_reordering(mut self) -> Self {
        self.enable_reordering = false;

        self
    }

    fn test_env_flag(name: &str) -> bool {
        env::var(name)
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false)
    }

    fn copy_settings_to(&self, mut builder: VideoDecoderBuilder) -> VideoDecoderBuilder {
        builder = builder.time_base(self.time_base).rotation(self.rotation);
        if let Some(thread_count) = self.thread_count {
            builder = builder.with_thread_count(thread_count);
        }
        if self.enable_reordering {
            builder = builder.enable_reordering();
        }
        builder
    }

    fn build_software_from_params(
        &self,
        params: &VideoCodecParameters,
    ) -> Result<VideoDecoder, Error> {
        let builder = self.copy_settings_to(VideoDecoderBuilder::from_codec_parameters(params)?);
        builder.open_and_finish()
    }

    fn build_ffmpeg_hardware_from_params(
        &self,
        params: &VideoCodecParameters,
    ) -> Result<VideoDecoder, Error> {
        if Self::test_env_flag("RECUT_SKIP_FFMPEG_HWACCEL") {
            return Err(Error::new("FFmpeg hardware decode skipped by environment"));
        }

        let mut builder = self.copy_settings_to(VideoDecoderBuilder::from_codec_parameters(params)?);
        let ret = unsafe { super::ffw_decoder_hwaccel_autoselect_device(builder.ptr) };
        if ret != 0 {
            return Err(Error::new(format!("hwaccel init failed: {}", ret)));
        }
        builder.decode_backend = DecodeBackend::FfmpegHardware;
        builder.open_and_finish()
    }

    fn open_and_finish(self) -> Result<VideoDecoder, Error> {
        if unsafe { super::ffw_decoder_open(self.ptr) } != 0 {
            return Err(Error::new("unable to open FFmpeg decoder context"));
        }

        self.finish_build()
    }

    /// Build the decoder.
    ///
    /// When hardware acceleration is optional, tries hardware first and falls back
    /// to software if hardware decoder creation fails.
    pub fn build(mut self) -> Result<VideoDecoder, Error> {
        // Open the FFmpeg decoder. The custom VideoToolbox path still keeps this
        // context around for codec parameters and decoder metadata.
        if unsafe { super::ffw_decoder_open(self.ptr) } != 0 {
            return Err(Error::new("unable to open FFmpeg decoder context"));
        }

        if self.hardware_accel == HardwareAccel::Disabled {
            return self.finish_build();
        }

        #[cfg(target_os = "macos")]
        {
            if !Self::test_env_flag("RECUT_SKIP_CUSTOM_VIDEOTOOLBOX") {
                let params = self.codec_parameters();
                match videotoolbox::VTDecoder::new(&params, self.time_base) {
                    Ok(decoder) => {
                        self.vt_decoder = Some(decoder);
                        self.decode_backend = DecodeBackend::VideoToolbox;
                        return self.finish_build();
                    }
                    Err(e) => {
                        if self.hardware_accel == HardwareAccel::Required {
                            warn!("Custom VideoToolbox decoder init failed: {}", e);
                        } else {
                            debug!("Custom VideoToolbox decoder init failed: {}", e);
                        }
                    }
                }
            }

            let params = self.codec_parameters();
            unsafe { super::ffw_decoder_free(self.ptr) };
            self.ptr = ptr::null_mut();

            match self.build_ffmpeg_hardware_from_params(&params) {
                Ok(decoder) => return Ok(decoder),
                Err(e) => {
                    if self.hardware_accel == HardwareAccel::Required {
                        warn!("FFmpeg hardware decoder init failed: {}", e);
                        return Err(Error::new(format!(
                            "unable to create required hardware decoder: {}",
                            e
                        )));
                    }
                    debug!("FFmpeg hardware decoder init failed: {}", e);
                    return self.build_software_from_params(&params);
                }
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            let params = self.codec_parameters();

            #[cfg(target_os = "windows")]
            {
                unsafe { super::ffw_decoder_free(self.ptr) };
                self.ptr = ptr::null_mut();

                match self.build_ffmpeg_hardware_from_params(&params) {
                    Ok(decoder) => return Ok(decoder),
                    Err(e) => {
                        if self.hardware_accel == HardwareAccel::Required {
                            warn!("FFmpeg hardware decoder init failed: {}", e);
                            return Err(Error::new(format!(
                                "unable to create required hardware decoder: {}",
                                e
                            )));
                        }
                        debug!("FFmpeg hardware decoder init failed: {}", e);
                        return self.build_software_from_params(&params);
                    }
                }
            }

            #[cfg(not(target_os = "windows"))]
            {
                if self.hardware_accel == HardwareAccel::Required {
                    return Err(Error::new("hardware decoding is not supported on this platform"));
                }

                unsafe { super::ffw_decoder_free(self.ptr) };
                self.ptr = ptr::null_mut();
                self.build_software_from_params(&params)
            }
        }
    }

    fn finish_build(mut self) -> Result<VideoDecoder, Error> {
        let ptr = self.ptr;

        // FFmpeg's decoders give frames in display order; VideoToolbox gives them as it
        // decodes them
        #[cfg(target_os = "macos")]
        let in_display_order = self.vt_decoder.is_none();
        #[cfg(not(target_os = "macos"))]
        let in_display_order = true;

        let order = if self.enable_reordering {
            if reorder_debug_enabled() {
                info!("VideoDecoder frame order: decoder in display order: {in_display_order}");
            }
            Some(FrameOrder::new(in_display_order))
        } else {
            None
        };

        self.ptr = ptr::null_mut();

        Ok(VideoDecoder {
            ptr,
            time_base: self.time_base,
            rotation: self.rotation,
            order,
            last_released_pts: None,
            output_empty: true,
            decode_backend: self.decode_backend,

            #[cfg(target_os = "macos")]
            vt_decoder: self.vt_decoder.take(),
        })
    }
}

impl Drop for VideoDecoderBuilder {
    fn drop(&mut self) {
        unsafe { super::ffw_decoder_free(self.ptr) }
    }
}

unsafe impl Send for VideoDecoderBuilder {}
unsafe impl Sync for VideoDecoderBuilder {}

/// Video decoder.
pub struct VideoDecoder {
    ptr: *mut c_void,
    time_base: TimeBase,

    /// The rotation of the underlying stream, in degrees.
    rotation: f64,

    /// Puts the frames in display order (`enable_reordering`)
    order: Option<FrameOrder<VideoFrame>>,
    /// The PTS of the last frame given out, to log one given out of order
    last_released_pts: Option<Timestamp>,
    /// The decoder had no frame to give at the last take (see `output_empty()`)
    output_empty: bool,
    decode_backend: DecodeBackend,

    #[cfg(target_os = "macos")]
    vt_decoder: Option<videotoolbox::VTDecoder>,
}

impl VideoDecoder {
    fn record_released_frame(&mut self, frame: &VideoFrame) {
        if let Some(last_pts) = self.last_released_pts {
            if frame.pts() < last_pts {
                info!(
                    "Frame order: gave pts {} after {}",
                    frame.pts().timestamp(),
                    last_pts.timestamp()
                );
            }
        }
        self.last_released_pts = Some(frame.pts());

        if reorder_debug_enabled() {
            info!("Frame order: gave pts {}", frame.pts().timestamp());
        }
    }

    /// A packet went into the decoder: its frame is waited for (unless it's not to be shown)
    fn note_pushed(&mut self, pts: Timestamp, dts: Timestamp, is_discard: bool) {
        if let Some(order) = &mut self.order {
            let known = |time: Timestamp| (!time.is_null()).then(|| time.timestamp());
            order.pushed(known(pts), known(dts), !is_discard);
        }
    }

    /// Create a new video decoder for a given codec.
    pub fn new(codec: &str) -> Result<Self, Error> {
        VideoDecoderBuilder::new(codec).and_then(|builder| builder.build())
    }

    /// Create a new video decoder builder from given codec parameters.
    pub fn from_codec_parameters(
        codec_parameters: &VideoCodecParameters,
    ) -> Result<VideoDecoderBuilder, Error> {
        VideoDecoderBuilder::from_codec_parameters(codec_parameters)
    }

    /// Create a new decoder for a given stream.
    ///
    /// Returns an error if this is not a video stream.
    pub fn from_stream(stream: &Stream) -> Result<VideoDecoderBuilder, Error> {
        let codec_parameters = stream
            .codec_parameters()
            .into_video_codec_parameters()
            .ok_or_else(|| {
                Error::new(format!(
                    "failed to create VideoDecoder from stream {} - not a video stream",
                    stream.index()
                ))
            })?;

        let builder = VideoDecoderBuilder::from_codec_parameters(&codec_parameters)?
            .time_base(stream.time_base())
            .rotation(stream.rotation());

        Ok(builder)
    }

    /// Get decoder builder for a given codec.
    pub fn builder(codec: &str) -> Result<VideoDecoderBuilder, Error> {
        VideoDecoderBuilder::new(codec)
    }

    pub fn set_discard(&mut self, discard: std::ffi::c_int) -> Result<(), Error> {
        unsafe {
            super::ffw_decoder_set_discard(self.ptr, discard);
            Ok(())
        }
    }

    /// Forget the frames waiting for their turn and the packets they wait for, as after a seek
    fn clear_frame_order(&mut self) {
        if let Some(order) = &mut self.order {
            order.clear();
        }
        self.last_released_pts = None;
    }

    pub fn decode_backend(&self) -> DecodeBackend {
        self.decode_backend
    }

    /// Whether the next frame (`enable_reordering`) is still being decoded and needs no more
    /// packets to come out: its packet went in, and every packet that could show a frame before
    /// it. Then `wait_for_decoder` brings it out; more packets don't bring it sooner, they only
    /// give the decoder more to do first. Only VideoToolbox decodes asynchronously like this:
    /// FFmpeg's decoders give a frame for more packets.
    pub fn next_frame_is_decoding(&self) -> bool {
        #[cfg(target_os = "macos")]
        if self.vt_decoder.is_some() {
            return self
                .order
                .as_ref()
                .is_some_and(|order| order.next_is_decoding());
        }
        false
    }

    /// Waits until the decoder has something for the next `take()` (a frame, or word that a
    /// packet won't give one), for at most a second. False if it didn't. VideoToolbox only:
    /// FFmpeg's decoders give their frames as they decode them, so they never have one coming.
    pub fn wait_for_decoder(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(vt_decoder) = &mut self.vt_decoder {
            return vt_decoder.wait_for_result(std::time::Duration::from_secs(1));
        }
        false
    }

    pub fn push_with_reset(&mut self, packet: Packet) -> Result<(), Error> {
        self.clear_frame_order();

        #[cfg(target_os = "macos")]
        if let Some(vt_decoder) = &mut self.vt_decoder {
            let packet = packet.with_time_base(self.time_base);
            let (pts, dts, is_discard) = (packet.pts(), packet.dts(), packet.is_discard());

            vt_decoder
                .push_with_reset(packet)
                .map_err(|err| err.unwrap_inner())?;
            self.note_pushed(pts, dts, is_discard);
            return Ok(());
        }

        self.push(packet)
    }
}

impl Decoder for VideoDecoder {
    type CodecParameters = VideoCodecParameters;
    type Frame = VideoFrame;

    fn codec_parameters(&self) -> VideoCodecParameters {
        let ptr = unsafe { super::ffw_decoder_get_codec_parameters(self.ptr) };

        if ptr.is_null() {
            panic!("unable to allocate codec parameters");
        }

        let params = unsafe { CodecParameters::from_raw_ptr(ptr) };

        params.into_video_codec_parameters().unwrap()
    }

    fn try_push(&mut self, packet: Packet) -> Result<(), CodecError> {
        let packet = packet.with_time_base(self.time_base);
        let (pts, dts, is_discard) = (packet.pts(), packet.dts(), packet.is_discard());

        #[cfg(target_os = "macos")]
        if let Some(vt_decoder) = &mut self.vt_decoder {
            vt_decoder.try_push(packet)?;
            self.note_pushed(pts, dts, is_discard);
            return Ok(());
        }

        unsafe {
            match super::ffw_decoder_push_packet(self.ptr, packet.as_ptr()) {
                1 => {
                    self.note_pushed(pts, dts, is_discard);
                    Ok(())
                }
                0 => Err(CodecError::again(
                    "all frames must be consumed before pushing a new packet",
                )),
                e => Err(CodecError::from_raw_error_code(e)),
            }
        }
    }

    fn try_flush(&mut self) -> Result<(), CodecError> {
        // The end: no packet is coming to show a frame before the ones waiting
        if let Some(order) = &mut self.order {
            order.flushed();
        }

        #[cfg(target_os = "macos")]
        if let Some(vt_decoder) = &mut self.vt_decoder {
            return vt_decoder.try_flush();
        }

        unsafe {
            match super::ffw_decoder_push_packet(self.ptr, ptr::null()) {
                1 => Ok(()),
                0 => Err(CodecError::again(
                    "all frames must be consumed before flushing",
                )),
                e => Err(CodecError::from_raw_error_code(e)),
            }
        }
    }

    fn flush_buffers(&mut self) {
        self.clear_frame_order();
        self.output_empty = true;

        #[cfg(target_os = "macos")]
        if let Some(vt_decoder) = &mut self.vt_decoder {
            return vt_decoder.flush_buffers();
        }

        unsafe { super::ffw_decoder_flush_buffers(self.ptr) }
    }

    /// The next frame. With `enable_reordering`, the next in display order, once no frame shown
    /// before it can still come; takes what the decoder has until then.
    fn take(&mut self) -> Result<Option<VideoFrame>, Error> {
        if self.order.is_none() {
            return self.take_frame_inner();
        }

        loop {
            // At the end, every frame the decoder still has goes in first: they can come in any
            // order
            let draining = self.order.as_ref().is_some_and(|order| order.is_draining());
            if !draining {
                if let Some(frame) = self.release_frame() {
                    return Ok(Some(frame));
                }
            }

            match self.take_output()? {
                Some(DecoderOutput::Frame(frame)) => {
                    let pts = frame.pts().timestamp();
                    if reorder_debug_enabled() {
                        info!("Frame order: decoded pts {pts}");
                    }
                    self.order.as_mut().unwrap().decoded(pts, frame);
                }
                Some(DecoderOutput::NoFrame(pts)) => {
                    self.order.as_mut().unwrap().not_decoded(pts);
                }
                None => return Ok(self.release_frame()),
            }
        }
    }
}

/// What a decoder gave at a take
enum DecoderOutput {
    Frame(VideoFrame),
    /// The packet with this PTS won't give a frame (VideoToolbox skipped it)
    NoFrame(i64),
}

impl VideoDecoder {
    /// The next frame in display order, if it's known to be next
    fn release_frame(&mut self) -> Option<VideoFrame> {
        let frame = self.order.as_mut()?.pop()?;
        self.record_released_frame(&frame);
        Some(frame)
    }

    /// Whether the decoder itself had no frame to give at the last `take()` (FFmpeg's receive
    /// gave EAGAIN or EOF). Frames waiting for their turn (`enable_reordering`) don't count: this
    /// is what FFmpeg means by its output being read, after which a packet `try_push()` refused
    /// (EAGAIN) is to be pushed again, and won't be refused.
    pub fn output_empty(&self) -> bool {
        self.output_empty
    }

    /// Get a frame from the underlying decoder
    fn take_frame_inner(&mut self) -> Result<Option<VideoFrame>, Error> {
        loop {
            match self.take_output()? {
                Some(DecoderOutput::Frame(frame)) => return Ok(Some(frame)),
                Some(DecoderOutput::NoFrame(_)) => continue,
                None => return Ok(None),
            }
        }
    }

    /// What the underlying decoder has next: a frame, or word that a packet won't give one
    fn take_output(&mut self) -> Result<Option<DecoderOutput>, Error> {
        let output = self.receive_output();
        self.output_empty = matches!(output, Ok(None));
        output
    }

    fn receive_output(&mut self) -> Result<Option<DecoderOutput>, Error> {
        #[cfg(target_os = "macos")]
        if let Some(vt_decoder) = &mut self.vt_decoder {
            use videotoolbox::DecodeResult;

            return match vt_decoder.take_result()? {
                Some(DecodeResult::Frame(frame)) => {
                    let frame = frame.into_raw_ptr();
                    let frame =
                        unsafe { VideoFrame::from_raw_ptr(frame, self.time_base, self.rotation) };
                    Ok(Some(DecoderOutput::Frame(frame)))
                }
                Some(DecodeResult::Skipped { pts }) => Ok(Some(DecoderOutput::NoFrame(pts))),
                Some(DecodeResult::Error { status, pts }) => {
                    if let Some(order) = &mut self.order {
                        order.not_decoded(pts);
                    }
                    Err(videotoolbox::VTDecoder::decode_error(status, pts))
                }
                None => Ok(None),
            };
        }

        let mut fptr = ptr::null_mut();

        unsafe {
            match super::ffw_decoder_take_frame(self.ptr, &mut fptr) {
                1 => {
                    if fptr.is_null() {
                        panic!("no frame received")
                    } else {
                        let frame = VideoFrame::from_raw_ptr(fptr, self.time_base, self.rotation);
                        Ok(Some(DecoderOutput::Frame(frame)))
                    }
                }
                0 => Ok(None),
                e => Err(Error::from_raw_error_code(e)),
            }
        }
    }
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        unsafe { super::ffw_decoder_free(self.ptr) }
    }
}

unsafe impl Send for VideoDecoder {}
unsafe impl Sync for VideoDecoder {}

/// Builder for the video encoder.
pub struct VideoEncoderBuilder {
    ptr: *mut c_void,

    time_base: TimeBase,

    format: Option<PixelFormat>,
    width: Option<usize>,
    height: Option<usize>,
    max_b_frames: Option<usize>,
    color_range: u32,
    color_space: u32,
    color_transfer: u32,
    color_primaries: u32,
    chroma_location: u32,
}

impl VideoEncoderBuilder {
    /// Create a new encoder builder for a given codec.
    fn new(codec: &str) -> Result<Self, Error> {
        let codec_str = CString::new(codec).expect("invalid codec name");

        let ptr = unsafe { super::ffw_encoder_new(codec_str.as_ptr() as _) };

        if ptr.is_null() {
            return Err(Error::new(format!("unknown codec {}", codec)));
        }

        unsafe {
            super::ffw_encoder_set_bit_rate(ptr, 0);
        }

        let res = Self {
            ptr,

            time_base: TimeBase::MICROSECONDS,

            format: None,
            width: None,
            height: None,
            max_b_frames: None,

            color_range: AVCOL_RANGE_UNSPECIFIED,
            color_space: AVCOL_SPC_UNSPECIFIED,
            color_transfer: AVCOL_TRC_UNSPECIFIED,
            color_primaries: AVCOL_PRI_UNSPECIFIED,
            chroma_location: AVCHROMA_LOC_UNSPECIFIED,
        };

        Ok(res)
    }

    /// Create a new encoder builder from given codec parameters.
    fn from_codec_parameters(codec_parameters: &VideoCodecParameters) -> Result<Self, Error> {
        let ptr = unsafe { super::ffw_encoder_from_codec_parameters(codec_parameters.as_ptr()) };

        if ptr.is_null() {
            return Err(Error::new("unable to create an encoder"));
        }

        let pixel_format;
        let width;
        let height;
        let max_b_frames;
        let color_range;
        let color_space;
        let color_transfer;
        let color_primaries;
        let chroma_location;

        unsafe {
            pixel_format = PixelFormat::from_raw(super::ffw_encoder_get_pixel_format(ptr));
            width = super::ffw_encoder_get_width(ptr) as _;
            height = super::ffw_encoder_get_height(ptr) as _;
            max_b_frames = super::ffw_encoder_get_max_b_frames(ptr) as _;
            color_range = super::ffw_encoder_get_color_range(ptr) as _;
            color_space = super::ffw_encoder_get_color_space(ptr) as _;
            color_transfer = super::ffw_encoder_get_color_transfer(ptr) as _;
            color_primaries = super::ffw_encoder_get_color_primaries(ptr) as _;
            chroma_location = super::ffw_encoder_get_chroma_location(ptr) as _;
        }

        let res = Self {
            ptr,

            time_base: TimeBase::MICROSECONDS,

            format: Some(pixel_format),
            width: Some(width),
            height: Some(height),
            max_b_frames: Some(max_b_frames),

            color_range,
            color_space,
            color_transfer,
            color_primaries,
            chroma_location,
        };

        Ok(res)
    }

    /// Set an encoder option.
    pub fn set_option<V>(self, name: &str, value: V) -> Self
    where
        V: ToString,
    {
        let name = CString::new(name).expect("invalid option name");
        let value = CString::new(value.to_string()).expect("invalid option value");

        let ret = unsafe {
            super::ffw_encoder_set_initial_option(self.ptr, name.as_ptr() as _, value.as_ptr() as _)
        };

        if ret < 0 {
            panic!("unable to allocate an option");
        }

        self
    }

    /// Set encoder bit rate. The default is 0 (i.e. automatic).
    pub fn bit_rate(self, bit_rate: u64) -> Self {
        unsafe {
            super::ffw_encoder_set_bit_rate(self.ptr, bit_rate as _);
        }

        self
    }

    /// Set encoder time base. The default time base is in microseconds.
    pub fn time_base(mut self, time_base: TimeBase) -> Self {
        self.time_base = time_base;
        self
    }

    /// Set encoder pixel format.
    pub fn pixel_format(mut self, format: PixelFormat) -> Self {
        self.format = Some(format);
        self
    }

    /// Set frame width.
    pub fn width(mut self, width: usize) -> Self {
        self.width = Some(width);
        self
    }

    /// Set frame height.
    pub fn height(mut self, height: usize) -> Self {
        self.height = Some(height);
        self
    }

    pub fn color_range(mut self, value: u32) -> Self {
        self.color_range = value;
        self
    }

    pub fn color_space(mut self, value: u32) -> Self {
        self.color_space = value;
        self
    }

    pub fn color_transfer(mut self, value: u32) -> Self {
        self.color_transfer = value;
        self
    }

    pub fn color_primaries(mut self, value: u32) -> Self {
        self.color_primaries = value;
        self
    }

    pub fn chroma_location(mut self, value: u32) -> Self {
        self.chroma_location = value;
        self
    }

    /// Set maximum number of B frames (defaults to 0).
    pub fn max_b_frames(mut self, max: usize) -> Self {
        self.max_b_frames = Some(max);
        self
    }

    /// Build the encoder.
    pub fn build(mut self) -> Result<VideoEncoder, Error> {
        let format = self
            .format
            .ok_or_else(|| Error::new("pixel format not set"))?;

        let width = self.width.ok_or_else(|| Error::new("width not set"))?;
        let height = self.height.ok_or_else(|| Error::new("height not set"))?;
        let max_b_frames = self.max_b_frames.unwrap_or(0);

        let tb = self.time_base;

        unsafe {
            super::ffw_encoder_set_time_base(self.ptr, tb.num() as _, tb.den() as _);
            super::ffw_encoder_set_pixel_format(self.ptr, format.into_raw());
            super::ffw_encoder_set_width(self.ptr, width as _);
            super::ffw_encoder_set_height(self.ptr, height as _);
            super::ffw_encoder_set_max_b_frames(self.ptr, max_b_frames as _);
            super::ffw_encoder_set_color_range(self.ptr, self.color_range as _);
            super::ffw_encoder_set_color_space(self.ptr, self.color_space as _);
            super::ffw_encoder_set_color_transfer(self.ptr, self.color_transfer as _);
            super::ffw_encoder_set_color_primaries(self.ptr, self.color_primaries as _);
            super::ffw_encoder_set_chroma_location(self.ptr, self.chroma_location as _);

            if super::ffw_encoder_open(self.ptr) != 0 {
                return Err(Error::new("unable to build the encoder"));
            }
        }

        let ptr = self.ptr;

        self.ptr = ptr::null_mut();

        let res = VideoEncoder { ptr, time_base: tb };

        Ok(res)
    }
}

impl Drop for VideoEncoderBuilder {
    fn drop(&mut self) {
        unsafe { super::ffw_encoder_free(self.ptr) }
    }
}

unsafe impl Send for VideoEncoderBuilder {}
unsafe impl Sync for VideoEncoderBuilder {}

/// Video encoder.
pub struct VideoEncoder {
    ptr: *mut c_void,
    time_base: TimeBase,
}

impl VideoEncoder {
    /// Create a new encoder from given codec parameters.
    pub fn from_codec_parameters(
        codec_parameters: &VideoCodecParameters,
    ) -> Result<VideoEncoderBuilder, Error> {
        VideoEncoderBuilder::from_codec_parameters(codec_parameters)
    }

    /// Get encoder builder for a given codec.
    pub fn builder(codec: &str) -> Result<VideoEncoderBuilder, Error> {
        VideoEncoderBuilder::new(codec)
    }
}

impl Encoder for VideoEncoder {
    type CodecParameters = VideoCodecParameters;
    type Frame = VideoFrame;

    fn codec_parameters(&self) -> VideoCodecParameters {
        let ptr = unsafe { super::ffw_encoder_get_codec_parameters(self.ptr) };

        if ptr.is_null() {
            panic!("unable to allocate codec parameters");
        }

        let params = unsafe { CodecParameters::from_raw_ptr(ptr) };

        params.into_video_codec_parameters().unwrap()
    }

    fn try_push(&mut self, frame: VideoFrame) -> Result<(), CodecError> {
        let frame = frame.with_time_base(self.time_base);

        unsafe {
            match super::ffw_encoder_push_frame(self.ptr, frame.as_ptr()) {
                1 => Ok(()),
                0 => Err(CodecError::again(
                    "all packets must be consumed before pushing a new frame",
                )),
                e => Err(CodecError::from_raw_error_code(e)),
            }
        }
    }

    fn try_flush(&mut self) -> Result<(), CodecError> {
        unsafe {
            match super::ffw_encoder_push_frame(self.ptr, ptr::null()) {
                1 => Ok(()),
                0 => Err(CodecError::again(
                    "all packets must be consumed before flushing",
                )),
                e => Err(CodecError::from_raw_error_code(e)),
            }
        }
    }

    fn take(&mut self) -> Result<Option<Packet>, Error> {
        let mut pptr = ptr::null_mut();

        unsafe {
            match super::ffw_encoder_take_packet(self.ptr, &mut pptr) {
                1 => {
                    if pptr.is_null() {
                        panic!("no packet received")
                    } else {
                        Ok(Some(Packet::from_raw_ptr(pptr, self.time_base)))
                    }
                }
                0 => Ok(None),
                e => Err(Error::from_raw_error_code(e)),
            }
        }
    }

    fn codec_name(&self) -> Option<&'static str> {
        unsafe {
            let ptr = super::ffw_encoder_get_name(self.ptr);

            if ptr.is_null() {
                None
            } else {
                let name = CStr::from_ptr(ptr as _);

                Some(name.to_str().unwrap())
            }
        }
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        unsafe { super::ffw_encoder_free(self.ptr) }
    }
}

unsafe impl Send for VideoEncoder {}
unsafe impl Sync for VideoEncoder {}
