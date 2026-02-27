use std::ffi::c_void;
use std::sync::mpsc::TryRecvError;

use log::{debug, trace, warn};

use crate::codec::CodecError;
use crate::packet::Packet;
use crate::time::TimeBase;
use crate::{codec::VideoCodecParameters, Error};

pub struct DecodedFrame {
    pub(crate) av_frame: *mut c_void, // AVFrame pointer from C
}

/// Result of an async decode operation from VideoToolbox
pub enum DecodeResult {
    /// Successfully decoded frame
    Frame(DecodedFrame),
    /// Decode error (HW couldn't decode this frame)
    Error { status: i32, pts: i64 },
    /// Frame was marked DoNotDisplay (not an error, just no frame)
    Skipped { pts: i64 },
}

pub struct VTDecoder {
    vt_decoder_ptr: *mut c_void,
    #[allow(dead_code)]
    time_base: TimeBase,
    #[allow(dead_code)]
    frame_tx: std::sync::mpsc::Sender<DecodeResult>,
    frame_rx: std::sync::mpsc::Receiver<DecodeResult>,
    // Store the raw pointer to the boxed sender so we can clean it up
    sender_ptr: *mut std::sync::mpsc::Sender<DecodeResult>,
}

type RustFrameCallback =
    unsafe extern "C" fn(context: *mut c_void, frame: *mut c_void, status: i32, pts: i64);

extern "C" {
    fn ffw_frame_free(frame: *mut c_void);
    pub(crate) fn vt_decoder_create(
        av_codec_context: *const c_void,
        callback: RustFrameCallback,
        callback_context: *mut c_void,
    ) -> *mut c_void;
    pub(crate) fn vt_decode_frame(
        decoder: *mut c_void,
        packet: *const c_void,
        reset_decoder: i32,
    ) -> i32;
    pub(crate) fn vt_decoder_can_decode_params(
        existing_decoder: *mut c_void,
        params: *const c_void,
    ) -> i32;
    pub(crate) fn vt_decoder_flush(decoder: *mut c_void);
    pub(crate) fn vt_decoder_free(decoder: *mut c_void);
}

// This is the Rust callback that will be called from C
unsafe extern "C" fn rust_frame_callback(
    context: *mut c_void,
    frame: *mut c_void,
    status: i32,
    pts: i64,
) {
    let sender = &*(context as *const std::sync::mpsc::Sender<DecodeResult>);

    // Check for decode error
    if status != 0 {
        let _ = sender.send(DecodeResult::Error { status, pts });
        return;
    }

    // NULL frame with success status means DoNotDisplay
    if frame.is_null() {
        let _ = sender.send(DecodeResult::Skipped { pts });
        return;
    }

    // Success - send the frame
    let decoded_frame = DecodedFrame { av_frame: frame };
    let _ = sender.send(DecodeResult::Frame(decoded_frame));
}

impl Drop for DecodedFrame {
    fn drop(&mut self) {
        if !self.av_frame.is_null() {
            unsafe {
                ffw_frame_free(self.av_frame);
            }
        }
    }
}

impl DecodedFrame {
    /// Transfer ownership of the raw AVFrame pointer, setting internal pointer to null
    /// to prevent double-free when this DecodedFrame is dropped.
    pub fn into_raw_ptr(mut self) -> *mut c_void {
        let ptr = self.av_frame;
        self.av_frame = std::ptr::null_mut();
        ptr
    }
}

impl VTDecoder {
    pub fn new(params: &VideoCodecParameters, time_base: TimeBase) -> Result<VTDecoder, Error> {
        // TODO NEXT: Maybe use a channel to send packets to the decoder? (and put the decoder in a separate thread)
        // Gotta fix the issue where it either freezes (using .recv()) or decodes infinitely-ish (using .try_recv()).
        // This might just be a mismatch between sync and async methods of decoding and it might not be possible to
        // express them both with the same API? I'm not sure though.
        let (frame_tx, frame_rx) = std::sync::mpsc::channel();

        // We need to pass a pointer to the sender that will outlive this function
        // Box it to put it on the heap and get a stable pointer
        let sender_box = Box::new(frame_tx.clone());
        let sender_ptr = Box::into_raw(sender_box);

        let vt_decoder_ptr = unsafe {
            vt_decoder_create(
                params.as_ptr(),
                rust_frame_callback,
                sender_ptr as *mut c_void,
            )
        };

        if vt_decoder_ptr.is_null() {
            // Clean up the boxed sender if decoder creation failed
            unsafe {
                drop(Box::from_raw(sender_ptr));
            }
            warn!("VideoToolbox decoder creation failed");
            return Err(Error::new("Failed to create VideoToolbox decoder"));
        }

        debug!("VideoToolbox decoder created successfully");
        Ok(VTDecoder {
            time_base,
            frame_tx,
            frame_rx,
            vt_decoder_ptr,
            sender_ptr,
        })
    }

