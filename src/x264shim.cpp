// Tiny libx264 wrapper: BGRA in, Annex-B access units out. Keeps the Rust side
// free of x264's huge structs.
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <x264.h>
#include <libyuv.h>

extern "C" {

typedef struct {
    x264_t *enc;
    x264_param_t param;
    x264_picture_t pic_in;
    x264_picture_t pic_out;
    int width, height, fps;
    int64_t pts;
    int force_idr;
} na_x264;

void *na_x264_open(int w, int h, int fps, int bitrate) {
    na_x264 *e = (na_x264 *)calloc(1, sizeof(na_x264));
    if (!e) return NULL;
    // ultrafast is ~2x faster than veryfast (a bit lower quality); the stream
    // is a UI screen, so this trade is worth it.
    if (x264_param_default_preset(&e->param, "ultrafast", "zerolatency") != 0) { free(e); return NULL; }
    x264_param_apply_profile(&e->param, "baseline");
    e->param.i_width = w;
    e->param.i_height = h;
    e->param.i_fps_num = fps;
    e->param.i_fps_den = 1;
    e->param.i_keyint_max = fps * 2;
    e->param.i_keyint_min = fps;
    e->param.rc.i_rc_method = X264_RC_ABR;
    e->param.rc.i_bitrate = bitrate / 1000;
    e->param.b_annexb = 1;
    e->param.b_repeat_headers = 1;
    // Let x264 pick the thread count (0 = auto, based on CPU cores).
    e->param.i_threads = 0;
    e->enc = x264_encoder_open(&e->param);
    if (!e->enc) { free(e); return NULL; }
    // I420 input: BGRA input would make x264 emit 4:4:4, which WebCodecs
    // (avc1.42xxxx) rejects. We convert BGRA -> I420 ourselves below.
    x264_picture_alloc(&e->pic_in, X264_CSP_I420, w, h);
    e->width = w; e->height = h; e->fps = fps; e->pts = 0;
    // The FIRST frame must be an IDR: a viewer configures its decoder from the
    // stream and cannot start on a delta frame. x264 does normally emit an IDR
    // first, but the pump overrides pic_in.i_type on every frame (AUTO when no
    // keyframe was requested), and i_keyint_min = fps then makes x264 wait a
    // full second before the next keyframe. Setting this here means the very
    // first encode is a keyframe regardless of how the caller drives it.
    e->force_idr = 1;
    return e;
}

// na_x264_encode encodes one BGRA frame; returns a malloc'd Annex-B buffer in
// *out (caller frees) and its length. Returns 0 and *out=NULL if no output yet.
int na_x264_encode(void *enc, const uint8_t *bgra, uint8_t **out, int *outlen) {
    na_x264 *e = (na_x264 *)enc;
    *out = NULL; *outlen = 0;
    // SIMD BGRA -> I420 into x264's OWN planes (libyuv: SSE/AVX, far faster
    // than a scalar loop; writing into its planes keeps x264's ownership valid).
    libyuv::ARGBToI420(
        bgra, e->width * 4,
        e->pic_in.img.plane[0], e->pic_in.img.i_stride[0],
        e->pic_in.img.plane[1], e->pic_in.img.i_stride[1],
        e->pic_in.img.plane[2], e->pic_in.img.i_stride[2],
        e->width, e->height);
    e->pic_in.i_pts = e->pts++;
    // A new viewer (or a keepalive) must get a decodable starting point.
    //
    // The flag is NOT cleared here. x264 can return 0 bytes (internal delay) and
    // the caller then drops the frame; clearing the flag up front lost the
    // keyframe request, so a viewer that connected during warm-up waited a full
    // keyint before its decoder could start and the canvas stayed black. It is
    // cleared below, only once an access unit was actually produced.
    int want_idr = e->force_idr;
    e->pic_in.i_type = want_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;

    x264_nal_t *nals;
    int n = 0;
    int rc = x264_encoder_encode(e->enc, &nals, &n, &e->pic_in, &e->pic_out);
    if (rc < 0) return -1;
    int total = 0;
    for (int i = 0; i < n; i++) total += nals[i].i_payload;
    if (total == 0) return 0;
    // A real access unit is being handed back, so a pending keyframe request has
    // been honoured. An IDR was requested for this picture iff i_type was IDR.
    if (want_idr) e->force_idr = 0;
    uint8_t *buf = (uint8_t *)malloc(total);
    int off = 0;
    for (int i = 0; i < n; i++) {
        memcpy(buf + off, nals[i].p_payload, nals[i].i_payload);
        off += nals[i].i_payload;
    }
    *out = buf; *outlen = total;
    return n;
}

void na_x264_close(void *enc) {
    na_x264 *e = (na_x264 *)enc;
    if (!e) return;
    if (e->enc) x264_encoder_close(e->enc);
    x264_picture_clean(&e->pic_in);
    free(e);
}

// na_x264_force_idr makes the NEXT encoded frame an IDR (keyframe).
// na_x264_encode_scaled is na_x264_encode for a frame whose captured size differs
// from the encoder's. libyuv scales and converts in one pass (I420Scale needs an
// intermediate I420, so scaling during the colour convert is both faster and one
// buffer less).
//
// Scaling HERE, not in the capture backend, is deliberate: wlr screencopy and
// X11 XGetImage both hand back the compositor's size and neither can be asked
// for another one without tearing the pipeline down. The encoder is the first
// place that legitimately owns an output geometry.
int na_x264_encode_scaled(void *enc, const uint8_t *bgra, int src_w, int src_h,
                          uint8_t **out, int *outlen) {
    na_x264 *e = (na_x264 *)enc;
    *out = NULL; *outlen = 0;
    if (src_w == e->width && src_h == e->height) {
        return na_x264_encode(enc, bgra, out, outlen);
    }
    // Convert at the SOURCE size first, then scale the planes into x264's own
    // buffers. ARGBScale would need RGB24, which is an extra conversion.
    static thread_local uint8_t *tmp = NULL;
    static thread_local size_t tmp_cap = 0;
    size_t need = (size_t)src_w * src_h * 3 / 2;
    if (tmp_cap < need) {
        uint8_t *p = (uint8_t *)realloc(tmp, need);
        if (!p) return -1;
        tmp = p; tmp_cap = need;
    }
    uint8_t *py = tmp;
    uint8_t *pu = tmp + (size_t)src_w * src_h;
    uint8_t *pv = pu + (size_t)(src_w / 2) * (src_h / 2);
    libyuv::ARGBToI420(
        bgra, src_w * 4,
        py, src_w,
        pu, src_w / 2,
        pv, src_w / 2,
        src_w, src_h);
    libyuv::I420Scale(
        py, src_w,
        pu, src_w / 2,
        pv, src_w / 2,
        src_w, src_h,
        e->pic_in.img.plane[0], e->pic_in.img.i_stride[0],
        e->pic_in.img.plane[1], e->pic_in.img.i_stride[1],
        e->pic_in.img.plane[2], e->pic_in.img.i_stride[2],
        e->width, e->height,
        libyuv::kFilterBilinear);
    e->pic_in.i_pts = e->pts++;
    int want_idr = e->force_idr;
    e->pic_in.i_type = want_idr ? X264_TYPE_IDR : X264_TYPE_AUTO;
    x264_nal_t *nals;
    int n = 0;
    int rc = x264_encoder_encode(e->enc, &nals, &n, &e->pic_in, &e->pic_out);
    if (rc < 0) return -1;
    int total = 0;
    for (int i = 0; i < n; i++) total += nals[i].i_payload;
    if (total == 0) return 0;
    if (want_idr) e->force_idr = 0;
    uint8_t *buf = (uint8_t *)malloc(total);
    if (!buf) return -1;
    int off = 0;
    for (int i = 0; i < n; i++) {
        memcpy(buf + off, nals[i].p_payload, nals[i].i_payload);
        off += nals[i].i_payload;
    }
    *out = buf; *outlen = total;
    return n;
}

void na_x264_force_idr(void *enc) {
    na_x264 *e = (na_x264 *)enc;
    if (e) e->force_idr = 1;
}

} // extern "C"
