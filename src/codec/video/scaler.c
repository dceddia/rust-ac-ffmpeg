#include <libavutil/frame.h>
#include <libavutil/opt.h>
#include <libavutil/pixdesc.h>
#include <libswscale/swscale.h>

#include <stdlib.h>
#include <string.h>

typedef struct FrameScaler {
    struct SwsContext* scale_context;
    struct AVFrame* frame;

    int sformat;
    int swidth;
    int sheight;

    int tformat;
    int twidth;
    int theight;

    int flags;

    // Byte alignment of the lines of the output frames
    int align;

    // The range the output frames are in, for YUV and gray targets: AVCOL_RANGE_MPEG or
    // AVCOL_RANGE_JPEG, or AVCOL_RANGE_UNSPECIFIED to keep each frame's own. (RGB is always
    // full range.)
    enum AVColorRange target_range;

    // The matrix the output frames are in, for YUV targets, or AVCOL_SPC_UNSPECIFIED to keep
    // each frame's own. A frame in another matrix that swscale knows is converted to it (through
    // RGB); one whose matrix is unspecified is taken to be in it already.
    enum AVColorSpace target_colorspace;

    // What the context was made for: the source frames' colorspace and whether their samples
    // are in full range, and whether the output's are. A frame that differs gets a new
    // context, since swscale picks its conversion (and whether it converts the range at all)
    // when it's made.
    enum AVColorSpace colorspace;
    int src_full_range;
    int dst_full_range;
    // The matrix the context's output is in
    enum AVColorSpace out_colorspace;
} FrameScaler;

FrameScaler* ffw_frame_scaler_new(
    int sformat, int swidth, int sheight,
    int tformat, int twidth, int theight,
    int flags, int align, int target_range, int target_colorspace);

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

// Whether a format's samples are YUV or gray (which have a range), not RGB
static int has_range(int format) {
    const AVPixFmtDescriptor* desc = av_pix_fmt_desc_get(format);

    return desc != NULL && !(desc->flags & AV_PIX_FMT_FLAG_RGB);
}

// Whether a frame's samples are in full range: its tag says so, or its format is one of the
// "J" formats (yuvj420p and the like), which are full range whatever the tag
static int is_full_range(const AVFrame* frame) {
    switch (frame->format) {
        case AV_PIX_FMT_YUVJ411P:
        case AV_PIX_FMT_YUVJ420P:
        case AV_PIX_FMT_YUVJ422P:
        case AV_PIX_FMT_YUVJ440P:
        case AV_PIX_FMT_YUVJ444P:
            return 1;
        default:
            return frame->color_range == AVCOL_RANGE_JPEG;
    }
}

// swscale's matrix for a colorspace, or 0 for one it doesn't know
static int known_sws_colorspace(enum AVColorSpace colorspace) {
    switch (colorspace) {
        case AVCOL_SPC_BT709: return SWS_CS_ITU709;
        case AVCOL_SPC_FCC: return SWS_CS_FCC;
        case AVCOL_SPC_BT470BG:
        case AVCOL_SPC_SMPTE170M: return SWS_CS_ITU601;
        case AVCOL_SPC_SMPTE240M: return SWS_CS_SMPTE240M;
        case AVCOL_SPC_BT2020_NCL:
        case AVCOL_SPC_BT2020_CL: return SWS_CS_BT2020;
        default: return 0;
    }
}

static int sws_colorspace(enum AVColorSpace colorspace) {
    int known = known_sws_colorspace(colorspace);

    return known ? known : SWS_CS_DEFAULT;
}

// The matrix a frame in `colorspace` comes out in: the target's, when the scaler has one, its
// output is YUV, and swscale knows both matrices (an unspecified or unknown one is left as it
// is); else its own
static enum AVColorSpace output_colorspace(const FrameScaler* scaler, enum AVColorSpace colorspace) {
    if (scaler->target_colorspace == AVCOL_SPC_UNSPECIFIED
        || !has_range(scaler->tformat)
        || !known_sws_colorspace(colorspace)
        || !known_sws_colorspace(scaler->target_colorspace)) {
        return colorspace;
    }

    return scaler->target_colorspace;
}