    pub fn is_supported(params: &VideoCodecParameters) -> bool {
        match params.decoder_name() {
            Some("h264") | Some("h265") | Some("hevc") | Some("vp9") => true,
            _ => false,
        }
    }

    pub fn try_push(&mut self, packet: Packet) -> Result<(), CodecError> {
        self.decode_frame(packet)
    }

    pub fn push_with_reset(&mut self, packet: Packet) -> Result<(), CodecError> {
        let ret = unsafe { vt_decode_frame(self.vt_decoder_ptr, packet.as_ptr(), 1) };
        if ret < 0 {
            warn!("VideoToolbox push_with_reset returned error {}", ret);
            return Err(CodecError::from_raw_error_code(ret));
        }
        Ok(())
    }

    pub fn try_flush(&mut self) -> Result<(), CodecError> {
        unsafe { vt_decoder_flush(self.vt_decoder_ptr) };
        Ok(())
        // Push an empty packet
        // let packet = PacketMut::new(0).with_time_base(self.time_base).freeze();
        // self.decode_frame(packet)
    }

    /// Can this decoder be reused to decode another stream?
    pub fn can_decode_params(&self, params: &VideoCodecParameters) -> bool {
        unsafe { vt_decoder_can_decode_params(self.vt_decoder_ptr, params.as_ptr()) == 1 }
    }

    fn decode_frame(&mut self, packet: Packet) -> Result<(), CodecError> {
        let ret = unsafe { vt_decode_frame(self.vt_decoder_ptr, packet.as_ptr(), 0) };
        if ret < 0 {
            warn!("VideoToolbox decode_frame returned error {}", ret);
            return Err(CodecError::from_raw_error_code(ret));
        }
        Ok(())
    }

    pub fn take_frame(&mut self) -> Result<Option<DecodedFrame>, Error> {
        match self.frame_rx.try_recv() {
            Ok(DecodeResult::Frame(decoded_frame)) => {
                trace!("VideoToolbox: got frame from channel");
                Ok(Some(decoded_frame))
            }
            Ok(DecodeResult::Error { status, pts }) => {
                warn!(
                    "VideoToolbox decode error: status={} pts={}",
                    status, pts
                );
                Err(Error::new(format!(
                    "Hardware decode error: status={} pts={}",
                    status, pts
                )))
            }
            Ok(DecodeResult::Skipped { pts: _ }) => {
                // DoNotDisplay frame - not an error, just no frame to return
                Ok(None)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Error::new("Receiver disconnected")),
        }
    }

    pub fn flush_buffers(&mut self) {
        debug!("VideoToolbox: flushing decoder");
        let _ = self.try_flush();
        let mut flushed_frames = 0;
        let mut flushed_errors = 0;
        while let Ok(result) = self.frame_rx.try_recv() {
            match result {
                DecodeResult::Frame(_) => flushed_frames += 1,
                DecodeResult::Error { .. } => flushed_errors += 1,
                DecodeResult::Skipped { .. } => {}
            }
        }
        debug!(
            "VideoToolbox: flushed {} frames, {} errors",
            flushed_frames, flushed_errors
        );
    }
}

impl Drop for VTDecoder {
    fn drop(&mut self) {
        // Free the C decoder first
        if !self.vt_decoder_ptr.is_null() {
            unsafe {
                vt_decoder_free(self.vt_decoder_ptr);
            }
        }

        // Clean up the boxed sender
        if !self.sender_ptr.is_null() {
            unsafe {
                drop(Box::from_raw(self.sender_ptr));
            }
        }
    }
}
