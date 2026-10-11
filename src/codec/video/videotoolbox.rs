use std::ffi::c_void;
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use log::{debug, trace, warn};

use crate::codec::CodecError;
use crate::packet::Packet;
use crate::time::TimeBase;
use crate::{codec::VideoCodecParameters, Error};

use super::VideoToolboxOutput;

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

/// A result from VideoToolbox's callback, with the decoder's generation when its packet was
/// pushed: results of a generation before the last `flush_buffers` are dropped
type Generational = (u64, DecodeResult);

pub struct VTDecoder {
    vt_decoder_ptr: *mut c_void,
    #[allow(dead_code)]
    time_base: TimeBase,
    #[allow(dead_code)]
    frame_tx: std::sync::mpsc::Sender<Generational>,
    frame_rx: std::sync::mpsc::Receiver<Generational>,
    /// The generation of the packets pushed since the last `flush_buffers`
    generation: u64,
    /// A result `wait_for_result` took, for the next `take_result`
    waited: Option<DecodeResult>,
    // Store the raw pointer to the boxed sender so we can clean it up
    sender_ptr: *mut std::sync::mpsc::Sender<Generational>,
}

type RustFrameCallback = unsafe extern "C" fn(
    context: *mut c_void,
    frame: *mut c_void,
    status: i32,
    pts: i64,
    generation: u64,
);

extern "C" {
    fn ffw_frame_free(frame: *mut c_void);
    pub(crate) fn vt_decoder_create(
        av_codec_context: *const c_void,
        chroma_format_idc: i32,
        bit_depth: i32,
        callback: RustFrameCallback,
        callback_context: *mut c_void,
    ) -> *mut c_void;
    pub(crate) fn vt_decode_frame(
        decoder: *mut c_void,
        packet: *const c_void,
        reset_decoder: i32,
    ) -> i32;
    pub(crate) fn vt_decoder_flush(decoder: *mut c_void);
    pub(crate) fn vt_decoder_flush_buffers(decoder: *mut c_void) -> u64;
    pub(crate) fn vt_decoder_free(decoder: *mut c_void);
    fn vt_decoder_uses_hardware(decoder: *mut c_void) -> i32;
}

