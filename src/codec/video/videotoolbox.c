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
    VTHWFrame *ref = (VTHWFrame *)data;
    if (ref->pixbuf)
        CVPixelBufferRelease(ref->pixbuf);
    av_free(data);
}

// Rust callback function type that receives an AVFrame (or error info)
// frame: the decoded frame, or NULL on error/skip
// status: 0 on success, OSStatus error code on failure
// pts: presentation timestamp of the frame/error
typedef void (*RustFrameCallback)(void *context, AVFrame *frame, int32_t status, int64_t pts);

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
    uint64_t generation;  // Incremented on flush/seek to filter stale async frames
    AVBufferRef *hw_device_ref;   // Hardware device context
    AVBufferRef *hw_frames_ref;   // Hardware frames context
    OSType output_pixel_format;   // NV12 (8-bit) or P010 (10-bit)
} VTDecoder;

typedef enum SeekSampleTreatment {
    SEEK_SAMPLE_TREATMENT_NORMAL = 0,
    SEEK_SAMPLE_TREATMENT_DECODE_ONLY = 1,
    SEEK_SAMPLE_TREATMENT_SAVE_FOR_FUTURE = 2,
    SEEK_SAMPLE_TREATMENT_DISPLAY_NOW = 3,
} SeekSampleTreatment;

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

static void dict_remove(CFMutableDictionaryRef dict, CFStringRef key) {
    CFDictionaryRemoveValue(dict, key);
}

static void dict_set_object(CFMutableDictionaryRef dict, CFStringRef key, CFTypeRef *value) {
    CFDictionarySetValue(dict, key, value);
}

