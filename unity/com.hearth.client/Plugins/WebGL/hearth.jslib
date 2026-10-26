// Hearth WebGL bridge: the wasm core (crates/hearth-unity, --features webgl)
// and browser WebSockets. C# polls everything from Update; nothing here calls
// back into C#.
//
// Byte results use a length-query-then-copy protocol: a call returns the
// length and keeps the bytes in Hearth.last; HearthCore_TakeLast copies them
// into a C#-provided buffer. Strings are returned with stringToNewUTF8, which
// the IL2CPP marshaller frees.
var HearthLib = {
    $Hearth: {
        core: null,
        ready: 0,
        last: null,
        lastIsText: 0,
        lastError: "",
        sockets: {},
        nextSocket: 1,

        call: function (fallback, body) {
            if (!Hearth.core) {
                Hearth.lastError = "hearth core is not loaded";
                return fallback;
            }
            try {
                return body(Hearth.core);
            } catch (e) {
                Hearth.lastError = "hearth core threw: " + e;
                return fallback;
            }
        },

        keep: function (bytes) {
            Hearth.last = bytes;
            Hearth.lastIsText = Hearth.core.last_is_text() ? 1 : 0;
            return bytes.length;
        },

        str: function (s) {
            return stringToNewUTF8(s || "");
        },
    },

    HearthCore_Init: function (baseUrlPtr) {
        if (Hearth.ready !== 0 || Hearth.loading) return;
        Hearth.loading = true;
        var base = UTF8ToString(baseUrlPtr);
        var script = document.createElement("script");
        script.src = base + "hearth_core.js";
        script.onload = function () {
            wasm_bindgen({ module_or_path: base + "hearth_core_bg.wasm" }).then(
                function () {
                    Hearth.core = wasm_bindgen;
                    Hearth.ready = 1;
                },
                function (e) {
                    console.error("Hearth: wasm core failed to load", e);
                    Hearth.ready = -1;
                }
            );
        };
        script.onerror = function () {
            console.error("Hearth: could not load " + script.src);
            Hearth.ready = -1;
        };
        document.head.appendChild(script);
    },

    HearthCore_Ready: function () {
        return Hearth.ready;
    },

    HearthCore_LastError: function () {
        var core = Hearth.core ? Hearth.core.last_error() : "";
        return Hearth.str(Hearth.lastError || core);
    },

    HearthCore_TakeLast: function (bufPtr) {
        if (Hearth.last) HEAPU8.set(Hearth.last, bufPtr);
        Hearth.last = null;
    },

    HearthCore_LastIsText: function () {
        return Hearth.lastIsText;
    },

    HearthCore_New: function (hashPtr, encoding) {
        var hash = UTF8ToString(hashPtr);
        return Hearth.call(0, function (c) { return c.client_new(hash, encoding); });
    },

    HearthCore_Free: function (h) {
        Hearth.call(0, function (c) { c.client_free(h); });
    },

    HearthCore_Hello: function (h, optsPtr) {
        var opts = UTF8ToString(optsPtr);
        return Hearth.call(0, function (c) { return Hearth.keep(c.client_hello(h, opts)); });
    },

    HearthCore_Handle: function (h, dataPtr, len, isText) {
        var data = HEAPU8.slice(dataPtr, dataPtr + len);
        return Hearth.str(Hearth.call("", function (c) { return c.client_handle(h, data, isText !== 0); }));
    },

    HearthCore_QueueIntent: function (h, jsonPtr) {
        var json = UTF8ToString(jsonPtr);
        return Hearth.call(0, function (c) { return c.client_queue_intent(h, json); });
    },

    HearthCore_PollSend: function (h, nowMs) {
        return Hearth.call(0, function (c) { return Hearth.keep(c.client_poll_send(h, nowMs)); });
    },

    HearthCore_Leave: function (h) {
        return Hearth.call(0, function (c) { return Hearth.keep(c.client_leave(h)); });
    },

    HearthCore_OnDisconnected: function (h) {
        return Hearth.str(Hearth.call("", function (c) { return c.client_on_disconnected(h); }));
    },

    HearthCore_State: function (h) {
        return Hearth.str(Hearth.call("", function (c) { return c.client_state(h); }));
    },

    HearthCore_Seq: function (h) {
        return Hearth.call(0, function (c) { return c.client_seq(h); });
    },

    HearthCore_ReconnectDecide: function (code, firstDropMs, attempt, nowMs) {
        return Hearth.call(-1, function (c) { return c.reconnect_decide(code, firstDropMs, attempt, nowMs); });
    },

    // ---- WebSocket ------------------------------------------------------

    HearthWS_Open: function (urlPtr) {
        var id = Hearth.nextSocket++;
        var queue = [];
        var ws;
        try {
            ws = new WebSocket(UTF8ToString(urlPtr));
        } catch (e) {
            queue.push({ kind: 4, code: 1006 });
            Hearth.sockets[id] = { ws: null, queue: queue };
            return id;
        }
        ws.binaryType = "arraybuffer";
        ws.onopen = function () { queue.push({ kind: 1 }); };
        ws.onmessage = function (e) {
            if (typeof e.data === "string") queue.push({ kind: 3, data: new TextEncoder().encode(e.data) });
            else queue.push({ kind: 2, data: new Uint8Array(e.data) });
        };
        ws.onclose = function (e) { queue.push({ kind: 4, code: e.code || 1006 }); };
        Hearth.sockets[id] = { ws: ws, queue: queue };
        return id;
    },

    HearthWS_Send: function (id, ptr, len, isText) {
        var s = Hearth.sockets[id];
        if (!s || !s.ws || s.ws.readyState !== 1) return;
        var bytes = HEAPU8.slice(ptr, ptr + len);
        s.ws.send(isText ? new TextDecoder().decode(bytes) : bytes);
    },

    HearthWS_Close: function (id, code) {
        var s = Hearth.sockets[id];
        delete Hearth.sockets[id];
        if (s && s.ws && s.ws.readyState <= 1) {
            try { s.ws.close(code); } catch (e) { s.ws.close(); }
        }
    },

    // Returns the event kind (0 none, 1 open, 2 binary, 3 text, 4 closed, 5
    // buffer too small) and writes the payload length (the close code for 4,
    // the needed size for 5) as little-endian int32 into the first 4 bytes,
    // followed by the payload.
    HearthWS_Poll: function (id, bufPtr, bufLen) {
        var s = Hearth.sockets[id];
        if (!s || s.queue.length === 0) return 0;
        var ev = s.queue[0];
        var len = ev.kind === 4 ? ev.code : ev.data ? ev.data.length : 0;
        if (ev.data && len + 4 > bufLen) {
            HEAPU8[bufPtr] = (len + 4) & 255;
            HEAPU8[bufPtr + 1] = ((len + 4) >> 8) & 255;
            HEAPU8[bufPtr + 2] = ((len + 4) >> 16) & 255;
            HEAPU8[bufPtr + 3] = ((len + 4) >> 24) & 255;
            return 5;
        }
        s.queue.shift();
        HEAPU8[bufPtr] = len & 255;
        HEAPU8[bufPtr + 1] = (len >> 8) & 255;
        HEAPU8[bufPtr + 2] = (len >> 16) & 255;
        HEAPU8[bufPtr + 3] = (len >> 24) & 255;
        if (ev.data) HEAPU8.set(ev.data, bufPtr + 4);
        return ev.kind;
    },
};

autoAddDeps(HearthLib, "$Hearth");
mergeInto(LibraryManager.library, HearthLib);
