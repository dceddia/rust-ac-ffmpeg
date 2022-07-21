#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavfilter/buffersink.h>
#include <libavfilter/buffersrc.h>
#include <libavutil/channel_layout.h>
#include <libavutil/opt.h>

typedef struct FilterGraph {
    struct AVFilterContext *buffersink_ctx;
    struct AVFilterContext *buffersrc_ctx;
    struct AVFilterGraph *filter_graph;

    const AVFilter *abuffersrc;
    const AVFilter *abuffersink;

    AVFilterInOut *outputs;
    AVFilterInOut *inputs;
} FilterGraph;

FilterGraph *ffw_filtergraph_new() {
    int ret;

    FilterGraph* res = malloc(sizeof(FilterGraph));
    if (!res) {
        return NULL;
    }

    res->filter_graph = avfilter_graph_alloc();
    res->inputs = avfilter_inout_alloc();
    res->outputs = avfilter_inout_alloc();
    if (!res->filter_graph || !res->inputs || !res->outputs) {
        ret = AVERROR(ENOMEM);
        goto end;
    }

    return res;

end:
    avfilter_graph_free(&res->filter_graph);
    avfilter_inout_free(&res->inputs);
    avfilter_inout_free(&res->outputs);

    return NULL;
}

int ffw_filtergraph_init_audio(
    FilterGraph *graph,
    int time_base_num,
    int time_base_den,
    uint64_t target_channel_layout,
    int target_sample_format,
    int target_sample_rate,
    uint64_t source_channel_layout,
    int source_sample_format,
    int source_sample_rate,
    const char *filter_description
) {
    char args[512];
    int ret = 0;
    const AVFilter *abuffersrc  = avfilter_get_by_name("abuffer");
    const AVFilter *abuffersink = avfilter_get_by_name("abuffersink");
    const enum AVSampleFormat out_sample_fmts[] = { target_sample_format, -1 };
    const int64_t out_channel_layouts[] = { target_channel_layout, -1 };
    const int out_sample_rates[] = { target_sample_rate, -1 };
    AVRational time_base = (AVRational){ time_base_num, time_base_den };

    /* buffer audio source: the decoded frames from the decoder will be inserted here. */
    snprintf(args, sizeof(args),
            "time_base=%d/%d:sample_rate=%d:sample_fmt=%s:channel_layout=0x%"PRIx64,
             time_base.num, time_base.den, source_sample_rate,
             av_get_sample_fmt_name(source_sample_format), source_channel_layout);
    ret = avfilter_graph_create_filter(&graph->buffersrc_ctx, abuffersrc, "in",
                                       args, NULL, graph->filter_graph);
    if (ret < 0) {
        av_log(NULL, AV_LOG_ERROR, "Cannot create audio buffer source\n");
        goto end;
    }

    /* buffer audio sink: to terminate the filter chain. */
    ret = avfilter_graph_create_filter(&graph->buffersink_ctx, abuffersink, "out",
                                       NULL, NULL, graph->filter_graph);
    if (ret < 0) {
        av_log(NULL, AV_LOG_ERROR, "Cannot create audio buffer sink\n");
        goto end;
    }

    ret = av_opt_set_int_list(graph->buffersink_ctx, "sample_fmts", out_sample_fmts, -1,
                              AV_OPT_SEARCH_CHILDREN);
    if (ret < 0) {
        av_log(NULL, AV_LOG_ERROR, "Cannot set output sample format\n");
        goto end;
    }

    ret = av_opt_set_int_list(graph->buffersink_ctx, "channel_layouts", out_channel_layouts, -1,
                              AV_OPT_SEARCH_CHILDREN);
    if (ret < 0) {
        av_log(NULL, AV_LOG_ERROR, "Cannot set output channel layout\n");
        goto end;
    }

    ret = av_opt_set_int_list(graph->buffersink_ctx, "sample_rates", out_sample_rates, -1,
                              AV_OPT_SEARCH_CHILDREN);
    if (ret < 0) {
        av_log(NULL, AV_LOG_ERROR, "Cannot set output sample rate\n");
        goto end;
    }

    /*
     * Set the endpoints for the filter graph. The filter_graph will
     * be linked to the graph described by filters_descr.
     */

    /*
     * The buffer source output must be connected to the input pad of
     * the first filter described by filters_descr; since the first
     * filter input label is not specified, it is set to "in" by
     * default.
     */
    graph->outputs->name       = av_strdup("in");
    graph->outputs->filter_ctx = graph->buffersrc_ctx;
    graph->outputs->pad_idx    = 0;
    graph->outputs->next       = NULL;

    /*
     * The buffer sink input must be connected to the output pad of
     * the last filter described by filters_descr; since the last
     * filter output label is not specified, it is set to "out" by
     * default.
     */
    graph->inputs->name       = av_strdup("out");
    graph->inputs->filter_ctx = graph->buffersink_ctx;
    graph->inputs->pad_idx    = 0;
    graph->inputs->next       = NULL;

    if ((ret = avfilter_graph_parse_ptr(graph->filter_graph, filter_description,
                                        &graph->inputs, &graph->outputs, NULL)) < 0)
        goto end;

    if ((ret = avfilter_graph_config(graph->filter_graph, NULL)) < 0)
        goto end;

    end:
        return ret;
}

int ffw_filtergraph_push_frame(FilterGraph *graph, AVFrame *frame) {
    int ret = av_buffersrc_add_frame_flags(graph->buffersrc_ctx, frame, AV_BUFFERSRC_FLAG_PUSH);

    if (ret == 0 || ret == AVERROR_EOF) {
        return 1;
    } else if (ret == AVERROR(EAGAIN)) {
        return 0;
    } else {
        return ret;
    }
}

int ffw_filtergraph_take_frame(FilterGraph *graph, AVFrame **frame) {
    if(!frame) {
        return -1;
    }

    if(!*frame) {
        *frame = av_frame_alloc();
        if(!*frame) {
            return 1;
        }
    }

    int ret = av_buffersink_get_frame(graph->buffersink_ctx, *frame);
    if (ret == AVERROR_EOF || ret == AVERROR(EAGAIN)) {
        return 0;
    } else if (ret < 0) {
        return ret;
    }

    return 1;
}

void ffw_filtergraph_free(FilterGraph *graph) {
    if(!graph) {
        return;
    }

    avfilter_graph_free(&graph->filter_graph);
    avfilter_inout_free(&graph->inputs);
    avfilter_inout_free(&graph->outputs);
    free(graph);
}