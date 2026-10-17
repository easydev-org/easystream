// Wayland capture via the xdg-desktop-portal ScreenCast API + PipeWire.
//
// This is the path that works on compositors which do NOT implement the
// wlroots-only zwlr_screencopy_manager_v1 (kwin, mutter/GNOME). The portal
// negotiates a PipeWire stream for a monitor and hands us a file descriptor;
// we then read frames from PipeWire and convert them to BGRA.
//
// Handshake (all on the SESSION bus):
//   CreateSession   -> session_handle
//   SelectSources   -> types=MONITOR, persist_mode=2, restore_token=<saved>
//   Start           -> streams [(node_id, size)], restore_token
//   OpenPipeWireRemote -> fd
// The restore_token (seeded into the golden disk, see bake/) makes this
// succeed with NO on-screen dialog, which is essential in a headless guest.
//
// Build: needs libpipewire-0.3 and libdbus-1 (see build.rs).
#define _GNU_SOURCE
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <errno.h>
#include <dbus/dbus.h>
#include <pipewire/pipewire.h>
#include <spa/param/video/format-utils.h>
#include <spa/param/video/raw-utils.h>
#include <spa/pod/builder.h>

#define PORTAL_BUS "org.freedesktop.portal.Desktop"
#define PORTAL_PATH "/org/freedesktop/portal/desktop"
#define IFACE_SC "org.freedesktop.portal.ScreenCast"
#define IFACE_REQ "org.freedesktop.portal.Request"

typedef struct {
    DBusConnection *conn;
    char *session_handle;     // portal session object path
    char *restore_token;      // persisted across runs when available
    uint32_t node_id;
    int width, height;

    struct pw_thread_loop *loop;
    struct pw_stream *stream;
    struct pw_context *context;
    struct pw_core *core;
    int got_frame;
    uint8_t *last;            // BGRA copy of the newest frame
    size_t last_len;
    int stride;
    int fps;
    struct spa_hook core_listener;
} na_portal;

static const char *restore_token_path(void) {
    static char buf[512];
    const char *h = getenv("HOME");
    snprintf(buf, sizeof(buf), "%s/.config/screencast-token", h ? h : "/root");
    return buf;
}

static char *read_file(const char *p) {
    FILE *f = fopen(p, "r");
    if (!f) return NULL;
    char *s = calloc(1, 1024);
    if (fgets(s, 1024, f)) {
        size_t n = strlen(s);
        while (n && (s[n-1] == '\n' || s[n-1] == '\r')) s[--n] = 0;
    } else { free(s); s = NULL; }
    fclose(f);
    return s;
}

static void write_file(const char *p, const char *v) {
    FILE *f = fopen(p, "w");
    if (!f) return;
    fputs(v, f);
    fputc('\n', f);
    fclose(f);
}

// ── D-Bus helpers ───────────────────────────────────────────────────────────

static DBusMessage *call_sync(na_portal *p, const char *iface, const char *method,
                              DBusMessage *msg, DBusError *err) {
    DBusMessage *reply = dbus_connection_send_with_reply_and_block(
        p->conn, msg, 25000, err);
    dbus_message_unref(msg);
    (void)iface; (void)method;
    return reply;
}

