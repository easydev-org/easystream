// Wayland capture via the xdg-desktop-portal ScreenCast API + PipeWire.
//
// This is the path that works on compositors which do NOT implement the
// wlroots-only zwlr_screencopy_manager_v1 (kwin, mutter/GNOME). The portal
// negotiates a PipeWire stream for a monitor and hands us a file descriptor;
// we then read frames from PipeWire and convert them to BGRA.
//
// Handshake (all on the SESSION bus):
//   CreateSession   -> session_handle
//   SelectSources   -> types=MONITOR, persist_mode=0
//   Start           -> streams [(node_id, size)]
//   OpenPipeWireRemote -> fd
// The pre-seeded PermissionStore entry (see bake/seed-screencast.sh) makes this
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
#define IFACE_RD "org.freedesktop.portal.RemoteDesktop"

// RemoteDesktop is the INPUT half of the same portal session ScreenCast uses.
//
// GNOME Shell and KDE Plasma are Wayland compositors that do not implement the
// wlroots protocols (zwlr_virtual_pointer_v1, zwlr_virtual_keyboard_v1,
// zwlr_screencopy_manager_v1). Their only input path is this portal interface,
// which the sandbox previously left unimplemented — so `capabilities` reported
// input=PortalRemoteDesktop while nothing could actually move the pointer.
//
// Both interfaces share ONE session: RemoteDesktop.SelectDevices declares what we
// want (pointer + keyboard), ScreenCast.SelectSources declares what we capture,
// and Start() starts both. That is why the calls are interleaved rather than
// living in separate sessions.

typedef struct {
    DBusConnection *conn;
    char *session_handle;     // portal session object path
    char *restore_token;      // persisted across runs when available
    uint32_t node_id;
    int width, height;

    // RemoteDesktop state. devices_ok is 0 when the portal would not give us
    // input on this session (an older xdg-desktop-portal, or a backend without
    // the interface), in which case the caller falls back rather than sending
    // events that go nowhere.
    int devices_ok;

    struct pw_thread_loop *loop;
    struct pw_stream *stream;
    struct pw_context *context;
    struct pw_core *core;
    int got_frame;
    int has_frame;   // a frame has EVER been written into last[]
    int stream_error;
    unsigned long long frames; // total frames seen, for stall visibility
    uint8_t *last;            // BGRA copy of the newest frame
    size_t last_len;
    int stride;
    int fps;
    struct spa_hook core_listener;
} na_portal;

