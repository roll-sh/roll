/*
 * Glue between Lua and roll's scripting engine (Rust).
 *
 * Lua raises errors with longjmp, which must never cross Rust frames. So Rust callbacks never
 * raise: they return ROLL_RAISE (error object on the stack top) or ROLL_RAISE_WHERE (message
 * string to prefix with the caller's position, like luaL_error), and the trampoline below raises
 * from C. Everything else that can raise (metamethods guarding the globals, the instruction-count
 * hook, math.random's argument checks) lives here too.
 */

#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "lauxlib.h"
#include "lua.h"
#include "lualib.h"

#define ROLL_RAISE (-1)
#define ROLL_RAISE_WHERE (-2)

typedef int (*roll_fn)(lua_State *L);
/* The debugger's line callback: non-zero (with an error object pushed) raises it. */
typedef int (*roll_ldb_fn)(lua_State *L, int count, int line, const char *src);

/* ---- state ---- */

/* Allocation failures abort instead of raising LUA_ERRMEM (which could unwind through Rust). */
static void *roll_alloc(void *ud, void *ptr, size_t osize, size_t nsize) {
    (void)ud;
    (void)osize;
    if (nsize == 0) {
        free(ptr);
        return NULL;
    }
    void *p = realloc(ptr, nsize);
    if (p == NULL) {
        fputs("lua: out of memory\n", stderr);
        abort();
    }
    return p;
}

static int roll_panic(lua_State *L) {
    const char *msg = lua_tostring(L, -1);
    fprintf(stderr, "lua: unprotected error: %s\n", msg ? msg : "(not a string)");
    abort();
    return 0;
}

lua_State *roll_newstate(void) {
    lua_State *L = lua_newstate(roll_alloc, NULL);
    if (L != NULL) lua_atpanic(L, roll_panic);
    return L;
}

/* ---- Rust callbacks ---- */

static int trampoline(lua_State *L) {
    roll_fn f;
    void *p = lua_touserdata(L, lua_upvalueindex(1));
    memcpy(&f, &p, sizeof(f));
    int n = f(L);
    if (n == ROLL_RAISE) return lua_error(L);
    if (n == ROLL_RAISE_WHERE) {
        luaL_where(L, 1);
        lua_insert(L, -2);
        lua_concat(L, 2);
        return lua_error(L);
    }
    return n;
}

void roll_pushfn(lua_State *L, roll_fn f) {
    void *p;
    memcpy(&p, &f, sizeof(p));
    lua_pushlightuserdata(L, p);
    lua_pushcclosure(L, trampoline, 1);
}

/* ---- libraries ---- */

LUALIB_API int luaopen_cjson(lua_State *L);
LUALIB_API int luaopen_struct(lua_State *L);
LUALIB_API int luaopen_cmsgpack(lua_State *L);
LUALIB_API int luaopen_bit(lua_State *L);

static void open_lib(lua_State *L, const char *name, lua_CFunction open) {
    lua_pushcfunction(L, open);
    lua_pushstring(L, name);
    lua_call(L, 1, 0);
}

/* The libraries Redis exposes (os is the sandboxed subset; no io, no package). */
void roll_open_libs(lua_State *L) {
    open_lib(L, "", luaopen_base);
    open_lib(L, LUA_TABLIBNAME, luaopen_table);
    open_lib(L, LUA_STRLIBNAME, luaopen_string);
    open_lib(L, LUA_MATHLIBNAME, luaopen_math);
    open_lib(L, LUA_DBLIBNAME, luaopen_debug);
    open_lib(L, LUA_OSLIBNAME, luaopen_os);
    open_lib(L, "cjson", luaopen_cjson);
    open_lib(L, "struct", luaopen_struct);
    open_lib(L, "cmsgpack", luaopen_cmsgpack);
    open_lib(L, "bit", luaopen_bit);
}

/* ---- globals protection ---- */

/* Globals that may be created while the state is set up; anything else is dropped. */
static const char *const allowed_globals[] = {
    "string", "cjson", "bit", "cmsgpack", "math", "table", "struct", "os",
    "redis", "__redis__err__handler",
    "xpcall", "tostring", "setmetatable", "next", "assert", "tonumber", "rawequal",
    "collectgarbage", "getmetatable", "rawset", "pcall", "coroutine", "type", "_G", "select",
    "unpack", "gcinfo", "pairs", "rawget", "loadstring", "ipairs", "_VERSION", "load", "error",
    "debug", /* removed again once the error handler captured it */
    NULL,
};