// Wait for the Response(ua{sv}) signal on the request path, returning the
// response code (0 = success) and optionally the results dict.
static int wait_response(na_portal *p, const char *request_path,
                         char **out_session_handle, uint32_t *out_node,
                         int *out_w, int *out_h, char **out_token) {
    dbus_connection_add_filter(p->conn, NULL, NULL, NULL); // no-op keep
    for (;;) {
        dbus_connection_read_write(p->conn, 1000);
        DBusMessage *m;
        while ((m = dbus_connection_pop_message(p->conn))) {
            if (dbus_message_is_signal(m, IFACE_REQ, "Response")) {
                const char *path = dbus_message_get_path(m);
                if (path && !strcmp(path, request_path)) {
                    dbus_uint32_t code = 1;
                    DBusMessageIter it, arr, entry, dict, kv, var;
                    dbus_message_iter_init(m, &it);
                    dbus_message_iter_get_basic(&it, &code);
                    dbus_message_iter_next(&it);
                    if (dbus_message_iter_get_arg_type(&it) == DBUS_TYPE_ARRAY) {
                        dbus_message_iter_recurse(&it, &arr);
                        while (dbus_message_iter_get_arg_type(&arr) == DBUS_TYPE_DICT_ENTRY) {
                            dbus_message_iter_recurse(&arr, &entry);
                            const char *key = NULL;
                            dbus_message_iter_get_basic(&entry, &key);
                            dbus_message_iter_next(&entry);
                            dbus_message_iter_recurse(&entry, &var);
                            if (key && !strcmp(key, "session_handle") &&
                                dbus_message_iter_get_arg_type(&var) == DBUS_TYPE_STRING) {
                                const char *v = NULL;
                                dbus_message_iter_get_basic(&var, &v);
                                if (out_session_handle) *out_session_handle = strdup(v);
                            } else if (key && !strcmp(key, "restore_token") &&
                                       dbus_message_iter_get_arg_type(&var) == DBUS_TYPE_STRING) {
                                const char *v = NULL;
                                dbus_message_iter_get_basic(&var, &v);
                                if (out_token) *out_token = strdup(v);
                            } else if (key && !strcmp(key, "streams") &&
                                       dbus_message_iter_get_arg_type(&var) == DBUS_TYPE_ARRAY) {
                                // a(ua{sv}) -> first struct: (node_id, props)
                                dbus_message_iter_recurse(&var, &arr);
                                if (dbus_message_iter_get_arg_type(&arr) == DBUS_TYPE_STRUCT) {
                                    dbus_message_iter_recurse(&arr, &entry);
                                    dbus_uint32_t node = 0;
                                    if (dbus_message_iter_get_arg_type(&entry) == DBUS_TYPE_UINT32) {
                                        dbus_message_iter_get_basic(&entry, &node);
                                        if (out_node) *out_node = node;
                                    }
                                    dbus_message_iter_next(&entry);
                                    if (dbus_message_iter_get_arg_type(&entry) == DBUS_TYPE_ARRAY) {
                                        dbus_message_iter_recurse(&entry, &dict);
                                        while (dbus_message_iter_get_arg_type(&dict) == DBUS_TYPE_DICT_ENTRY) {
                                            dbus_message_iter_recurse(&dict, &kv);
                                            const char *k = NULL;
                                            dbus_message_iter_get_basic(&kv, &k);
                                            dbus_message_iter_next(&kv);
                                            dbus_message_iter_recurse(&kv, &var);
                                            if (k && !strcmp(k, "size") &&
                                                dbus_message_iter_get_arg_type(&var) == DBUS_TYPE_STRUCT) {
                                                DBusMessageIter st;
                                                dbus_message_iter_recurse(&var, &st);
                                                dbus_int32_t w=0,hh=0;
                                                dbus_message_iter_get_basic(&st, &w);
                                                dbus_message_iter_next(&st);
                                                dbus_message_iter_get_basic(&st, &hh);
                                                if (out_w) *out_w = w;
                                                if (out_h) *out_h = hh;
                                            }
                                            dbus_message_iter_next(&dict);
                                        }
                                    }
                                }
                            }
                            dbus_message_iter_next(&arr);
                        }
                    }
                    dbus_message_unref(m);
                    return (int)code;
                }
            }
            dbus_message_unref(m);
        }
    }
}

static int portal_call(na_portal *p, const char *method, DBusMessage *msg,
                       const char *req_path, char **sh, uint32_t *node,
                       int *w, int *h, char **tok) {
    DBusError err;
    dbus_error_init(&err);
    DBusMessage *reply = call_sync(p, IFACE_SC, method, msg, &err);
    if (!reply) {
        fprintf(stderr, "portal %s: %s\n", method, err.message ? err.message : "no reply");
        dbus_error_free(&err);
        return -1;
    }
    dbus_message_unref(reply);
    int code = wait_response(p, req_path, sh, node, w, h, tok);
    return code;
}

