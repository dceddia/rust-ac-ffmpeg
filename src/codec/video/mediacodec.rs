//! Android's hardware video decoders, through the NDK's MediaCodec (`AMediaCodec`): H.264 and
//! HEVC, in ByteBuffer mode (each picture copied out of MediaCodec's output buffer into an
//! ordinary software frame: NV12, YUV420P or P010, whichever the decoder gives).
//!
//! Recut's own, as `videotoolbox` is on Apple's systems, rather than FFmpeg 5.1's
//! `h264_mediacodec`/`hevc_mediacodec`: those reach MediaCodec through its Java API over JNI, and
//! in the Android emulator they opened the hardware decoder, fed it packets and never got a
//! picture back, where the NDK's API decodes the same stream. The NDK's needs no `JavaVM`, so it
//! also runs in a test binary started from `adb shell`, and it can give 10-bit pictures (P010),
//! which FFmpeg 5.1's copy can't. FFmpeg 6 moved its MediaCodec decoders to the NDK too.
//! FFmpeg's are still reachable for comparison on a phone (`RECUT_MEDIACODEC=ffmpeg`, with the
//! `JavaVM` given through [`set_java_vm`]).
//!
//! MediaCodec decodes asynchronously: a packet goes into one of a few input buffers and its
//! picture comes out of an output buffer later, after more packets or not. So a push is refused
//! (EAGAIN) while every input buffer is still in use, though no picture is ready yet: the caller
//! takes and pushes again after a moment. Pictures come out in display order.

use std::{
    collections::VecDeque,
    ffi::{CStr, CString},
    os::raw::{c_char, c_int, c_void},
    ptr,
    time::{Duration, Instant},
};

use log::{debug, info};

use crate::{
    codec::{CodecError, VideoCodecParameters},
    packet::Packet,
    time::{TimeBase, Timestamp},
    Error,
};

use super::VideoFrame;

extern "C" {
    fn av_jni_set_java_vm(vm: *mut c_void, log_ctx: *mut c_void) -> c_int;
    fn av_jni_get_java_vm(log_ctx: *mut c_void) -> *mut c_void;

    fn ffw_mediacodec_pixel_format(color_format: c_int) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn ffw_mediacodec_frame_new(
        data: *const u8,
        size: usize,
        color_format: c_int,
        stride: c_int,
        slice_height: c_int,
        left: c_int,
        top: c_int,
        width: c_int,
        height: c_int,
        params: *const c_void,
        pts: i64,
        duration: i64,
    ) -> *mut c_void;
}

/// The NDK's MediaCodec (media/NdkMediaCodec.h, NdkMediaFormat.h), what this uses of it
mod ndk {
    use std::os::raw::{c_char, c_void};

    pub type AMediaCodec = c_void;
    pub type AMediaFormat = c_void;
    /// 0 is AMEDIA_OK
    pub type MediaStatus = i32;

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct AMediaCodecBufferInfo {
        pub offset: i32,
        pub size: i32,
        pub presentation_time_us: i64,
        pub flags: u32,
    }

    pub const INFO_OUTPUT_BUFFERS_CHANGED: isize = -3;
    pub const INFO_OUTPUT_FORMAT_CHANGED: isize = -2;
    pub const INFO_TRY_AGAIN_LATER: isize = -1;
    pub const BUFFER_FLAG_END_OF_STREAM: u32 = 4;