static const char *restore_token_path(void) {
    static char buf[512];
    const char *h = getenv("HOME");
    // The token that matters for input is the one in the "remote-desktop" table,
    // not "screencast": SelectDevices looks up REMOTE_DESKTOP_TABLE, so a token
    // taken from the screen cast table would never be found and Start would fall
    // back to the consent dialog.
    snprintf(buf, sizeof(buf), "%s/.config/remote-desktop-token",
             h ? h : "/root");
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


// Helpers that build a{sv} entries correctly. Every value inside {sv} MUST be
// wrapped in a variant; getting this wrong makes libdbus abort with
// "Array or variant type requires that type variant be written".
static void dict_add_str(DBusMessageIter *dict, const char *key, const char *val) {
    DBusMessageIter e, v;
    dbus_message_iter_open_container(dict, DBUS_TYPE_DICT_ENTRY, NULL, &e);
    dbus_message_iter_append_basic(&e, DBUS_TYPE_STRING, &key);
    dbus_message_iter_open_container(&e, DBUS_TYPE_VARIANT, "s", &v);
    dbus_message_iter_append_basic(&v, DBUS_TYPE_STRING, &val);
    dbus_message_iter_close_container(&e, &v);
    dbus_message_iter_close_container(dict, &e);
}
static void dict_add_u32(DBusMessageIter *dict, const char *key, uint32_t val) {
    DBusMessageIter e, v;
    dbus_message_iter_open_container(dict, DBUS_TYPE_DICT_ENTRY, NULL, &e);
    dbus_message_iter_append_basic(&e, DBUS_TYPE_STRING, &key);
    dbus_message_iter_open_container(&e, DBUS_TYPE_VARIANT, "u", &v);
    dbus_message_iter_append_basic(&v, DBUS_TYPE_UINT32, &val);
    dbus_message_iter_close_container(&e, &v);
    dbus_message_iter_close_container(dict, &e);
}
static void dict_add_bool(DBusMessageIter *dict, const char *key, int val) {
    DBusMessageIter e, v;
    dbus_int32_t b = val ? 1 : 0;
    dbus_message_iter_open_container(dict, DBUS_TYPE_DICT_ENTRY, NULL, &e);
    dbus_message_iter_append_basic(&e, DBUS_TYPE_STRING, &key);
    dbus_message_iter_open_container(&e, DBUS_TYPE_VARIANT, "b", &v);
    dbus_message_iter_append_basic(&v, DBUS_TYPE_BOOLEAN, &b);
    dbus_message_iter_close_container(&e, &v);
    dbus_message_iter_close_container(dict, &e);
}

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
    int waited_ms = 0;
    for (;;) {
        if (waited_ms > 40000) {
            fprintf(stderr, "portal: no Response for %s\n", request_path);
            return -1;
        }
        dbus_connection_read_write(p->conn, 200);
        waited_ms += 200;
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
    if (dbus_message_get_type(reply) == DBUS_MESSAGE_TYPE_ERROR) {
        DBusError e2; dbus_error_init(&e2);
        dbus_set_error_from_message(&e2, reply);
        fprintf(stderr, "portal %s error: %s\n", method, e2.message ? e2.message : "?");
        dbus_error_free(&e2);
        dbus_message_unref(reply);
        return -1;
    }
    const char *rpath = req_path;
    char *returned = NULL;
    if (dbus_message_get_args(reply, &err, DBUS_TYPE_OBJECT_PATH, &returned, DBUS_TYPE_INVALID)
        && returned) {
        rpath = returned;
    }
    dbus_message_unref(reply);
    return wait_response(p, rpath, sh, node, w, h, tok);
}

// portal_call_iface is portal_call for a caller-named interface and no results.
//
// portal_call hard-codes IFACE_SC because every ScreenCast call needs its reply
// unpacked (session handle, node id, size). RemoteDesktop.SelectDevices needs
// none of that — only "did it succeed" — so it gets this simpler form instead of
// a second copy of the reply parsing.
static int portal_call_iface(na_portal *p, const char *iface, const char *method,
                             DBusMessage *msg, const char *req_path) {
    DBusError err;
    dbus_error_init(&err);
    DBusMessage *reply = call_sync(p, iface, method, msg, &err);
    if (!reply) {
        fprintf(stderr, "portal %s: %s\n", method, err.message ? err.message : "no reply");
        dbus_error_free(&err);
        return -1;
    }
    if (dbus_message_get_type(reply) == DBUS_MESSAGE_TYPE_ERROR) {
        DBusError e2; dbus_error_init(&e2);
        dbus_set_error_from_message(&e2, reply);
        fprintf(stderr, "portal %s error: %s\n", method, e2.message ? e2.message : "?");
        dbus_error_free(&e2);
        dbus_message_unref(reply);
        return -1;
    }
    // Same reply handling as portal_call, but only the success code matters here.
    const char *rpath = req_path;
    char *returned = NULL;
    if (dbus_message_get_args(reply, &err, DBUS_TYPE_OBJECT_PATH, &returned, DBUS_TYPE_INVALID)
        && returned) {
        rpath = returned;
    }
    dbus_message_unref(reply);
    return wait_response(p, rpath, NULL, NULL, NULL, NULL, NULL) == 0 ? 0 : -1;
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
        // Accept the frame even when the buffer is SHORTER than height*stride.
        //
        // It usually is: the last row's padding is often not transmitted, so
        // size is a few bytes (or a whole row) short of need. The old code
        // required size >= need and therefore dropped every frame, which is why
        // the stream sat in "streaming" with grab() reporting "no frame yet"
        // forever. Copy what is actually there and leave the tail as it was.
        size_t copy = size < need ? size : need;
        if (p->last && copy > 0) {
            memcpy(p->last, src, copy);
            p->stride = (int)stride;
            p->got_frame = 1;
            p->has_frame = 1;
        }
        // Count frames so a stalled stream is visible in the log rather than
        // looking identical to a working one.
        p->frames = p->frames + 1;
        if (p->frames == 1 || (p->frames % 300) == 0)
            fprintf(stderr, "portal: frame #%llu %ux%u stride=%u size=%u need=%zu\n",
                    (unsigned long long)p->frames, p->width, p->height,
                    stride, size, need);
    }
    pw_stream_queue_buffer(p->stream, b);
}

