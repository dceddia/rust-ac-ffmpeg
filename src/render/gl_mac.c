#include <assert.h>
#include <IOSurface/IOSurface.h>
#include <CoreVideo/CoreVideo.h>
#include <OpenGL/gl.h>
#include <OpenGL/OpenGL.h>
#include <OpenGL/CGLIOSurface.h>
#include <libavutil/hwcontext.h>

typedef struct RenderContext
{
  CVPixelBufferRef pixbuf;
  CVOpenGLTextureCacheRef textureCache;
  CVOpenGLTextureRef lumaTexture;
  CVOpenGLTextureRef chromaTexture;
  GLuint textures[2];
} RenderContext;

RenderContext *gl_renderer_new();
int gl_renderer_render(RenderContext *ctx, CVPixelBufferRef pixelBuffer, GLint sampler_y_location, GLint sampler_uv_location);
// int gl_renderer_render(RenderContext *ctx, const void *context, CVPixelBufferRef pixelBuffer, uint num_textures, uint *textures);
void cleanup_textures(RenderContext *ctx);
void gl_renderer_free(RenderContext *ctx);

struct pixel_attr
{
  CGLPixelFormatAttribute attr;
  const char *attr_name;
};

static struct pixel_attr pixel_attrs[] = {
    {kCGLPFAAllRenderers, "All Renderers"},
    {kCGLPFADoubleBuffer, "Double Buffered"},
    {kCGLPFAAuxBuffers, "Aux Buffers"},
    {kCGLPFAColorSize, "Color Size"},
    {kCGLPFAAlphaSize, "Alpha Size"},
    {kCGLPFADepthSize, "Depth Size"},
    {kCGLPFAStencilSize, "Stencil Size"},
    {kCGLPFAAccumSize, "Accum Size"},
    {kCGLPFAMinimumPolicy, "Minimum Policy"},
    {kCGLPFAMaximumPolicy, "Maximum Policy"},
    {kCGLPFASampleBuffers, "Sample Buffers"},
    {kCGLPFASamples, "Samples"},
    {kCGLPFAAuxDepthStencil, "Aux Depth Stencil"},
    {kCGLPFAColorFloat, "Color Float"},
    {kCGLPFAMultisample, "Multisample"},
    {kCGLPFASupersample, "Supersample"},
    {kCGLPFARendererID, "Renderer ID"},
    {kCGLPFANoRecovery, "No Recovery"},
    {kCGLPFAAccelerated, "Accelerated"},
    {kCGLPFAClosestPolicy, "Closest Policy"},
    {kCGLPFABackingStore, "Backing Store"},
    {kCGLPFADisplayMask, "Display Mask"},
    {kCGLPFAAllowOfflineRenderers, "Allow Offline Renderers"},
    {kCGLPFAAcceleratedCompute, "Accelerated Compute"},
    {kCGLPFAOpenGLProfile, "OpenGL Profile"},
    {kCGLPFAVirtualScreenCount, "Virtual Screen Count"},
#if MAC_OS_X_VERSION_MAX_ALLOWED < MAC_OS_X_VERSION_10_11
    {kCGLPFAStereo, "Stereo"},
#endif
#if MAC_OS_X_VERSION_MAX_ALLOWED < MAC_OS_X_VERSION_10_9
    {kCGLPFACompliant, "Compliant"},
    {kCGLPFARemotePBuffer, "Remote PBuffer"},
    {kCGLPFASingleRenderer, "Single Renderer"},
    {kCGLPFAWindow, "Window"},
#endif
#if MAC_OS_X_VERSION_MAX_ALLOWED < MAC_OS_X_VERSION_10_7
//  {kCGLPFAOffScreen, "Off Screen"},
//  {kCGLPFAPBuffer, "PBuffer"},
#endif
#if MAC_OS_X_VERSION_MAX_ALLOWED < MAC_OS_X_VERSION_10_6
//  {kCGLPFAFullScreen, "Full Screen"},
#endif
#if MAC_OS_X_VERSION_MAX_ALLOWED < MAC_OS_X_VERSION_10_5
//  {kCGLPFAMPSafe, "MP Safe"},
//  {kCGLPFAMultiScreen, "Multi Screen"},
//  {kCGLPFARobust, "Robust"},
#endif
};