    #[link(name = "mediandk")]
    extern "C" {
        pub fn AMediaCodec_createDecoderByType(mime_type: *const c_char) -> *mut AMediaCodec;
        pub fn AMediaCodec_delete(codec: *mut AMediaCodec) -> MediaStatus;
        pub fn AMediaCodec_getName(codec: *mut AMediaCodec, name: *mut *mut c_char) -> MediaStatus;
        pub fn AMediaCodec_releaseName(codec: *mut AMediaCodec, name: *mut c_char);
        pub fn AMediaCodec_configure(
            codec: *mut AMediaCodec,
            format: *const AMediaFormat,
            surface: *mut c_void,
            crypto: *mut c_void,
            flags: u32,
        ) -> MediaStatus;
        pub fn AMediaCodec_start(codec: *mut AMediaCodec) -> MediaStatus;
        pub fn AMediaCodec_stop(codec: *mut AMediaCodec) -> MediaStatus;
        pub fn AMediaCodec_flush(codec: *mut AMediaCodec) -> MediaStatus;
        pub fn AMediaCodec_dequeueInputBuffer(codec: *mut AMediaCodec, timeout_us: i64) -> isize;
        pub fn AMediaCodec_getInputBuffer(
            codec: *mut AMediaCodec,
            index: usize,
            size: *mut usize,
        ) -> *mut u8;
        pub fn AMediaCodec_queueInputBuffer(
            codec: *mut AMediaCodec,
            index: usize,
            offset: i64,
            size: usize,
            time_us: u64,
            flags: u32,
        ) -> MediaStatus;
        pub fn AMediaCodec_dequeueOutputBuffer(
            codec: *mut AMediaCodec,
            info: *mut AMediaCodecBufferInfo,
            timeout_us: i64,
        ) -> isize;
        pub fn AMediaCodec_getOutputBuffer(
            codec: *mut AMediaCodec,
            index: usize,
            size: *mut usize,
        ) -> *mut u8;
        pub fn AMediaCodec_releaseOutputBuffer(
            codec: *mut AMediaCodec,
            index: usize,
            render: bool,
        ) -> MediaStatus;
        pub fn AMediaCodec_getOutputFormat(codec: *mut AMediaCodec) -> *mut AMediaFormat;

        pub fn AMediaFormat_new() -> *mut AMediaFormat;
        pub fn AMediaFormat_delete(format: *mut AMediaFormat) -> MediaStatus;
        pub fn AMediaFormat_setString(
            format: *mut AMediaFormat,
            name: *const c_char,
            value: *const c_char,
        );
        pub fn AMediaFormat_setInt32(format: *mut AMediaFormat, name: *const c_char, value: i32);
        pub fn AMediaFormat_setBuffer(
            format: *mut AMediaFormat,
            name: *const c_char,
            data: *const c_void,
            size: usize,
        );
        pub fn AMediaFormat_getInt32(
            format: *mut AMediaFormat,
            name: *const c_char,
            value: *mut i32,
        ) -> bool;
        pub fn AMediaFormat_getRect(
            format: *mut AMediaFormat,
            name: *const c_char,
            left: *mut i32,
            top: *mut i32,
            right: *mut i32,
            bottom: *mut i32,
        ) -> bool;
    }
}

/// An `AMediaFormat`, deleted when dropped
struct Format(*mut ndk::AMediaFormat);

impl Format {
    fn new() -> Self {
        Self(unsafe { ndk::AMediaFormat_new() })
    }

    fn key(name: &str) -> CString {
        CString::new(name).expect("a key without NUL")
    }

    fn set_str(&mut self, name: &str, value: &str) {
        let value = CString::new(value).expect("a value without NUL");
        unsafe { ndk::AMediaFormat_setString(self.0, Self::key(name).as_ptr(), value.as_ptr()) }
    }

    fn set_i32(&mut self, name: &str, value: i32) {
        unsafe { ndk::AMediaFormat_setInt32(self.0, Self::key(name).as_ptr(), value) }
    }

    fn set_buffer(&mut self, name: &str, data: &[u8]) {
        unsafe {
            ndk::AMediaFormat_setBuffer(
                self.0,
                Self::key(name).as_ptr(),
                data.as_ptr().cast(),
                data.len(),
            )
        }
    }

    fn i32(&self, name: &str) -> Option<i32> {
        let mut value = 0;
        unsafe { ndk::AMediaFormat_getInt32(self.0, Self::key(name).as_ptr(), &mut value) }
            .then_some(value)
    }

    fn rect(&self, name: &str) -> Option<(i32, i32, i32, i32)> {
        let (mut left, mut top, mut right, mut bottom) = (0, 0, 0, 0);
        unsafe {
            ndk::AMediaFormat_getRect(
                self.0,
                Self::key(name).as_ptr(),
                &mut left,
                &mut top,
                &mut right,
                &mut bottom,
            )
        }
        .then_some((left, top, right, bottom))
    }
}

impl Drop for Format {
    fn drop(&mut self) {
        unsafe { ndk::AMediaFormat_delete(self.0) };
    }
}

/// An error from a MediaCodec call that returned `status`
fn check(status: ndk::MediaStatus, what: &str, name: &str) -> Result<(), Error> {
    if status == 0 {
        Ok(())
    } else {
        Err(Error::new(format!(
            "MediaCodec: {name} {what}: status {status}"
        )))
    }
}

