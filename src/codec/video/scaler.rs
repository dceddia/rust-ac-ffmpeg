//! Video frame scaler.

use std::os::raw::{c_int, c_void};

use crate::{
    codec::video::{PixelFormat, VideoFrame, AVCOL_RANGE_UNSPECIFIED, AVCOL_SPC_UNSPECIFIED},
    Error,
};

const ALG_ID_FAST_BILINEAR: usize = 0;
const ALG_ID_BILINEAR: usize = 1;
const ALG_ID_BICUBIC: usize = 2;

extern "C" {
    fn ffw_frame_scaler_new(
        sformat: c_int,
        swidth: c_int,
        sheight: c_int,
        tformat: c_int,
        twidth: c_int,
        theight: c_int,
        flags: c_int,
        align: c_int,
        target_range: c_int,
        target_colorspace: c_int,
    ) -> *mut c_void;

    fn ffw_frame_scaler_scale(scaler: *mut c_void, src: *const c_void) -> *mut c_void;

    fn ffw_frame_scaler_free(scaler: *mut c_void);

    fn ffw_alg_id_to_flags(id: usize) -> c_int;
}

/// Scaling algorithm.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum Algorithm {
    FastBilinear,
    Bilinear,
    Bicubic,
}

impl Algorithm {
    /// Get algorithm ID.
    fn id(self) -> usize {
        match self {
            Algorithm::FastBilinear => ALG_ID_FAST_BILINEAR,
            Algorithm::Bilinear => ALG_ID_BILINEAR,
            Algorithm::Bicubic => ALG_ID_BICUBIC,
        }
    }
}

/// Builder for a video frame scaler.
pub struct VideoFrameScalerBuilder {
    sformat: c_int,
    swidth: c_int,
    sheight: c_int,

    tformat: Option<c_int>,
    twidth: c_int,
    theight: c_int,

    flags: c_int,

    alignment: c_int,

    target_range: c_int,

    target_colorspace: c_int,
}

impl VideoFrameScalerBuilder {
    /// Create a new video frame scaler builder.
    fn new() -> Self {
        let default_algorithm = Algorithm::Bicubic;

        let flags = unsafe { ffw_alg_id_to_flags(default_algorithm.id()) };

        Self {
            sformat: -1,
            swidth: 0,
            sheight: 0,

            tformat: None,
            twidth: 0,
            theight: 0,

            flags,

            // Default alignment of `0` means libav will choose the optimal alignment
            alignment: 0,

            target_range: AVCOL_RANGE_UNSPECIFIED as c_int,

            target_colorspace: AVCOL_SPC_UNSPECIFIED as c_int,
        }
    }

    /// Set source pixel format.
    pub fn source_pixel_format(mut self, format: PixelFormat) -> Self {
        self.sformat = format.into_raw();
        self
    }

    /// Set source frame width.
    pub fn source_width(mut self, width: usize) -> Self {
        self.swidth = width as _;
        self
    }

    /// Set source frame height.
    pub fn source_height(mut self, height: usize) -> Self {
        self.sheight = height as _;
        self
    }

    /// Set target pixel format. The default is equal to the source format.
    pub fn target_pixel_format(mut self, format: PixelFormat) -> Self {
        self.tformat = Some(format.into_raw());
        self
    }

    /// Set target frame width.
    pub fn target_width(mut self, width: usize) -> Self {
        self.twidth = width as _;
        self
    }

    /// Set target frame height.
    pub fn target_height(mut self, height: usize) -> Self {
        self.theight = height as _;
        self
    }

    /// Set scaling algorithm. The default is bicubic.
    pub fn algorithm(mut self, algorithm: Algorithm) -> Self {
        self.flags = unsafe { ffw_alg_id_to_flags(algorithm.id()) };

        self
    }

    /// Set the byte alignment of the output frame lines. The default is 0,
    /// which means libav will choose an optimal alignment and the output frames
    /// might have a line size that's larger than the image width.
    ///
    /// The extra padding can cause issues for some operations, like rotation.
    /// Specifying an align of `1` will ensure no padding is added.
    pub fn align(mut self, alignment: usize) -> Self {
        self.alignment = alignment as _;

        self
    }

