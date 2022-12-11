use crate::{codec::video::VideoFrame, Error};
use std::os::raw::{c_int, c_uint, c_void};

#[cfg(target_os = "macos")]
#[link(name = "IOSurface", kind = "framework")]
extern "C" {
    fn gl_renderer_new() -> *mut c_void;
    fn gl_renderer_render(renderer: *mut c_void, cv_pixel_buffer_ref: *mut c_void) -> c_int;
    fn gl_renderer_free(renderer: *mut c_void);
}

pub struct GlRenderer {
    ptr: *mut c_void,
}

impl GlRenderer {
    /// Create a renderer that will render frames to the given textures.
    ///
    /// This assumes you've already called glGenTextures(), and those IDs are what get passed here.
    pub fn new() -> Self {
        let ptr = unsafe { gl_renderer_new() };
        if ptr.is_null() {
            panic!("unable to allocate a renderer context");
        }
        GlRenderer { ptr }
    }

    pub fn render(&self, frame: &VideoFrame) -> Result<(), Error> {
        let pixel_buffer = frame.planes()[3].data().as_ptr();
        println!("GlRenderer::render pixel_buffer {:p}", pixel_buffer);
        let res = unsafe { gl_renderer_render(self.ptr, pixel_buffer as _) };

        if res != 0 {
            Err(Error::new(format!("failed to render frame")))
        } else {
            Ok(())
        }
    }
}

impl Drop for GlRenderer {
    fn drop(&mut self) {
        unsafe { gl_renderer_free(self.ptr) };
    }
}