// ── PipeWire ────────────────────────────────────────────────────────────────

static void on_process(void *data) {
    na_portal *p = data;
    struct pw_buffer *b;
    if (!(b = pw_stream_dequeue_buffer(p->stream))) return;
    struct spa_buffer *buf = b->buffer;
    if (buf->n_datas > 0 && buf->datas[0].data) {
        uint32_t offs = SPA_MIN(buf->datas[0].chunk->offset, buf->datas[0].maxsize);
        uint32_t size = SPA_MIN(buf->datas[0].chunk->size, buf->datas[0].maxsize - offs);
        uint8_t *src = (uint8_t *)buf->datas[0].data + offs;
        // Assume BGRx/BGRA at p->width*p->height; copy row by row honouring stride.
        uint32_t stride = buf->datas[0].chunk->stride;
        if (stride <= 0) stride = (uint32_t)p->width * 4;
        size_t need = (size_t)p->height * stride;
        if (!p->last || p->last_len < need) {
            free(p->last);
            p->last = malloc(need);
            p->last_len = need;
        }
        if (p->last && size >= need) {
            memcpy(p->last, src, need);
            p->stride = (int)stride;
            p->got_frame = 1;
        }
    }
    pw_stream_queue_buffer(p->stream, b);
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .process = on_process,
};

// ── public API (mirrors wlshim.c) ───────────────────────────────────────────