// A context for frames of this colorspace and range, to this range and matrix, or NULL if
// swscale can't make it. The ranges are given to swscale before it initializes, so it chooses a
// conversion that applies them: set afterwards, a context whose source and target formats match
// (a same-size yuv420p copy, or nv12 to yuv420p) keeps its unscaled path, which ignores them.
static struct SwsContext* new_context(
    const FrameScaler* scaler,
    enum AVColorSpace colorspace,
    int src_full_range,
    enum AVColorSpace out_colorspace,
    int dst_full_range) {
    struct SwsContext* context = sws_alloc_context();

    if (context == NULL) {
        return NULL;
    }

    // A "J" format is its plain one in full range, which `src_full_range` says (swscale warns
    // about the deprecated formats)
    int sformat = scaler->sformat;
    switch (sformat) {
        case AV_PIX_FMT_YUVJ420P: sformat = AV_PIX_FMT_YUV420P; break;
        case AV_PIX_FMT_YUVJ422P: sformat = AV_PIX_FMT_YUV422P; break;
        case AV_PIX_FMT_YUVJ444P: sformat = AV_PIX_FMT_YUV444P; break;
        case AV_PIX_FMT_YUVJ440P: sformat = AV_PIX_FMT_YUV440P; break;
        case AV_PIX_FMT_YUVJ411P: sformat = AV_PIX_FMT_YUV411P; break;
        default: break;
    }

    av_opt_set_int(context, "srcw", scaler->swidth, 0);
    av_opt_set_int(context, "srch", scaler->sheight, 0);
    av_opt_set_int(context, "src_format", sformat, 0);
    av_opt_set_int(context, "dstw", scaler->twidth, 0);
    av_opt_set_int(context, "dsth", scaler->theight, 0);
    av_opt_set_int(context, "dst_format", scaler->tformat, 0);
    av_opt_set_int(context, "sws_flags", scaler->flags, 0);
    av_opt_set_int(context, "src_range", src_full_range, 0);
    av_opt_set_int(context, "dst_range", dst_full_range, 0);

    if (sws_init_context(context, NULL, NULL) < 0) {
        sws_freeContext(context);
        return NULL;
    }

    // The matrices (for conversions to and from RGB, and from one YUV matrix to another, which
    // swscale makes through RGB), with the same ranges
    const int* src_table = sws_getCoefficients(sws_colorspace(colorspace));
    const int* dst_table = sws_getCoefficients(sws_colorspace(out_colorspace));
    int ret = sws_setColorspaceDetails(context,
        src_table, src_full_range,
        dst_table, dst_full_range,
        0, 1 << 16, 1 << 16);
    if (ret < 0 && src_table != dst_table) {
        sws_freeContext(context);
        return NULL;
    }

    return context;
}

// A context for frames of this colorspace and range, to this range, and the matrix its output
// is in (`out_colorspace`): the target's, if swscale can convert to it, else the frame's own
static struct SwsContext* make_context(
    const FrameScaler* scaler,
    enum AVColorSpace colorspace,
    int src_full_range,
    int dst_full_range,
    enum AVColorSpace* out_colorspace) {
    *out_colorspace = output_colorspace(scaler, colorspace);
    struct SwsContext* context =
        new_context(scaler, colorspace, src_full_range, *out_colorspace, dst_full_range);
    if (context != NULL || *out_colorspace == colorspace) {
        return context;
    }

    // swscale can't convert the matrix between these formats (a conversion it already makes in
    // two steps): the samples keep theirs, and are tagged so
    av_log(NULL, AV_LOG_WARNING, "The scaler can't convert the matrix from %s to %s\n",
        av_color_space_name(colorspace),
        av_color_space_name(*out_colorspace));
    *out_colorspace = colorspace;

    return new_context(scaler, colorspace, src_full_range, colorspace, dst_full_range);
}

// Whether a frame goes from NV12 or NV21 to yuv420p as it is: the same size, range and matrix.
// swscale makes that copy with its unscaled path, whose x86 SIMD (`ff_nv12ToUV`) writes each
// chroma row with aligned stores: on a tightly packed output frame (`align` 1), whose rows
// mostly aren't 16-byte aligned, it crashed (Windows, Intel Macs; arm64 is fine).
static int is_plain_deinterleave(const FrameScaler* scaler, const AVFrame* src) {
    return (scaler->sformat == AV_PIX_FMT_NV12 || scaler->sformat == AV_PIX_FMT_NV21)
        && scaler->tformat == AV_PIX_FMT_YUV420P
        && scaler->swidth == scaler->twidth
        && scaler->sheight == scaler->theight
        && scaler->src_full_range == scaler->dst_full_range
        && scaler->out_colorspace == src->colorspace;
}

