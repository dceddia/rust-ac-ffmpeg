//! A/V demuxer.

use std::{
    borrow::{Borrow, BorrowMut},
    convert::TryInto,
    ffi::{CStr, CString},
    io::Read,
    ops::{Deref, DerefMut},
    os::raw::{c_char, c_int, c_uint, c_void},
    ptr,
    time::Duration,
};

use crate::{
    format::{io::IO, stream::Stream},
    packet::Packet,
    time::{TimeBase, Timestamp},
    Error,
};

use super::stream::Discard;

extern "C" {
    fn ffw_guess_input_format(
        short_name: *const c_char,
        file_name: *const c_char,
        mime_type: *const c_char,
    ) -> *mut c_void;

    fn ffw_demuxer_new() -> *mut c_void;
    fn ffw_demuxer_init(
        demuxer: *mut c_void,
        io_context: *mut c_void,
        format: *mut c_void,
    ) -> c_int;
    fn ffw_demuxer_set_initial_option(
        demuxer: *mut c_void,
        key: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn ffw_demuxer_set_option(
        demuxer: *mut c_void,
        key: *const c_char,
        value: *const c_char,
    ) -> c_int;
    fn ffw_demuxer_find_stream_info(demuxer: *mut c_void, max_analyze_duration: i64) -> c_int;
    fn ffw_demuxer_guess_frame_rate(
        demuxer: *mut c_void,
        stream_index: c_uint,
        num_out: *mut u32,
        den_out: *mut u32,
    ) -> c_int;
    fn ffw_demuxer_get_nb_streams(demuxer: *const c_void) -> c_uint;
    fn ffw_demuxer_get_duration(demuxer: *const c_void) -> i64;
    fn ffw_demuxer_get_stream(demuxer: *mut c_void, index: c_uint) -> *mut c_void;
    fn ffw_demuxer_read_frame(
        demuxer: *mut c_void,
        packet: *mut *mut c_void,
        tb_num: *mut u32,
        tb_den: *mut u32,
    ) -> c_int;
    fn ffw_demuxer_seek(
        demuxer: *mut c_void,
        timestamp: i64,
        seek_by: c_int,
        seek_target: c_int,
    ) -> c_int;
    fn ffw_demuxer_free(demuxer: *mut c_void);
    fn ffw_demuxer_get_metadata(demuxer: *mut c_void, key: *const c_char) -> *const c_char;
}

/// Seek type/mode.
enum SeekType {
    Time,
    Byte,
    Frame,
}

impl SeekType {
    /// Get the internal raw representation.
    fn into_raw(self) -> i32 {
        match self {
            SeekType::Time => 0,
            SeekType::Byte => 1,
            SeekType::Frame => 2,
        }
    }
}

/// Used to specify a search direction when a stream cannot seek exactly to the requested target
/// point; timestamp, frame or byte.
pub enum SeekTarget {
    /// Seek, at least, to the requested target point in the stream. If the target cannot be met
    /// then move forward in the stream until a possible seek target can be hit.
    From,
    /// Seek, at most, to the requested target point in the stream. If the target cannot be met
    /// then move backward in the stream until a possible seek target can be hit.
    UpTo,
    /// Force seeking to the requested target point in the stream, even if the Demuxer for this
    /// type of stream, does not support it.
    Precise,
}

impl SeekTarget {
    /// Get the internal raw representation.
    fn into_raw(self) -> i32 {
        match self {
            SeekTarget::From => 0,
            SeekTarget::UpTo => 1,
            SeekTarget::Precise => 2,
        }
    }
}

/// Demuxer builder.
pub struct DemuxerBuilder {
    ptr: *mut c_void,
    input_format: Option<InputFormat>,
}

impl DemuxerBuilder {
    /// Create a new demuxer builder.
    fn new() -> DemuxerBuilder {
        let ptr = unsafe { ffw_demuxer_new() };

        if ptr.is_null() {
            panic!("unable to allocate a demuxer context");
        }

        DemuxerBuilder {
            ptr,
            input_format: None,
        }
    }

    /// Set a demuxer option.
    pub fn set_option<V>(self, name: &str, value: V) -> DemuxerBuilder
    where
        V: ToString,
    {
        let name = CString::new(name).expect("invalid option name");
        let value = CString::new(value.to_string()).expect("invalid option value");

        let ret = unsafe {
            ffw_demuxer_set_initial_option(self.ptr, name.as_ptr() as _, value.as_ptr() as _)
        };

        if ret < 0 {
            panic!("unable to allocate an option");
        }

        self
    }

    /// Set input format. If the input format is not set, it will be guessed from the input.
    pub fn input_format(mut self, format: Option<InputFormat>) -> DemuxerBuilder {
        self.input_format = format;
        self
    }

    fn guess_frame_rate(&self, stream_index: usize) -> Result<TimeBase, Error> {
        let mut num: u32 = 0;
        let mut den: u32 = 0;

        let res = unsafe {
            ffw_demuxer_guess_frame_rate(
                self.ptr,
                stream_index as u32,
                &mut num as *mut u32,
                &mut den as *mut u32,
            )
        };

        if res != 0 {
            Err(Error::new("could not get frame rate, invalid stream index"))
        } else {
            Ok(TimeBase::new(num, den))
        }
    }

    /// Build the demuxer.
    ///
    /// # Arguments
    /// * `io` - an AVIO reader
    pub fn build<T>(mut self, mut io: IO<T>) -> Result<Demuxer<T>, Error>
    where
        T: Read,
    {
        let io_context_ptr = io.io_context_mut().as_mut_ptr();

        let format_ptr = self
            .input_format
            .take()
            .map(|f| f.ptr)
            .unwrap_or(ptr::null_mut());

        let ret = unsafe { ffw_demuxer_init(self.ptr, io_context_ptr, format_ptr) };

        if ret < 0 {
            return Err(Error::from_raw_error_code(ret));
        }

        let stream_count = unsafe { ffw_demuxer_get_nb_streams(self.ptr) };

        let mut streams = Vec::with_capacity(stream_count as usize);

        for i in 0..stream_count {
            let mut stream = unsafe {
                let ptr = ffw_demuxer_get_stream(self.ptr, i as _);

                if ptr.is_null() {
                    return Err(Error::new(format!("unable to get info for stream {}", i)));
                }

                Stream::from_raw_ptr(ptr)
            };

            stream.set_frame_rate_guess(self.guess_frame_rate(i as usize).ok());
            streams.push(stream);
        }

        let ptr = self.ptr;

        self.ptr = ptr::null_mut();

        let res = Demuxer { ptr, io, streams };

        Ok(res)
    }
}

impl Drop for DemuxerBuilder {
    fn drop(&mut self) {
        unsafe { ffw_demuxer_free(self.ptr) }
    }
}

unsafe impl Send for DemuxerBuilder {}
unsafe impl Sync for DemuxerBuilder {}

/// Demuxer.
pub struct Demuxer<T> {
    ptr: *mut c_void,
    io: IO<T>,
    streams: Vec<Stream>,
}

impl Demuxer<()> {
    /// Get a demuxer builder.
    pub fn builder() -> DemuxerBuilder {
        DemuxerBuilder::new()
    }
}

impl<T> Demuxer<T> {
    /// Set an option.
    pub fn set_option<V>(&mut self, name: &str, value: V) -> Result<(), Error>
    where
        V: ToString,
    {
        let name = CString::new(name).expect("invalid option name");
        let value = CString::new(value.to_string()).expect("invalid option value");

        let ret =
            unsafe { ffw_demuxer_set_option(self.ptr, name.as_ptr() as _, value.as_ptr() as _) };

        if ret < 0 {
            Err(Error::from_raw_error_code(ret))
        } else {
            Ok(())
        }
    }

    /// Take the next packet from the demuxer or `None` on EOF.
    pub fn take(&mut self) -> Result<Option<Packet>, Error> {
        let mut pptr = ptr::null_mut();

        let mut tb_num = 0;
        let mut tb_den = 0;

        let ret = unsafe { ffw_demuxer_read_frame(self.ptr, &mut pptr, &mut tb_num, &mut tb_den) };

        if ret < 0 {
            Err(Error::from_raw_error_code(ret))
        } else if pptr.is_null() {
            Ok(None)
        } else {
            let packet = unsafe { Packet::from_raw_ptr(pptr, TimeBase::new(tb_num, tb_den)) };

            Ok(Some(packet))
        }
    }

    /// Seek to a specific timestamp in the stream.
    pub fn seek_to_timestamp(
        &self,
        timestamp: &Timestamp,
        seek_target: SeekTarget,
    ) -> Result<(), Error> {
        let micros = timestamp
            .as_micros()
            .ok_or_else(|| Error::new("null timestamp"))?;

        self.seek(micros, SeekType::Time, seek_target)
    }

    /// Seek to a specific frame in the stream.
    pub fn seek_to_frame(&self, frame: u64, seek_target: SeekTarget) -> Result<(), Error> {
        self.seek(frame as _, SeekType::Frame, seek_target)
    }

    /// Seek to a specific byte offset in the stream.
    pub fn seek_to_byte(&self, offset: u64) -> Result<(), Error> {
        // use SeekTarget::Precise here since this flag seems to be ignored by FFmpeg
        self.seek(offset as _, SeekType::Byte, SeekTarget::Precise)
    }

    /// Seek to a given position.
    fn seek(
        &self,
        target_position: i64,
        seek_by: SeekType,
        seek_target: SeekTarget,
    ) -> Result<(), Error> {
        let res = unsafe {
            ffw_demuxer_seek(
                self.ptr,
                target_position,
                seek_by.into_raw(),
                seek_target.into_raw(),
            )
        };

        if res >= 0 {
            Ok(())
        } else {
            Err(Error::from_raw_error_code(res))
        }
    }

    /// Get streams.
    ///
    /// These are potentially missing some information because stream info hasn't been loaded yet,
    /// so if you need something that's missing, or you want to be absolutely sure the values are as correct
    /// as they can be, call find_stream_info.
    pub fn streams(&self) -> &[Stream] {
        &self.streams
    }

    /// Set the discard flags on the streams such that packets
    /// will only come from the desired stream.
    pub fn read_single_stream(&mut self, stream_index: usize) {
        for stream in &mut self.streams {
            if stream.index() == stream_index {
                stream.set_discard(Discard::Default);
            } else {
                stream.set_discard(Discard::All);
            }
        }
    }

    /// Set the discard flags on the streams such that packets
    /// will only come from the desired streams.
    pub fn enable_streams(&mut self, enabled_streams: &[usize]) {
        for stream in &mut self.streams {
            if enabled_streams.contains(&stream.index()) {
                stream.set_discard(Discard::Default);
            } else {
                stream.set_discard(Discard::All);
            }
        }
    }

    /// Reset the discard flags so that packets come from every stream.
    pub fn read_all_streams(&mut self) {
        for stream in &mut self.streams {
            stream.set_discard(Discard::Default);
        }
    }

    /// Get global metadata.
    pub fn get_metadata(&self, key: &str) -> Option<&'static str> {
        let key = CString::new(key).expect("invalid metadata key");

        let value = unsafe { ffw_demuxer_get_metadata(self.ptr, key.as_ptr()) };

        if value.is_null() {
            None
        } else {
            let value = unsafe { CStr::from_ptr(value as _) };
            Some(value.to_str().unwrap())
        }
    }

    /// Try to find stream info. Optionally, you can pass `max_analyze_duration` which tells FFmpeg
    /// how far it should look for stream info.
    pub fn find_stream_info(
        self,
        max_analyze_duration: Option<Duration>,
    ) -> Result<DemuxerWithStreamInfo<T>, (Self, Error)> {
        let max_analyze_duration = max_analyze_duration
            .unwrap_or_else(|| Duration::from_secs(0))
            .as_micros()
            .try_into()
            .unwrap();

        let ret = unsafe { ffw_demuxer_find_stream_info(self.ptr, max_analyze_duration) };

        if ret < 0 {
            return Err((self, Error::from_raw_error_code(ret)));
        }

        let stream_count = unsafe { ffw_demuxer_get_nb_streams(self.ptr) };

        let mut streams = Vec::with_capacity(stream_count as usize);

        for i in 0..stream_count {
            let mut stream = unsafe {
                let ptr = ffw_demuxer_get_stream(self.ptr, i as _);

                if ptr.is_null() {
                    return Err((
                        self,
                        Error::new(format!("unable to get info for stream {}", i)),
                    ));
                }

                Stream::from_raw_ptr(ptr)
            };

            stream.set_frame_rate_guess(self.guess_frame_rate(i as usize).ok());
            streams.push(stream);
        }

        let res = DemuxerWithStreamInfo {
            inner: self,
            streams,
        };

        Ok(res)
    }

    /// Fill out the streams with whatever info was found in the header, without decoding anything.
    /// This could give misleading or missing values for some stuff. Use find_stream_info to do a
    /// thorough search.
    pub fn with_basic_stream_info(self) -> Result<DemuxerWithStreamInfo<T>, (Self, Error)> {
        let stream_count = unsafe { ffw_demuxer_get_nb_streams(self.ptr) };

        let mut streams = Vec::with_capacity(stream_count as usize);

        for i in 0..stream_count {
            let mut stream = unsafe {
                let ptr = ffw_demuxer_get_stream(self.ptr, i as _);

                if ptr.is_null() {
                    panic!("unable to get stream info");
                }

                Stream::from_raw_ptr(ptr)
            };

            stream.set_frame_rate_guess(self.guess_frame_rate(i as usize).ok());
            streams.push(stream);
        }

        let res = DemuxerWithStreamInfo {
            inner: self,
            streams,
        };

        Ok(res)
    }

    pub fn guess_frame_rate(&self, stream_index: usize) -> Result<TimeBase, Error> {
        let mut num: u32 = 0;
        let mut den: u32 = 0;

        let res = unsafe {
            ffw_demuxer_guess_frame_rate(
                self.ptr,
                stream_index as u32,
                &mut num as *mut u32,
                &mut den as *mut u32,
            )
        };

        if res != 0 {
            Err(Error::new("could not get frame rate, invalid stream index"))
        } else {
            Ok(TimeBase::new(num, den))
        }
    }

    /// Get the duration of the container
    pub fn duration(&self) -> Timestamp {
        let res = unsafe { ffw_demuxer_get_duration(self.ptr) };

        if res == 0 {
            Timestamp::null()
        } else {
            Timestamp::new(res, TimeBase::MICROSECONDS)
        }
    }

    /// Get reference to the underlying IO.
    pub fn io(&self) -> &IO<T> {
        &self.io
    }

    /// Get mutable reference to the underlying IO.
    pub fn io_mut(&mut self) -> &mut IO<T> {
        &mut self.io
    }
}