/// Give FFmpeg the process's `JavaVM` (a `JavaVM*`), which FFmpeg's own MediaCodec decoders
/// (`h264_mediacodec`, `RECUT_MEDIACODEC=ffmpeg`) call MediaCodec's Java API with. Once, before
/// the first opens; FFmpeg refuses another VM after. Recut's decoder here doesn't need it.
///
/// # Safety
/// `vm` must be the process's `JavaVM*`, valid for the life of the process.
pub unsafe fn set_java_vm(vm: *mut c_void) -> Result<(), Error> {
    match av_jni_set_java_vm(vm, std::ptr::null_mut()) {
        0 => Ok(()),
        error => Err(Error::from_raw_error_code(error)),
    }
}

/// Whether FFmpeg has a `JavaVM` ([`set_java_vm`]): without one, its MediaCodec decoders don't
/// open.
pub fn has_java_vm() -> bool {
    !unsafe { av_jni_get_java_vm(std::ptr::null_mut()) }.is_null()
}

/// FFmpeg's own MediaCodec decoder for a codec, by FFmpeg's name for its software decoder
pub(crate) fn ffmpeg_decoder_name(codec: &str) -> Option<&'static str> {
    match codec {
        "h264" => Some("h264_mediacodec"),
        "hevc" => Some("hevc_mediacodec"),
        _ => None,
    }
}

/// Whether to open FFmpeg's MediaCodec decoders rather than Recut's (`RECUT_MEDIACODEC=ffmpeg`)
pub(crate) fn use_ffmpeg_decoders() -> bool {
    std::env::var("RECUT_MEDIACODEC").as_deref() == Ok("ffmpeg")
}

/// MediaCodec's timestamps are a decoder's to keep and give back, and some decoders treat them
/// as unsigned: packets' times go in this far ahead, so an edit list's negative times don't
/// reach it
const TIMESTAMP_OFFSET_US: i64 = 1_000_000_000;

/// How long a push waits for an input buffer to free up before it's refused (EAGAIN)
const INPUT_WAIT: Duration = Duration::from_millis(2);

/// The longest a take waits for the last pictures once the end of the stream went in
const DRAIN_WAIT: Duration = Duration::from_secs(1);

/// The most packets a decoder may take after a flush (a seek) without giving a picture before
/// it counts as broken. An H.264 decoder holds 16 pictures at most for reordering; the emulator's
/// gives nothing at all after starting at an open GOP's I-frame (not an IDR), until the next
/// IDR, as some phones' decoders are said to: then the caller decodes in software.
const MOST_PACKETS_WITHOUT_A_PICTURE: usize = 40;

/// How long the decoder must have taken no packet and given no picture before the end of the
/// stream goes in. The emulator's decoder (`c2.goldfish.h264.decoder`) dropped every picture
/// still decoding when the end came in right behind its packets (a seek near a file's end: nine
/// packets, then the end, then nothing but the end came out); with this wait it gives them all.
const END_SETTLE: Duration = Duration::from_millis(20);

/// Names of decoders that run on the CPU (Android's own and old OMX ones): not what this is for,
/// FFmpeg's software decoders do as well
fn is_software_decoder(name: &str) -> bool {
    name.starts_with("c2.android.")
        || name.starts_with("OMX.google.")
        || name.contains(".sw.")
        || name.ends_with(".sw")
}

/// How the decoder's output buffers hold a picture (from its output format)
#[derive(Clone, Copy, Debug)]
struct OutputLayout {
    color_format: i32,
    stride: i32,
    slice_height: i32,
    left: i32,
    top: i32,
    width: i32,
    height: i32,
}

impl OutputLayout {
    fn from_format(format: &Format) -> Self {
        let width = format.i32("width").unwrap_or(0);
        let height = format.i32("height").unwrap_or(0);
        let color_format = format.i32("color-format").unwrap_or(0);
        let bytes_per_sample = if color_format == 0x36 { 2 } else { 1 };
        let stride = format
            .i32("stride")
            .filter(|&stride| stride > 0)
            .unwrap_or(width * bytes_per_sample);
        let slice_height = format
            .i32("slice-height")
            .filter(|&rows| rows > 0)
            .unwrap_or(height);
        // the picture inside: "crop" (inclusive edges), or the older crop-* keys
        let (left, top, right, bottom) = format.rect("crop").unwrap_or_else(|| {
            (
                format.i32("crop-left").unwrap_or(0),
                format.i32("crop-top").unwrap_or(0),
                format.i32("crop-right").unwrap_or(width - 1),
                format.i32("crop-bottom").unwrap_or(height - 1),
            )
        });
        Self {
            color_format,
            stride,
            slice_height,
            left,
            top,
            width: right - left + 1,
            height: bottom - top + 1,
        }
    }
}

