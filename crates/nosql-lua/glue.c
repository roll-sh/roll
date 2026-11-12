/* Glue for roll nosql's Lua UDFs on Lua 5.4: bounded states (allocation limit, deadline hook),
 * the libraries a UDF sees, ScyllaDB's decimal userdata, and a trampoline for Rust callbacks.
 *
 * Lua raises errors with longjmp, which must never cross Rust frames: Rust callbacks are pushed
 * with nl_pushfn and only use non-raising API calls; they report errors and allocating results
 * through their nl_slice and return code, and this file's trampoline raises or allocates. */

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include "lauxlib.h"
#include "lua.h"
#include "lualib.h"

/* Limits of a state made by nl_newstate. Owned by the caller, who keeps it alive and at a fixed
 * address until lua_close. While `armed`, an allocation growing the state past `limit` bytes
 * fails (Lua raises "not enough memory"), and once CLOCK_MONOTONIC passes `deadline_ns` (or
 * `expired` is set by the host) the hook stops the call: it yields where it can (the host sees
 * the yield with `expired` set, which pcall can't intercept) and raises otherwise; after that
 * every allocation fails. Disarmed (setting up, pushing arguments, reading results) allocations
 * never fail on purpose. `host` is the caller's, for its callbacks. */
typedef struct {
    size_t used;
    size_t limit;
    int armed;
    int expired;
    long long deadline_ns;
    void *host;
} nl_bounds;

/* A Rust callback's out-parameter: an error message (NL_RAISE) or a decimal's bytes
 * (NL_DECIMAL), valid until the callback's state runs again. */
typedef struct {
    const char *ptr;
    size_t len;
} nl_slice;

typedef int (*nl_fn)(lua_State *L, nl_slice *out);

#define NL_RAISE (-1)
#define NL_DECIMAL (-2)

static const char *const DECIMAL_MT = "Scylla.decimal";
/* The registry also keeps the decimal metatable under this address (lookups that can't raise). */
static const char DECIMAL_KEY = 0;

static void *bounded_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
    nl_bounds *b = (nl_bounds *)ud;
    /* For a new object, osize is its type, not a size. */
    size_t old = ptr ? osize : 0;
    if (nsize == 0) {
        free(ptr);
        b->used -= old;
        return NULL;
    }
    if (b->armed && nsize > old && (b->expired || b->used - old + nsize > b->limit)) return NULL;
    void *p = realloc(ptr, nsize);
    if (p == NULL) {
        /* Only a growing allocation may fail, and only inside the call. */
        if (b->armed && nsize > old) return NULL;
        fputs("lua: out of memory\n", stderr);
        abort();
    }
    b->used = b->used - old + nsize;
    return p;
}

static int nl_panic(lua_State *L) {
    const char *msg = lua_tostring(L, -1);
    fprintf(stderr, "lua: unprotected error: %s\n", msg ? msg : "?");
    abort();
    return 0;
}

lua_State *nl_newstate(nl_bounds *b) {
    lua_State *L = lua_newstate(bounded_alloc, b);
    if (L != NULL) lua_atpanic(L, nl_panic);
    return L;
}

long long nl_monotonic_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

static nl_bounds *bounds_of(lua_State *L) {
    void *ud;
    lua_getallocf(L, &ud);
    return (nl_bounds *)ud;
}

/* The bounds of a state made by nl_newstate (for callbacks). Doesn't raise. */
nl_bounds *nl_bounds_of(lua_State *L) {
    return bounds_of(L);
}

/* Every 1000 instructions (every instruction once stopped). */
static void deadline_hook(lua_State *L, lua_Debug *ar) {
    (void)ar;
    nl_bounds *b = bounds_of(L);
    if (!b->armed) return;
    if (!b->expired) {
        if (b->deadline_ns == 0 || nl_monotonic_ns() < b->deadline_ns) return;
        b->expired = 1;
    }
    lua_sethook(L, deadline_hook, LUA_MASKCOUNT, 1);
    if (lua_isyieldable(L)) {
        lua_yield(L, 0);
        return;
    }
    lua_pushliteral(L, "timeout");
    lua_error(L);
}

/* Installs the deadline hook on L (threads created from L inherit it). */
void nl_sethook(lua_State *L) {
    lua_sethook(L, deadline_hook, LUA_MASKCOUNT, 1000);
}

/* ---- the libraries ------------------------------------------------------------------------ */

static int fake_print(lua_State *L) {
    (void)L;
    return 0;
}

static int fake_warn(lua_State *L) {
    int n = lua_gettop(L);
    luaL_checkstring(L, 1);
    for (int i = 2; i <= n; i++) luaL_checkstring(L, i);
    return 0;
}

/* No file exists: dofile/loadfile of a name fail as fopen would; without one (stdin) they load
 * an empty chunk. */
static int fake_dofile(lua_State *L) {
    const char *fname = luaL_optstring(L, 1, NULL);
    if (fname == NULL) return 0;
    lua_pushfstring(L, "cannot open %s: No such file or directory", fname);
    return lua_error(L);
}

static int fake_loadfile(lua_State *L) {
    const char *fname = luaL_optstring(L, 1, NULL);
    if (fname == NULL) {
        if (luaL_loadbufferx(L, "", 0, "=stdin", "t") != LUA_OK) return lua_error(L);
        return 1;
    }
    luaL_pushfail(L);
    lua_pushfstring(L, "cannot open %s: No such file or directory", fname);
    return 2;
}