impl<T> Drop for Demuxer<T> {
    fn drop(&mut self) {
        unsafe { ffw_demuxer_free(self.ptr) }
    }
}

unsafe impl<T> Send for Demuxer<T> where T: Send {}
unsafe impl<T> Sync for Demuxer<T> where T: Sync {}

/// Demuxer with information about individual streams.
pub struct DemuxerWithStreamInfo<T> {
    inner: Demuxer<T>,
    streams: Vec<Stream>,
}

impl<T> DemuxerWithStreamInfo<T> {
    /// Get streams.
    pub fn streams(&self) -> &[Stream] {
        &self.streams
    }

    /// Get the underlying demuxer.
    pub fn into_demuxer(self) -> Demuxer<T> {
        self.inner
    }

    /// Set the discard flags on the streams such that packets
    /// will only come from the desired stream.
    pub fn read_single_stream(&mut self, stream_index: usize) {
        for stream in &mut self.streams {
            if stream.index() == stream_index {
                stream.set_discard(Discard::Default);
            } else {
                stream.set_discard(Discard::All);
            }
        }
    }

    /// Reset the discard flags so that packets come from every stream.
    pub fn read_all_streams(&mut self) {
        for stream in &mut self.streams {
            stream.set_discard(Discard::Default);
        }
    }

