#include "libavcodec/defs.h"
#include "libavcodec/packet.h"
#include <libavutil/avutil.h>
#include <libavutil/channel_layout.h>
#include <libavutil/frame.h>
#include <libavutil/imgutils.h>
#include <libavutil/pixdesc.h>
#include <libavutil/pixfmt.h>
#include <libavutil/samplefmt.h>
#include <libavutil/hwcontext.h>
#include <libavcodec/avcodec.h>
#include <libavcodec/codec_par.h>
#include <libavcodec/packet.h>
#include <CoreVideo/CoreVideo.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMedia/CoreMedia.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdbool.h>
#include <stdint.h>


// A struct to hold a reference to the pixel buffer. This gets stored on the frame (AVFrame)
// and uses the frame's buf[0] for refcounting.
typedef struct VTHWFrame {
    CVPixelBufferRef pixbuf;
} VTHWFrame;

static void vthw_buffer_release(void *opaque, uint8_t *data)
{
    (void)opaque;
    VTHWFrame *ref = (VTHWFrame *)data;
    if (ref->pixbuf)
        CVPixelBufferRelease(ref->pixbuf);
    av_free(data);
}

// Rust callback function type that receives an AVFrame (or error info)
// frame: the decoded frame, or NULL on error/skip
// status: 0 on success, OSStatus error code on failure
// pts: presentation timestamp of the frame/error
// `generation` is the decoder's generation when the packet was pushed (see `generation`)
typedef void (*RustFrameCallback)(void *context, AVFrame *frame, int32_t status, int64_t pts, uint64_t generation);

static void didDecompress(
    void *decompressionOutputRefCon,
    void *sourceFrameRefCon,
    OSStatus status,
    VTDecodeInfoFlags infoFlags,
    CVImageBufferRef pixelBuffer,
    CMTime presentationTimeStamp,
    CMTime presentationDuration
);

typedef struct VTDecoder {
    AVCodecParameters *params;
    CMVideoFormatDescriptionRef formatDecsription;
    VTDecompressionSessionRef decompressSession;
    RustFrameCallback rust_callback;
    void *rust_context;
    // Incremented on flush/seek to filter stale async frames. Each frame's callback carries
    // the generation it was pushed in, so the Rust side drops stale ones whenever they arrive.
    uint64_t generation;
    AVBufferRef *hw_device_ref;   // Hardware device context
    AVBufferRef *hw_frames_ref;   // Hardware frames context
    // The pixel buffers VideoToolbox may give: NV12 (8-bit) or P010 (10-bit), in video range
    // and in full range
    OSType output_pixel_formats[2];
} VTDecoder;

static void dict_set_i32(CFMutableDictionaryRef dict, CFStringRef key, int32_t value) {
    CFNumberRef number;
    number = CFNumberCreate(NULL, kCFNumberSInt32Type, &value);
    CFDictionarySetValue(dict, key, number);
    CFRelease(number);
}

static void dict_set_data(CFMutableDictionaryRef dict, CFStringRef key, uint8_t * value, uint64_t length) {
    CFDataRef data;
    data = CFDataCreate(NULL, value, (CFIndex)length);
    CFDictionarySetValue(dict, key, data);
    CFRelease(data);
}

static void dict_set_string(CFMutableDictionaryRef dict, CFStringRef key, const char * value) {
    CFStringRef string;
    string = CFStringCreateWithCString(NULL, value, kCFStringEncodingASCII);
    CFDictionarySetValue(dict, key, string);
    CFRelease(string);
}

static void dict_set_boolean(CFMutableDictionaryRef dict, CFStringRef key, bool value) {
    CFDictionarySetValue(dict, key, value ? kCFBooleanTrue: kCFBooleanFalse);
}

static void dict_set_object(CFMutableDictionaryRef dict, CFStringRef key, CFTypeRef *value) {
    CFDictionarySetValue(dict, key, value);
}

