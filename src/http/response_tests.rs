use super::*;
use reqwest::{StatusCode, Version};
use std::{
    future::{poll_fn, Future},
    io::{Read, Write},
    net::TcpListener,
    sync::mpsc,
    task::Poll,
    time::Duration,
};

const URL: &str = "https://example.invalid/eapi/a%2Fb?key=a%2Fb";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Marker(u8);

fn response(body: &[u8], options: BodyOptions) -> LuaResponse {
    let mut response = http::Response::builder()
        .status(StatusCode::CREATED)
        .version(Version::HTTP_2)
        .url(reqwest::Url::parse(URL).unwrap())
        .header(CONTENT_LENGTH, body.len())
        .header(TRANSFER_ENCODING, "chunked")
        .header("x-repeated", "first")
        .header("x-repeated", "second")
        .header("set-cookie", "first=one")
        .header("set-cookie", "second=two")
        .body(body.to_vec())
        .unwrap();
    response.extensions_mut().insert(Marker(7));
    LuaResponse::with_options(response.into(), options)
}

fn lua() -> Lua {
    let lua = Lua::new();
    lua.globals()
        .set("errors", crate::error::create_module(&lua).unwrap())
        .unwrap();
    lua.globals()
        .set("J", crate::json::strict::create_module(&lua).unwrap())
        .unwrap();
    lua
}

fn install(lua: &Lua, body: &[u8], options: BodyOptions) -> LuaAnyUserData {
    let response = lua.create_userdata(response(body, options)).unwrap();
    lua.globals().set("rsp", response.clone()).unwrap();
    response
}

struct Server {
    release: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.release.take().unwrap().send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

async fn wire_response(body: Option<&'static [u8]>) -> (LuaResponse, Server) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/body", listener.local_addr().unwrap());
    let (release, wait) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 2048];
        stream.read(&mut request).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\n")
            .unwrap();
        stream.flush().unwrap();
        if let Some(body) = body {
            stream.write_all(body).unwrap();
        } else {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        }
    });
    let response = reqwest::Client::new().get(url).send().await.unwrap();
    (
        LuaResponse::from_response(response),
        Server {
            release: Some(release),
            thread: Some(thread),
        },
    )
}

