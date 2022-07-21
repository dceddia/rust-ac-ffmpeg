use std::{
    ffi::CString,
    os::raw::{c_char, c_int, c_void},
    ptr,
};

use crate::{
    codec::{
        audio::{AudioFrame, ChannelLayout, SampleFormat},
        CodecError,
    },
    time::TimeBase,
    Error,
};

extern "C" {
    fn ffw_filtergraph_new() -> *mut c_void;
    fn ffw_filtergraph_init_audio(
        filtergraph: *mut c_void,
        time_base_num: u32,
        time_base_den: u32,
        target_channel_layout: u64,
        target_sample_format: c_int,
        target_sample_rate: c_int,
        source_channel_layout: u64,
        source_sample_format: c_int,
        source_sample_rate: c_int,
        filter_description: *const c_char,
    ) -> c_int;
    fn ffw_filtergraph_push_frame(filtergraph: *mut c_void, frame: *mut c_void) -> c_int;
    fn ffw_filtergraph_take_frame(filtergraph: *mut c_void, frame: *mut *mut c_void) -> c_int;
    fn ffw_filtergraph_free(filtergraph: *mut c_void);
}

pub struct FilterGraphBuilder {
    time_base: Option<TimeBase>,

    source_channel_layout: Option<ChannelLayout>,
    source_sample_format: Option<SampleFormat>,
    source_sample_rate: Option<u32>,

    target_channel_layout: Option<ChannelLayout>,
    target_sample_format: Option<SampleFormat>,
    target_sample_rate: Option<u32>,

    filter_description: Option<String>,
}

impl FilterGraphBuilder {
    /// Create a new builder.
    fn new() -> Self {
        Self {
            time_base: None,

            source_channel_layout: None,
            source_sample_format: None,
            source_sample_rate: None,

            target_channel_layout: None,
            target_sample_format: None,
            target_sample_rate: None,

            filter_description: None,
        }
    }

    /// Set source time base.
    pub fn time_base(mut self, time_base: TimeBase) -> Self {
        self.time_base = Some(time_base);
        self
    }

    /// Set the filter description (the actual filter to perform)
    pub fn filter_description(mut self, filter_description: &str) -> Self {
        self.filter_description = Some(filter_description.to_string());
        self
    }

    /// Set source channel layout.
    pub fn source_channel_layout(mut self, channel_layout: ChannelLayout) -> Self {
        self.source_channel_layout = Some(channel_layout);
        self
    }

    /// Set source sample format.
    pub fn source_sample_format(mut self, sample_format: SampleFormat) -> Self {
        self.source_sample_format = Some(sample_format);
        self
    }

    /// Set source sample rate.
    pub fn source_sample_rate(mut self, sample_rate: u32) -> Self {
        self.source_sample_rate = Some(sample_rate);
        self
    }

    /// Set target channel layout.
    pub fn target_channel_layout(mut self, channel_layout: ChannelLayout) -> Self {
        self.target_channel_layout = Some(channel_layout);
        self
    }

    /// Set target sample format.
    pub fn target_sample_format(mut self, sample_format: SampleFormat) -> Self {
        self.target_sample_format = Some(sample_format);
        self
    }

    /// Set target sample rate.
    pub fn target_sample_rate(mut self, sample_rate: u32) -> Self {
        self.target_sample_rate = Some(sample_rate);
        self
    }

    /// Build the resampler.
    pub fn build(self) -> Result<FilterGraph, Error> {
        let time_base = self
            .time_base
            .ok_or_else(|| Error::new("source time base was not set"))?;
        let source_channel_layout = self
            .source_channel_layout
            .ok_or_else(|| Error::new("source channel layout was not set"))?;
        let source_sample_format = self
            .source_sample_format
            .ok_or_else(|| Error::new("source sample format was not set"))?;
        let source_sample_rate = self
            .source_sample_rate
            .ok_or_else(|| Error::new("source sample rate was not set"))?;

        let target_channel_layout = self
            .target_channel_layout
            .ok_or_else(|| Error::new("target channel layout was not set"))?;
        let target_sample_format = self
            .target_sample_format
            .ok_or_else(|| Error::new("target sample format was not set"))?;
        let target_sample_rate = self
            .target_sample_rate
            .ok_or_else(|| Error::new("target sample rate was not set"))?;

        let filter_description = self
            .filter_description
            .ok_or_else(|| Error::new("filter description was not set"))?;

        let filterdesc = CString::new(filter_description).expect("invalid filter description");

        let ptr = unsafe { ffw_filtergraph_new() };

        if ptr.is_null() {
            return Err(Error::new("unable to allocate filter graph"));
        }

        let ret = unsafe {
            ffw_filtergraph_init_audio(
                ptr,
                time_base.num(),
                time_base.den(),
                target_channel_layout.into_raw() as _,
                target_sample_format.into_raw() as _,
                target_sample_rate as _,
                source_channel_layout.into_raw() as _,
                source_sample_format.into_raw() as _,
                source_sample_rate as _,
                filterdesc.as_ptr() as _,
            )
        };
        if ret < 0 {
            unsafe { ffw_filtergraph_free(ptr) };
            return Err(Error::new(
                "unable to create a filter graph for the given configuration",
            ));
        }

        let res = FilterGraph {
            ptr,

            source_channel_layout,
            source_sample_format,
            source_sample_rate,
            target_sample_rate,
        };

        Ok(res)
    }
}

