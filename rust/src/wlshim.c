// Wayland capture via wlr-screencopy (pure protocol, no external binary).
// Binds zwlr_screencopy_manager_v1 + wl_shm, asks for a frame into an shm
// buffer we own (memfd + mmap), and copies it out as BGRA.
#define _GNU_SOURCE
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include "wlr-screencopy-client.h"

typedef struct {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_shm *shm;
    struct wl_output *output;
    struct zwlr_screencopy_manager_v1 *manager;
    int width, height, stride;
    int buf_w, buf_h;
    int done, failed;
    void *map;
    size_t map_len;
    struct wl_buffer *buf;
    struct wl_shm_pool *pool;
} na_wl;

static void reg_global(void *data, struct wl_registry *reg, uint32_t name,
                       const char *iface, uint32_t version) {
    na_wl *w = (na_wl *)data;
    (void)version;
    if (!strcmp(iface, "wl_shm")) {
        w->shm = wl_registry_bind(reg, name, &wl_shm_interface, 1);
    } else if (!strcmp(iface, "wl_output")) {
        if (!w->output) w->output = wl_registry_bind(reg, name, &wl_output_interface, 1);
    } else if (!strcmp(iface, "zwlr_screencopy_manager_v1")) {
        w->manager = wl_registry_bind(reg, name, &zwlr_screencopy_manager_v1_interface, 3);
    }
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d; (void)r; (void)n; }
static const struct wl_registry_listener reg_listener = { reg_global, reg_remove };

static void fr_buffer(void *data, struct zwlr_screencopy_frame_v1 *f,
                      uint32_t format, uint32_t width, uint32_t height, uint32_t stride) {
    (void)f; (void)format;
    na_wl *w = (na_wl *)data;
    w->width = width; w->height = height; w->stride = stride;
}
static void fr_flags(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t fl) { (void)d;(void)f;(void)fl; }
static void fr_ready(void *data, struct zwlr_screencopy_frame_v1 *f,
                     uint32_t a, uint32_t b, uint32_t c) { (void)f;(void)a;(void)b;(void)c;
    ((na_wl *)data)->done = 1; }
static void fr_failed(void *data, struct zwlr_screencopy_frame_v1 *f) { (void)f; ((na_wl *)data)->failed = 1; }
static void fr_damage(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t a,uint32_t b,uint32_t c,uint32_t e){ (void)d;(void)f;(void)a;(void)b;(void)c;(void)e; }
static void fr_dmabuf(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t a,uint32_t b,uint32_t c){ (void)d;(void)f;(void)a;(void)b;(void)c; }
static void fr_bufdone(void *data, struct zwlr_screencopy_frame_v1 *f) {
    na_wl *w = (na_wl *)data;
    size_t size = (size_t)w->stride * w->height;
    // Reuse the shm pool/buffer/mapping across frames: creating a memfd, an
    // ftruncate, a wl_shm_pool, a wl_buffer AND an mmap on EVERY frame was the
    // wayland CPU hog (several syscalls + a page-fault storm per grab). The
    // pool is only rebuilt when the frame geometry changes.
    if (!w->map || w->map_len != size || w->width != (int)w->buf_w || w->height != (int)w->buf_h) {
        if (w->buf) { wl_buffer_destroy(w->buf); w->buf = NULL; }
        if (w->pool) { wl_shm_pool_destroy(w->pool); w->pool = NULL; }
        if (w->map) { munmap(w->map, w->map_len); w->map = NULL; }
        int fd = memfd_create("wl_shm", MFD_CLOEXEC);
        if (fd < 0) return;
        if (ftruncate(fd, size) != 0) { close(fd); return; }
        w->pool = wl_shm_create_pool(w->shm, fd, (int32_t)size);
        w->buf = wl_shm_pool_create_buffer(w->pool, 0, w->width, w->height,
                                           (int32_t)w->stride, WL_SHM_FORMAT_XRGB8888);
        w->map = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        close(fd);
        if (w->map == MAP_FAILED) { w->map = NULL; return; }
        w->map_len = size;
        w->buf_w = w->width;
        w->buf_h = w->height;
    }
    zwlr_screencopy_frame_v1_copy(f, w->buf);
}
static const struct zwlr_screencopy_frame_v1_listener frame_listener = {
    fr_buffer, fr_flags, fr_ready, fr_failed, fr_damage, fr_dmabuf, fr_bufdone,
};

void *na_wl_open(void) {
    na_wl *w = (na_wl *)calloc(1, sizeof(na_wl));
    w->display = wl_display_connect(NULL);
    if (!w->display) { free(w); return NULL; }
    w->registry = wl_display_get_registry(w->display);
    wl_registry_add_listener(w->registry, &reg_listener, w);
    wl_display_roundtrip(w->display);
    if (!w->manager || !w->shm || !w->output) { free(w); return NULL; }
    wl_display_roundtrip(w->display);
    return w;
}

// na_wl_grab returns a pointer to the shim's REUSED shm mapping (top-down BGRA,
// stride == width*4 for the wlr-screencopy XRGB path). The pointer is valid
// until the next grab / na_wl_close — the caller must NOT free it, and must copy
// only if it needs to keep the frame.
int na_wl_grab(void *h, uint8_t **out, int *ow, int *oh) {
    na_wl *w = (na_wl *)h;
    w->done = w->failed = 0;
    // NOTE: do NOT reset w->map/buf/pool here. They are reused across frames
    // (fr_bufdone rebuilds them only when the geometry changes). Clearing them
    // dropped the only reference to the live shm pool/buffer/mapping, so every
    // frame leaked ~8 MB (mmap + memfd) until the pod was OOM-killed.
    struct zwlr_screencopy_frame_v1 *frame =
        zwlr_screencopy_manager_v1_capture_output(w->manager, 1 /*overlay_cursor*/, w->output);
    zwlr_screencopy_frame_v1_add_listener(frame, &frame_listener, w);
    while (!w->done && !w->failed) {
        if (wl_display_dispatch(w->display) < 0) return -1;
    }
    zwlr_screencopy_frame_v1_destroy(frame);
    if (w->failed || !w->map) return -1;
    // No copy: hand back the mapping directly (the pool is reused next frame).
    // wlr-screencopy gives stride == width*4 for XRGB8888, so the buffer is
    // already tightly packed BGRA.
    *out = (uint8_t *)w->map; *ow = w->width; *oh = w->height;
    return 0;
}

void na_wl_close(void *h) {
    na_wl *w = (na_wl *)h;
    if (!w) return;
    if (w->buf) wl_buffer_destroy(w->buf);
    if (w->pool) wl_shm_pool_destroy(w->pool);
    if (w->map) munmap(w->map, w->map_len);
    if (w->manager) zwlr_screencopy_manager_v1_destroy(w->manager);
    if (w->shm) wl_shm_destroy(w->shm);
    if (w->output) wl_output_destroy(w->output);
    if (w->registry) wl_registry_destroy(w->registry);
    if (w->display) wl_display_disconnect(w->display);
    free(w);
}
