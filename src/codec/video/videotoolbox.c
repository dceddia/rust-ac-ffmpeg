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
#include <libavcodec/packet.h>
#include <CoreVideo/CoreVideo.h>
#include <CoreFoundation/CoreFoundation.h>
#include <VideoToolbox/VideoToolbox.h>
#include <stdbool.h>

// Rust callback function type that receives an AVFrame
typedef void (*RustFrameCallback)(void *context, AVFrame *frame);

static void didDecompress(void *decompressionOutputRefCon,
  void *sourceFrameRefCon,
  OSStatus status,
  VTDecodeInfoFlags infoFlags,
  CVImageBufferRef pixelBuffer,
  CMTime presentationTimeStamp,
  CMTime presentationDuration);

typedef struct VTDecoder {
  CMVideoFormatDescriptionRef formatDecsription;
  VTDecompressionSessionRef decompressSession;
  RustFrameCallback rust_callback;
  void *rust_context;
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

VTDecoder *vt_decoder_create(AVCodecContext *cc, RustFrameCallback callback, void *callback_context) {
  int width = cc->width;
  int height = cc->height;
  int extradata_size = cc->extradata_size;
  uint8_t *extradata = cc->extradata;
  OSStatus status;

  VTDecoder* decoder = malloc(sizeof(VTDecoder));
  if(!decoder) {
    return NULL;
  }
  
  decoder->rust_callback = callback;
  decoder->rust_context = callback_context;

  CFMutableDictionaryRef par = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
  CFMutableDictionaryRef atoms = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
  CFMutableDictionaryRef extensions = CFDictionaryCreateMutable(NULL, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);

  /* CVPixelAspectRatio dict */
  dict_set_i32(par, CFSTR ("HorizontalSpacing"), 0);
  dict_set_i32(par, CFSTR ("VerticalSpacing"), 0);
  /* SampleDescriptionExtensionAtoms dict */
  dict_set_data(atoms, CFSTR ("avcC"), (uint8_t *)extradata, extradata_size);
  /* Extensions dict */
  dict_set_string(extensions, CFSTR ("CVImageBufferChromaLocationBottomField"), "left");
  dict_set_string(extensions, CFSTR ("CVImageBufferChromaLocationTopField"), "left");
  dict_set_boolean(extensions, CFSTR("FullRangeVideo"), false);
  dict_set_object(extensions, CFSTR ("CVPixelAspectRatio"), (CFTypeRef *) par);
  dict_set_object(extensions, CFSTR ("SampleDescriptionExtensionAtoms"), (CFTypeRef *) atoms);

  status = CMVideoFormatDescriptionCreate(NULL, kCMVideoCodecType_H264, width, height, extensions, &(decoder->formatDecsription));

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

  outputCallback.decompressionOutputCallback = didDecompress;
  outputCallback.decompressionOutputRefCon = decoder;
  status = VTDecompressionSessionCreate(kCFAllocatorDefault, decoder->formatDecsription, NULL, destinationPixelBufferAttributes, &outputCallback, &(decoder->decompressSession));
  if (status != noErr) {
    printf("Error: Creating decompression session failed with code %d\n", status);
    return NULL;
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
    
    free(decoder);
}

int vt_decode_frame(VTDecoder *decoder, AVPacket* packet) {

    CVPixelBufferRef outputPixelBuffer = NULL;
    CMBlockBufferRef blockBuffer = NULL;
    OSStatus status = CMBlockBufferCreateWithMemoryBlock(kCFAllocatorDefault, (void *)packet->data, packet->size, kCFAllocatorNull, NULL, 0, packet->size, 0, &blockBuffer);
    if (status != kCMBlockBufferNoErr) {
        printf("Error: Creating block buffer failed.\n");
        return -1;
    }

    CMSampleBufferRef sampleBuffer = NULL;
    const size_t sampleSizeArray[] = { packet->size };
    status = CMSampleBufferCreateReady(kCFAllocatorDefault,
                                       blockBuffer,
                                       decoder->formatDecsription,
                                       1,
                                       0,
                                       NULL,
                                       1,
                                       sampleSizeArray,
                                       &sampleBuffer);
    if (status != kCMBlockBufferNoErr || !sampleBuffer) {
        printf("Error: Creating sample buffer failed.\n");
        return -1;
    }

    VTDecodeFrameFlags flags = kVTDecodeFrame_EnableAsynchronousDecompression;

    // Performance improvement: don't produce a frame if it will not be displayed. HUGE speedup.
    if(packet->flags & AVDISCARD_ALL) {
      flags |= kVTDecodeFrame_DoNotOutputFrame;
    }

    VTDecodeInfoFlags flagOut = 0;
    status = VTDecompressionSessionDecodeFrame(decoder->decompressSession, sampleBuffer, flags, &outputPixelBuffer, &flagOut);
    switch (status) {
        case noErr:
            printf("Decoding one frame succeeded.\n");
            break;
        case kVTInvalidSessionErr:
            printf("Error: Invalid session. Reset decoder.\n");
            break;
        case kVTVideoDecoderBadDataErr:
            printf("Error: decode failed. status=%d (Bad data)\n", status);
            break;
        default:
            printf("Error: decode failed. status=%d\n", status);
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
                          void *sourceFrameRefCon __attribute__((unused)),
                          OSStatus status __attribute__((unused)),
                          VTDecodeInfoFlags infoFlags __attribute__((unused)),
                          CVImageBufferRef pixelBuffer,
                          CMTime presentationTimeStamp,
                          CMTime presentationDuration )
{
    VTDecoder *decoder = (VTDecoder *)decompressionOutputRefCon;
    if (!decoder || !decoder->rust_callback) {
        return;
    }
    
    // Call the Rust callback with NULL if pixelBuffer is NULL (for DoNotDisplay frames)
    if (pixelBuffer == NULL) {
        decoder->rust_callback(decoder->rust_context, NULL);
        return;
    }
    
    // Create an AVFrame and populate it with the decoded data
    AVFrame *frame = av_frame_alloc();
    if (!frame) {
        return;
    }
    
    // Get dimensions from the pixel buffer
    size_t width = CVPixelBufferGetWidth(pixelBuffer);
    size_t height = CVPixelBufferGetHeight(pixelBuffer);
    
    // Set up the frame
    frame->format = AV_PIX_FMT_VIDEOTOOLBOX;
    frame->width = width;
    frame->height = height;
    frame->data[3] = (uint8_t *)CVPixelBufferRetain(pixelBuffer);
    
    // Set timing information
    frame->pts = presentationTimeStamp.value;
    frame->pkt_duration = presentationDuration.value;
    frame->time_base.num = 1;
    frame->time_base.den = presentationTimeStamp.timescale;
    
    // Call the Rust callback
    decoder->rust_callback(decoder->rust_context, frame);
    
    // Note: The Rust side is responsible for calling av_frame_free when done
}