/// Audio resampler.
///
///  # Resampler operation
/// 1. Push an audio frame to the resampler.
/// 2. Take all frames from the resampler until you get None.
/// 3. If there are more frames to be resampled, continue with 1.
/// 4. Flush the resampler.
/// 5. Take all frames from the resampler until you get None.
///
/// Timestamps of the output frames will be in 1 / target_sample_rate time
/// base.
pub struct FilterGraph {
    ptr: *mut c_void,

    source_channel_layout: ChannelLayout,
    source_sample_format: SampleFormat,
    source_sample_rate: u32,
    target_sample_rate: u32,
}

impl FilterGraph {
    /// Get a builder for the filter graph.
    pub fn builder() -> FilterGraphBuilder {
        FilterGraphBuilder::new()
    }

    /// Push a given frame to the graph.
    ///
    /// # Panics
    /// The method panics if the operation is not expected (i.e. another
    /// operation needs to be done).
    pub fn push(&mut self, frame: AudioFrame) -> Result<(), Error> {
        self.try_push(frame).map_err(|err| err.unwrap_inner())
    }

    /// Push a given frame to the graph.
    pub fn try_push(&mut self, frame: AudioFrame) -> Result<(), CodecError> {
        if frame.channel_layout() != self.source_channel_layout {
            return Err(CodecError::error(
                "invalid frame, channel layout does not match",
            ));
        }

        if frame.sample_format() != self.source_sample_format {
            return Err(CodecError::error(
                "invalid frame, sample format does not match",
            ));
        }

        if frame.sample_rate() != self.source_sample_rate {
            return Err(CodecError::error(
                "invalid frame, sample rate does not match",
            ));
        }

        let frame = frame.with_time_base(TimeBase::new(1, self.source_sample_rate));

        unsafe {
            match ffw_filtergraph_push_frame(self.ptr, frame.as_ptr() as *mut c_void) {
                1 => Ok(()),
                0 => Err(CodecError::again(
                    "all frames must be consumed before pushing a new frame",
                )),
                e => Err(CodecError::from_raw_error_code(e)),
            }
        }
    }

    /// Flush the resampler.
    ///
    /// # Panics
    /// The method panics if the operation is not expected (i.e. another
    /// operation needs to be done).
    pub fn flush(&mut self) -> Result<(), Error> {
        self.try_flush().map_err(|err| err.unwrap_inner())
    }

    /// Flush the resampler.
    pub fn try_flush(&mut self) -> Result<(), CodecError> {
        unsafe {
            match ffw_filtergraph_push_frame(self.ptr, ptr::null_mut()) {
                1 => Ok(()),
                0 => Err(CodecError::again(
                    "all frames must be consumed before flushing",
                )),
                e => Err(CodecError::from_raw_error_code(e)),
            }
        }
    }

    /// Take a frame from the resampler (if available).
    pub fn take(&mut self) -> Result<Option<AudioFrame>, Error> {
        let mut fptr = ptr::null_mut();

        let tb = TimeBase::new(1, self.target_sample_rate);

        unsafe {
            match ffw_filtergraph_take_frame(self.ptr, &mut fptr) {
                1 => {
                    if fptr.is_null() {
                        panic!("unable to allocate an audio frame")
                    } else {
                        Ok(Some(AudioFrame::from_raw_ptr(fptr, tb)))
                    }
                }
                0 => Ok(None),
                e => Err(Error::from_raw_error_code(e)),
            }
        }
    }
}

impl Drop for FilterGraph {
    fn drop(&mut self) {
        unsafe { ffw_filtergraph_free(self.ptr) }
    }
}

unsafe impl Send for FilterGraph {}
unsafe impl Sync for FilterGraph {}