/// A packet that went in: what its picture is to come out with
struct Pushed {
    key_us: i64,
    pts: i64,
    duration: i64,
}

/// A MediaCodec decoder for one stream
pub(crate) struct MediaCodecDecoder {
    codec: *mut ndk::AMediaCodec,
    /// The decoder MediaCodec chose (`c2.qti.avc.decoder`, `c2.goldfish.h264.decoder`, ...)
    name: String,
    params: VideoCodecParameters,
    time_base: TimeBase,
    /// The length of the NAL units' size fields in the packets (MP4's), `None` if the packets
    /// are Annex B already (start codes, as MPEG-TS's): MediaCodec takes Annex B
    nal_length_size: Option<usize>,
    /// The stream's parameter sets (SPS, PPS; VPS for HEVC) in Annex B, from its extradata
    parameter_sets: Vec<u8>,
    /// Whether the next keyframe goes in after the parameter sets: after a flush, which the
    /// emulator's decoder forgets them at (it gave nothing after a seek without them)
    parameter_sets_due: bool,
    layout: Option<OutputLayout>,
    /// The packets in the decoder whose pictures haven't come out, oldest first
    pushed: VecDeque<Pushed>,
    /// An input buffer taken from the decoder and not yet given back
    input_buffer: Option<usize>,
    /// The end of the stream went in, or is to go in once an input buffer is free
    /// (`end_due`): takes wait for the last pictures
    draining: bool,
    end_due: bool,
    /// The end of the stream came out: nothing more until a flush
    ended: bool,
    /// When a packet last went in or a picture came out (`END_SETTLE`)
    last_progress: Instant,
    /// Packets in and pictures out since the last flush (`MOST_PACKETS_WITHOUT_A_PICTURE`)
    pushed_since_flush: usize,
    given_since_flush: usize,
}

// MediaCodec may be called from any thread, one call at a time
unsafe impl Send for MediaCodecDecoder {}
unsafe impl Sync for MediaCodecDecoder {}

impl MediaCodecDecoder {
    /// A hardware decoder for the stream, started: Err if the codec isn't H.264 or HEVC, the
    /// device's decoder for it runs in software, or it won't take the stream
    pub fn new(params: &VideoCodecParameters, time_base: TimeBase) -> Result<Self, Error> {
        let mime = match params.decoder_name() {
            Some("h264") => "video/avc",
            Some("hevc") => "video/hevc",
            _ => return Err(Error::new("MediaCodec: not H.264 or HEVC")),
        };
        let extradata = params.extradata().unwrap_or(&[]);
        let (csd, nal_length_size) = match mime {
            "video/avc" => avc_parameter_sets(extradata)?,
            _ => hevc_parameter_sets(extradata)?,
        };

        let opening = Instant::now();
        let mime_c = CString::new(mime).expect("a MIME type without NUL");
        let codec = unsafe { ndk::AMediaCodec_createDecoderByType(mime_c.as_ptr()) };
        if codec.is_null() {
            return Err(Error::new(format!("MediaCodec: no decoder for {mime}")));
        }
        // from here on, dropping it deletes the codec
        let mut decoder = Self {
            codec,
            name: String::new(),
            params: params.clone(),
            time_base,
            nal_length_size,
            parameter_sets: csd.clone(),
            parameter_sets_due: false,
            layout: None,
            pushed: VecDeque::new(),
            input_buffer: None,
            draining: false,
            end_due: false,
            ended: false,
            last_progress: Instant::now(),
            pushed_since_flush: 0,
            given_since_flush: 0,
        };
        decoder.name = decoder.codec_name();
        if is_software_decoder(&decoder.name) {
            return Err(Error::new(format!(
                "MediaCodec: the {mime} decoder, {}, is software",
                decoder.name
            )));
        }

        let mut format = Format::new();
        format.set_str("mime", mime);
        format.set_i32("width", params.width() as i32);
        format.set_i32("height", params.height() as i32);
        if !csd.is_empty() {
            format.set_buffer("csd-0", &csd);
        }
        // the range the stream says it's in (MediaFormat's COLOR_RANGE_FULL 1, _LIMITED 2): told
        // nothing, the emulator's decoder squeezed a full-range stream's samples into 16-235
        const AVCOL_RANGE_JPEG: u32 = 2;
        const AVCOL_RANGE_MPEG: u32 = 1;
        match params.color_range() {
            AVCOL_RANGE_JPEG => format.set_i32("color-range", 1),
            AVCOL_RANGE_MPEG => format.set_i32("color-range", 2),
            _ => {}
        }
        let status = unsafe {
            ndk::AMediaCodec_configure(codec, format.0, ptr::null_mut(), ptr::null_mut(), 0)
        };
        check(status, "configure", &decoder.name)?;
        check(
            unsafe { ndk::AMediaCodec_start(codec) },
            "start",
            &decoder.name,
        )?;
        info!(
            "MediaCodec: {} decodes {mime} {}x{} (opened in {:?})",
            decoder.name,
            params.width(),
            params.height(),
            opening.elapsed()
        );
        decoder.read_output_format();
        Ok(decoder)
    }

