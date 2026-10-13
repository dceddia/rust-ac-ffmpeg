// Frames out of Android's MediaCodec (mediacodec.rs): a decoded picture in a MediaCodec output
// buffer, copied into an AVFrame of the matching FFmpeg pixel format, with the stream's colors
// as FFmpeg's decoder would tag them.

#include <libavcodec/avcodec.h>
#include <libavutil/frame.h>
#include <string.h>

// MediaCodecInfo.CodecCapabilities' color formats that ByteBuffer output comes in
#define COLOR_FormatYUV420Planar 0x13
#define COLOR_FormatYUV420SemiPlanar 0x15
#define COLOR_FormatYUVP010 0x36

// The FFmpeg pixel format of MediaCodec output buffers in `color_format`, AV_PIX_FMT_NONE for
// one this can't copy (vendor tiled layouts, flexible formats)
int ffw_mediacodec_pixel_format(int color_format) {
    switch (color_format) {
    case COLOR_FormatYUV420Planar:
        return AV_PIX_FMT_YUV420P;
    case COLOR_FormatYUV420SemiPlanar:
        return AV_PIX_FMT_NV12;
    case COLOR_FormatYUVP010:
        return AV_PIX_FMT_P010LE;
    default:
        return AV_PIX_FMT_NONE;
    }
}

static void copy_rows(uint8_t* dst, int dst_stride, const uint8_t* src, int src_stride,
                      int row_bytes, int rows) {
    for (int row = 0; row < rows; row++) {
        memcpy(dst + (size_t)row * dst_stride, src + (size_t)row * src_stride, row_bytes);
    }
}

// A frame of the picture in `data` (`size` bytes, as a MediaCodec output buffer holds it: the
// planes one after another, `stride` bytes a row and `slice_height` rows a plane), cropped to
// `width` x `height` at (`left`, `top`), with the colors and sample aspect ratio of `params`,
// `pts` and `duration`. NULL if the buffer is too small for that or the format is unknown.
AVFrame* ffw_mediacodec_frame_new(const uint8_t* data, size_t size, int color_format,
                                  int stride, int slice_height, int left, int top, int width,
                                  int height, const AVCodecParameters* params, int64_t pts,
                                  int64_t duration) {
    enum AVPixelFormat format = ffw_mediacodec_pixel_format(color_format);
    if (format == AV_PIX_FMT_NONE || width <= 0 || height <= 0) {
        return NULL;
    }
    // even crops only: the chroma planes are subsampled 2x2
    left &= ~1;
    top &= ~1;

    int bytes_per_sample = format == AV_PIX_FMT_P010LE ? 2 : 1;
    if (stride <= 0 || slice_height <= 0 || left + width > stride / bytes_per_sample
        || top + height > slice_height) {
        return NULL;
    }
    int chroma_rows = (height + 1) / 2;
    // YUV420P: Cb then Cr, each half as wide; NV12 and P010: Cb and Cr interleaved, rows as
    // many bytes as a luma row
    int chroma_stride = format == AV_PIX_FMT_YUV420P ? stride / 2 : stride;
    int chroma_row_bytes = format == AV_PIX_FMT_YUV420P ? (width + 1) / 2
                                                        : ((width + 1) & ~1) * bytes_per_sample;
    size_t chroma_plane_size = (size_t)chroma_stride * (slice_height / 2);
    const uint8_t* luma = data + (size_t)top * stride + (size_t)left * bytes_per_sample;
    const uint8_t* chroma = data + (size_t)stride * slice_height
                            + (size_t)(top / 2) * chroma_stride
                            + (format == AV_PIX_FMT_YUV420P ? left / 2
                                                            : (size_t)left * bytes_per_sample);
    const uint8_t* last_chroma = format == AV_PIX_FMT_YUV420P ? chroma + chroma_plane_size
                                                              : chroma;
    // the buffer holds every row read, the last plane's last row up to the picture's width
    const uint8_t* end = last_chroma + (size_t)(chroma_rows - 1) * chroma_stride
                         + chroma_row_bytes;
    if (end > data + size) {
        return NULL;
    }

    AVFrame* frame = av_frame_alloc();
    if (frame == NULL) {
        return NULL;
    }
    frame->format = format;
    frame->width = width;
    frame->height = height;
    if (av_frame_get_buffer(frame, 0) < 0) {
        av_frame_free(&frame);
        return NULL;
    }

    copy_rows(frame->data[0], frame->linesize[0], luma, stride, width * bytes_per_sample,
              height);
    copy_rows(frame->data[1], frame->linesize[1], chroma, chroma_stride, chroma_row_bytes,
              chroma_rows);
    if (format == AV_PIX_FMT_YUV420P) {
        copy_rows(frame->data[2], frame->linesize[2], chroma + chroma_plane_size, chroma_stride,
                  chroma_row_bytes, chroma_rows);
    }

    // The stream's colors, as FFmpeg's decoder tags its frames: MediaCodec's ByteBuffer output
    // holds the samples as they are coded
    frame->colorspace = params->color_space;
    frame->color_primaries = params->color_primaries;
    frame->color_trc = params->color_trc;
    frame->color_range = params->color_range;
    frame->chroma_location = params->chroma_location;
    frame->sample_aspect_ratio = params->sample_aspect_ratio;
    frame->pts = pts;
    frame->pkt_duration = duration;
    return frame;
}