// This is the Rust callback that will be called from C
unsafe extern "C" fn rust_frame_callback(
    context: *mut c_void,
    frame: *mut c_void,
    status: i32,
    pts: i64,
    generation: u64,
) {
    let sender = &*(context as *const std::sync::mpsc::Sender<Generational>);

    // Check for decode error
    if status != 0 {
        let _ = sender.send((generation, DecodeResult::Error { status, pts }));
        return;
    }

    // NULL frame with success status means DoNotDisplay
    if frame.is_null() {
        let _ = sender.send((generation, DecodeResult::Skipped { pts }));
        return;
    }

    // Success - send the frame
    let decoded_frame = DecodedFrame { av_frame: frame };
    let _ = sender.send((generation, DecodeResult::Frame(decoded_frame)));
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
    /// A decoder of the stream `params` describes, giving its frames in `output`'s layout
    pub fn new(
        params: &VideoCodecParameters,
        time_base: TimeBase,
        output: VideoToolboxOutput,
    ) -> Result<VTDecoder, Error> {
        // TODO NEXT: Maybe use a channel to send packets to the decoder? (and put the decoder in a separate thread)
        // Gotta fix the issue where it either freezes (using .recv()) or decodes infinitely-ish (using .try_recv()).
        // This might just be a mismatch between sync and async methods of decoding and it might not be possible to
        // express them both with the same API? I'm not sure though.
        let (frame_tx, frame_rx) = std::sync::mpsc::channel();

        // We need to pass a pointer to the sender that will outlive this function
        // Box it to put it on the heap and get a stable pointer
        let sender_box = Box::new(frame_tx.clone());
        let sender_ptr = Box::into_raw(sender_box);

        let (chroma_format_idc, bit_depth) = match output {
            VideoToolboxOutput::Yuv420 => (-1, 0),
            VideoToolboxOutput::Source {
                chroma_format_idc,
                bit_depth,
            } => (chroma_format_idc as i32, bit_depth as i32),
        };
        let vt_decoder_ptr = unsafe {
            vt_decoder_create(
                params.as_ptr(),
                chroma_format_idc,
                bit_depth,
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

        let decoder = VTDecoder {
            time_base,
            frame_tx,
            frame_rx,
            generation: 0,
            waited: None,
            vt_decoder_ptr,
            sender_ptr,
        };
        let kind = match decoder.uses_hardware() {
            Some(true) => "hardware",
            Some(false) => "software",
            None => "unknown",
        };
        debug!("VideoToolbox decoder created ({kind} decoder)");
        Ok(decoder)
    }

    /// Whether VideoToolbox decodes on the media engine (`Some(true)`) or in software
    /// (`Some(false)`, e.g. for formats the hardware can't decode), from the session's
    /// `UsingHardwareAcceleratedVideoDecoder` property. `None` if it doesn't say.
    pub fn uses_hardware(&self) -> Option<bool> {
        match unsafe { vt_decoder_uses_hardware(self.vt_decoder_ptr) } {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        }
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
        // Push an empty packet
        // note, later on: This does NOT work. It fails, and doesn't act like a "done" signal like I hoped.
        // let packet = crate::packet::PacketMut::new(0)
        //     .with_time_base(self.time_base)
        //     .freeze();
        // self.decode_frame(packet);
        unsafe { vt_decoder_flush(self.vt_decoder_ptr) };
        Ok(())
    }

    /// Waits until VideoToolbox has finished with another packet (or has one `take_result`
    /// hasn't given yet), for at most `timeout`. False if it didn't.
    pub fn wait_for_result(&mut self, timeout: Duration) -> bool {
        if self.waited.is_some() {
            return true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.frame_rx.recv_timeout(left) {
                Ok((generation, result)) if generation == self.generation => {
                    self.waited = Some(result);
                    return true;
                }
                // from before the last flush
                Ok(_) => continue,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return false,
            }
        }
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
        // Consume any number of DoNotDisplay markers and return the next real frame.
        // Returning `None` on a skip would be indistinguishable from an empty channel,
        // which prematurely ends draining at EOF (a skip can sit ahead of real frames
        // still queued behind it). Only a truly empty channel means "no frame".
        loop {
            match self.take_result()? {
                Some(DecodeResult::Frame(decoded_frame)) => return Ok(Some(decoded_frame)),
                Some(DecodeResult::Error { status, pts }) => {
                    return Err(Self::decode_error(status, pts))
                }
                // DoNotDisplay frame - not an error and not a frame; keep looking.
                Some(DecodeResult::Skipped { pts: _ }) => continue,
                None => return Ok(None),
            }
        }
    }

    /// What VideoToolbox did with the next packet it finished with: its frame, or that it
    /// skipped it or couldn't decode it. `None` while it hasn't finished with one since the last
    /// take (it decodes asynchronously).
    pub fn take_result(&mut self) -> Result<Option<DecodeResult>, Error> {
        let received = match self.waited.take() {
            Some(result) => Ok(result),
            None => self.try_receive(),
        };
        match received {
            Ok(result) => {
                match &result {
                    DecodeResult::Frame(_) => trace!("VideoToolbox: got frame from channel"),
                    DecodeResult::Error { status, pts } => {
                        warn!("VideoToolbox decode error: status={} pts={}", status, pts)
                    }
                    DecodeResult::Skipped { .. } => {}
                }
                Ok(Some(result))
            }
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => Err(Error::new("Receiver disconnected")),
        }
    }

    /// The next result VideoToolbox gave since the last flush, if it has given one
    fn try_receive(&mut self) -> Result<DecodeResult, TryRecvError> {
        loop {
            let (generation, result) = self.frame_rx.try_recv()?;
            if generation == self.generation {
                return Ok(result);
            }
        }
    }

    /// The error for a packet VideoToolbox couldn't decode
    pub fn decode_error(status: i32, pts: i64) -> Error {
        Error::new(format!(
            "Hardware decode error: status={} pts={}",
            status, pts
        ))
    }

    /// Forget the packets pushed so far: their results are dropped, whenever they come. It
    /// doesn't wait for VideoToolbox to finish them (a seek's reading and seeking go on
    /// meanwhile). The next packet pushed starts the decoder over: it forgets the reference
    /// frames it holds (`ResetDecoderBeforeDecoding`), as FFmpeg's decoders do on a flush.
    pub fn flush_buffers(&mut self) {
        debug!("VideoToolbox: flush_buffers (reset)");
        self.generation = unsafe { vt_decoder_flush_buffers(self.vt_decoder_ptr) };
        self.waited = None;
        // what came in already (the rest is dropped as it's taken)
        while self.frame_rx.try_recv().is_ok() {}
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
