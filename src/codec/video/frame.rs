use std::ptr;

use libc::{c_int, c_void, int64_t};

pub type PixelFormat = c_int;

extern "C" {
    fn ffw_frame_get_format(frame: *const c_void) -> c_int;
    fn ffw_frame_get_width(frame: *const c_void) -> c_int;
    fn ffw_frame_get_height(frame: *const c_void) -> c_int;
    fn ffw_frame_get_pts(frame: *const c_void) -> int64_t;
    fn ffw_frame_set_pts(frame: *mut c_void, pts: int64_t);
    fn ffw_frame_clone(frame: *const c_void) -> *mut c_void;
    fn ffw_frame_free(frame: *mut c_void);
}

/// Mutable video frame.
pub struct VideoFrameMut {
    ptr: *mut c_void,
}

impl VideoFrameMut {
    /// Create a new video frame from its raw representation.
    pub unsafe fn from_raw_ptr(ptr: *mut c_void) -> VideoFrameMut {
        VideoFrameMut { ptr: ptr }
    }

    /// Get frame pixel format.
    pub fn pixel_format(&self) -> PixelFormat {
        unsafe { ffw_frame_get_format(self.ptr) }
    }

    /// Get frame width.
    pub fn width(&self) -> usize {
        unsafe { ffw_frame_get_width(self.ptr) as _ }
    }

    /// Get frame height.
    pub fn height(&self) -> usize {
        unsafe { ffw_frame_get_height(self.ptr) as _ }
    }

    /// Get presentation timestamp.
    pub fn pts(&self) -> i64 {
        unsafe { ffw_frame_get_pts(self.ptr) as _ }
    }

    /// Set presentation timestamp.
    pub fn with_pts(self, pts: i64) -> VideoFrameMut {
        unsafe { ffw_frame_set_pts(self.ptr, pts as _) }

        self
    }

    /// Get raw pointer.
    pub fn as_ptr(&self) -> *const c_void {
        self.ptr
    }

    /// Get mutable raw pointer.
    pub fn as_mut_ptr(&mut self) -> *mut c_void {
        self.ptr
    }

    /// Make the frame immutable.
    pub fn freeze(mut self) -> VideoFrame {
        let ptr = self.ptr;

        self.ptr = ptr::null_mut();

        VideoFrame { ptr: ptr }
    }
}

impl Drop for VideoFrameMut {
    fn drop(&mut self) {
        unsafe { ffw_frame_free(self.ptr) }
    }
}

unsafe impl Send for VideoFrameMut {}
unsafe impl Sync for VideoFrameMut {}

/// Immutable video frame.
pub struct VideoFrame {
    ptr: *mut c_void,
}

impl VideoFrame {
    /// Create a new video frame from its raw representation.
    pub unsafe fn from_raw_ptr(ptr: *mut c_void) -> VideoFrame {
        VideoFrame { ptr: ptr }
    }

    /// Get frame pixel format.
    pub fn pixel_format(&self) -> PixelFormat {
        unsafe { ffw_frame_get_format(self.ptr) }
    }

    /// Get frame width.
    pub fn width(&self) -> usize {
        unsafe { ffw_frame_get_width(self.ptr) as _ }
    }

    /// Get frame height.
    pub fn height(&self) -> usize {
        unsafe { ffw_frame_get_height(self.ptr) as _ }
    }

    /// Get presentation timestamp.
    pub fn pts(&self) -> i64 {
        unsafe { ffw_frame_get_pts(self.ptr) as _ }
    }

    /// Set presentation timestamp.
    pub fn with_pts(self, pts: i64) -> VideoFrame {
        unsafe { ffw_frame_set_pts(self.ptr, pts as _) }

        self
    }

    /// Get raw pointer.
    pub fn as_ptr(&self) -> *const c_void {
        self.ptr
    }
}

impl Clone for VideoFrame {
    fn clone(&self) -> VideoFrame {
        let ptr = unsafe { ffw_frame_clone(self.ptr) };

        if ptr.is_null() {
            panic!("unable to clone a frame");
        }

        VideoFrame { ptr: ptr }
    }
}

impl Drop for VideoFrame {
    fn drop(&mut self) {
        unsafe { ffw_frame_free(self.ptr) }
    }
}

unsafe impl Send for VideoFrame {}
unsafe impl Sync for VideoFrame {}