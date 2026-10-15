// Tiny libxcb wrapper: capture (XGetImage) + input (XTest). Uses the REAL xcb
// headers, so the Rust side never hand-rolls xcb struct layouts.
#define _GNU_SOURCE
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <xcb/xcb.h>
#include <xcb/xtest.h>
#include <xcb/shm.h>
#include <xcb/xfixes.h>
#include <xcb/render.h>
#include <sys/mman.h>
#include <unistd.h>

// libxcb is NOT safe to call from multiple threads unless the process called
// XInitThreads() first. We do not link libX11, so serialize every xcb call with
// one global mutex (the video pump holds it for the duration of one grab).
static pthread_mutex_t g_xcb_lock = PTHREAD_MUTEX_INITIALIZER;
#define XLOCK() pthread_mutex_lock(&g_xcb_lock)
#define XUNLOCK() pthread_mutex_unlock(&g_xcb_lock)

typedef struct {
    xcb_connection_t *conn;
    xcb_screen_t *screen;
    // XSHM: one shared segment reused for every grab (no per-frame socket copy).
    xcb_shm_seg_t shmseg;
    void *shm_map;
    size_t shm_len;
    // XFixes: composite the cursor into each frame (XGetImage omits it).
    int has_xfixes;
    xcb_xfixes_get_cursor_image_cookie_t cursor_ck;
    xcb_xfixes_get_cursor_image_reply_t *cursor_reply;
} na_x11;

// na_x11_composite_cursor overlays the X cursor onto a BGRA frame in place.
// Uses XFixesGetCursorImage (cursor pixels + position) — the standard way to
// include the cursor in a screenshot, since XGetImage never captures it.
static void na_x11_composite_cursor(na_x11 *x, uint8_t *px, int w, int h) {
    if (!x->has_xfixes) return;
    xcb_xfixes_get_cursor_image_cookie_t ck =
        xcb_xfixes_get_cursor_image(x->conn);
    xcb_xfixes_get_cursor_image_reply_t *r =
        xcb_xfixes_get_cursor_image_reply(x->conn, ck, NULL);
    if (!r) return;
    int cx = r->x, cy = r->y;
    int cw = r->width, ch = r->height;
    uint32_t *cpix = xcb_xfixes_get_cursor_image_cursor_image(r);
    for (int j = 0; j < ch; j++) {
        int y = cy + j;
        if (y < 0 || y >= h) continue;
        for (int i = 0; i < cw; i++) {
            int xx = cx + i;
            if (xx < 0 || xx >= w) continue;
            uint32_t c = cpix[j * cw + i];
            unsigned a = (c >> 24) & 0xff;
            if (a == 0) continue;
            size_t o = ((size_t)y * w + xx) * 4;
            unsigned cr = c & 0xff, cg = (c >> 8) & 0xff, cb = (c >> 16) & 0xff;
            // BGRA, straight (non-premultiplied) alpha blend.
            px[o + 0] = (uint8_t)((cb * a + px[o + 0] * (255 - a)) / 255);
            px[o + 1] = (uint8_t)((cg * a + px[o + 1] * (255 - a)) / 255);
            px[o + 2] = (uint8_t)((cr * a + px[o + 2] * (255 - a)) / 255);
            px[o + 3] = 0xff;
        }
    }
    free(r);
}

