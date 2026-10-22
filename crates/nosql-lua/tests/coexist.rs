//! rolld links this crate's Lua 5.4 and `redis-lua`'s Lua 5.1 into one binary. Both export
//! Lua's symbol names unless `build.rs` keeps ours private, so this links both and runs a chunk
//! in each: a clash fails the link, and a wrong resolution returns the other Lua's `_VERSION`.

use std::ffi::{CStr, c_int};

/// Runs `src` and returns its first result as a string.
unsafe fn run_54(src: &[u8]) -> String {
    use nosql_lua::*;
    let mut b = Box::new(nl_bounds::default());
    unsafe {
        let l = nl_newstate(&mut *b);
        assert!(!l.is_null());
        nl_openlibs(l);
        assert_eq!(
            luaL_loadbufferx(
                l,
                src.as_ptr().cast(),
                src.len(),
                c"=t".as_ptr(),
                c"t".as_ptr()
            ),
            LUA_OK
        );
        let co = lua_newthread(l);
        lua_rotate(l, -2, 1);
        lua_xmove(l, co, 1);
        let mut n: c_int = 0;
        assert_eq!(lua_resume(co, l, 0, &mut n), LUA_OK);
        let mut len = 0;
        let p = lua_tolstring(co, -1, &mut len);
        let out =
            String::from_utf8_lossy(std::slice::from_raw_parts(p.cast::<u8>(), len)).into_owned();
        lua_close(l);
        out
    }
}

unsafe fn run_51(src: &[u8]) -> String {
    use redis_lua::*;
    unsafe {
        let l = roll_newstate();
        assert!(!l.is_null());
        roll_open_libs(l);
        assert_eq!(
            luaL_loadbuffer(l, src.as_ptr().cast(), src.len(), c"=t".as_ptr()),
            0
        );
        assert_eq!(lua_pcall(l, 0, 1, 0), 0);
        let p = lua_tolstring(l, -1, std::ptr::null_mut());
        let out = CStr::from_ptr(p).to_string_lossy().into_owned();
        lua_close(l);
        out
    }
}

#[test]
fn both_luas_link_and_run_side_by_side() {
    let src = b"return _VERSION .. ' ' .. type(bit) .. ' ' .. type(math)";
    unsafe {
        assert_eq!(run_54(src), "Lua 5.4 nil nil");
        assert_eq!(run_51(src), "Lua 5.1 table table");
        // Interleaved, so neither build leaves global state the other trips on.
        assert_eq!(run_54(b"return tostring(7 // 2)"), "3");
        assert_eq!(run_51(b"return tostring(cjson.encode({1, 2}))"), "[1,2]");
        assert_eq!(
            run_54(b"return string.format('%d', 1 << 40)"),
            "1099511627776"
        );
    }
}
