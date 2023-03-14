#include <libavutil/avutil.h>
#include <libavutil/mathematics.h>

int64_t ffw_rescale_rnd(int64_t n, uint32_t aq_num, uint32_t aq_den, uint32_t bq_num, uint32_t bq_den, uint32_t rnd)
{
    int64_t a = aq_num * (int64_t)bq_den;
    int64_t b = bq_num * (int64_t)aq_den;

    return av_rescale_rnd(n, a, b, rnd);
}