// NV12 (or NV21) to yuv420p of the same size: the luma rows copied, the chroma deinterleaved
static void deinterleave(const FrameScaler* scaler, const AVFrame* src, AVFrame* dst) {
    int chroma_width = (scaler->swidth + 1) / 2;
    int chroma_height = (scaler->sheight + 1) / 2;
    int u_offset = scaler->sformat == AV_PIX_FMT_NV12 ? 0 : 1;

    for (int y = 0; y < scaler->sheight; y++) {
        memcpy(dst->data[0] + y * dst->linesize[0],
            src->data[0] + y * src->linesize[0],
            scaler->swidth);
    }
    for (int y = 0; y < chroma_height; y++) {
        const uint8_t* uv = src->data[1] + y * src->linesize[1];
        uint8_t* u = dst->data[1] + y * dst->linesize[1];
        uint8_t* v = dst->data[2] + y * dst->linesize[2];
        for (int x = 0; x < chroma_width; x++) {
            u[x] = uv[2 * x + u_offset];
            v[x] = uv[2 * x + 1 - u_offset];
        }
    }
}

FrameScaler* ffw_frame_scaler_new(
    int sformat, int swidth, int sheight,
    int tformat, int twidth, int theight,
    int flags, int align, int target_range, int target_colorspace) {
    FrameScaler* res = malloc(sizeof(FrameScaler));
    if (res == NULL) {
        return NULL;
    }

    res->scale_context = NULL;
    res->frame = NULL;

    res->sformat = sformat;
    res->swidth = swidth;
    res->sheight = sheight;

    res->tformat = tformat;
    res->twidth = twidth;
    res->theight = theight;

    res->flags = flags;
    res->align = align;
    res->target_range = target_range;
    res->target_colorspace = target_colorspace;

    // Check now that swscale can make this conversion at all: a context for frames in the
    // format's own range
    int src_full_range = sformat == AV_PIX_FMT_YUVJ420P
        || sformat == AV_PIX_FMT_YUVJ422P
        || sformat == AV_PIX_FMT_YUVJ444P
        || sformat == AV_PIX_FMT_YUVJ440P
        || sformat == AV_PIX_FMT_YUVJ411P;
    int dst_full_range = !has_range(tformat)
        || (target_range == AVCOL_RANGE_UNSPECIFIED ? src_full_range : target_range == AVCOL_RANGE_JPEG);

    res->colorspace = AVCOL_SPC_UNSPECIFIED;
    res->src_full_range = src_full_range;
    res->dst_full_range = dst_full_range;
    res->scale_context = make_context(
        res, res->colorspace, src_full_range, dst_full_range, &res->out_colorspace);

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

    // The frame's own range, and the output's: the target's, or the frame's if the scaler keeps
    // each frame's. RGB is full range.
    int src_full_range = is_full_range(src);
    int dst_full_range;
    if (!has_range(scaler->tformat)) {
        dst_full_range = 1;
    } else if (scaler->target_range == AVCOL_RANGE_UNSPECIFIED) {
        dst_full_range = src_full_range;
    } else {
        dst_full_range = scaler->target_range == AVCOL_RANGE_JPEG;
    }

    if (src->colorspace != scaler->colorspace
        || src_full_range != scaler->src_full_range
        || dst_full_range != scaler->dst_full_range) {
        enum AVColorSpace out_colorspace;
        struct SwsContext* context = make_context(
            scaler, src->colorspace, src_full_range, dst_full_range, &out_colorspace);
        if (context == NULL) {
            return NULL;
        }

        sws_freeContext(scaler->scale_context);
        scaler->scale_context = context;
        scaler->colorspace = src->colorspace;
        scaler->src_full_range = src_full_range;
        scaler->dst_full_range = dst_full_range;
        scaler->out_colorspace = out_colorspace;
    }

    AVFrame* dst = scaler->frame;

    dst->pts = src->pts;

    // The output's colors: the source's, in the output's matrix and range
    dst->colorspace = scaler->out_colorspace;
    dst->color_primaries = src->color_primaries;
    dst->color_trc = src->color_trc;
    dst->chroma_location = src->chroma_location;
    if (has_range(scaler->tformat)) {
        dst->color_range = dst_full_range ? AVCOL_RANGE_JPEG : AVCOL_RANGE_MPEG;
    } else {
        dst->color_range = AVCOL_RANGE_JPEG;
    }

    if (is_plain_deinterleave(scaler, src)) {
        deinterleave(scaler, src, dst);
    } else {
        sws_scale(scaler->scale_context,
            (const uint8_t* const*)src->data, src->linesize, 0, src->height,
            dst->data, dst->linesize);
    }

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
