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
    // Error tracking for fallback detection
    int consecutive_errors;       // Count of consecutive decode errors
    int total_frames_decoded;     // Total successful frames
    int decode_failed;            // Set to 1 when we've determined HW decode won't work
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
    decoder->consecutive_errors = 0;
    decoder->total_frames_decoded = 0;
    decoder->decode_failed = 0;

    // Determine codec type and extradata atom name based on codec_id
    CMVideoCodecType codecType;
    const char *extradataAtomName;

    switch (params->codec_id) {
        case AV_CODEC_ID_HEVC:
            // kCMVideoCodecType_HEVC = 'hvc1' (available macOS 10.13+)
            codecType = 'hvc1';
            extradataAtomName = "hvcC";
            break;
        case AV_CODEC_ID_H264:
        default:
            codecType = kCMVideoCodecType_H264;
            extradataAtomName = "avcC";
            break;
    }

    // Diagnostic: Log codec type being used for decoder creation
    printf("VT: Creating decoder for codec_id=%d, using codecType='%.4s' (%d)\n",
           params->codec_id, (char*)&codecType, codecType);

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
    dict_set_i32(destinationPixelBufferAttributes, kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange);
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
    printf("VT: Decompression session created successfully\n");

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
    frames_ctx->sw_format = AV_PIX_FMT_NV12;  // VideoToolbox outputs NV12
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
    CMBlockBufferRef blockBuffer = NULL;
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, (void *)packet->data, packet->size, kCFAllocatorNull, NULL, 0, packet->size, 0, &blockBuffer);
    if (status != kCMBlockBufferNoErr) {
        printf("Error: Creating block buffer failed.\n");
        return -1;
    }

    CMSampleBufferRef sampleBuffer = NULL;
    const size_t sampleSizeArray[] = { packet->size };

    // Set up timing info from AVPacket
    CMTime presentationTime = CMTimeMake(packet->pts * packet->time_base.num, packet->time_base.den);
    CMTime duration = CMTimeMake(packet->duration * packet->time_base.num, packet->time_base.den);
    CMTime decodeTimeStamp = CMTimeMake(packet->dts * packet->time_base.num, packet->time_base.den);
    CMSampleTimingInfo timingInfo = {
        .duration = duration,
        .presentationTimeStamp = presentationTime,
        .decodeTimeStamp = decodeTimeStamp
    };

    VTDecodeFrameFlags flags = kVTDecodeFrame_EnableAsynchronousDecompression;// | kVTDecodeFrame_EnableTemporalProcessing;

    // Performance improvement: don't produce a frame if it will not be displayed. HUGE speedup.
    if(packet->flags & AV_PKT_FLAG_DISCARD) {
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

    // Set sample attachments based on DISCARD flag
    CFArrayRef sampleAttachmentsArray = CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, true);
    if (sampleAttachmentsArray && CFArrayGetCount(sampleAttachmentsArray) > 0) {
        CFMutableDictionaryRef sampleAttachments = (CFMutableDictionaryRef)CFArrayGetValueAtIndex(sampleAttachmentsArray, 0);

        if (reset_decoder) {
            CFDictionarySetValue(sampleAttachments, kCMSampleBufferAttachmentKey_ResetDecoderBeforeDecoding, kCFBooleanTrue);
        }

        // More attempts at performance improvement during seeking, not sure if this helps honestly.
        // QuickTime sets these keys though.
        if (packet->flags & AV_PKT_FLAG_DISCARD) {
            //CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_NotSync, kCFBooleanTrue);
            CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_DoNotDisplay, kCFBooleanTrue);
        } else {
            // CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_NotSync, kCFBooleanTrue);
            //CFDictionarySetValue(sampleAttachments, kCMSampleAttachmentKey_DisplayImmediately, kCFBooleanTrue);
        }
    }

    VTDecodeInfoFlags flagOut = 0;

    // Diagnostic: Log packet info before decode
    printf("VT: Decoding packet pts=%lld size=%d\n", (long long)packet->pts, packet->size);

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

    int64_t pts = presentationTimeStamp.value;

    // Check for decode errors
    if (status != noErr) {
        printf("VT: didDecompress error status=%d pts=%lld\n", (int)status, (long long)pts);
        decoder->consecutive_errors++;
        // If we've never successfully decoded a frame and hit 10+ errors, mark as failed
        if (decoder->total_frames_decoded == 0 && decoder->consecutive_errors >= 10) {
            if (!decoder->decode_failed) {
                printf("VT: Hardware decode appears unsupported (10 consecutive errors, 0 frames)\n");
                decoder->decode_failed = 1;
            }
        }
        // Send error to Rust
        decoder->rust_callback(decoder->rust_context, NULL, (int32_t)status, pts);
        return;
    }

    // Filter out stale frames from before a seek/flush
    uint64_t frame_generation = (uint64_t)(uintptr_t)sourceFrameRefCon;
    if (frame_generation != decoder->generation) {
        // Don't send stale frames or errors
        return;
    }

    // Call the Rust callback with NULL if pixelBuffer is NULL (for DoNotDisplay frames)
    if (pixelBuffer == NULL) {
        decoder->rust_callback(decoder->rust_context, NULL, 0, pts);
        return;
    }

    printf("VT: didDecompress success, pixelBuffer=%p\n", (void*)pixelBuffer);

    // Reset error tracking on successful decode
    decoder->consecutive_errors = 0;
    decoder->total_frames_decoded++;

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

void vt_decoder_flush(VTDecoder *decoder) {
    if (!decoder) {
        return;
    }

    decoder->generation++;
    VTDecompressionSessionFinishDelayedFrames(decoder->decompressSession);
}

// Returns 1 if hardware decode has failed and fallback is needed
int vt_decoder_has_failed(VTDecoder *decoder) {
    if (!decoder) {
        return 0;
    }
    return decoder->decode_failed;
}

// Returns the number of consecutive decode errors
int vt_decoder_error_count(VTDecoder *decoder) {
    if (!decoder) {
        return 0;
    }
    return decoder->consecutive_errors;
}