void *na_portal_open(void) {
    na_portal *p = calloc(1, sizeof(*p));
    DBusError err;
    dbus_error_init(&err);
    p->conn = dbus_bus_get(DBUS_BUS_SESSION, &err);
    if (!p->conn) {
        fprintf(stderr, "portal: no session bus: %s\n", err.message ? err.message : "?");
        free(p);
        return NULL;
    }
    p->restore_token = read_file(restore_token_path());

    char tok[64], stok[64];
    snprintf(tok, sizeof(tok), "eas%d", (int)getpid());
    snprintf(stok, sizeof(stok), "ess%d", (int)getpid());
    char sender[128];
    const char *uniq = dbus_bus_get_unique_name(p->conn);
    // The request path is /org/freedesktop/portal/desktop/request/<sender>/<token>
    snprintf(sender, sizeof(sender), "%s", uniq ? uniq + 1 : "0");
    for (char *c = sender; *c; c++) if (*c == '.') *c = '_';

    char reqpath[256];

    // 1) CreateSession
    snprintf(reqpath, sizeof(reqpath),
             "/org/freedesktop/portal/desktop/request/%s/%s", sender, tok);
    DBusMessage *m = dbus_message_new_method_call(PORTAL_BUS, PORTAL_PATH, IFACE_SC, "CreateSession");
    DBusMessageIter it, dict, entry, var;
    dbus_message_iter_init_append(m, &it);
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_open_container(&dict, DBUS_TYPE_DICT_ENTRY, NULL, &entry);
    const char *k1 = "handle_token"; const char *v1 = tok;
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &k1);
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &v1);
    dbus_message_iter_close_container(&dict, &entry);
    dbus_message_iter_open_container(&dict, DBUS_TYPE_DICT_ENTRY, NULL, &entry);
    const char *k2 = "session_handle_token"; const char *v2 = stok;
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &k2);
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &v2);
    dbus_message_iter_close_container(&dict, &entry);
    dbus_message_iter_close_container(&it, &dict);
    char *sh = NULL;
    if (portal_call(p, "CreateSession", m, reqpath, &sh, NULL, NULL, NULL, NULL) != 0 || !sh) {
        free(p); return NULL;
    }
    p->session_handle = sh;

    // 2) SelectSources(types=MONITOR, persist_mode=2, restore_token)
    snprintf(tok, sizeof(tok), "ess%d", (int)getpid());
    snprintf(reqpath, sizeof(reqpath),
             "/org/freedesktop/portal/desktop/request/%s/%s", sender, tok);
    m = dbus_message_new_method_call(PORTAL_BUS, PORTAL_PATH, IFACE_SC, "SelectSources");
    dbus_message_iter_init_append(m, &it);
    const char *shp = p->session_handle;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_open_container(&dict, DBUS_TYPE_DICT_ENTRY, NULL, &entry);
    const char *k3 = "handle_token"; const char *v3 = tok;
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &k3);
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &v3);
    dbus_message_iter_close_container(&dict, &entry);
    dbus_message_iter_open_container(&dict, DBUS_TYPE_DICT_ENTRY, NULL, &entry);
    const char *k4 = "types"; dbus_uint32_t v4 = 1; // MONITOR
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &k4);
    dbus_message_iter_open_container(&entry, DBUS_TYPE_VARIANT, "u", &var);
    dbus_message_iter_append_basic(&var, DBUS_TYPE_UINT32, &v4);
    dbus_message_iter_close_container(&entry, &var);
    dbus_message_iter_close_container(&dict, &entry);
    dbus_message_iter_open_container(&dict, DBUS_TYPE_DICT_ENTRY, NULL, &entry);
    const char *k5 = "persist_mode"; dbus_uint32_t v5 = 2;
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &k5);
    dbus_message_iter_open_container(&entry, DBUS_TYPE_VARIANT, "u", &var);
    dbus_message_iter_append_basic(&var, DBUS_TYPE_UINT32, &v5);
    dbus_message_iter_close_container(&entry, &var);
    dbus_message_iter_close_container(&dict, &entry);
    if (p->restore_token) {
        dbus_message_iter_open_container(&dict, DBUS_TYPE_DICT_ENTRY, NULL, &entry);
        const char *k6 = "restore_token"; const char *v6 = p->restore_token;
        dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &k6);
        dbus_message_iter_open_container(&entry, DBUS_TYPE_VARIANT, "s", &var);
        dbus_message_iter_append_basic(&var, DBUS_TYPE_STRING, &v6);
        dbus_message_iter_close_container(&entry, &var);
        dbus_message_iter_close_container(&dict, &entry);
    }
    dbus_message_iter_close_container(&it, &dict);
    if (portal_call(p, "SelectSources", m, reqpath, NULL, NULL, NULL, NULL, NULL) != 0) {
        return NULL;
    }

    // 3) Start
    snprintf(tok, sizeof(tok), "est%d", (int)getpid());
    snprintf(reqpath, sizeof(reqpath),
             "/org/freedesktop/portal/desktop/request/%s/%s", sender, tok);
    m = dbus_message_new_method_call(PORTAL_BUS, PORTAL_PATH, IFACE_SC, "Start");
    dbus_message_iter_init_append(m, &it);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    const char *parent = "";
    dbus_message_iter_append_basic(&it, DBUS_TYPE_STRING, &parent);
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_open_container(&dict, DBUS_TYPE_DICT_ENTRY, NULL, &entry);
    const char *k7 = "handle_token"; const char *v7 = tok;
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &k7);
    dbus_message_iter_append_basic(&entry, DBUS_TYPE_STRING, &v7);
    dbus_message_iter_close_container(&dict, &entry);
    dbus_message_iter_close_container(&it, &dict);
    char *newtok = NULL;
    if (portal_call(p, "Start", m, reqpath, NULL, &p->node_id, &p->width, &p->height, &newtok) != 0) {
        return NULL;
    }
    if (newtok) {
        write_file(restore_token_path(), newtok);
        free(newtok);
    }
    if (p->width <= 0 || p->height <= 0) { p->width = 1024; p->height = 768; }
    return p;
}