void *na_x11_open(void) {
    XLOCK();
    int screen_num = 0;
    xcb_connection_t *conn = xcb_connect(NULL, &screen_num);
    if (!conn || xcb_connection_has_error(conn)) {
        XUNLOCK();
        return NULL;
    }
    xcb_screen_t *screen = xcb_setup_roots_iterator(xcb_get_setup(conn)).data;
    if (!screen) {
        XUNLOCK();
        return NULL;
    }
    na_x11 *x = (na_x11 *)calloc(1, sizeof(na_x11));
    x->conn = conn;
    x->screen = screen;
    // XSHM: allocate one screen-sized shared segment and attach it.
    x->shm_len = (size_t)screen->width_in_pixels * screen->height_in_pixels * 4;
    int fd = memfd_create("xshm", MFD_CLOEXEC);
    if (fd >= 0) {
        if (ftruncate(fd, x->shm_len) == 0) {
            x->shm_map = mmap(NULL, x->shm_len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            if (x->shm_map != MAP_FAILED) {
                x->shmseg = xcb_generate_id(conn);
                xcb_shm_attach_fd(conn, x->shmseg, fd, 0);
                xcb_flush(conn);
            } else {
                x->shm_map = NULL;
            }
        }
        close(fd);
    }
    // XFixes: detect the extension so grabs can composite the cursor.
    {
        xcb_xfixes_query_version_cookie_t q =
            xcb_xfixes_query_version(conn, XCB_XFIXES_MAJOR_VERSION, XCB_XFIXES_MINOR_VERSION);
        xcb_xfixes_query_version_reply_t *qr =
            xcb_xfixes_query_version_reply(conn, q, NULL);
        if (qr) { x->has_xfixes = 1; free(qr); }
    }
    XUNLOCK();
    return x;
}

// na_x11_grab_shm grabs the screen via XSHM into *out (malloc'd BGRA). Returns 0
// on success; -1 if XSHM is unavailable (caller falls back to XGetImage).
int na_x11_grab_shm(void *h, uint8_t **out, int *ow, int *oh) {
    XLOCK();
    na_x11 *x = (na_x11 *)h;
    if (!x->shm_map) { XUNLOCK(); return -1; }
    xcb_screen_t *s = x->screen;
    int w = s->width_in_pixels, hh = s->height_in_pixels;
    xcb_shm_get_image_cookie_t ck = xcb_shm_get_image(
        x->conn, s->root, 0, 0, (uint16_t)w, (uint16_t)hh, 0xffffffff,
        XCB_IMAGE_FORMAT_Z_PIXMAP, x->shmseg, 0);
    xcb_shm_get_image_reply_t *r = xcb_shm_get_image_reply(x->conn, ck, NULL);
    if (!r) { XUNLOCK(); return -1; }
    free(r);
    // The pixels are already in our mapped segment: copy whole rows out.
    uint8_t *px = (uint8_t *)malloc((size_t)w * hh * 4);
    const uint8_t *src = (const uint8_t *)x->shm_map;
    for (int y = 0; y < hh; y++) {
        memcpy(px + (size_t)y * w * 4, src + (size_t)y * w * 4, (size_t)w * 4);
        for (int xx = 0; xx < w; xx++) px[(size_t)y * w * 4 + xx * 4 + 3] = 0xff;
    }
    *out = px; *ow = w; *oh = hh;
    XUNLOCK();
    return 0;
}

// na_x11_grab_shm_ptr captures the screen via XSHM and returns a pointer INTO
// the mapped shared segment (NO copy). The pointer is valid until the next grab.
// Returns NULL if XSHM is unavailable.
const uint8_t *na_x11_grab_shm_ptr(void *h, int *ow, int *oh) {
    XLOCK();
    na_x11 *x = (na_x11 *)h;
    if (!x->shm_map) { XUNLOCK(); return NULL; }
    xcb_screen_t *s = x->screen;
    int w = s->width_in_pixels, hh = s->height_in_pixels;
    xcb_shm_get_image_cookie_t ck = xcb_shm_get_image(
        x->conn, s->root, 0, 0, (uint16_t)w, (uint16_t)hh, 0xffffffff,
        XCB_IMAGE_FORMAT_Z_PIXMAP, x->shmseg, 0);
    xcb_shm_get_image_reply_t *r = xcb_shm_get_image_reply(x->conn, ck, NULL);
    if (!r) { XUNLOCK(); return NULL; }
    free(r);
    *ow = w; *oh = hh;
    // Composite the cursor directly into the shared segment so the caller's
    // pointer already includes it.
    na_x11_composite_cursor(x, (uint8_t *)x->shm_map, w, hh);
    const uint8_t *p = (const uint8_t *)x->shm_map;
    XUNLOCK();
    return p;
}

void na_x11_size(void *h, int *w, int *hh) {
    na_x11 *x = (na_x11 *)h;
    *w = x->screen->width_in_pixels;
    *hh = x->screen->height_in_pixels;
}

// na_x11_grab captures the whole screen as top-down BGRA into *out (malloc'd,
// caller frees); returns 0 on success.
int na_x11_grab(void *h, uint8_t **out, int *w, int *hh, int *stride_out) {
    XLOCK();
    na_x11 *x = (na_x11 *)h;
    xcb_screen_t *s = x->screen;
    xcb_get_image_cookie_t ck = xcb_get_image(
        x->conn, XCB_IMAGE_FORMAT_Z_PIXMAP, s->root, 0, 0,
        s->width_in_pixels, s->height_in_pixels, 0xffffffff);
    xcb_get_image_reply_t *reply = xcb_get_image_reply(x->conn, ck, NULL);
    if (!reply) {
        XUNLOCK();
        return -1;
    }
    // We always request the whole screen, so the reply is screen-sized; use the
    // screen dimensions (the reply's own width/height have no public accessor).
    int iw = s->width_in_pixels;
    int ih = s->height_in_pixels;
    uint8_t *data = xcb_get_image_data(reply);
    int len = xcb_get_image_data_length(reply);
    int stride = ih > 0 ? len / ih : 0;
    int bpp = iw > 0 ? stride / iw : 4;
    uint8_t *px = (uint8_t *)malloc((size_t)iw * ih * 4);
    // The X server gives ZPixmap 32bpp (BGRX) already; copy whole ROWS (memcpy)
    // instead of a per-pixel loop, and force alpha opaque.
    for (int y = 0; y < ih; y++) {
        if (bpp == 4 && stride >= iw * 4) {
            memcpy(px + (size_t)y * iw * 4, data + (size_t)y * stride, (size_t)iw * 4);
        } else {
            for (int xx = 0; xx < iw; xx++) {
                int off = y * stride + xx * bpp;
                int d = (y * iw + xx) * 4;
                if (off + 3 < len) {
                    px[d] = data[off];
                    px[d + 1] = data[off + 1];
                    px[d + 2] = data[off + 2];
                }
            }
        }
        for (int xx = 0; xx < iw; xx++) px[(size_t)y * iw * 4 + xx * 4 + 3] = 0xff;
    }
    free(reply);
    na_x11_composite_cursor(x, px, iw, ih);
    *out = px;
    *w = iw;
    *hh = ih;
    *stride_out = iw * 4;
    XUNLOCK();
    return 0;
}

void na_x11_move(void *h, int x, int y) {
    XLOCK();
    na_x11 *c = (na_x11 *)h;
    xcb_test_fake_input(c->conn, XCB_MOTION_NOTIFY, 0, XCB_CURRENT_TIME, XCB_NONE, x, y, 0);
    xcb_flush(c->conn);
    XUNLOCK();
}

void na_x11_button(void *h, int num, int down) {
    XLOCK();
    na_x11 *c = (na_x11 *)h;
    uint8_t t = down ? XCB_BUTTON_PRESS : XCB_BUTTON_RELEASE;
    xcb_test_fake_input(c->conn, t, num, XCB_CURRENT_TIME, XCB_NONE, 0, 0, 0);
    xcb_flush(c->conn);
    XUNLOCK();
}

void na_x11_key(void *h, int keycode, int down) {
    XLOCK();
    na_x11 *c = (na_x11 *)h;
    uint8_t t = down ? XCB_KEY_PRESS : XCB_KEY_RELEASE;
    xcb_test_fake_input(c->conn, t, keycode, XCB_CURRENT_TIME, XCB_NONE, 0, 0, 0);
    xcb_flush(c->conn);
    XUNLOCK();
}

void na_x11_close(void *h) {
    XLOCK();
    na_x11 *x = (na_x11 *)h;
    if (!x) {
        XUNLOCK();
        return;
    }
    if (x->conn) xcb_disconnect(x->conn);
    free(x);
    XUNLOCK();
}
