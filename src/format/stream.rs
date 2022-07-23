//! A/V stream information.

use std::{
    ffi::{CStr, CString},
    os::raw::{c_char, c_int, c_void},
};

use crate::{
    codec::CodecParameters,
    time::{TimeBase, Timestamp},
};

extern "C" {
    fn ffw_stream_get_time_base(stream: *const c_void, num: *mut u32, den: *mut u32);
    fn ffw_stream_get_index(stream: *const c_void) -> c_int;
    fn ffw_stream_get_start_time(stream: *const c_void) -> i64;
    fn ffw_stream_get_duration(stream: *const c_void) -> i64;
    fn ffw_stream_get_nb_frames(stream: *const c_void) -> i64;
    fn ffw_stream_get_r_frame_rate(stream: *const c_void, num: *mut u32, den: *mut u32) -> i64;
    fn ffw_stream_set_discard(stream: *mut c_void, discard: c_int);
    fn ffw_stream_get_codec_parameters(stream: *const c_void) -> *mut c_void;
    fn ffw_stream_set_metadata(
        stream: *mut c_void,
        key: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn ffw_stream_get_metadata(stream: *mut c_void, key: *const c_char) -> *const c_char;
}

/// Used to specify whether (and how) a stream's packets should be discarded while demuxing.
pub enum Discard {
    None,
    Default,
    NonRef,
    BiDir,
    NonIntra,
    NonKey,
    All,
}
impl Discard {
    /// Get the internal raw representation.
    fn into_raw(self) -> i32 {
        match self {
            Discard::None => -16,
            Discard::Default => 0,
            Discard::NonRef => 8,
            Discard::BiDir => 16,
            Discard::NonIntra => 24,
            Discard::NonKey => 32,
            Discard::All => 48,
        }
    }
}

/// Stream.
pub struct Stream {
    ptr: *mut c_void,
    time_base: TimeBase,
    frame_rate_guess: Option<TimeBase>,
}

impl Stream {
    /// Create a new stream from its raw representation.
    pub(crate) unsafe fn from_raw_ptr(ptr: *mut c_void) -> Self {
        let mut num = 0_u32;
        let mut den = 0_u32;

        ffw_stream_get_time_base(ptr, &mut num, &mut den);

        Stream {
            ptr,
            time_base: TimeBase::new(num, den),
            frame_rate_guess: None,
        }
    }

    /// Get stream time base.
    pub fn time_base(&self) -> TimeBase {
        self.time_base
    }

    /// Get the best-guess frame rate
    pub fn frame_rate_guess(&self) -> Option<TimeBase> {
        self.frame_rate_guess
    }

    pub fn set_frame_rate_guess(&mut self, guess: Option<TimeBase>) {
        self.frame_rate_guess = guess;
    }

    /// Get the r_frame_rate
    pub fn r_frame_rate(&self) -> TimeBase {
        let mut num = 0_u32;
        let mut den = 0_u32;

        unsafe { ffw_stream_get_r_frame_rate(self.ptr, &mut num, &mut den) };

        // avoid division by zero
        let den = if den > 0 { den } else { 1 };

        TimeBase::new(num, den)
    }

    /// Get the stream's index.
    pub fn index(&self) -> usize {
        unsafe { ffw_stream_get_index(self.ptr) as _ }
    }

    /// Get the pts of the first frame of the stream in presentation order.
    pub fn start_time(&self) -> Timestamp {
        let pts = unsafe { ffw_stream_get_start_time(self.ptr) as _ };

        Timestamp::new(pts, self.time_base)
    }

    /// Get the duration of the stream.
    pub fn duration(&self) -> Timestamp {
        let pts = unsafe { ffw_stream_get_duration(self.ptr) as _ };

        Timestamp::new(pts, self.time_base)
    }

    /// Get the number of frames in the stream.
    ///
    /// # Note
    /// The number may not represent the total number of frames, depending on the type of the
    /// stream and the demuxer it may represent only the total number of keyframes.
    pub fn frames(&self) -> Option<u64> {
        let count = unsafe { ffw_stream_get_nb_frames(self.ptr) };

        if count <= 0 {
            None
        } else {
            Some(count as _)
        }
    }

    /// Set the discard flag for this stream.
    pub fn set_discard(&mut self, discard: Discard) {
        unsafe { ffw_stream_set_discard(self.ptr, discard.into_raw()) };
    }

    /// Get codec parameters.
    pub fn codec_parameters(&self) -> CodecParameters {
        unsafe {
            let ptr = ffw_stream_get_codec_parameters(self.ptr);

            if ptr.is_null() {
                panic!("unable to allocate codec parameters");
            }

            CodecParameters::from_raw_ptr(ptr)
        }
    }

    /// Set stream metadata.
    pub fn set_metadata<V>(&mut self, key: &str, value: V)
    where
        V: ToString,
    {
        let key = CString::new(key).expect("invalid metadata key");
        let value = CString::new(value.to_string()).expect("invalid metadata value");

        let ret = unsafe { ffw_stream_set_metadata(self.ptr, key.as_ptr(), value.as_ptr()) };

        if ret < 0 {
            panic!("unable to allocate metadata");
        }
    }

    /// Get stream metadata.
    pub fn get_metadata(&self, key: &str) -> Option<&'static str> {
        let key = CString::new(key).expect("invalid metadata key");

        let value = unsafe { ffw_stream_get_metadata(self.ptr, key.as_ptr()) };

        if value.is_null() {
            None
        } else {
            let value = unsafe { CStr::from_ptr(value as _) };
            Some(value.to_str().unwrap())
        }
    }
}

unsafe impl Send for Stream {}
unsafe impl Sync for Stream {}