    /// Get global metadata.
    pub fn get_metadata(&self, key: &str) -> Option<&'static str> {
        let key = CString::new(key).expect("invalid metadata key");

        let value = unsafe { ffw_demuxer_get_metadata(self.ptr, key.as_ptr()) };

        if value.is_null() {
            None
        } else {
            let value = unsafe { CStr::from_ptr(value as _) };
            Some(value.to_str().unwrap())
        }
    }
}

impl<T> AsRef<Demuxer<T>> for DemuxerWithStreamInfo<T> {
    fn as_ref(&self) -> &Demuxer<T> {
        &self.inner
    }
}

impl<T> AsMut<Demuxer<T>> for DemuxerWithStreamInfo<T> {
    fn as_mut(&mut self) -> &mut Demuxer<T> {
        &mut self.inner
    }
}

impl<T> Borrow<Demuxer<T>> for DemuxerWithStreamInfo<T> {
    fn borrow(&self) -> &Demuxer<T> {
        &self.inner
    }
}

impl<T> BorrowMut<Demuxer<T>> for DemuxerWithStreamInfo<T> {
    fn borrow_mut(&mut self) -> &mut Demuxer<T> {
        &mut self.inner
    }
}

impl<T> Deref for DemuxerWithStreamInfo<T> {
    type Target = Demuxer<T>;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for DemuxerWithStreamInfo<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// FFmpeg input format.
pub struct InputFormat {
    ptr: *mut c_void,
}

impl InputFormat {
    /// Try to find an input format by its name.
    pub fn find_by_name(name: &str) -> Option<InputFormat> {
        let name = CString::new(name).expect("invalid format name");

        let ptr = unsafe { ffw_guess_input_format(name.as_ptr(), ptr::null(), ptr::null()) };

        if ptr.is_null() {
            return None;
        }

        let res = InputFormat { ptr };

        Some(res)
    }

    /// Try to find an input format by a given MIME type.
    pub fn find_by_mime_type(mime_type: &str) -> Option<InputFormat> {
        let mime_type = CString::new(mime_type).expect("invalid MIME type");

        let ptr = unsafe { ffw_guess_input_format(ptr::null(), ptr::null(), mime_type.as_ptr()) };

        if ptr.is_null() {
            return None;
        }

        let res = InputFormat { ptr };

        Some(res)
    }

    /// Try to guess an input format based on a given file name.
    pub fn guess_from_file_name(file_name: &str) -> Option<InputFormat> {
        let file_name = CString::new(file_name).expect("invalid file name");

        let ptr = unsafe { ffw_guess_input_format(ptr::null(), file_name.as_ptr(), ptr::null()) };

        if ptr.is_null() {
            return None;
        }

        let res = InputFormat { ptr };

        Some(res)
    }
}

unsafe impl Send for InputFormat {}
unsafe impl Sync for InputFormat {}