    /// The range of the output frames when they're YUV or gray (`AVCOL_RANGE_MPEG` or
    /// `AVCOL_RANGE_JPEG`): each frame is converted from its own range (its tag, or its "J"
    /// format) to this one, and tagged with it. `AVCOL_RANGE_UNSPECIFIED` (the default) keeps
    /// each frame's range. RGB output is always full range.
    pub fn target_color_range(mut self, range: u32) -> Self {
        self.target_range = range as _;

        self
    }

    /// The matrix of the output frames when they're YUV (an `AVCOL_SPC_*` value): each frame in
    /// another matrix is converted to it (swscale goes through RGB for that) and tagged with
    /// it. A frame whose matrix is unspecified, or one swscale doesn't know, keeps its own, as
    /// it does when swscale can't convert between the formats. `AVCOL_SPC_UNSPECIFIED` (the
    /// default) keeps each frame's matrix.
    pub fn target_colorspace(mut self, colorspace: u32) -> Self {
        self.target_colorspace = colorspace as _;

        self
    }

    /// Build the video frame scaler.
    pub fn build(self) -> Result<VideoFrameScaler, Error> {
        let tformat = self.tformat.unwrap_or(self.sformat);

        if self.sformat < 0 {
            return Err(Error::new("invalid source format"));
        } else if tformat < 0 {
            return Err(Error::new("invalid target format"));
        } else if self.swidth < 1 {
            return Err(Error::new("invalid source width"));
        } else if self.sheight < 1 {
            return Err(Error::new("invalid source height"));
        } else if self.twidth < 1 {
            return Err(Error::new("invalid target width"));
        } else if self.theight < 1 {
            return Err(Error::new("invalid target height"));
        }

        let ptr = unsafe {
            ffw_frame_scaler_new(
                self.sformat,
                self.swidth,
                self.sheight,
                tformat,
                self.twidth,
                self.theight,
                self.flags,
                self.alignment,
                self.target_range,
                self.target_colorspace,
            )
        };

        if ptr.is_null() {
            return Err(Error::new("unable to create a frame scaler"));
        }

        let res = VideoFrameScaler {
            ptr,

            sformat: PixelFormat::from_raw(self.sformat),
            swidth: self.swidth as _,
            sheight: self.sheight as _,
        };

        Ok(res)
    }
}

/// Video frame scaler.
pub struct VideoFrameScaler {
    ptr: *mut c_void,

    sformat: PixelFormat,
    swidth: usize,
    sheight: usize,
}

impl VideoFrameScaler {
    /// Get a frame scaler builder.
    pub fn builder() -> VideoFrameScalerBuilder {
        VideoFrameScalerBuilder::new()
    }

    /// Scale a given frame.
    pub fn scale(&mut self, frame: &VideoFrame) -> Result<VideoFrame, Error> {
        if self.swidth != frame.width() {
            return Err(Error::new("frame width does not match"));
        } else if self.sheight != frame.height() {
            return Err(Error::new("frame height does not match"));
        } else if self.sformat != frame.pixel_format() {
            return Err(Error::new("frame pixel format does not match"));
        }

        let res = unsafe { ffw_frame_scaler_scale(self.ptr, frame.as_ptr()) };

        if res.is_null() {
            return Err(Error::new("unable to scale a frame - out of memory?"));
        }

        // swscale carries the source PTS through, but `from_raw_ptr` derives the new
        // frame's original_pts from that PTS. Preserve the source's original_pts so it
        // survives scaling (e.g. a held VFR frame keeps its source timestamp).
        let scaled = unsafe { VideoFrame::from_raw_ptr(res, frame.time_base(), frame.rotation()) }
            .with_original_pts(frame.original_pts())
            .with_ambient_illuminance(frame.ambient_illuminance())
            .with_dolby_vision(frame.dolby_vision());

        Ok(scaled)
    }
}

impl Drop for VideoFrameScaler {
    fn drop(&mut self) {
        unsafe { ffw_frame_scaler_free(self.ptr) }
    }
}

unsafe impl Send for VideoFrameScaler {}
unsafe impl Sync for VideoFrameScaler {}