static void on_state_changed(void *data, enum pw_stream_state old,
                             enum pw_stream_state state, const char *error) {
    na_portal *p = data;
    (void)old;
    fprintf(stderr, "portal: stream state=%s error=%s\n",
            pw_stream_state_as_string(state), error ? error : "(none)");
    if (state == PW_STREAM_STATE_ERROR) p->stream_error = 1;
    if (state == PW_STREAM_STATE_PAUSED && p->width > 0) {
        // Negotiated: now we can ask for frames.
        pw_stream_set_active(p->stream, true);
    }
}

static void on_param_changed(void *data, uint32_t id, const struct spa_pod *param) {
    na_portal *p = data;
    if (id != SPA_PARAM_Format || !param) return;
    struct spa_video_info_raw vi;
    if (spa_format_video_raw_parse(param, &vi) < 0) return;
    p->width = vi.size.width;
    p->height = vi.size.height;
    fprintf(stderr, "portal: negotiated %dx%d format=%d\n",
            p->width, p->height, vi.format);
}

static const struct pw_stream_events stream_events = {
    PW_VERSION_STREAM_EVENTS,
    .process = on_process,
    .state_changed = on_state_changed,
    .param_changed = on_param_changed,
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
    // The portal replies with a Response SIGNAL on the request object, and
    // without a match rule the bus filters those out and we block forever.
    dbus_bus_add_match(p->conn,
        "type='signal',interface='org.freedesktop.portal.Request',member='Response'",
        &err);
    dbus_connection_flush(p->conn);
    p->restore_token = read_file(restore_token_path());

    char tok[64], stok[64];
    // A per-process counter, NOT the pid: the portal rejects a second session
    // whose handle_token matches an existing one ("An object is already exported
    // for the interface org.freedesktop.impl.portal.Session"), and the pid is
    // the same for every request in one process.
    static unsigned seq = 0;
    unsigned id = ++seq;
    snprintf(tok, sizeof(tok), "eas%u", id);
    snprintf(stok, sizeof(stok), "ess%u", id);
    char sender[128];
    const char *uniq = dbus_bus_get_unique_name(p->conn);
    // The request path is /org/freedesktop/portal/desktop/request/<sender>/<token>
    snprintf(sender, sizeof(sender), "%s", uniq ? uniq + 1 : "0");
    for (char *c = sender; *c; c++) if (*c == '.') *c = '_';

    char reqpath[256];

    // 1) CreateSession
    snprintf(reqpath, sizeof(reqpath),
             "/org/freedesktop/portal/desktop/request/%s/%s", sender, tok);
    // CreateSession on REMOTE DESKTOP, not ScreenCast.
    //
    // The two interfaces create different GObject types — ScreenCastSession and
    // RemoteDesktopSession — and a session can only be driven by the interface
    // family that created it:
    //
    //   screen-cast.c handle_select_sources:
    //     if (IS_SCREEN_CAST_SESSION (session)) { ...state machine... }
    //     else if (IS_REMOTE_DESKTOP_SESSION (session)) { can_select_sources ... }
    //
    // Creating it via ScreenCast and then calling RemoteDesktop.SelectDevices
    // left the portal with a session it had to close, so every following
    // SelectSources returned "Invalid session" and nothing ever streamed.
    //
    // RemoteDesktop is a superset for our purposes: it exposes CreateSession,
    // SelectDevices, Start AND the Notify* input calls, while ScreenCast
    // contributes SelectSources and OpenPipeWireRemote on the same session.
    DBusMessage *m = dbus_message_new_method_call(PORTAL_BUS, PORTAL_PATH, IFACE_RD, "CreateSession");
    DBusMessageIter it, dict;
    dbus_message_iter_init_append(m, &it);
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dict_add_str(&dict, "handle_token", tok);
    dict_add_str(&dict, "session_handle_token", stok);
    dbus_message_iter_close_container(&it, &dict);
    char *sh = NULL;
    if (portal_call(p, "CreateSession", m, reqpath, &sh, NULL, NULL, NULL, NULL) != 0 || !sh) {
        free(p);
        return NULL;
    }
    p->session_handle = sh;

    // 1b) SelectDevices on RemoteDesktop, BEFORE SelectSources.
    //
    // The session is shared between the two interfaces, and RemoteDesktop wants
    // to know which devices it may inject before the session is started. Doing
    // this after Start() is rejected by the portal.
    //
    // Failure is NOT fatal: capture is the primary job, and an input-less
    // sandbox is still better than no sandbox. devices_ok records the outcome so
    // na_portal_rd_* can refuse rather than silently doing nothing.
    p->devices_ok = 0;
    {
        const char *shp = p->session_handle;
        snprintf(tok, sizeof(tok), "esd%u", id);
        snprintf(reqpath, sizeof(reqpath),
                 "/org/freedesktop/portal/desktop/request/%s/%s", sender, tok);
        DBusMessage *dm = dbus_message_new_method_call(
            PORTAL_BUS, PORTAL_PATH, IFACE_RD, "SelectDevices");
        if (dm) {
            DBusMessageIter dit, ddict;
            dbus_message_iter_init_append(dm, &dit);
            dbus_message_iter_append_basic(&dit, DBUS_TYPE_OBJECT_PATH, &shp);
            dbus_message_iter_open_container(&dit, DBUS_TYPE_ARRAY, "{sv}", &ddict);
            dict_add_str(&ddict, "handle_token", tok);
            // 1 = keyboard, 2 = pointer (bitmask, from the RemoteDesktop API).
            dict_add_u32(&ddict, "types", 1 | 2);
            // persist_mode is a u32 enum (0 = do not persist), not a bool. As a
            // bool the portal rejects the whole call:
            //   Expected type 'u' for option 'persist_mode', got 'b'
            // and because the failure is non-fatal by design, every input event
            // was then silently dropped — the desktop rendered and ignored clicks.
            //
            // It is 2 ("persist until revoked") on purpose. 0 means Start shows
            // the consent dialog every time, and a headless guest has nobody to
            // click it, so the session never starts. 2 lets the pre-baked token
            // in the remote-desktop table be replayed instead.
            //
            // Unlike SelectSources, SelectDevices does NOT reject persistence on
            // a RemoteDesktopSession: the "Remote desktop sessions cannot persist"
            // check lives in screen-cast.c and only fires for SelectSources. The
            // restore DATA that Start consumes is looked up from this table by
            // this very token.
            dict_add_u32(&ddict, "persist_mode", 2);
            if (p->restore_token && p->restore_token[0]) {
                dict_add_str(&ddict, "restore_token", p->restore_token);
                fprintf(stderr, "portal: SelectDevices token=%s persist_mode=2\n",
                        p->restore_token);
            } else {
                // Say so loudly: without a token the portal WILL prompt, and in a
                // headless guest that means the session never starts at all.
                fprintf(stderr, "portal: SelectDevices has NO restore token "
                                "(checked %s) — Start will ask for consent\n",
                        restore_token_path());
            }
            dbus_message_iter_close_container(&dit, &ddict);
            if (portal_call_iface(p, IFACE_RD, "SelectDevices", dm, reqpath) == 0) {
                p->devices_ok = 1;
                fprintf(stderr, "portal: SelectDevices ok\n");
            } else {
                fprintf(stderr, "portal: RemoteDesktop.SelectDevices unavailable; "
                                "input will fall back\n");
            }
        }
    }

    // 2) SelectSources(types=MONITOR, persist_mode=0)
    snprintf(tok, sizeof(tok), "ess%u", id);
    snprintf(reqpath, sizeof(reqpath),
             "/org/freedesktop/portal/desktop/request/%s/%s", sender, tok);
    m = dbus_message_new_method_call(PORTAL_BUS, PORTAL_PATH, IFACE_SC, "SelectSources");
    dbus_message_iter_init_append(m, &it);
    const char *shp = p->session_handle;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dict_add_str(&dict, "handle_token", tok);
    dict_add_u32(&dict, "types", 1);          // 1 = MONITOR
    // persist_mode 0 = "do not persist", and it has to be 0 here.
    //
    // It was 2 ("persist until revoked"), which makes the portal hand back a
    // restore_token to store for next time. xdg-desktop-portal-gnome SEGFAULTS
    // while handling that combination on this image — the daemon logs
    //   screen cast session owned by ':1.91' created      (three times)
    //   GLib-GObject-CRITICAL: g_object_set_qdata_full: assertion 'quark > 0'
    // and dies, so every SelectSources fails and nothing streams.
    //
    // Persistence is not needed anyway: the disk ships a pre-seeded
    // PermissionStore entry (bake/seed-screencast.sh), so the grant already
    // exists and no dialog appears. A persisted token would only help across
    // reboots, and the disk itself provides that.
    dict_add_u32(&dict, "persist_mode", 0);
    // cursor_mode: 1 = EMBEDDED, i.e. draw the pointer INTO the frames.
    //
    // Omitting it leaves the portal default of 0 (HIDDEN), so the guest's own
    // cursor was never composited into the capture and a remote viewer had no
    // pointer at all - the desktop responded to clicks but nothing showed where
    // the pointer was. 2 (METADATA) would instead require the client to render
    // its own cursor from position updates, which this protocol does not carry.
    dict_add_u32(&dict, "cursor_mode", 1);
    dbus_message_iter_close_container(&it, &dict);
    if (portal_call(p, "SelectSources", m, reqpath, NULL, NULL, NULL, NULL, NULL) != 0) {
        return NULL;
    }

    // 3) Start
    snprintf(tok, sizeof(tok), "est%u", id);
    snprintf(reqpath, sizeof(reqpath),
             "/org/freedesktop/portal/desktop/request/%s/%s", sender, tok);
    // Start on RemoteDesktop: the session was created there, and Start belongs to
    // the creating interface. Calling ScreenCast.Start on a RemoteDesktopSession
    // returns "Sources not selected".
    m = dbus_message_new_method_call(PORTAL_BUS, PORTAL_PATH, IFACE_RD, "Start");
    dbus_message_iter_init_append(m, &it);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    const char *parent = "";
    dbus_message_iter_append_basic(&it, DBUS_TYPE_STRING, &parent);
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dict_add_str(&dict, "handle_token", tok);
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
    // Offer the formats the portal actually produces. GNOME's ScreenCast
    // exposes BGRx (XRGB8888/BGRx8888), and a single-format offer is refused
    // with "no more input formats" when it does not match.
    struct spa_video_info_raw vi = {0};
    vi.size.width = p->width;
    vi.size.height = p->height;
    // The portal node advertises framerate {0,1} ("any"); asking for a fixed
    // 30/1 did not match and the stream failed with "no more input formats".
    vi.framerate = (struct spa_fraction){ 0, 1 };
    const struct spa_pod *params[2];
    vi.format = SPA_VIDEO_FORMAT_BGRx;
    params[0] = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &vi);
    vi.format = SPA_VIDEO_FORMAT_BGRA;
    params[1] = spa_format_video_raw_build(&b, SPA_PARAM_EnumFormat, &vi);
    pw_stream_connect(p->stream, PW_DIRECTION_INPUT, p->node_id,
                      PW_STREAM_FLAG_AUTOCONNECT | PW_STREAM_FLAG_MAP_BUFFERS,
                      params, 2);
    pw_thread_loop_unlock(p->loop);
    pw_thread_loop_start(p->loop);
    return 0;
}