    fn codec_name(&self) -> String {
        let mut name: *mut c_char = ptr::null_mut();
        if unsafe { ndk::AMediaCodec_getName(self.codec, &mut name) } != 0 || name.is_null() {
            return "?".to_string();
        }
        let owned = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned();
        unsafe { ndk::AMediaCodec_releaseName(self.codec, name) };
        owned
    }

    fn read_output_format(&mut self) {
        let format = Format(unsafe { ndk::AMediaCodec_getOutputFormat(self.codec) });
        if format.0.is_null() {
            return;
        }
        let layout = OutputLayout::from_format(&format);
        debug!("MediaCodec: {} gives {layout:?}", self.name);
        self.layout = Some(layout);
    }

    /// An input buffer, if the decoder has one free within `timeout`
    fn free_input_buffer(&mut self, timeout: Duration) -> Result<Option<usize>, Error> {
        if let Some(index) = self.input_buffer.take() {
            return Ok(Some(index));
        }
        let index =
            unsafe { ndk::AMediaCodec_dequeueInputBuffer(self.codec, timeout.as_micros() as i64) };
        match index {
            ndk::INFO_TRY_AGAIN_LATER => Ok(None),
            index if index < 0 => Err(Error::new(format!(
                "MediaCodec: {} input: {index}",
                self.name
            ))),
            index => Ok(Some(index as usize)),
        }
    }

    /// Give the decoder a packet (in the decoder's time base). EAGAIN while every input buffer
    /// is in use.
    pub fn try_push(&mut self, packet: &Packet) -> Result<(), CodecError> {
        if self.draining || self.ended {
            return Err(CodecError::error(
                "MediaCodec: a packet after the end of the stream",
            ));
        }
        if self.given_since_flush == 0 && self.pushed_since_flush >= MOST_PACKETS_WITHOUT_A_PICTURE
        {
            return Err(CodecError::from(self.gave_nothing()));
        }
        // a moment for one to free up, rather than have the caller spin through takes
        let Some(index) = self.free_input_buffer(INPUT_WAIT)? else {
            return Err(CodecError::again(
                "MediaCodec: every input buffer is in use",
            ));
        };
        let mut capacity = 0;
        let buffer = unsafe { ndk::AMediaCodec_getInputBuffer(self.codec, index, &mut capacity) };
        if buffer.is_null() {
            return Err(CodecError::error("MediaCodec: no input buffer"));
        }
        let buffer = unsafe { std::slice::from_raw_parts_mut(buffer, capacity) };
        let parameter_sets = if self.parameter_sets_due && packet.is_key() {
            self.parameter_sets.as_slice()
        } else {
            &[]
        };
        let written = buffer
            .get_mut(..parameter_sets.len())
            .map(|start| start.copy_from_slice(parameter_sets))
            .ok_or_else(|| Error::new("MediaCodec: parameter sets bigger than an input buffer"))
            .and_then(|()| {
                write_annex_b(
                    packet.data(),
                    self.nal_length_size,
                    &mut buffer[parameter_sets.len()..],
                )
            });
        let size = match written {
            Ok(size) => parameter_sets.len() + size,
            Err(error) => {
                // the buffer is still free: the next packet goes in it
                self.input_buffer = Some(index);
                return Err(CodecError::from(error));
            }
        };

        let pts = if packet.pts().is_null() {
            packet.dts()
        } else {
            packet.pts()
        };
        let pts = pts.with_time_base(self.time_base).timestamp();
        let key_us = Timestamp::new(pts, self.time_base)
            .with_time_base(TimeBase::MICROSECONDS)
            .timestamp()
            + TIMESTAMP_OFFSET_US;
        let duration = packet.duration().with_time_base(self.time_base).timestamp();
        let status = unsafe {
            ndk::AMediaCodec_queueInputBuffer(self.codec, index, 0, size, key_us as u64, 0)
        };
        check(status, "queue", &self.name)?;
        if packet.is_key() {
            self.parameter_sets_due = false;
        }
        self.last_progress = Instant::now();
        self.pushed_since_flush += 1;
        self.pushed.push_back(Pushed {
            key_us,
            pts,
            duration,
        });
        Ok(())
    }