// na_portal_connect_stream opens the PipeWire remote and starts the stream.
// Kept separate so the caller can report size before frames flow.
int na_portal_start(void *h) {
    na_portal *p = h;
    DBusError err; dbus_error_init(&err);
    DBusMessage *m = dbus_message_new_method_call(PORTAL_BUS, PORTAL_PATH, IFACE_SC,
                                                 "OpenPipeWireRemote");
    DBusMessageIter it, dict;
    dbus_message_iter_init_append(m, &it);
    const char *shp = p->session_handle;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_close_container(&it, &dict);
    DBusMessage *reply = dbus_connection_send_with_reply_and_block(p->conn, m, 20000, &err);
    dbus_message_unref(m);
    if (!reply) {
        fprintf(stderr, "OpenPipeWireRemote: %s\n", err.message ? err.message : "?");
        return -1;
    }
    int fd = -1;
    if (!dbus_message_get_args(reply, &err, DBUS_TYPE_UNIX_FD, &fd, DBUS_TYPE_INVALID)) {
        fprintf(stderr, "OpenPipeWireRemote args: %s\n", err.message ? err.message : "?");
        dbus_message_unref(reply);
        return -1;
    }
    dbus_message_unref(reply);

    pw_init(NULL, NULL);
    p->loop = pw_thread_loop_new("easystream-portal", NULL);
    pw_thread_loop_lock(p->loop);
    p->context = pw_context_new(pw_thread_loop_get_loop(p->loop), NULL, 0);
    p->core = pw_context_connect_fd(p->context, fd, NULL, 0);
    if (!p->core) {
        pw_thread_loop_unlock(p->loop);
        fprintf(stderr, "portal: pw_context_connect_fd failed\n");
        return -1;
    }
    p->stream = pw_stream_new(p->core, "easystream", NULL);
    pw_stream_add_listener(p->stream, &p->core_listener, &stream_events, p);

    uint8_t buf[1024];
    struct spa_pod_builder b = SPA_POD_BUILDER_INIT(buf, sizeof(buf));
    struct spa_video_info_raw vi = {0};
    vi.format = SPA_VIDEO_FORMAT_BGRA;
    vi.size.width = p->width;
    vi.size.height = p->height;
    vi.framerate = (struct spa_fraction){ 30, 1 };
    const struct spa_pod *params[1];
    params[0] = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &vi);
    pw_stream_connect(p->stream, PW_DIRECTION_INPUT, p->node_id,
                      PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                      params, 1);
    pw_thread_loop_unlock(p->loop);
    pw_thread_loop_start(p->loop);
    return 0;
}

// na_portal_grab copies the newest BGRA frame into a freshly allocated buffer.
int na_portal_grab(void *h, uint8_t **out, int *ow, int *oh) {
    na_portal *p = h;
    for (int i = 0; i < 200 && !p->got_frame; i++) usleep(10000);
    if (!p->got_frame || !p->last) return 0;
    int w = p->width, hh = p->height;
    size_t len = (size_t)w * hh * 4;
    uint8_t *dst = malloc(len);
    if (!dst) return 0;
    // compact rows (PipeWire stride may be padded)
    for (int y = 0; y < hh; y++) {
        memcpy(dst + (size_t)y * w * 4, p->last + (size_t)y * p->stride, (size_t)w * 4);
    }
    *out = dst; *ow = w; *oh = hh;
    p->got_frame = 0;
    return 1;
}

void na_portal_size(void *h, int *w, int *hh) {
    na_portal *p = h;
    if (w) *w = p->width;
    if (hh) *hh = p->height;
}

void na_portal_close(void *h) {
    na_portal *p = h;
    if (!p) return;
    if (p->loop) {
        pw_thread_loop_stop(p->loop);
        if (p->stream) pw_stream_destroy(p->stream);
        if (p->core) pw_core_disconnect(p->core);
        if (p->context) pw_context_destroy(p->context);
        pw_thread_loop_destroy(p->loop);
    }
    free(p->last);
    free(p->session_handle);
    free(p->restore_token);
    free(p);
}