// na_portal_grab copies the newest BGRA frame into a freshly allocated buffer.
int na_portal_grab(void *h, uint8_t **out, int *ow, int *oh) {
    na_portal *p = h;
    // Wait briefly for a NEW frame, but do not fail when none arrives.
    //
    // The portal capture is frame-driven: Mutter emits a buffer when the screen
    // changes, so a static desktop delivers one frame and then nothing. Waiting
    // only for got_frame made grab() return 0 forever after that single frame, so
    // the pump never set its last buffer, the encoder was never fed, and a viewer
    // sat on connecting with no video at all - while the stream itself was healthy
    // and had already produced a frame.
    //
    // Newest available frame is the useful semantic: hand back the last frame we
    // kept when no new one has arrived. Callers that care whether the screen
    // actually changed do their own pixel comparison.
    for (int i = 0; i < 200 && !p->got_frame && !p->has_frame; i++) usleep(10000);
    if (!p->has_frame || !p->last) return 0;
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


// ── RemoteDesktop input ─────────────────────────────────────────────────────
//
// The input half of the shared portal session. Every call is a fire-and-forget
// method on the session object; the portal injects into the compositor.
//
// All of them return -1 when SelectDevices did not succeed, so the Rust side can
// fall back explicitly instead of pretending to have moved a pointer.

static int rd_session_ok(na_portal *p) {
    return p && p->conn && p->session_handle && p->devices_ok;
}

int na_portal_rd_move(void *h, double x, double y) {
    na_portal *p = h;
    if (!rd_session_ok(p)) return -1;
    DBusMessage *m = dbus_message_new_method_call(
        PORTAL_BUS, PORTAL_PATH, IFACE_RD, "NotifyPointerMotionAbsolute");
    if (!m) return -1;
    DBusMessageIter it;
    dbus_message_iter_init_append(m, &it);
    const char *shp = p->session_handle;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    DBusMessageIter dict;
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_close_container(&it, &dict);
    // The stream id from Start's streams list, not 0.
    //
    // 0 was assumed to mean "the session's sole stream", but the portal treats it
    // as a node id and rejects the call: NotifyPointerMotionAbsolute takes the
    // actual PipeWire node, which start_done reports as the first member of each
    // a(ua{sv}) entry. With 0 the pointer never moved, so every click from the
    // web ui was silently dropped while capabilities still advertised
    // input=PortalRemoteDesktop.
    dbus_uint32_t stream = p->node_id;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_UINT32, &stream);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_DOUBLE, &x);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_DOUBLE, &y);
    DBusError err; dbus_error_init(&err);
    DBusMessage *r = call_sync(p, IFACE_RD, "NotifyPointerMotionAbsolute", m, &err);
    if (!r) { dbus_error_free(&err); return -1; }
    int bad = dbus_message_get_type(r) == DBUS_MESSAGE_TYPE_ERROR;
    dbus_message_unref(r);
    return bad ? -1 : 0;
}

