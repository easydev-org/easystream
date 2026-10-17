// Wayland capability probe: connect once, enumerate the registry globals, and
// report which compositor protocols are present.
//
// This is the RELIABLE way to pick a capture/input path: asking the compositor
// what it implements is instant, whereas the old "try the portal, wait for it
// to fail, then fall back" cost a D-Bus timeout on every attempt (and a
// screenshot opened a fresh capture each time).
#define _GNU_SOURCE
#include <stdint.h>
#include <string.h>
#include <stdlib.h>
#include <wayland-client.h>

typedef struct {
    struct wl_display *display;
    struct wl_registry *registry;
    // Capabilities we care about.
    int has_screencopy;        // zwlr_screencopy_manager_v1
    int has_virtual_pointer;   // zwlr_virtual_pointer_manager_v1
    int has_virtual_keyboard;  // zwp_virtual_keyboard_manager_v1
    int has_wl_output;
    int has_wl_shm;
    int has_xdg_output;        // zxdg_output_manager_v1
    int globals;               // total globals seen
} na_probe;

static void probe_global(void *data, struct wl_registry *reg, uint32_t name,
                         const char *iface, uint32_t version) {
    na_probe *p = (na_probe *)data;
    (void)reg; (void)name; (void)version;
    p->globals++;
    if (!strcmp(iface, "zwlr_screencopy_manager_v1"))          p->has_screencopy = 1;
    else if (!strcmp(iface, "zwlr_virtual_pointer_manager_v1")) p->has_virtual_pointer = 1;
    else if (!strcmp(iface, "zwp_virtual_keyboard_manager_v1")) p->has_virtual_keyboard = 1;
    else if (!strcmp(iface, "wl_output"))                       p->has_wl_output = 1;
    else if (!strcmp(iface, "wl_shm"))                          p->has_wl_shm = 1;
    else if (!strcmp(iface, "zxdg_output_manager_v1"))          p->has_xdg_output = 1;
}
static void probe_remove(void *d, struct wl_registry *r, uint32_t n) { (void)d;(void)r;(void)n; }
static const struct wl_registry_listener probe_listener = { probe_global, probe_remove };

// na_probe_wayland fills `out` (an array of 8 ints, see probe.rs) and returns
// 0 on success. It never aborts: no Wayland means all zeros.
int na_probe_wayland(int *out) {
    na_probe p;
    memset(&p, 0, sizeof(p));
    p.display = wl_display_connect(NULL);
    if (!p.display) return -1;
    p.registry = wl_display_get_registry(p.display);
    wl_registry_add_listener(p.registry, &probe_listener, &p);
    wl_display_roundtrip(p.display);   // one roundtrip delivers every global

    out[0] = p.has_screencopy;
    out[1] = p.has_virtual_pointer;
    out[2] = p.has_virtual_keyboard;
    out[3] = p.has_wl_output;
    out[4] = p.has_wl_shm;
    out[5] = p.has_xdg_output;
    out[6] = p.globals;
    out[7] = 1; // connected

    if (p.registry) wl_registry_destroy(p.registry);
    wl_display_disconnect(p.display);
    return 0;
}
