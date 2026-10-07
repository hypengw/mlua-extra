use mlua::prelude::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn create_module(lua: &Lua) -> LuaResult<LuaTable> {
    let t = lua.create_table()?;
    t.set(
        "parse_rfc3339",
        lua.create_function(|_, value: String| {
            chrono::DateTime::parse_from_rfc3339(&value)
                .map(|v| v.timestamp_millis())
                .map_err(LuaError::external)
        })?,
    )?;
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
    fn parses_rfc3339_in_milliseconds() {
        let lua = Lua::new();
        let parse = create_module(&lua)
            .unwrap()
            .get::<LuaFunction>("parse_rfc3339")
            .unwrap();
        assert_eq!(
            parse.call::<i64>("1970-01-01T01:00:01.123+01:00").unwrap(),
            1123
        );
        assert_eq!(parse.call::<i64>("1969-12-31T23:59:59Z").unwrap(), -1000);
        assert!(parse.call::<i64>("not a date").is_err());
    }
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