int na_portal_rd_button(void *h, int button, int down) {
    na_portal *p = h;
    if (!rd_session_ok(p)) return -1;
    DBusMessage *m = dbus_message_new_method_call(
        PORTAL_BUS, PORTAL_PATH, IFACE_RD, "NotifyPointerButton");
    if (!m) return -1;
    DBusMessageIter it;
    dbus_message_iter_init_append(m, &it);
    const char *shp = p->session_handle;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    DBusMessageIter dict;
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_close_container(&it, &dict);
    dbus_int32_t btn = button;
    dbus_uint32_t state = down ? 1 : 0;   // 1 = pressed
    dbus_message_iter_append_basic(&it, DBUS_TYPE_INT32, &btn);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_UINT32, &state);
    DBusError err; dbus_error_init(&err);
    DBusMessage *r = call_sync(p, IFACE_RD, "NotifyPointerButton", m, &err);
    if (!r) { dbus_error_free(&err); return -1; }
    int bad = dbus_message_get_type(r) == DBUS_MESSAGE_TYPE_ERROR;
    dbus_message_unref(r);
    return bad ? -1 : 0;
}

int na_portal_rd_axis(void *h, double dx, double dy) {
    na_portal *p = h;
    if (!rd_session_ok(p)) return -1;
    DBusMessage *m = dbus_message_new_method_call(
        PORTAL_BUS, PORTAL_PATH, IFACE_RD, "NotifyPointerAxis");
    if (!m) return -1;
    DBusMessageIter it;
    dbus_message_iter_init_append(m, &it);
    const char *shp = p->session_handle;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    DBusMessageIter dict;
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_close_container(&it, &dict);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_DOUBLE, &dx);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_DOUBLE, &dy);
    DBusError err; dbus_error_init(&err);
    DBusMessage *r = call_sync(p, IFACE_RD, "NotifyPointerAxis", m, &err);
    if (!r) { dbus_error_free(&err); return -1; }
    int bad = dbus_message_get_type(r) == DBUS_MESSAGE_TYPE_ERROR;
    dbus_message_unref(r);
    return bad ? -1 : 0;
}