VTDecoder *vt_decoder_create(AVCodecParameters *params, RustFrameCallback callback, void *callback_context) {
    int width = params->width;
    int height = params->height;
    int extradata_size = params->extradata_size;
    uint8_t *extradata = params->extradata;
    OSStatus status;

    VTDecoder* decoder = malloc(sizeof(VTDecoder));
    if(!decoder) {
        return NULL;
    }

    AVCodecParameters *params_copy = avcodec_parameters_alloc();
    if (!params_copy) {
        free(decoder);
        return NULL;
    }
    if(avcodec_parameters_copy(params_copy, params) < 0) {
        avcodec_parameters_free(&params_copy);
        free(decoder);
        return NULL;
    }

    decoder->params = params_copy;
    decoder->rust_callback = callback;
    decoder->rust_context = callback_context;
    decoder->generation = 0;
    // The range the stream's samples are in, as FFmpeg's decoder tags its frames (the caller
    // gives parameters with the SPS's colors where it signals them, else the container's)
    bool full_range = params->color_range == AVCOL_RANGE_JPEG;

    // VideoToolbox is offered pixel buffers in either range and picks the one its decoder works
    // in, so it hands the samples over as they are. Asked for one range, it converts video in
    // the other (a full-range stream asked for video-range buffers came out range-compressed).
    // Its own idea of the range is the bitstream's (an H.264 SPS's VUI; limited where that's
    // silent, whatever the description says), which is the frames' range but where only the
    // container signals full range: `didDecompress` tags each frame.
    bool is_10bit = params->bits_per_raw_sample > 8
                 || params->profile == FF_PROFILE_HEVC_MAIN_10;
    if (is_10bit) {
        decoder->output_pixel_formats[0] = kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange;
        decoder->output_pixel_formats[1] = kCVPixelFormatType_420YpCbCr10BiPlanarFullRange;
    } else {
        decoder->output_pixel_formats[0] = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;
        decoder->output_pixel_formats[1] = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange;
    }

    // Determine codec type and extradata atom name based on codec_id
    CMVideoCodecType codecType;
    const char *extradataAtomName;

    switch (params->codec_id) {
        case AV_CODEC_ID_HEVC:
            // kCMVideoCodecType_HEVC = 'hvc1' (available macOS 10.13+)
            codecType = kCMVideoCodecType_HEVC;
            extradataAtomName = "hvcC";
            break;
        case AV_CODEC_ID_H264:
            codecType = kCMVideoCodecType_H264;
            extradataAtomName = "avcC";
            break;
        default:
            avcodec_parameters_free(&params_copy);
            free(decoder);
            return NULL;
    }


    CFMutableDictionaryRef par = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    CFMutableDictionaryRef atoms = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    CFMutableDictionaryRef extensions = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);

    /* CVPixelAspectRatio dict */
    dict_set_i32(par, CFSTR ("HorizontalSpacing"), 0);
    dict_set_i32(par, CFSTR ("VerticalSpacing"), 0);
    /* SampleDescriptionExtensionAtoms dict - use correct atom name for codec */
    CFStringRef atomKey = CFStringCreateWithCString(NULL, extradataAtomName, kCFStringEncodingASCII);
    dict_set_data(atoms, atomKey, (uint8_t *)extradata, extradata_size);
    CFRelease(atomKey);
    /* Extensions dict */
    dict_set_string(extensions, CFSTR ("CVImageBufferChromaLocationBottomField"), "left");
    dict_set_string(extensions, CFSTR ("CVImageBufferChromaLocationTopField"), "left");
    dict_set_boolean(extensions, CFSTR("FullRangeVideo"), full_range);
    dict_set_object(extensions, CFSTR ("CVPixelAspectRatio"), (CFTypeRef *) par);
    dict_set_object(extensions, CFSTR ("SampleDescriptionExtensionAtoms"), (CFTypeRef *) atoms);

    status = CMVideoFormatDescriptionCreate(kCFAllocatorDefault, codecType, width, height, extensions, &(decoder->formatDecsription));

    CFRelease(extensions);
    CFRelease(atoms);
    CFRelease(par);

    if (status != 0) {
        printf("Error: Creating format description failed with code %d\n", status);
        return NULL;
    }

    CFMutableDictionaryRef destinationPixelBufferAttributes;
    VTDecompressionOutputCallbackRecord outputCallback;

    destinationPixelBufferAttributes = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFNumberRef formats[2] = {
        CFNumberCreate(NULL, kCFNumberSInt32Type, &decoder->output_pixel_formats[0]),
        CFNumberCreate(NULL, kCFNumberSInt32Type, &decoder->output_pixel_formats[1]),
    };
    CFArrayRef formatArray = CFArrayCreate(NULL, (const void **)formats, 2, &kCFTypeArrayCallBacks);
    CFDictionarySetValue(destinationPixelBufferAttributes, kCVPixelBufferPixelFormatTypeKey, formatArray);
    CFRelease(formatArray);
    CFRelease(formats[0]);
    CFRelease(formats[1]);
    dict_set_i32(destinationPixelBufferAttributes, kCVPixelBufferWidthKey, width);
    dict_set_i32(destinationPixelBufferAttributes, kCVPixelBufferHeightKey, height);
    dict_set_boolean(destinationPixelBufferAttributes, kCVPixelBufferMetalCompatibilityKey, true);

    CFMutableDictionaryRef decoderSpec = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    dict_set_boolean(decoderSpec, kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder, true);

    outputCallback.decompressionOutputCallback = didDecompress;
    outputCallback.decompressionOutputRefCon = decoder;
    // NOTE: This is a very expensive call! I've seen it take anywhere from 9ms to 67ms.
    // Definitely try to avoid calling vt_decoder_create more than necessary.
    status = VTDecompressionSessionCreate(kCFAllocatorDefault, decoder->formatDecsription, decoderSpec, destinationPixelBufferAttributes, &outputCallback, &(decoder->decompressSession));
    CFRelease(decoderSpec);
    CFRelease(destinationPixelBufferAttributes);
    if (status != noErr) {
        printf("VT: Decompression session FAILED with status %d\n", (int)status);
        return NULL;
    }

    // Set up hardware device and frames context for av_hwframe_transfer_data support
    decoder->hw_device_ref = NULL;
    decoder->hw_frames_ref = NULL;

    // Create VideoToolbox hardware device context
    int ret = av_hwdevice_ctx_create(&decoder->hw_device_ref, AV_HWDEVICE_TYPE_VIDEOTOOLBOX, NULL, NULL, 0);
    if (ret < 0) {
        printf("Warning: Failed to create VideoToolbox device context: %d\n", ret);
        // Continue without hw context - frames will still work but copy_to_software won't
        return decoder;
    }

    // Create hardware frames context
    decoder->hw_frames_ref = av_hwframe_ctx_alloc(decoder->hw_device_ref);
    if (!decoder->hw_frames_ref) {
        printf("Warning: Failed to allocate hardware frames context\n");
        av_buffer_unref(&decoder->hw_device_ref);
        return decoder;
    }

    AVHWFramesContext *frames_ctx = (AVHWFramesContext *)decoder->hw_frames_ref->data;
    frames_ctx->format = AV_PIX_FMT_VIDEOTOOLBOX;
    frames_ctx->sw_format = is_10bit ? AV_PIX_FMT_P010 : AV_PIX_FMT_NV12;
    frames_ctx->width = width;
    frames_ctx->height = height;

    ret = av_hwframe_ctx_init(decoder->hw_frames_ref);
    if (ret < 0) {
        printf("Warning: Failed to initialize hardware frames context: %d\n", ret);
        av_buffer_unref(&decoder->hw_frames_ref);
        av_buffer_unref(&decoder->hw_device_ref);
        decoder->hw_frames_ref = NULL;
        decoder->hw_device_ref = NULL;
    }

    return decoder;
}