    /// No more packets: the decoder gives out the pictures it holds, then ends. The end goes in
    /// at a take, once the decoder has settled (`END_SETTLE`) and an input buffer is free
    /// (every one may be waiting for its picture to be taken).
    pub fn try_flush(&mut self) -> Result<(), CodecError> {
        if self.draining || self.ended {
            return Ok(());
        }
        self.draining = true;
        self.end_due = true;
        Ok(())
    }

    /// The end of the stream, into a free input buffer if there's one and the decoder has
    /// settled
    fn queue_end(&mut self) -> Result<(), Error> {
        if self.last_progress.elapsed() < END_SETTLE {
            return Ok(());
        }
        let Some(index) = self.free_input_buffer(Duration::ZERO)? else {
            return Ok(());
        };
        let status = unsafe {
            ndk::AMediaCodec_queueInputBuffer(
                self.codec,
                index,
                0,
                0,
                0,
                ndk::BUFFER_FLAG_END_OF_STREAM,
            )
        };
        check(status, "end of stream", &self.name)?;
        self.end_due = false;
        Ok(())
    }

    /// Forget every packet and picture in the decoder (a seek)
    pub fn flush_buffers(&mut self) {
        let status = unsafe { ndk::AMediaCodec_flush(self.codec) };
        if status != 0 {
            debug!("MediaCodec: {} flush: status {status}", self.name);
        }
        self.pushed.clear();
        self.input_buffer = None;
        self.parameter_sets_due = !self.parameter_sets.is_empty();
        self.draining = false;
        self.end_due = false;
        self.ended = false;
        self.pushed_since_flush = 0;
        self.given_since_flush = 0;
    }

    /// The decoder took packets and gave no picture: it's of no use for this stream
    fn gave_nothing(&self) -> Error {
        Error::new(format!(
            "MediaCodec: {} gave no picture for {} packets",
            self.name, self.pushed_since_flush
        ))
    }

    /// The next picture, if one is out, rotated by `rotation` degrees to display. While
    /// draining, waits for it (or the end).
    pub fn take(&mut self, rotation: f64) -> Result<Option<VideoFrame>, Error> {
        if self.ended {
            return Ok(None);
        }
        let started = Instant::now();
        loop {
            if self.end_due {
                self.queue_end()?;
            }
            let timeout_us = if self.end_due {
                5_000
            } else if self.draining {
                10_000
            } else {
                0
            };
            let mut info = ndk::AMediaCodecBufferInfo::default();
            let index =
                unsafe { ndk::AMediaCodec_dequeueOutputBuffer(self.codec, &mut info, timeout_us) };
            match index {
                ndk::INFO_TRY_AGAIN_LATER => {
                    if !self.draining || started.elapsed() > DRAIN_WAIT {
                        return Ok(None);
                    }
                }
                ndk::INFO_OUTPUT_FORMAT_CHANGED => self.read_output_format(),
                ndk::INFO_OUTPUT_BUFFERS_CHANGED => {}
                index if index < 0 => {
                    return Err(Error::new(format!(
                        "MediaCodec: {} output: {index}",
                        self.name
                    )));
                }
                index => {
                    let index = index as usize;
                    let frame = if info.size > 0 {
                        Some(self.frame_in(index, &info, rotation))
                    } else {
                        None
                    };
                    let status =
                        unsafe { ndk::AMediaCodec_releaseOutputBuffer(self.codec, index, false) };
                    check(status, "release", &self.name)?;
                    self.last_progress = Instant::now();
                    if info.flags & ndk::BUFFER_FLAG_END_OF_STREAM != 0 {
                        self.ended = true;
                    }
                    match frame {
                        Some(frame) => {
                            self.given_since_flush += 1;
                            return frame.map(Some);
                        }
                        None if self.ended
                            && self.given_since_flush == 0
                            && self.pushed_since_flush > 0 =>
                        {
                            return Err(self.gave_nothing());
                        }
                        None if self.ended => return Ok(None),
                        None => {}
                    }
                }
            }
        }
    }