// na_portal_rd_key takes the guest's EVDEV keycode (what the browser sends via
// xkb, already offset by 8 relative to X11). 1 = pressed, 0 = released.
int na_portal_rd_key(void *h, int keycode, int down) {
    na_portal *p = h;
    if (!rd_session_ok(p)) return -1;
    DBusMessage *m = dbus_message_new_method_call(
        PORTAL_BUS, PORTAL_PATH, IFACE_RD, "NotifyKeyboardKeycode");
    if (!m) return -1;
    DBusMessageIter it;
    dbus_message_iter_init_append(m, &it);
    const char *shp = p->session_handle;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_OBJECT_PATH, &shp);
    DBusMessageIter dict;
    dbus_message_iter_open_container(&it, DBUS_TYPE_ARRAY, "{sv}", &dict);
    dbus_message_iter_close_container(&it, &dict);
    dbus_int32_t code = keycode;
    dbus_uint32_t state = down ? 1 : 0;
    dbus_message_iter_append_basic(&it, DBUS_TYPE_INT32, &code);
    dbus_message_iter_append_basic(&it, DBUS_TYPE_UINT32, &state);
    DBusError err; dbus_error_init(&err);
    DBusMessage *r = call_sync(p, IFACE_RD, "NotifyKeyboardKeycode", m, &err);
    if (!r) { dbus_error_free(&err); return -1; }
    int bad = dbus_message_get_type(r) == DBUS_MESSAGE_TYPE_ERROR;
    dbus_message_unref(r);
    return bad ? -1 : 0;
}

int na_portal_rd_ok(void *h) {
    return rd_session_ok(h) ? 1 : 0;
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