#[tokio::test(flavor = "current_thread")]
async fn parts_preserve_metadata_and_transfer_the_real_replacement() {
    let lua = lua();
    let original = install(&lua, b"\0cipher", BodyOptions::default());
    lua.globals().set("url", URL).unwrap();
    lua.load(
        r#"
        local parts, bytes = rsp:read_body()
        assert(type(parts) == 'userdata' and bytes == '\0cipher')
        assert(rsp:body_state() == 'consumed' and rsp:status() == 201 and rsp:url() == url)
        assert(rsp:headers()['content-length'] == '7')
        assert(#rsp:header_values('x-repeated') == 2)
        assert(not pcall(function() parts.headers = {} end))
        rebuilt = parts:with_body('中\0body')
        assert(not rawequal(rsp, rebuilt) and rebuilt:body_state() == 'available')
        assert(rebuilt:headers()['content-length'] == tostring(#'中\0body'))
        assert(rebuilt:headers()['transfer-encoding'] == nil)
        assert(#rebuilt:header_values('set-cookie') == 2)
        local ok, err = pcall(function() parts:with_body('again') end)
        assert(not ok and errors.inspect(err).code == 'consumed')
        rsp:close(); rsp:close()
        assert(rsp:body_state() == 'consumed' and rebuilt:body_state() == 'available')
        "#,
    )
    .exec_async()
    .await
    .unwrap();

    let rebuilt: LuaAnyUserData = lua.globals().get("rebuilt").unwrap();
    let transferred = rebuilt
        .borrow_mut::<LuaResponse>()
        .unwrap()
        .take_response()
        .unwrap();
    assert_eq!(transferred.url().as_str(), URL);
    assert_eq!(transferred.status(), StatusCode::CREATED);
    assert_eq!(transferred.version(), Version::HTTP_2);
    assert_eq!(transferred.extensions().get::<Marker>(), Some(&Marker(7)));
    assert_eq!(
        transferred.headers().get_all("x-repeated").iter().count(),
        2
    );
    assert_eq!(
        transferred.headers().get_all("set-cookie").iter().count(),
        2
    );
    assert_eq!(transferred.headers()[CONTENT_LENGTH], "8");
    assert!(!transferred.headers().contains_key(TRANSFER_ENCODING));
    assert_eq!(
        transferred.bytes().await.unwrap().as_ref(),
        "中\0body".as_bytes()
    );
    assert_eq!(
        original.borrow::<LuaResponse>().unwrap().url().as_str(),
        URL
    );
    assert_eq!(
        original.borrow::<LuaResponse>().unwrap().body_state(),
        "consumed"
    );
    assert!(original
        .borrow_mut::<LuaResponse>()
        .unwrap()
        .take_response()
        .is_err());
    assert_eq!(
        rebuilt.borrow::<LuaResponse>().unwrap().body_state(),
        "transferred"
    );
    assert!(rebuilt
        .borrow_mut::<LuaResponse>()
        .unwrap()
        .take_response()
        .is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn parts_retain_json_codec_and_tightened_limit_for_one_final_read() {
    let lua = lua();
    install(
        &lua,
        b"cipher",
        BodyOptions {
            strict: true,
            limit: Some(256),
        },
    );
    lua.load(
        r#"
        local ok, err = pcall(function() rsp:read_body{codec='legacy'} end)
        assert(not ok and errors.inspect(err).code == 'invalid_argument')
        assert(rsp:body_state() == 'available')
        local parts, bytes = rsp:read_body{max_bytes=128}
        assert(bytes == 'cipher')
        rebuilt = parts:with_body('{"code":200,"xs":[null],"n":9223372036854775807}')
        local value = rebuilt:json()
        assert(value.code == 200 and value.xs[1] == J.null and value.n == math.maxinteger)
        assert(rebuilt:body_state() == 'consumed' and rsp:body_state() == 'consumed')
        local ok, err = pcall(function() rebuilt:text() end)
        assert(not ok and errors.inspect(err).code == 'consumed')
        rebuilt:close(); rsp:close()
        "#,
    )
    .exec_async()
    .await
    .unwrap();
    let rebuilt: LuaAnyUserData = lua.globals().get("rebuilt").unwrap();
    let rebuilt = rebuilt.borrow::<LuaResponse>().unwrap();
    assert!(rebuilt.options.strict);
    assert_eq!(rebuilt.options.limit, Some(128));
}

#[tokio::test(flavor = "current_thread")]
async fn parts_validation_errors_preserve_the_token_and_limits_cannot_be_widened() {
    let lua = lua();
    install(
        &lua,
        b"a",
        BodyOptions {
            strict: false,
            limit: Some(32),
        },
    );
    lua.load(
        r#"
        local parts = rsp:read_body{max_bytes=4}
        for _, invalid in ipairs({1, false, {}}) do
            local ok, err = pcall(function() parts:with_body(invalid) end)
            assert(not ok and errors.inspect(err).code == 'invalid_argument')
        end
        local ok, err = pcall(function() parts:with_body(nil) end)
        assert(not ok and errors.inspect(err).code == 'invalid_argument')
        local ok, err = pcall(function() parts:with_body('large') end)
        assert(not ok and errors.inspect(err).code == 'limit')
        local rebuilt = parts:with_body('body')
        local next_parts, bytes = rebuilt:read_body{max_bytes=100}
        assert(bytes == 'body')
        local ok, err = pcall(function() next_parts:with_body('large') end)
        assert(not ok and errors.inspect(err).code == 'limit')
        local last = next_parts:with_body('abc')
        last:close(); last:close()
        assert(last:body_state() == 'closed' and rsp:body_state() == 'consumed')
        local ok, err = pcall(function() last:read_body() end)
        assert(not ok and errors.inspect(err).code == 'consumed')
        "#,
    )
    .exec_async()
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn read_body_enforces_input_limit_and_never_revives_unavailable_responses() {
    let lua = lua();
    install(
        &lua,
        b"large",
        BodyOptions {
            strict: false,
            limit: Some(4),
        },
    );
    lua.load(
        r#"
        local ok, err = pcall(function() rsp:read_body{max_bytes=100} end)
        assert(not ok and errors.inspect(err).code == 'limit')
        assert(rsp:body_state() == 'consumed')
        local ok, err = pcall(function() rsp:read_body() end)
        assert(not ok and errors.inspect(err).code == 'consumed')
        "#,
    )
    .exec_async()
    .await
    .unwrap();

    let closed = install(&lua, b"a", BodyOptions::default());
    closed.borrow_mut::<LuaResponse>().unwrap().close().unwrap();
    lua.load(
        r#"
        local ok, err = pcall(function() rsp:read_body() end)
        assert(not ok and errors.inspect(err).code == 'consumed' and rsp:body_state() == 'closed')
    "#,
    )
    .exec_async()
    .await
    .unwrap();

    let transferred = install(&lua, b"a", BodyOptions::default());
    drop(
        transferred
            .borrow_mut::<LuaResponse>()
            .unwrap()
            .take_response()
            .unwrap(),
    );
    lua.load(r#"
        local ok, err = pcall(function() rsp:read_body() end)
        assert(not ok and errors.inspect(err).code == 'consumed' and rsp:body_state() == 'transferred')
    "#).exec_async().await.unwrap();

    install(&lua, b"v1", BodyOptions::default());
    lua.load(
        r#"
        assert(rsp:bytes() == 'v1')
        local ok, err = pcall(function() rsp:read_body() end)
        assert(not ok and errors.inspect(err).code == 'consumed' and rsp:body_state() == 'consumed')
    "#,
    )
    .exec_async()
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn read_body_transport_error_and_read_timeout_consume_the_response() {
    let lua = lua();
    let (response, _server) = wire_response(Some(b"ab")).await;
    lua.globals().set("rsp", response).unwrap();
    lua.load(
        r#"
        local ok, err = pcall(function() rsp:read_body() end)
        assert(not ok and errors.inspect(err).code == 'transport')
        assert(rsp:body_state() == 'consumed' and rsp:status() == 200)
        rsp:close()
    "#,
    )
    .exec_async()
    .await
    .unwrap();

    let (response, _server) = wire_response(None).await;
    lua.globals().set("rsp", response).unwrap();
    lua.load(
        r#"
        local ok, err = pcall(function() rsp:read_body{timeout_ms=5} end)
        assert(not ok and errors.inspect(err).code == 'timeout')
        assert(rsp:body_state() == 'consumed')
        rsp:close()
    "#,
    )
    .exec_async()
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn read_body_is_busy_until_read_completion_or_public_coroutine_cancellation() {
    let lua = lua();
    let (response, _server) = wire_response(None).await;
    let response = lua.create_userdata(response).unwrap();
    lua.globals().set("rsp", response.clone()).unwrap();
    let thread = lua
        .create_thread(lua.load("return rsp:read_body()").into_function().unwrap())
        .unwrap();
    {
        let mut call = Box::pin(thread.clone().into_async::<LuaMultiValue>(()));
        poll_fn(|cx| {
            assert!(call.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
        assert_eq!(
            response.borrow::<LuaResponse>().unwrap().body_state(),
            "reading"
        );
        lua.load(
            r#"
            for _, method in ipairs({'read_body', 'bytes', 'close'}) do
                local ok, err = pcall(function() return rsp[method](rsp) end)
                assert(not ok and errors.inspect(err).code == 'busy', method)
            end
            assert(rsp:status() == 200)
        "#,
        )
        .exec_async()
        .await
        .unwrap();
    }
    thread
        .reset(lua.create_function(|_, ()| Ok(())).unwrap())
        .unwrap();
    // Lua GC finalizes suspended callback userdata after the coroutine is reset.
    lua.gc_collect().unwrap();
    assert_eq!(
        response.borrow::<LuaResponse>().unwrap().body_state(),
        "consumed"
    );
    response
        .borrow_mut::<LuaResponse>()
        .unwrap()
        .close()
        .unwrap();
}
