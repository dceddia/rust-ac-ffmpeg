use std::ffi::c_void;
use std::sync::mpsc::TryRecvError;

use crate::codec::CodecError;
use crate::packet::{Packet, PacketMut};
use crate::time::TimeBase;
use crate::{codec::VideoCodecParameters, Error};

pub struct DecodedFrame {
    pub(crate) av_frame: *mut c_void, // AVFrame pointer from C
}

pub struct VTDecoder {
    vt_decoder_ptr: *mut c_void,
    #[allow(dead_code)]
    time_base: TimeBase, // Kept for potential future use
    #[allow(dead_code)]
    frame_tx: std::sync::mpsc::Sender<DecodedFrame>, // Kept for potential future use
    frame_rx: std::sync::mpsc::Receiver<DecodedFrame>,
    // Store the raw pointer to the boxed sender so we can clean it up
    sender_ptr: *mut std::sync::mpsc::Sender<DecodedFrame>,
}

type RustFrameCallback = unsafe extern "C" fn(context: *mut c_void, frame: *mut c_void);

extern "C" {
    pub(crate) fn vt_decoder_create(
        av_codec_context: *const c_void,
        callback: RustFrameCallback,
        callback_context: *mut c_void,
    ) -> *mut c_void;
    pub(crate) fn vt_decode_frame(decoder: *mut c_void, packet: *const c_void) -> i32;
    pub(crate) fn vt_decoder_free(decoder: *mut c_void);
}

// This is the Rust callback that will be called from C
unsafe extern "C" fn rust_frame_callback(context: *mut c_void, frame: *mut c_void) {
    // If frame is NULL, it means the frame was marked DoNotDisplay
    if frame.is_null() {
        return;
    }

    let decoded_frame = DecodedFrame { av_frame: frame };

    // Get the sender from the context
    let sender = &*(context as *const std::sync::mpsc::Sender<DecodedFrame>);
    let _ = sender.send(decoded_frame);
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
            return Err(Error::new("Failed to create VideoToolbox decoder"));
        }

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
        let packet = packet.with_time_base(self.time_base);
        self.decode_frame(packet)
    }

    pub fn try_flush(&mut self) -> Result<(), CodecError> {
        // Push an empty packet
        let packet = PacketMut::new(0).with_time_base(self.time_base).freeze();
        self.decode_frame(packet)
    }

    fn decode_frame(&mut self, packet: Packet) -> Result<(), CodecError> {
        let ret = unsafe { vt_decode_frame(self.vt_decoder_ptr, packet.as_ptr()) };
        if ret < 0 {
            return Err(CodecError::from_raw_error_code(ret));
        }
        Ok(())
    }

    pub fn take_frame(&mut self) -> Result<Option<DecodedFrame>, Error> {
        match self.frame_rx.try_recv() {
            Ok(decoded_frame) => Ok(Some(decoded_frame)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Error::new("Receiver disconnected")),
        }
    }

    pub fn flush_buffers(&mut self) {
        // Drain any pending frames from the channel
        while self.frame_rx.try_recv().is_ok() {}
        // TODO: Add VTDecompressionSession flush if needed
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
