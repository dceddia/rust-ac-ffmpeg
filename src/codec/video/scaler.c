#include <libavutil/frame.h>
#include <libswscale/swscale.h>

#include <stdlib.h>

typedef struct FrameScaler {
    struct SwsContext* scale_context;
    struct AVFrame* frame;

    int tformat;
    int twidth;
    int theight;

    // Byte alignment of the lines of the output frames
    int align;

    // Track the last color info we configured, to avoid reconfiguring every frame
    enum AVColorSpace last_colorspace;
    enum AVColorRange last_color_range;
} FrameScaler;

FrameScaler* ffw_frame_scaler_new(
    int sformat, int swidth, int sheight,
    int tformat, int twidth, int theight,
    int flags, int align);

AVFrame* ffw_frame_scaler_scale(FrameScaler* scaler, const AVFrame* src);
void ffw_frame_scaler_free(FrameScaler* scaler);
int ffw_alg_id_to_flags(size_t id);

static AVFrame* alloc_frame(int format, int width, int height, int align) {
    AVFrame* frame = av_frame_alloc();

    if (frame == NULL) {
        return NULL;
    }

    frame->format = format;
    frame->width = width;
    frame->height = height;

    if (av_frame_get_buffer(frame, align) != 0) {
        av_frame_free(&frame);
    }

    return frame;
}

FrameScaler* ffw_frame_scaler_new(
    int sformat, int swidth, int sheight,
    int tformat, int twidth, int theight,
    int flags, int align) {
    FrameScaler* res = malloc(sizeof(FrameScaler));
    if (res == NULL) {
        return NULL;
    }

    res->scale_context = NULL;
    res->frame = NULL;

    res->tformat = tformat;
    res->twidth = twidth;
    res->theight = theight;
    res->align = align;

    // Initialize color tracking to -1 to force first-frame configuration
    res->last_colorspace = AVCOL_SPC_UNSPECIFIED;
    res->last_color_range = AVCOL_RANGE_UNSPECIFIED;

    res->scale_context = sws_getContext(
        swidth, sheight, sformat,
        twidth, theight, tformat,
        flags, NULL, NULL, NULL);

    if (res->scale_context == NULL) {
        goto err;
    }

    return res;

err:
    ffw_frame_scaler_free(res);

    return NULL;
}

AVFrame* ffw_frame_scaler_scale(FrameScaler* scaler, const AVFrame* src) {
    if (scaler->frame == NULL || !av_frame_is_writable(scaler->frame)) {
        // the internal frame has not been initialized yet or someone holds a
        // reference to it, we need to drop it and create a new one
        av_frame_free(&scaler->frame);

        scaler->frame = alloc_frame(scaler->tformat, scaler->twidth, scaler->theight, scaler->align);

        if (scaler->frame == NULL) {
            return NULL;
        }
    }

    // Configure color space if it changed from last time
    if (src->colorspace != scaler->last_colorspace || src->color_range != scaler->last_color_range) {
        scaler->last_colorspace = src->colorspace;
        scaler->last_color_range = src->color_range;

        // Map FFmpeg colorspace to sws colorspace
        int sws_cs = SWS_CS_DEFAULT;
        switch (src->colorspace) {
            case AVCOL_SPC_BT709: sws_cs = SWS_CS_ITU709; break;
            case AVCOL_SPC_BT470BG:
            case AVCOL_SPC_SMPTE170M: sws_cs = SWS_CS_ITU601; break;
            case AVCOL_SPC_SMPTE240M: sws_cs = SWS_CS_SMPTE240M; break;
            case AVCOL_SPC_BT2020_NCL:
            case AVCOL_SPC_BT2020_CL: sws_cs = SWS_CS_BT2020; break;
            default: sws_cs = SWS_CS_DEFAULT; break;
        }

        const int *table = sws_getCoefficients(sws_cs);
        // AVCOL_RANGE_MPEG = 1 (video range), AVCOL_RANGE_JPEG = 2 (full range)
        int src_full_range = (src->color_range == AVCOL_RANGE_JPEG) ? 1 : 0;
        int dst_full_range = 1;  // Output to full range RGB

        sws_setColorspaceDetails(scaler->scale_context,
            table, src_full_range,
            table, dst_full_range,
            0, 1 << 16, 1 << 16);
    }

    AVFrame* dst = scaler->frame;

    dst->pts = src->pts;

    sws_scale(scaler->scale_context,
        (const uint8_t* const*)src->data, src->linesize, 0, src->height,
        dst->data, dst->linesize);

    return av_frame_clone(dst);
}

void ffw_frame_scaler_free(FrameScaler* scaler) {
    if (scaler == NULL) {
        return;
    }

    av_frame_free(&scaler->frame);
    sws_freeContext(scaler->scale_context);
    free(scaler);
}

#define ALG_ID_FAST_BILINEAR 0
#define ALG_ID_BILINEAR      1
#define ALG_ID_BICUBIC       2

int ffw_alg_id_to_flags(size_t id) {
    switch (id) {
        case ALG_ID_FAST_BILINEAR: return SWS_FAST_BILINEAR;
        case ALG_ID_BILINEAR:      return SWS_BILINEAR;
        case ALG_ID_BICUBIC:       return SWS_BICUBIC;
        default: return 0;
    }
}
