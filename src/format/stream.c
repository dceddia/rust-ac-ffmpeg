#include <libavformat/avformat.h>
#include <libavutil/display.h>

void ffw_stream_get_time_base(const AVStream* stream, uint32_t* num, uint32_t* den);
void ffw_stream_get_r_frame_rate(const AVStream* stream, uint32_t* num, uint32_t* den);
int ffw_stream_get_index(const AVStream* stream);
int64_t ffw_stream_get_start_time(const AVStream* stream);
int64_t ffw_stream_get_duration(const AVStream* stream);
int64_t ffw_stream_get_nb_frames(const AVStream* stream);
double ffw_stream_get_rotation(const AVStream* stream);
void ffw_stream_set_discard(AVStream* stream, int discard);
AVCodecParameters* ffw_stream_get_codec_parameters(const AVStream* stream);
int ffw_stream_set_metadata(AVStream* stream, const char* key, const char* value);

void ffw_stream_get_time_base(const AVStream* stream, uint32_t* num, uint32_t* den) {
    *num = stream->time_base.num;
    *den = stream->time_base.den;
}

void ffw_stream_get_r_frame_rate(const AVStream* stream, uint32_t* num, uint32_t* den) {
    *num = stream->r_frame_rate.num;
    *den = stream->r_frame_rate.den;
}

int ffw_stream_get_index(const AVStream* stream) {
    return stream->index;
}

int64_t ffw_stream_get_start_time(const AVStream* stream) {
    return stream->start_time;
}

int64_t ffw_stream_get_duration(const AVStream* stream) {
    return stream->duration;
}

int64_t ffw_stream_get_nb_frames(const AVStream* stream) {
    return stream->nb_frames;
}

double ffw_stream_get_rotation(const AVStream* stream) {
    uint8_t* displaymatrix = av_stream_get_side_data(stream, AV_PKT_DATA_DISPLAYMATRIX, NULL);
    double degrees = 0;

    if (displaymatrix) {
        degrees = -av_display_rotation_get((int32_t*) displaymatrix);
        degrees -= 360*floor(degrees/360 + 0.9/360);
    }

    return degrees;
}

void ffw_stream_set_discard(AVStream* stream, int discard) {
    stream->discard = discard;
}

AVCodecParameters* ffw_stream_get_codec_parameters(const AVStream* stream) {
    AVCodecParameters* res = avcodec_parameters_alloc();
    if (!res) {
        return NULL;
    }

    if (avcodec_parameters_copy(res, stream->codecpar) < 0) {
        goto err;
    }

    return res;

err:
    avcodec_parameters_free(&res);

    return NULL;
}

int ffw_stream_set_metadata(AVStream* stream, const char* key, const char* value) {
    return av_dict_set(&stream->metadata, key, value, 0);
}

const char* ffw_stream_get_metadata(AVStream* stream, const char* key) {
    AVDictionaryEntry *entry = av_dict_get(stream->metadata, key, NULL, 0);

    if(!entry) {
        return NULL;
    }

    return entry->value;
}
