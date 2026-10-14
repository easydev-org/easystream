// Wayland input via wlr-virtual-pointer + wlroots virtual-keyboard (pure
// protocol). The keyboard needs an xkb keymap (generated with libxkbcommon).
#define _GNU_SOURCE
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>
#include "wlr-virtual-pointer-unstable-v1-client.h"
#include "wlr-virtual-keyboard-unstable-v1-client.h"

typedef struct {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_seat *seat;
    struct zwlr_virtual_pointer_manager_v1 *ptr_mgr;
    struct zwlr_virtual_pointer_v1 *ptr;
    struct zwp_virtual_keyboard_manager_v1 *kbd_mgr;
    struct zwp_virtual_keyboard_v1 *kbd;
    uint32_t time;
} na_wlin;

static void reg_global(void *data, struct wl_registry *reg, uint32_t name,
                       const char *iface, uint32_t version) {
    na_wlin *w = (na_wlin *)data;
    (void)version;
    if (!strcmp(iface, "wl_seat")) {
        if (!w->seat) w->seat = wl_registry_bind(reg, name, &wl_seat_interface, 1);
    } else if (!strcmp(iface, "zwlr_virtual_pointer_manager_v1")) {
        w->ptr_mgr = wl_registry_bind(reg, name, &zwlr_virtual_pointer_manager_v1_interface, 2);
    } else if (!strcmp(iface, "zwp_virtual_keyboard_manager_v1")) {
        w->kbd_mgr = wl_registry_bind(reg, name, &zwp_virtual_keyboard_manager_v1_interface, 1);
    }
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d;(void)r;(void)n; }
static const struct wl_registry_listener reg_listener = { reg_global, reg_remove };

static void send_keymap(na_wlin *w) {
    struct xkb_context *ctx = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    if (!ctx) return;
    struct xkb_rule_names names = { .rules = NULL, .model = NULL, .layout = "us",
                                    .variant = NULL, .options = NULL };
    struct xkb_keymap *km = xkb_keymap_new_from_names(ctx, &names, XKB_KEYMAP_COMPILE_NO_FLAGS);
    if (!km) { xkb_context_unref(ctx); return; }
    char *str = xkb_keymap_get_as_string(km, XKB_KEYMAP_FORMAT_TEXT_V1);
    if (!str) { xkb_keymap_unref(km); xkb_context_unref(ctx); return; }
    size_t len = strlen(str) + 1;
    int fd = memfd_create("xkb", MFD_CLOEXEC);
    if (fd >= 0) {
        if (ftruncate(fd, len) == 0) {
            void *m = mmap(NULL, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
            if (m != MAP_FAILED) {
                memcpy(m, str, len);
                munmap(m, len);
                zwp_virtual_keyboard_v1_keymap(w->kbd, WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1, fd, (uint32_t)len);
                // The fd is read when the request is FLUSHED, so flush (and let
                // the server ack) BEFORE closing it, or the server gets a stale
                // fd and the keyboard never works.
                wl_display_flush(w->display);
                wl_display_roundtrip(w->display);
            }
        }
        close(fd);
    }
    free(str);
    xkb_keymap_unref(km);
    xkb_context_unref(ctx);
}

void *na_wlin_open(void) {
    na_wlin *w = (na_wlin *)calloc(1, sizeof(na_wlin));
    w->display = wl_display_connect(NULL);
    if (!w->display) { free(w); return NULL; }
    w->registry = wl_display_get_registry(w->display);
    wl_registry_add_listener(w->registry, &reg_listener, w);
    wl_display_roundtrip(w->display);
    if (!w->seat || !w->ptr_mgr || !w->kbd_mgr) { free(w); return NULL; }
    w->ptr = zwlr_virtual_pointer_manager_v1_create_virtual_pointer(w->ptr_mgr, w->seat);
    w->kbd = zwp_virtual_keyboard_manager_v1_create_virtual_keyboard(w->kbd_mgr, w->seat);
    send_keymap(w);
    wl_display_roundtrip(w->display);
    return w;
}

void na_wlin_move(void *h, int x, int y) {
    na_wlin *w = (na_wlin *)h;
    // motion_absolute(time, x, y, x_extent, y_extent): the extent MUST be the
    // output size, not 0, or the pointer does not move.
    zwlr_virtual_pointer_v1_motion_absolute(w->ptr, w->time++, (uint32_t)x, (uint32_t)y, 1920, 1080);
    zwlr_virtual_pointer_v1_frame(w->ptr);
    wl_display_flush(w->display);
}

void na_wlin_button(void *h, int button, int down) {
    na_wlin *w = (na_wlin *)h;
    // Wayland buttons are Linux evdev codes (BTN_LEFT=0x110 ...).
    uint32_t b = 0x110; // BTN_LEFT
    if (button == 2) b = 0x112;      // BTN_MIDDLE
    else if (button == 3) b = 0x111; // BTN_RIGHT
    zwlr_virtual_pointer_v1_button(w->ptr, w->time++, b, down ? 1 : 0);
    zwlr_virtual_pointer_v1_frame(w->ptr);
    wl_display_flush(w->display);
}

void na_wlin_axis(void *h, int dx, int dy) {
    na_wlin *w = (na_wlin *)h;
    if (dy) zwlr_virtual_pointer_v1_axis(w->ptr, w->time++, 0 /*vertical*/, wl_fixed_from_int(dy * 15));
    if (dx) zwlr_virtual_pointer_v1_axis(w->ptr, w->time++, 1 /*horizontal*/, wl_fixed_from_int(dx * 15));
    zwlr_virtual_pointer_v1_frame(w->ptr);
    wl_display_flush(w->display);
}

void na_wlin_key(void *h, uint32_t keycode, int down) {
    na_wlin *w = (na_wlin *)h;
    zwp_virtual_keyboard_v1_key(w->kbd, w->time++, keycode, down ? 1 : 0);
    wl_display_flush(w->display);
}

void na_wlin_mods(void *h, uint32_t depressed, uint32_t latched, uint32_t locked, uint32_t group) {
    na_wlin *w = (na_wlin *)h;
    zwp_virtual_keyboard_v1_modifiers(w->kbd, depressed, latched, locked, group);
    wl_display_flush(w->display);
}

void na_wlin_close(void *h) {
    na_wlin *w = (na_wlin *)h;
    if (!w) return;
    if (w->ptr) zwlr_virtual_pointer_v1_destroy(w->ptr);
    if (w->kbd) zwp_virtual_keyboard_v1_destroy(w->kbd);
    if (w->ptr_mgr) zwlr_virtual_pointer_manager_v1_destroy(w->ptr_mgr);
    if (w->kbd_mgr) zwp_virtual_keyboard_manager_v1_destroy(w->kbd_mgr);
    if (w->seat) wl_seat_destroy(w->seat);
    if (w->registry) wl_registry_destroy(w->registry);
    if (w->display) wl_display_disconnect(w->display);
    free(w);
}