/* Only with lua-enable-deprecated-api. */
static const char *const deprecated_globals[] = {"newproxy", "setfenv", "getfenv", NULL};

static int listed(const char *const *list, const char *name) {
    for (; *list; ++list)
        if (strcmp(*list, name) == 0) return 1;
    return 0;
}

static int allowlist_newindex(lua_State *L) {
    if (lua_gettop(L) != 3) luaL_error(L, "Wrong number of arguments to luaNewIndexAllowList");
    if (!lua_istable(L, -3)) luaL_error(L, "first argument to luaNewIndexAllowList must be a table");
    if (!lua_isstring(L, -2) && !lua_isnumber(L, -2))
        luaL_error(L, "Second argument to luaNewIndexAllowList must be a string or number");
    const char *name = lua_tostring(L, -2);
    int deprecated_ok = lua_toboolean(L, lua_upvalueindex(1));
    if (listed(allowed_globals, name) || (deprecated_ok && listed(deprecated_globals, name)))
        lua_rawset(L, -3);
    return 0;
}

/* Guard the table on the stack top: only allow-listed globals can be created. */
void roll_set_allowlist(lua_State *L, int deprecated_api) {
    lua_newtable(L);
    lua_pushboolean(L, deprecated_api);
    lua_pushcclosure(L, allowlist_newindex, 1);
    lua_setfield(L, -2, "__newindex");
    lua_setmetatable(L, -2);
}

static int missing_global(lua_State *L) {
    if (lua_gettop(L) != 2) luaL_error(L, "Wrong number of arguments to luaProtectedTableError");
    if (!lua_isstring(L, -1) && !lua_isnumber(L, -1))
        luaL_error(L, "Second argument to luaProtectedTableError must be a string or number");
    luaL_error(L, "Script attempted to access nonexistent global variable '%s'", lua_tostring(L, -1));
    return 0;
}

/* Reading a missing field of the table on the stack top raises an error. */
void roll_set_error_metatable(lua_State *L) {
    lua_newtable(L);
    lua_pushcfunction(L, missing_global);
    lua_setfield(L, -2, "__index");
    lua_setmetatable(L, -2);
}

/* Make the table on the stack top read-only, with every table reachable from it (values and
 * metatables). Already read-only tables are skipped, which also stops cycles like _G._G. */
void roll_protect(lua_State *L) {
    if (lua_isreadonlytable(L, -1)) return;
    lua_enablereadonlytable(L, -1, 1);
    lua_checkstack(L, 2);
    lua_pushnil(L);
    while (lua_next(L, -2)) {
        if (lua_istable(L, -1)) roll_protect(L);
        lua_pop(L, 1);
    }
    if (lua_getmetatable(L, -1)) {
        roll_protect(L);
        lua_pop(L, 1);
    }
}

/* Make the metatables shared by every value of a basic type (strings) read-only. */
void roll_protect_basic_types(lua_State *L) {
    for (int i = 0; i < 6; i++) {
        switch (i) {
        case 0: lua_pushstring(L, ""); break;
        case 1: lua_pushnumber(L, 0); break;
        case 2: lua_pushboolean(L, 0); break;
        case 3: lua_pushnil(L); break;
        case 4: lua_newthread(L); break;
        default: lua_pushlightuserdata(L, (void *)L); break;
        }
        if (lua_getmetatable(L, -1)) {
            roll_protect(L);
            lua_pop(L, 1);
        }
        lua_pop(L, 1);
    }
}

/* ---- pcall ---- */

/* Errors raised by redis.call are tables ({err = "..."}); pcall hands scripts the message string
 * instead, like Lua errors always were. */
static int roll_pcall_impl(lua_State *L) {
    int argc = lua_gettop(L);
    lua_pushboolean(L, 1);
    lua_insert(L, 1);
    if (lua_pcall(L, argc - 1, LUA_MULTRET, 0)) {
        lua_remove(L, 1);
        if (lua_istable(L, -1)) {
            lua_getfield(L, -1, "err");
            if (lua_isstring(L, -1)) lua_replace(L, -2);
        }
        lua_pushboolean(L, 0);
        lua_insert(L, 1);
    }
    return lua_gettop(L);
}

void roll_push_pcall(lua_State *L) {
    lua_pushcfunction(L, roll_pcall_impl);
}

/* ---- math.random ---- */

/* drand48's generator, so a seed gives the same sequence on every platform. */
typedef struct {
    uint64_t x;
} rand48;