/* `load` restricted to text chunks: binary chunks aren't verified by Lua. */
static int text_load(lua_State *L) {
    int n = lua_gettop(L);
    if (n < 2) {
        lua_settop(L, 2);
        n = 2;
    }
    if (n >= 3) lua_remove(L, 3);
    lua_pushliteral(L, "t");
    lua_insert(L, 3);
    lua_pushvalue(L, lua_upvalueindex(1));
    lua_insert(L, 1);
    lua_call(L, n >= 3 ? n : 3, LUA_MULTRET);
    return lua_gettop(L);
}

static void open_lib(lua_State *L, const char *name, lua_CFunction f, int global) {
    luaL_requiref(L, name, f, global);
    lua_pop(L, 1);
}

/* The libraries a UDF sees, as ScyllaDB opens them: base, coroutine, string and table (no math,
 * io, os, utf8, debug or package); dofile/loadfile see no files, print and warn print nothing,
 * load takes text chunks only. */
void nl_openlibs(lua_State *L) {
    open_lib(L, LUA_GNAME, luaopen_base, 1);
    open_lib(L, LUA_COLIBNAME, luaopen_coroutine, 1);
    open_lib(L, LUA_STRLIBNAME, luaopen_string, 1);
    open_lib(L, LUA_TABLIBNAME, luaopen_table, 1);
    lua_pushcfunction(L, fake_print);
    lua_setglobal(L, "print");
    lua_pushcfunction(L, fake_warn);
    lua_setglobal(L, "warn");
    lua_pushcfunction(L, fake_dofile);
    lua_setglobal(L, "dofile");
    lua_pushcfunction(L, fake_loadfile);
    lua_setglobal(L, "loadfile");
    lua_getglobal(L, "load");
    lua_pushcclosure(L, text_load, 1);
    lua_setglobal(L, "load");
}

/* ---- Rust callbacks ----------------------------------------------------------------------- */

static void push_decimal(lua_State *L, const char *bytes, size_t len);

static int trampoline(lua_State *L) {
    nl_fn f = (nl_fn)(uintptr_t)lua_touserdata(L, lua_upvalueindex(1));
    nl_slice out = {NULL, 0};
    int n = f(L, &out);
    if (n == NL_RAISE) {
        lua_pushlstring(L, out.ptr, out.len);
        return lua_error(L);
    }
    if (n == NL_DECIMAL) {
        push_decimal(L, out.ptr, out.len);
        return 1;
    }
    return n;
}

void nl_pushfn(lua_State *L, nl_fn f) {
    lua_pushlightuserdata(L, (void *)(uintptr_t)f);
    lua_pushcclosure(L, trampoline, 1);
}

/* ---- ScyllaDB's decimal userdata ------------------------------------------------------------
 * Varint and decimal arguments are full userdata whose metatable is
 * {__add, __sub, __gc, __index = itself, __name = "Scylla.decimal"}; the block holds the
 * decimal's CQL encoding (4-byte scale, then the unscaled value) after its length. */

static int decimal_gc(lua_State *L) {
    (void)L;
    return 0;
}

/* Creates the decimal metatable (in the registry under its name) with the host's arithmetic. */
void nl_decimal_mt(lua_State *L, nl_fn add, nl_fn sub) {
    luaL_newmetatable(L, DECIMAL_MT); /* sets __name */
    nl_pushfn(L, add);
    lua_setfield(L, -2, "__add");
    nl_pushfn(L, sub);
    lua_setfield(L, -2, "__sub");
    lua_pushcfunction(L, decimal_gc);
    lua_setfield(L, -2, "__gc");
    lua_pushvalue(L, -1);
    lua_setfield(L, -2, "__index");
    lua_rawsetp(L, LUA_REGISTRYINDEX, &DECIMAL_KEY);
}

static void push_decimal(lua_State *L, const char *bytes, size_t len) {
    unsigned char *p = (unsigned char *)lua_newuserdatauv(L, sizeof(size_t) + len, 0);
    memcpy(p, &len, sizeof(size_t));
    memcpy(p + sizeof(size_t), bytes, len);
    luaL_setmetatable(L, DECIMAL_MT);
}

void nl_pushdecimal(lua_State *L, const char *bytes, size_t len) {
    push_decimal(L, bytes, len);
}

/* The bytes of the decimal at idx (NULL if it isn't one). Doesn't raise. */
const char *nl_todecimal(lua_State *L, int idx, size_t *len) {
    unsigned char *p = (unsigned char *)lua_touserdata(L, idx);
    if (p == NULL || lua_type(L, idx) != LUA_TUSERDATA || !lua_getmetatable(L, idx)) return NULL;
    lua_rawgetp(L, LUA_REGISTRYINDEX, &DECIMAL_KEY);
    int ours = lua_rawequal(L, -1, -2);
    lua_pop(L, 2);
    if (!ours) return NULL;
    memcpy(len, p, sizeof(size_t));
    return (const char *)p + sizeof(size_t);
}

/* lua_Number2str: "%.14g", with ".0" when it looks like an integer. */
int nl_fmt_number(double d, char *buf, size_t n) {
    int len = snprintf(buf, n, LUA_NUMBER_FMT, d);
    if (len > 0 && (size_t)len + 2 < n && buf[strspn(buf, "-0123456789")] == '\0') {
        buf[len++] = '.';
        buf[len++] = '0';
        buf[len] = '\0';
    }
    return len;
}