    /// A frame of the picture in output buffer `index`, with the time and duration of its packet
    fn frame_in(
        &mut self,
        index: usize,
        info: &ndk::AMediaCodecBufferInfo,
        rotation: f64,
    ) -> Result<VideoFrame, Error> {
        let key_us = info.presentation_time_us;
        let (pts, duration) = match self.pushed.iter().position(|p| p.key_us == key_us) {
            Some(at) => {
                let pushed = self.pushed.remove(at).expect("a pushed packet");
                (pushed.pts, pushed.duration)
            }
            None => {
                let pts = Timestamp::new(key_us - TIMESTAMP_OFFSET_US, TimeBase::MICROSECONDS)
                    .with_time_base(self.time_base)
                    .timestamp();
                (pts, 0)
            }
        };
        let layout = self
            .layout
            .ok_or_else(|| Error::new("MediaCodec: a picture before its format"))?;
        if unsafe { ffw_mediacodec_pixel_format(layout.color_format) } < 0 {
            return Err(Error::new(format!(
                "MediaCodec: {} gives color format {:#x}, which Recut can't read",
                self.name, layout.color_format
            )));
        }

        let mut capacity = 0;
        let data = unsafe { ndk::AMediaCodec_getOutputBuffer(self.codec, index, &mut capacity) };
        let (offset, size) = (info.offset.max(0) as usize, info.size.max(0) as usize);
        if data.is_null() || offset + size > capacity {
            return Err(Error::new(format!(
                "MediaCodec: {} gave no picture in its output buffer",
                self.name
            )));
        }
        let frame = unsafe {
            ffw_mediacodec_frame_new(
                data.add(offset),
                size,
                layout.color_format,
                layout.stride,
                layout.slice_height,
                layout.left,
                layout.top,
                layout.width,
                layout.height,
                self.params.as_ptr(),
                pts,
                duration,
            )
        };
        if frame.is_null() {
            return Err(Error::new(format!(
                "MediaCodec: {} gave a picture that doesn't fit its format {layout:?} ({size} bytes)",
                self.name
            )));
        }
        Ok(unsafe { VideoFrame::from_raw_ptr(frame, self.time_base, rotation) })
    }
}

impl Drop for MediaCodecDecoder {
    fn drop(&mut self) {
        unsafe {
            ndk::AMediaCodec_stop(self.codec);
            ndk::AMediaCodec_delete(self.codec);
        }
    }
}

/// Copy a packet into `buffer` as Annex B (each NAL unit after a start code), from MP4's
/// length-prefixed NAL units (`nal_length_size` bytes each), or as it is. Its size.
fn write_annex_b(
    data: &[u8],
    nal_length_size: Option<usize>,
    buffer: &mut [u8],
) -> Result<usize, Error> {
    let mut written = 0;
    let mut write = |bytes: &[u8]| -> Result<(), Error> {
        let end = written + bytes.len();
        if end > buffer.len() {
            return Err(Error::new(
                "MediaCodec: a packet bigger than its input buffer",
            ));
        }
        buffer[written..end].copy_from_slice(bytes);
        written = end;
        Ok(())
    };
    let Some(length_size) = nal_length_size else {
        write(data)?;
        return Ok(written);
    };
    let mut rest = data;
    while !rest.is_empty() {
        if rest.len() < length_size {
            return Err(Error::new("MediaCodec: a NAL unit's length is cut off"));
        }
        let length = rest[..length_size]
            .iter()
            .fold(0usize, |length, &byte| (length << 8) | byte as usize);
        rest = &rest[length_size..];
        if length > rest.len() {
            return Err(Error::new("MediaCodec: a NAL unit runs past its packet"));
        }
        write(&[0, 0, 0, 1])?;
        write(&rest[..length])?;
        rest = &rest[length..];
    }
    Ok(written)
}

/// Whether `data` starts with an Annex B start code
fn is_annex_b(data: &[u8]) -> bool {
    data.starts_with(&[0, 0, 1]) || data.starts_with(&[0, 0, 0, 1])
}

/// Appends a NAL unit after a start code
fn push_nal_unit(out: &mut Vec<u8>, nal_unit: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1]);
    out.extend_from_slice(nal_unit);
}