static int32_t rand48_next(rand48 *r) {
    r->x = (r->x * 0x5DEECE66DULL + 0xB) & ((1ULL << 48) - 1);
    return (int32_t)(r->x >> 17);
}

#define RAND48_MAX INT32_MAX

static int math_random(lua_State *L) {
    rand48 *r = (rand48 *)lua_touserdata(L, lua_upvalueindex(1));
    lua_Number v = (lua_Number)(rand48_next(r) % RAND48_MAX) / (lua_Number)RAND48_MAX;
    switch (lua_gettop(L)) {
    case 0:
        lua_pushnumber(L, v);
        break;
    case 1: {
        int u = luaL_checkint(L, 1);
        luaL_argcheck(L, 1 <= u, 1, "interval is empty");
        lua_pushnumber(L, floor(v * u) + 1);
        break;
    }
    case 2: {
        int l = luaL_checkint(L, 1);
        int u = luaL_checkint(L, 2);
        luaL_argcheck(L, l <= u, 2, "interval is empty");
        lua_pushnumber(L, floor(v * (u - l + 1)) + l);
        break;
    }
    default:
        return luaL_error(L, "wrong number of arguments");
    }
    return 1;
}

static int math_randomseed(lua_State *L) {
    rand48 *r = (rand48 *)lua_touserdata(L, lua_upvalueindex(1));
    uint32_t seed = (uint32_t)luaL_checkint(L, 1);
    r->x = ((uint64_t)seed << 16) | 0x330E;
    return 0;
}

/* Replace math.random / math.randomseed in the table on the stack top (the math library). */
void roll_set_random(lua_State *L) {
    rand48 *r = (rand48 *)lua_newuserdata(L, sizeof(rand48));
    r->x = 0x1234ABCD330EULL;
    lua_pushvalue(L, -1);
    lua_pushcclosure(L, math_random, 1);
    lua_setfield(L, -3, "random");
    lua_pushcclosure(L, math_randomseed, 1);
    lua_setfield(L, -2, "randomseed");
}

/* ---- busy scripts ---- */

static roll_fn hook_check;

/* Every 100k instructions: ask Rust whether to stop (SCRIPT KILL, FUNCTION LOAD timeout). Once
 * stopping, the hook fires on every line so pcall can't swallow the error and carry on. */
static void count_hook(lua_State *L, lua_Debug *ar) {
    (void)ar;
    if (hook_check == NULL || !hook_check(L)) return;
    lua_sethook(L, count_hook, LUA_MASKLINE, 0);
    lua_error(L); /* hook_check pushed the error object */
}

void roll_sethook(lua_State *L, roll_fn check) {
    if (check) {
        hook_check = check;
        lua_sethook(L, count_hook, LUA_MASKCOUNT, 100000);
    } else {
        lua_sethook(L, NULL, 0, 0);
    }
}

/* ---- the Lua debugger ---- */

static roll_ldb_fn ldb_line;

/* Every line and every 100k instructions: hand the current frame to Rust, which may stop there
 * for the debugging client and asks for an error (pushed) to end the script. */
static void ldb_hook(lua_State *L, lua_Debug *ar) {
    int count = ar->event == LUA_HOOKCOUNT;
    lua_getstack(L, 0, ar);
    lua_getinfo(L, "Sl", ar);
    if (ldb_line != NULL && ldb_line(L, count, ar->currentline, ar->short_src)) lua_error(L);
}

void roll_setldbhook(lua_State *L, roll_ldb_fn line) {
    ldb_line = line;
    lua_sethook(L, ldb_hook, LUA_MASKLINE | LUA_MASKCOUNT, 100000);
}

/* Stack frame `level`: 0 when there is none, else 1 with its current line, short source and
 * name (NULL when unknown). The strings stay valid until the next call. */
int roll_frame(lua_State *L, int level, int *line, const char **src, const char **name) {
    static lua_Debug ar;
    if (!lua_getstack(L, level, &ar)) return 0;
    lua_getinfo(L, "Snl", &ar);
    *line = ar.currentline;
    *src = ar.short_src;
    *name = ar.name;
    return 1;
}

/* Local `n` (from 1) of frame `level`: its value pushed and its name returned, or NULL (nothing
 * pushed) past the last one. */
const char *roll_getlocal(lua_State *L, int level, int n) {
    lua_Debug ar;
    if (!lua_getstack(L, level, &ar)) return NULL;
    return lua_getlocal(L, &ar, n);
}
