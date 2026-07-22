use mlua::prelude::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "now",
        lua.create_function(|_, ()| {
            let millis = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            Ok(millis)
        })?,
    )?;
    t.set(
        "unix",
        lua.create_function(|_, ()| {
            Ok(SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs())
        })?,
    )?;
    t.set(
        "sleep",
        lua.create_async_function(|_, millis: u64| async move {
            tokio::time::sleep(Duration::from_millis(millis)).await;
            Ok(())
        })?,
    )?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_millisecond_and_second_timestamps() {
        let lua = Lua::new();
        let module = create_module(&lua).unwrap();
        let now = module
            .get::<LuaFunction>("now")
            .unwrap()
            .call::<i64>(())
            .unwrap();
        let unix = module
            .get::<LuaFunction>("unix")
            .unwrap()
            .call::<u64>(())
            .unwrap();

        assert!(unix > 1_000_000_000);
        let now_seconds = u64::try_from(now).unwrap() / 1000;
        assert!(now_seconds.abs_diff(unix) <= 1);
    }
}