void gst_gl_context_cocoa_dump_pixel_format(CGLPixelFormatObj fmt)
{
  int i;

  for (i = 0; i < 26; i++)
  {
    GLint val;
    CGLError ret = CGLDescribePixelFormat(fmt, 0, pixel_attrs[i].attr, &val);

    if (ret != kCGLNoError)
    {
      printf("failed to get pixel format %p attribute %s\n", fmt, pixel_attrs[i].attr_name);
    }
    else
    {
      printf("Pixel format %p attr %s = %i\n", fmt, pixel_attrs[i].attr_name, val);
    }
  }
}

RenderContext *gl_renderer_new()
{
  RenderContext *ctx = malloc(sizeof(RenderContext));

  CGLContextObj glContext = CGLGetCurrentContext();
  if (!glContext)
  {
    free(ctx);
    return NULL;
  }

  // TODO/BUG: does this need to be retained?
  CGLPixelFormatObj pixelFormat = CGLGetPixelFormat(glContext);

  // Create a texture cache. This will be used later to efficiently create textures out of CVPixelBuffers
  // kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
  printf("glContext pixelformat = 0x%x\n", pixelFormat);
  gst_gl_context_cocoa_dump_pixel_format(pixelFormat);
  CVOpenGLTextureCacheCreate(kCFAllocatorDefault, NULL, glContext, pixelFormat, NULL, &ctx->textureCache);

  glGenTextures(2, ctx->textures);

  ctx->pixbuf = NULL;
  ctx->lumaTexture = NULL;
  ctx->chromaTexture = NULL;

  return ctx;
}