/// Reads a big-endian u16 at `at`
fn u16_at(data: &[u8], at: usize) -> Result<usize, Error> {
    data.get(at..at + 2)
        .map(|bytes| (bytes[0] as usize) << 8 | bytes[1] as usize)
        .ok_or_else(|| Error::new("MediaCodec: the stream's parameter sets are cut off"))
}

/// H.264's SPS and PPS as Annex B (MediaCodec's `csd-0`; it takes both there), and the length
/// of the packets' NAL size fields: from an `avcC` box, or extradata already in Annex B
fn avc_parameter_sets(extradata: &[u8]) -> Result<(Vec<u8>, Option<usize>), Error> {
    if extradata.is_empty() || is_annex_b(extradata) {
        return Ok((extradata.to_vec(), None));
    }
    let cut_off = || Error::new("MediaCodec: the avcC box is cut off");
    if extradata.len() < 7 || extradata[0] != 1 {
        return Err(Error::new("MediaCodec: unknown H.264 extradata"));
    }
    let length_size = (extradata[4] & 3) as usize + 1;
    let mut csd = Vec::new();
    let mut at = 5;
    for set_count_mask in [0x1f, 0xff] {
        let count = (*extradata.get(at).ok_or_else(cut_off)? & set_count_mask) as usize;
        at += 1;
        for _ in 0..count {
            let length = u16_at(extradata, at)?;
            at += 2;
            let nal_unit = extradata.get(at..at + length).ok_or_else(cut_off)?;
            push_nal_unit(&mut csd, nal_unit);
            at += length;
        }
    }
    Ok((csd, Some(length_size)))
}

/// HEVC's VPS, SPS and PPS (and SEI) as Annex B (MediaCodec's `csd-0`), and the length of the
/// packets' NAL size fields: from an `hvcC` box, or extradata already in Annex B
fn hevc_parameter_sets(extradata: &[u8]) -> Result<(Vec<u8>, Option<usize>), Error> {
    if extradata.is_empty() || is_annex_b(extradata) {
        return Ok((extradata.to_vec(), None));
    }
    let cut_off = || Error::new("MediaCodec: the hvcC box is cut off");
    if extradata.len() < 23 {
        return Err(Error::new("MediaCodec: unknown HEVC extradata"));
    }
    let length_size = (extradata[21] & 3) as usize + 1;
    let arrays = extradata[22] as usize;
    let mut csd = Vec::new();
    let mut at = 23;
    for _ in 0..arrays {
        at += 1; // array_completeness, NAL unit type
        let count = u16_at(extradata, at)?;
        at += 2;
        for _ in 0..count {
            let length = u16_at(extradata, at)?;
            at += 2;
            let nal_unit = extradata.get(at..at + length).ok_or_else(cut_off)?;
            push_nal_unit(&mut csd, nal_unit);
            at += length;
        }
    }
    Ok((csd, Some(length_size)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_prefixed_nal_units_become_annex_b() {
        let packet = [0, 0, 0, 2, 0x65, 0xaa, 0, 0, 0, 1, 0x41];
        let mut buffer = vec![0u8; 32];
        let size = write_annex_b(&packet, Some(4), &mut buffer).unwrap();
        assert_eq!(buffer[..size], [0, 0, 0, 1, 0x65, 0xaa, 0, 0, 0, 1, 0x41]);

        let mut small = vec![0u8; 4];
        assert!(write_annex_b(&packet, Some(4), &mut small).is_err());
        assert!(write_annex_b(&[0, 0, 0, 9, 1], Some(4), &mut buffer).is_err());
    }

    #[test]
    fn an_avcc_box_gives_its_parameter_sets_in_annex_b() {
        let avcc = [
            1, 0x64, 0, 0x1f, 0xff, 0xe1, 0, 2, 0x67, 0x64, 1, 0, 1, 0x68,
        ];
        let (csd, length_size) = avc_parameter_sets(&avcc).unwrap();
        assert_eq!(csd, [0, 0, 0, 1, 0x67, 0x64, 0, 0, 0, 1, 0x68]);
        assert_eq!(length_size, Some(4));
        assert!(avc_parameter_sets(&avcc[..10]).is_err());
        let annex_b = [0, 0, 0, 1, 0x67];
        assert_eq!(
            avc_parameter_sets(&annex_b).unwrap(),
            (annex_b.to_vec(), None)
        );
    }
}
