#include <libavutil/avutil.h>
#include <libavutil/channel_layout.h>
#include <libavutil/frame.h>
#include <libavutil/imgutils.h>
#include <libavutil/pixdesc.h>
#include <libavutil/pixfmt.h>
#include <libavutil/samplefmt.h>
#include <libavutil/hwcontext.h>
#include <CoreVideo/CoreVideo.h>

AVFrame* ffw_frame_new_videotoolbox(CVPixelBufferRef cv_pixel_buffer, int width, int height, int64_t duration_ts) {
    AVFrame* frame;

    frame = av_frame_alloc();
    if (frame == NULL) {
      return NULL;
    }

    frame->format = AV_PIX_FMT_VIDEOTOOLBOX;
    frame->width = width;
    frame->height = height;
    frame->data[0] = NULL;
    frame->data[1] = NULL;
    frame->data[2] = NULL;
    frame->data[3] = (void *)cv_pixel_buffer;
    frame->pkt_duration = duration_ts;

    return frame;
}