void vt_decoder_free(VTDecoder *decoder) {
    if (!decoder) {
        return;
    }

    if (decoder->decompressSession) {
        VTDecompressionSessionInvalidate(decoder->decompressSession);
        CFRelease(decoder->decompressSession);
    }

    if (decoder->formatDecsription) {
        CFRelease(decoder->formatDecsription);
    }

    if (decoder->params) {
        avcodec_parameters_free(&decoder->params);
    }

    if (decoder->hw_frames_ref) {
        av_buffer_unref(&decoder->hw_frames_ref);
    }

    if (decoder->hw_device_ref) {
        av_buffer_unref(&decoder->hw_device_ref);
    }

    free(decoder);
}

int vt_decode_frame(VTDecoder *decoder, AVPacket* packet, int reset_decoder) {
    (void)reset_decoder;

    CMBlockBufferRef blockBuffer = NULL;
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, (void *)packet->data, packet->size, kCFAllocatorNull, NULL, 0, packet->size, 0, &blockBuffer);
    if (status != kCMBlockBufferNoErr) {
        printf("Error: Creating block buffer failed.\n");
        return -1;
    }

    CMSampleBufferRef sampleBuffer = NULL;
    const size_t sampleSizeArray[] = { packet->size };

    CMTime presentationTime = CMTimeMake(packet->pts * packet->time_base.num, packet->time_base.den);
    CMTime duration = CMTimeMake(packet->duration * packet->time_base.num, packet->time_base.den);
    CMTime decodeTimeStamp = CMTimeMake(packet->dts * packet->time_base.num, packet->time_base.den);

    CMSampleTimingInfo timingInfo = {
        .duration = duration,
        .presentationTimeStamp = presentationTime,
        .decodeTimeStamp = decodeTimeStamp
    };

    VTDecodeFrameFlags flags = kVTDecodeFrame_EnableAsynchronousDecompression;

    if ((packet->flags & AV_PKT_FLAG_DISCARD) != 0) {
        flags |= kVTDecodeFrame_DoNotOutputFrame;
    }

    status = CMSampleBufferCreateReady(kCFAllocatorDefault,
                                       blockBuffer,
                                       decoder->formatDecsription,
                                       1,
                                       1,
                                       &timingInfo,
                                       1,
                                       sampleSizeArray,
                                       &sampleBuffer);
    if (status != kCMBlockBufferNoErr || !sampleBuffer) {
        printf("Error: Creating sample buffer failed.\n");
        return -1;
    }

    CFArrayRef sampleAttachmentsArray = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, true);
    if (sampleAttachmentsArray && CFArrayGetCount(sampleAttachmentsArray) > 0) {
        CFMutableDictionaryRef sampleAttachments = (CFMutableDictionaryRef)CFArrayGetValueAtIndex(sampleAttachmentsArray, 0);

        if ((packet->flags & AV_PKT_FLAG_DISCARD) != 0) {
            dict_set_boolean(sampleAttachments, kCMSampleAttachmentKey_DoNotDisplay, true);
        }

        if (packet->flags & AV_PKT_FLAG_KEY) {
            CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_DependsOnOthers, kCFBooleanFalse);
        } else {
            CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_NotSync, kCFBooleanTrue);
            CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_DependsOnOthers, kCFBooleanTrue);
        }
    }

    VTDecodeInfoFlags flagOut = 0;

    uint64_t generation = __atomic_load_n(&decoder->generation, __ATOMIC_ACQUIRE);
    status = VTDecompressionSessionDecodeFrame(decoder->decompressSession, sampleBuffer, flags, (void*)(uintptr_t)generation, &flagOut);

    switch (status) {
        case noErr:
            break;
        case kVTInvalidSessionErr:
            printf("VT: DecodeFrame FAILED status=%d (Invalid session)\n", (int)status);
            break;
        case kVTVideoDecoderBadDataErr:
            printf("VT: DecodeFrame FAILED status=%d (Bad data)\n", (int)status);
            break;
        default:
            printf("VT: DecodeFrame FAILED status=%d\n", (int)status);
            break;
    }

    CFRelease(sampleBuffer);
    CFRelease(blockBuffer);

    if (status != noErr) {
        return -1;
    }

    return 0;
}

