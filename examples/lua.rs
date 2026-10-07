use mlua::prelude::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> LuaResult<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let script = args
        .first()
        .ok_or_else(|| LuaError::runtime("expected a Lua script"))?;
    let lua = Lua::new();
    let runtime = lua.create_table()?;
    runtime.set("json", mlua_extra::json::create_module(&lua)?)?;
    runtime.set("url", mlua_extra::url::create_module(&lua)?)?;
    runtime.set("error", mlua_extra::error::create_module(&lua)?)?;
    let client = reqwest::Client::new();
    runtime.set(
        "http",
        mlua_extra::http::LuaHttpClient::new(client.clone()).with_stream_client(client),
    )?;
    lua.globals().set("runtime", runtime)?;
    lua.globals().set(
        "arg",
        lua.create_sequence_from(args.iter().skip(1).map(String::as_str))?,
    )?;
    let source = std::fs::read(script).map_err(LuaError::external)?;
    lua.load(&source).set_name(script).exec_async().await
}