// Pass a frame to render, and an array of textures to render to (1 per plane)
int gl_renderer_render(RenderContext *ctx, CVPixelBufferRef pixelBuffer, GLint sampler_y_location, GLint sampler_uv_location)
{
  GLuint internal_formats[2] = {GL_R8, GL_RG8};
  GLenum formats[2] = {GL_RED, GL_RG};

  // Get the underlying IOSurface
  IOSurfaceRef surface = CVPixelBufferGetIOSurface(pixelBuffer);
  if (!surface)
  {
    return -1;
  }

  bool is_planar = CVPixelBufferIsPlanar(pixelBuffer);
  int num_planes = CVPixelBufferGetPlaneCount(pixelBuffer);
  if (num_planes != 2)
  {
    printf("Expected 2 planes, got a pixel buffer with %d\n", num_planes);
    return -1;
  }

  int plane = 0;
  glActiveTexture(GL_TEXTURE0);
  glBindTexture(GL_TEXTURE_RECTANGLE_ARB, ctx->textures[0]);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
  glUniform1i(sampler_y_location, 0);

  CGLContextObj glContext = CGLGetCurrentContext();
  printf("Current context obj is %p\n", ctx);
  CGLError err = CGLTexImageIOSurface2D(
      glContext,
      GL_TEXTURE_RECTANGLE_ARB,
      GL_R8, // GL_R8 for first plane, GL_RG8 for second plane
      IOSurfaceGetWidthOfPlane(surface, plane),
      IOSurfaceGetHeightOfPlane(surface, plane),
      GL_RED, // GL_RED for first plane, GL_RG for second plane
      GL_UNSIGNED_BYTE,
      surface,
      plane);
  if (err)
  {
    fprintf(stderr, "Error in CGLTexImageIOSurface2D, plane 0: %d\n", err);
  }

  plane = 1;
  glActiveTexture(GL_TEXTURE1);
  glBindTexture(GL_TEXTURE_RECTANGLE_ARB, ctx->textures[1]);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
  glTexParameteri(GL_TEXTURE_RECTANGLE_ARB, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
  glUniform1i(sampler_uv_location, 1);

  err = CGLTexImageIOSurface2D(
      glContext,
      GL_TEXTURE_RECTANGLE_ARB,
      GL_RG8, // GL_R8 for first plane, GL_RG8 for second plane
      IOSurfaceGetWidthOfPlane(surface, plane),
      IOSurfaceGetHeightOfPlane(surface, plane),
      GL_RG, // GL_RED for first plane, GL_RG for second plane
      GL_UNSIGNED_BYTE,
      surface,
      plane);
  if (err)
  {
    fprintf(stderr, "Error in CGLTexImageIOSurface2D, plane 1: %d\n", err);
  }

  if (err != kCGLNoError)
  {
    fprintf(stderr, "Error in CGLTexImageIOSurface2D: %d\n", err);
    return -1;
  }

  printf("done render\n");

  return 0;
}

const char printPixelFormatType(CVPixelBufferRef pixelBuffer)
{
  FourCharCode type = CVPixelBufferGetPixelFormatType(pixelBuffer);
  // byteswapped, print in reverse
  for (int i = 3; i >= 0; i--)
  {
    printf("%c", ((char *)&type)[i]);
  }
}

// // Pass a frame to render, and an array of textures to render to (1 per plane)
// int gl_renderer_render(RenderContext *ctx, CVPixelBufferRef pixelBuffer)
// {
//   CVReturn err;
//   size_t width = CVPixelBufferGetWidth(pixelBuffer);
//   size_t height = CVPixelBufferGetHeight(pixelBuffer);

//   printf("gl_renderer_render pixelBuffer is %p, format: ", pixelBuffer);
//   printPixelFormatType(pixelBuffer);
//   printf("\n");
//   if (!ctx->textureCache)
//   {
//     return -1;
//   }

//   cleanup_textures(ctx);

//   // CVOpenGLESTextureCacheCreateTextureFromImage will create GLES texture
//   // optimally from CVImageBufferRef.

//   // Y-plane
//   glActiveTexture(GL_TEXTURE0);
//   err = CVOpenGLTextureCacheCreateTextureFromImage(kCFAllocatorDefault,
//                                                    ctx->textureCache,
//                                                    pixelBuffer,
//                                                    NULL,
//                                                    &ctx->lumaTexture);
//   if (err)
//   {
//     fprintf(stderr, "Error creating luma texture: %d\n", err);
//   }
//   else
//   {
//     printf("luma: success!\n");
//   }

//   glBindTexture(CVOpenGLTextureGetTarget(ctx->lumaTexture), CVOpenGLTextureGetName(ctx->lumaTexture));
//   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
//   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

//   // UV-plane
//   glActiveTexture(GL_TEXTURE1);
//   err = CVOpenGLTextureCacheCreateTextureFromImage(kCFAllocatorDefault,
//                                                    ctx->textureCache,
//                                                    pixelBuffer,
//                                                    NULL,
//                                                    &ctx->chromaTexture);
//   if (err)
//   {
//     fprintf(stderr, "Error creating chroma texture: %d\n", err);
//   }
//   else
//   {
//     printf("chroma: success!\n");
//   }

//   glBindTexture(CVOpenGLTextureGetTarget(ctx->chromaTexture), CVOpenGLTextureGetName(ctx->chromaTexture));
//   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
//   glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

//   return 0;
// }

// Called every frame to clean up the previous textures
void cleanup_textures(RenderContext *ctx)
{
  if (ctx->lumaTexture)
  {
    CFRelease(ctx->lumaTexture);
    ctx->lumaTexture = NULL;
  }

  if (ctx->chromaTexture)
  {
    CFRelease(ctx->chromaTexture);
    ctx->chromaTexture = NULL;
  }

  // Periodic texture cache flush every frame
  CVOpenGLTextureCacheFlush(ctx->textureCache, 0);
}

void gl_renderer_free(RenderContext *ctx)
{
  if (ctx)
  {
    cleanup_textures(ctx);
    free(ctx);
  }
}