static void didDecompress(void *decompressionOutputRefCon,
                          void *sourceFrameRefCon,
                          OSStatus status,
                          VTDecodeInfoFlags infoFlags __attribute__((unused)),
                          CVImageBufferRef pixelBuffer,
                          CMTime presentationTimeStamp,
                          CMTime presentationDuration )
{
    VTDecoder *decoder = (VTDecoder *)decompressionOutputRefCon;
    if (!decoder || !decoder->rust_callback) {
        return;
    }

    // Filter out stale frames from before a seek/flush.
    // This MUST happen before error/skip handling — otherwise stale errors
    // from a previous generation leak into the channel and get misattributed
    // to the current decode cycle.
    uint64_t frame_generation = (uint64_t)(uintptr_t)sourceFrameRefCon;
    if (frame_generation != __atomic_load_n(&decoder->generation, __ATOMIC_ACQUIRE)) {
        return;
    }

    int64_t pts = presentationTimeStamp.value;

    // These frames come with the error kVTVideoDecoderReferenceMissingErr but it's not really an error.
    //
    // From the doc string:
    //   The kVTDecodeInfo_SkippedLeadingFrameDropped may be set if a leading frame after a sync frame is dropped.
    //   This can happen when a seek to a sync frame is initiated and, due to frame reordering, there are leading
    //   frames following the sync frame that cannot be decoded due to missing references.  Dropping these frames
    //   has no impact to playback since the non-decodeable frames will not be rendered.
    //   If kVTDecodeInfo_SkippedLeadingFrameDropped is set, kVTDecodeInfo_FrameDropped will also be set.
    //
    // I saw these on IMG_0535.mov when seeking: the first 3 frames after a key frame actually had a PTS that fell *before*
    // the key frame, so they referenced the previous GOP. They weren't meant to be rendered.
    if (
      (infoFlags & kVTDecodeInfo_SkippedLeadingFrameDropped) != 0 ||
      (infoFlags & kVTDecodeInfo_FrameDropped) != 0
    ) {
        // Report as skipped, not error
        decoder->rust_callback(decoder->rust_context, NULL, 0, pts, frame_generation);
        return;
    }

    // Check for decode errors
    // Soft errors (bad data, missing reference) - skip this frame, decoder will recover at next keyframe
    // Hard errors (invalid session, malfunction) - report to Rust for fallback handling
    if (status != noErr) {
        switch (status) {
            case kVTVideoDecoderReferenceMissingErr:  // -17694: missing reference frame
                printf("VT: kVTVideoDecoderReferenceMissingErr status=%d pts=%lld\n", (int)status, (long long)pts);
                decoder->rust_callback(decoder->rust_context, NULL, 0, pts, frame_generation);  // Report as skipped, not error
                return;
            case kVTVideoDecoderBadDataErr:  // -12909: corrupted frame data
                printf("VT: kVTVideoDecoderBadDataErr status=%d pts=%lld\n", (int)status, (long long)pts);
                decoder->rust_callback(decoder->rust_context, NULL, (int32_t)status, pts, frame_generation);
                return;
            default:
                // Hard error - report to Rust for potential fallback
                printf("VT: didDecompress error status=%d pts=%lld\n", (int)status, (long long)pts);
                decoder->rust_callback(decoder->rust_context, NULL, (int32_t)status, pts, frame_generation);
                return;
        }
    }

    // Call the Rust callback with NULL if pixelBuffer is NULL (for DoNotDisplay frames)
    if (pixelBuffer == NULL) {
        decoder->rust_callback(decoder->rust_context, NULL, 0, pts, frame_generation);
        return;
    }

    // Create an AVFrame and populate it with the decoded data
    AVFrame *frame = av_frame_alloc();
    if (!frame) {
        return;
    }

    // Create the hardware frame structure to keep track of refcount on the pixel buffer
    VTHWFrame *hw_frame = av_mallocz(sizeof(VTHWFrame));
    if (!hw_frame) {
        av_frame_free(&frame);
        return;
    }
    hw_frame->pixbuf = CVPixelBufferRetain(pixelBuffer);

    // Create AVBufferRef with custom release function. av_frame_clone needs this in buf[0] in order
    // to know that the frame is refcounted, and will ref and unref this buffer appropriately.
    AVBufferRef *buf = av_buffer_create((uint8_t*)hw_frame, sizeof(VTHWFrame),
                                       vthw_buffer_release, NULL, 0);
    if (!buf) {
        CVPixelBufferRelease(hw_frame->pixbuf);
        av_free(hw_frame);
        av_frame_free(&frame);
        return;
    }
    frame->buf[0] = buf;

    // Get dimensions from the pixel buffer
    size_t width = CVPixelBufferGetWidth(pixelBuffer);
    size_t height = CVPixelBufferGetHeight(pixelBuffer);

    // Set up the frame
    frame->format = AV_PIX_FMT_VIDEOTOOLBOX;
    frame->width = width;
    frame->height = height;
    frame->data[3] = (uint8_t *)pixelBuffer;

    // Set hw_frames_ctx so av_hwframe_transfer_data works
    if (decoder->hw_frames_ref) {
        frame->hw_frames_ctx = av_buffer_ref(decoder->hw_frames_ref);
    }

    // The frame's colors: the stream's, as FFmpeg's decoder would tag them. A full-range
    // buffer holds a stream that signals full range in its bitstream, which FFmpeg follows too;
    // a video-range one holds the samples as they are, in whatever range the stream is.
    frame->colorspace = decoder->params->color_space;
    frame->color_primaries = decoder->params->color_primaries;
    frame->color_trc = decoder->params->color_trc;
    OSType buffer_format = CVPixelBufferGetPixelFormatType(pixelBuffer);
    if (buffer_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        || buffer_format == kCVPixelFormatType_420YpCbCr10BiPlanarFullRange
        || decoder->params->color_range == AVCOL_RANGE_JPEG) {
        frame->color_range = AVCOL_RANGE_JPEG;
    } else {
        frame->color_range = AVCOL_RANGE_MPEG;
    }

    // Set timing information
    frame->pts = presentationTimeStamp.value;
    frame->pkt_duration = presentationDuration.value;
    frame->time_base.num = 1;
    frame->time_base.den = presentationTimeStamp.timescale;

    // Call the Rust callback with success
    decoder->rust_callback(decoder->rust_context, frame, 0, pts, frame_generation);

    // Note: The Rust side is responsible for calling av_frame_free when done
}