// Check if an existing decoder's params match some other set of params
int vt_decoder_can_decode_params(VTDecoder *existing_decoder, AVCodecParameters *params) {
    if (!existing_decoder || !params) {
        return 0;
    }

    if (existing_decoder->params->width != params->width || existing_decoder->params->height != params->height) {
        printf("vt_decoder_can_decode_params: width or height mismatch %dx%d vs %dx%d\n", existing_decoder->params->width, existing_decoder->params->height, params->width, params->height);
        return 0;
    }

    if (existing_decoder->params->extradata_size != params->extradata_size) {
        printf("vt_decoder_can_decode_params: extradata size mismatch\n");
        return 0;
    }

    if (memcmp(existing_decoder->params->extradata, params->extradata, params->extradata_size) != 0) {
        printf("vt_decoder_can_decode_params: extradata mismatch\n");
        return 0;
    }

    if (existing_decoder->params->codec_id != params->codec_id) {
        printf("vt_decoder_can_decode_params: codec ID mismatch\n");
        return 0;
    }

    return 1;
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
    // Choose output pixel format based on bit depth
    bool is_10bit = params->bits_per_raw_sample > 8
                 || params->profile == FF_PROFILE_HEVC_MAIN_10;
    decoder->output_pixel_format = is_10bit
        ? kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange
        : kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange;

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
            printf("Error: Unsupported codec ID %d for VideoToolbox decoding\n", params->codec_id);
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
    dict_set_boolean(extensions, CFSTR("FullRangeVideo"), false);
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
    dict_set_i32(destinationPixelBufferAttributes, kCVPixelBufferPixelFormatTypeKey, decoder->output_pixel_format);
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

    // Match QuickTime: set RealTime mode for better error recovery
    VTSessionSetProperty(decoder->decompressSession, CFSTR("RealTime"), kCFBooleanTrue);

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

int vt_decode_frame(
    VTDecoder *decoder,
    AVPacket* packet,
    int reset_decoder,
    int treatment_raw,
    int hevc_sync_nal_type
) {
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

    SeekSampleTreatment treatment = (SeekSampleTreatment)treatment_raw;
    bool legacy_discard = (packet->flags & AV_PKT_FLAG_DISCARD) != 0;
    if (treatment == SEEK_SAMPLE_TREATMENT_NORMAL && legacy_discard) {
        treatment = SEEK_SAMPLE_TREATMENT_DECODE_ONLY;
    }

    VTDecodeFrameFlags flags =
        kVTDecodeFrame_EnableAsynchronousDecompression |
        kVTDecodeFrame_EnableTemporalProcessing;

    if (treatment == SEEK_SAMPLE_TREATMENT_DECODE_ONLY) {
        flags |= kVTDecodeFrame_DoNotOutputFrame;
    }

    // For keyframes, create a fresh CMFormatDescription. This may signal VT's
    // HEVC decoder to reinitialize parser state at CRA random access points,
    // preventing -12909 (BadDataErr) on RASL frames after a flush+seek.
    CMFormatDescriptionRef fmtDesc = decoder->formatDecsription;
    CMFormatDescriptionRef freshFmtDesc = NULL;
    if (packet->flags & AV_PKT_FLAG_KEY) {
        OSStatus fmtStatus = CMVideoFormatDescriptionCreate(
            kCFAllocatorDefault,
            CMFormatDescriptionGetMediaSubType(decoder->formatDecsription),
            decoder->params->width,
            decoder->params->height,
            CMFormatDescriptionGetExtensions(decoder->formatDecsription),
            &freshFmtDesc);
        if (fmtStatus == noErr) {
            fmtDesc = freshFmtDesc;
        }
    }

    status = CMSampleBufferCreateReady(kCFAllocatorDefault,
                                       blockBuffer,
                                       fmtDesc,
                                       1,
                                       1,
                                       &timingInfo,
                                       1,
                                       sampleSizeArray,
                                       &sampleBuffer);
    if (status != kCMBlockBufferNoErr || !sampleBuffer) {
        printf("Error: Creating sample buffer failed.\n");
        if (freshFmtDesc) CFRelease(freshFmtDesc);
        return -1;
    }

    // Set sample attachments based on seek treatment.
    CFArrayRef sampleAttachmentsArray = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, true);
    if (sampleAttachmentsArray && CFArrayGetCount(sampleAttachmentsArray) > 0) {
        CFMutableDictionaryRef sampleAttachments = (CFMutableDictionaryRef)CFArrayGetValueAtIndex(sampleAttachmentsArray, 0);
        CFStringRef saveForFutureKey = CFSTR("SaveForFuture");
        CFStringRef hevcSyncNalKey = CFSTR("HEVCSyncSampleNALUnitType");

        // reset_decoder flag no longer sets ResetDecoderBeforeDecoding.
        // See comment below on keyframes — relying on NAL-level signaling instead.
        (void)reset_decoder;

        dict_set_boolean(sampleAttachments, kCMSampleAttachmentKey_EarlierDisplayTimesAllowed, false);
        dict_remove(sampleAttachments, kCMSampleAttachmentKey_DoNotDisplay);
        dict_remove(sampleAttachments, kCMSampleAttachmentKey_DisplayImmediately);
        dict_remove(sampleAttachments, kCMSampleAttachmentKey_IsDependedOnByOthers);
        dict_remove(sampleAttachments, saveForFutureKey);

        if (treatment == SEEK_SAMPLE_TREATMENT_DECODE_ONLY) {
            dict_set_boolean(sampleAttachments, kCMSampleAttachmentKey_DoNotDisplay, true);
        } else if (treatment == SEEK_SAMPLE_TREATMENT_SAVE_FOR_FUTURE) {
            dict_set_boolean(sampleAttachments, kCMSampleAttachmentKey_DoNotDisplay, true);
            CFDictionaryRef saveForFutureTime = CMTimeCopyAsDictionary(presentationTime, kCFAllocatorDefault);
            if (saveForFutureTime) {
                CFDictionarySetValue(sampleAttachments, saveForFutureKey, saveForFutureTime);
                CFRelease(saveForFutureTime);
            }
        } else if (treatment == SEEK_SAMPLE_TREATMENT_DISPLAY_NOW) {
            dict_set_boolean(sampleAttachments, kCMSampleAttachmentKey_DisplayImmediately, true);
            dict_set_boolean(sampleAttachments, kCMSampleAttachmentKey_IsDependedOnByOthers, false);
        }

        if (packet->flags & AV_PKT_FLAG_KEY) {
            CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_DependsOnOthers, kCFBooleanFalse);
            if (hevc_sync_nal_type >= 0) {
                dict_set_i32(sampleAttachments, hevcSyncNalKey, hevc_sync_nal_type);
            }
            // NOTE: Not setting ResetDecoderBeforeDecoding here — QuickTime doesn't set it.
            // For HEVC CRA keyframes, the NAL type (21) signals the access point at the
            // bitstream level. ResetDecoderBeforeDecoding may clear internal HEVC state
            // that the decoder needs to properly identify RASL/RADL leading pictures,
            // causing -12909 (BadDataErr) instead of -17694 (ReferenceMissingErr).
        } else {
            CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_NotSync, kCFBooleanTrue);
            CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_DependsOnOthers, kCFBooleanTrue);
        }
    }

    VTDecodeInfoFlags flagOut = 0;

    status = VTDecompressionSessionDecodeFrame(decoder->decompressSession, sampleBuffer, flags, (void*)(uintptr_t)decoder->generation, &flagOut);

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
    if (freshFmtDesc) CFRelease(freshFmtDesc);

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
    if (frame_generation != decoder->generation) {
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
        decoder->rust_callback(decoder->rust_context, NULL, 0, pts);
        return;
    }

    // Check for decode errors
    // Soft errors (bad data, missing reference) - skip this frame, decoder will recover at next keyframe
    // Hard errors (invalid session, malfunction) - report to Rust for fallback handling
    if (status != noErr) {
        switch (status) {
            case kVTVideoDecoderReferenceMissingErr:  // -17694: missing reference frame
                printf("VT: kVTVideoDecoderReferenceMissingErr status=%d pts=%lld\n", (int)status, (long long)pts);
                decoder->rust_callback(decoder->rust_context, NULL, 0, pts);  // Report as skipped, not error
                return;
            case kVTVideoDecoderBadDataErr:  // -12909: corrupted frame data
                printf("VT: kVTVideoDecoderBadDataErr status=%d pts=%lld\n", (int)status, (long long)pts);
                decoder->rust_callback(decoder->rust_context, NULL, (int32_t)status, pts);
                return;
            default:
                // Hard error - report to Rust for potential fallback
                printf("VT: didDecompress error status=%d pts=%lld\n", (int)status, (long long)pts);
                decoder->rust_callback(decoder->rust_context, NULL, (int32_t)status, pts);
                return;
        }
    }

    // Call the Rust callback with NULL if pixelBuffer is NULL (for DoNotDisplay frames)
    if (pixelBuffer == NULL) {
        decoder->rust_callback(decoder->rust_context, NULL, 0, pts);
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

    // Set color information from codec parameters
    frame->color_range = decoder->params->color_range;
    frame->colorspace = decoder->params->color_space;
    frame->color_primaries = decoder->params->color_primaries;
    frame->color_trc = decoder->params->color_trc;

    // VideoToolbox outputs video range by default, so if unspecified, assume video range
    if (frame->color_range == AVCOL_RANGE_UNSPECIFIED) {
        frame->color_range = AVCOL_RANGE_MPEG;  // Video range (16-235)
    }

    // Set timing information
    frame->pts = presentationTimeStamp.value;
    frame->pkt_duration = presentationDuration.value;
    frame->time_base.num = 1;
    frame->time_base.den = presentationTimeStamp.timescale;

    // Call the Rust callback with success
    decoder->rust_callback(decoder->rust_context, frame, 0, pts);

    // Note: The Rust side is responsible for calling av_frame_free when done
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
// Bumps generation so the callback filters out stale frames.
void vt_decoder_flush_buffers(VTDecoder *decoder) {
    if (!decoder) {
        return;
    }
    decoder->generation++;
    VTDecompressionSessionWaitForAsynchronousFrames(decoder->decompressSession);
}