// Whether VideoToolbox chose a hardware decoder for this session: 1 if it did, 0 if it
// decodes in software (e.g. a format the media engine can't decode), -1 if it doesn't say.
int vt_decoder_uses_hardware(VTDecoder *decoder) {
    if (!decoder || !decoder->decompressSession) {
        return -1;
    }
    CFBooleanRef usingHardware = NULL;
    OSStatus status = VTSessionCopyProperty(
        decoder->decompressSession,
        kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
        kCFAllocatorDefault,
        &usingHardware
    );
    if (status != noErr || !usingHardware) {
        return -1;
    }
    int result = CFBooleanGetValue(usingHardware) ? 1 : 0;
    CFRelease(usingHardware);
    return result;
}

// Drain: emit all delayed frames and wait for them to arrive.
// Does NOT bump generation — we want these frames.
void vt_decoder_flush(VTDecoder *decoder) {
    if (!decoder) {
        return;
    }
    VTDecompressionSessionWaitForAsynchronousFrames(decoder->decompressSession);
}

// Reset: discard all in-flight frames and prepare for new input (e.g. after seek).
// Bumps the generation and returns the new one: the frames in flight still come out, with the
// old one, and are dropped. It doesn't wait for them: the reads and seeking that come next can
// go on while VideoToolbox finishes them.
uint64_t vt_decoder_flush_buffers(VTDecoder *decoder) {
    if (!decoder) {
        return 0;
    }
    return __atomic_add_fetch(&decoder->generation, 1, __ATOMIC_ACQ_REL);
}
