use super::{http_error, take_request, LuaResponse};
use mlua::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use tokio::task::{Id, JoinSet};

pub struct LuaBatch {
    state: Arc<Mutex<State>>,
}
struct State {
    tasks: JoinSet<LuaResult<Payload>>,
    keys: HashMap<Id, String>,
    seen: HashSet<String>,
    limit: usize,
    response_mode: bool,
    sealed: bool,
    cancelled: bool,
    waiting: bool,
    sequence: usize,
    waiter_done: Arc<tokio::sync::Notify>,
}
enum Payload {
    Response(LuaResponse),
    Bytes(Vec<u8>),
}

impl LuaBatch {
    pub fn new(options: Option<LuaTable>) -> LuaResult<Self> {
        let response_mode = options.is_some();
        let limit = if let Some(options) = options {
            if options.get::<String>("results")? != "response" {
                return Err(http_error(
                    "invalid_argument",
                    "batch results must be response",
                ));
            }
            options.get::<Option<usize>>("limit")?.unwrap_or(12)
        } else {
            usize::MAX
        };
        if limit == 0 || (response_mode && limit > 1024) {
            return Err(http_error(
                "invalid_argument",
                "batch limit must be between 1 and 1024",
            ));
        }
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                tasks: JoinSet::new(),
                keys: HashMap::new(),
                seen: HashSet::new(),
                limit,
                response_mode,
                sealed: false,
                cancelled: false,
                waiting: false,
                sequence: 0,
                waiter_done: Arc::new(tokio::sync::Notify::new()),
            })),
        })
    }
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.cancelled = true;
        state.tasks.abort_all();
    }
}
impl Drop for LuaBatch {
    fn drop(&mut self) {
        self.cancel();
    }
}

struct WaitGuard {
    state: Arc<Mutex<State>>,
    finished: bool,
}
impl Drop for WaitGuard {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();
        state.waiting = false;
        state.waiter_done.notify_waiters();
        if !self.finished {
            state.cancelled = true;
            state.tasks.abort_all();
        }
    }
}

impl LuaUserData for LuaBatch {
    fn add_methods<M: LuaUserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("add", |_, this, args: LuaMultiValue| {
            let mut state = this.state.lock().unwrap();
            if state.cancelled {
                return Err(http_error("cancelled", "batch was cancelled"));
            }
            if state.sealed {
                return Err(http_error("invalid_state", "batch is sealed"));
            }
            if state.keys.len() >= state.limit {
                return Err(http_error("capacity", "batch is full"));
            }
            let (key, ud) = if state.response_mode {
                if args.len() != 2 {
                    return Err(http_error("invalid_argument", "expected key and request"));
                }
                let Some(LuaValue::String(key)) = args.front() else {
                    return Err(http_error("invalid_argument", "batch key must be a string"));
                };
                let Some(LuaValue::UserData(ud)) = args.get(1) else {
                    return Err(http_error("invalid_argument", "expected request"));
                };
                (key.to_str()?.to_owned(), ud.clone())
            } else {
                let Some(LuaValue::UserData(ud)) = args.front() else {
                    return Err(http_error("invalid_argument", "expected request builder"));
                };
                (state.sequence.to_string(), ud.clone())
            };
            if state.seen.contains(&key) {
                return Err(http_error("duplicate_key", "batch key already used"));
            }
            if state.response_mode && state.seen.len() >= 100_000 {
                return Err(http_error("limit", "batch key limit exceeded"));
            }
            tokio::runtime::Handle::try_current()
                .map_err(|_| http_error("invalid_state", "batch requires a Tokio runtime"))?;
            let request = take_request(&ud)?;
            let response_mode = state.response_mode;
            let handle = state.tasks.spawn(async move {
                let mut response = request.send().await?;
                if response_mode {
                    Ok(Payload::Response(response))
                } else {
                    Ok(Payload::Bytes(
                        response
                            .take_response()?
                            .bytes()
                            .await
                            .map_err(super::transport_error)?
                            .to_vec(),
                    ))
                }
            });
            state.keys.insert(handle.id(), key.clone());
            if response_mode {
                state.seen.insert(key);
            }
            state.sequence += 1;
            Ok(state.keys.len())
        });
        methods.add_method("add_rsp", |_, this, ud: LuaAnyUserData| {
            let mut state = this.state.lock().unwrap();
            if state.response_mode {
                return Err(http_error(
                    "invalid_state",
                    "add_rsp is only available in legacy bytes mode",
                ));
            }
            if state.cancelled || state.sealed {
                return Err(http_error("invalid_state", "batch is closed"));
            }
            tokio::runtime::Handle::try_current()
                .map_err(|_| http_error("invalid_state", "batch requires a Tokio runtime"))?;
            let response = ud.borrow_mut::<LuaResponse>()?.take_response()?;
            let handle = state.tasks.spawn(async move {
                Ok(Payload::Bytes(
                    response
                        .bytes()
                        .await
                        .map_err(super::transport_error)?
                        .to_vec(),
                ))
            });
            let key = state.sequence.to_string();
            state.keys.insert(handle.id(), key);
            state.sequence += 1;
            Ok(state.keys.len())
        });
        methods.add_method("seal", |_, this, ()| {
            this.state.lock().unwrap().sealed = true;
            Ok(())
        });
        methods.add_method("cancel", |_, this, ()| {
            this.cancel();
            Ok(())
        });
        methods.add_async_function("close", |_, ud: LuaAnyUserData| async move {
            let state = ud.borrow::<LuaBatch>()?.state.clone();
            let notify = {
                let mut state = state.lock().unwrap();
                state.cancelled = true;
                state.tasks.abort_all();
                state.waiter_done.clone()
            };
            loop {
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if !state.lock().unwrap().waiting {
                    break;
                }
                notified.await;
            }
            let mut tasks = {
                let mut state = state.lock().unwrap();
                state.keys.clear();
                std::mem::replace(&mut state.tasks, JoinSet::new())
            };
            tasks.shutdown().await;
            Ok(())
        });
        methods.add_async_function("wait_one", |lua, ud: LuaAnyUserData| async move {
            let state = ud.borrow::<LuaBatch>()?.state.clone();
            {
                let mut state = state.lock().unwrap();
                if state.waiting {
                    return Err(http_error("busy", "batch already has a waiter"));
                }
                if state.cancelled {
                    return Err(http_error("cancelled", "batch was cancelled"));
                }
                if state.keys.is_empty() {
                    return if state.sealed || !state.response_mode {
                        Ok(LuaValue::Nil)
                    } else {
                        Err(http_error("invalid_state", "unsealed batch is empty"))
                    };
                }
                state.waiting = true;
            }
            let mut guard = WaitGuard {
                state: state.clone(),
                finished: false,
            };
            let result =
                std::future::poll_fn(|cx| state.lock().unwrap().tasks.poll_join_next_with_id(cx))
                    .await;
            let (key, result, response_mode) = {
                let mut state = state.lock().unwrap();
                if state.cancelled {
                    return Err(http_error("cancelled", "batch was cancelled"));
                }
                let (id, result) = match result {
                    Some(Ok((id, result))) => (id, result),
                    Some(Err(error)) => (error.id(), Err(http_error("task", error.to_string()))),
                    None => return Err(http_error("invalid_state", "batch task disappeared")),
                };
                let key = state
                    .keys
                    .remove(&id)
                    .ok_or_else(|| http_error("invalid_state", "batch task has no key"))?;
                (key, result, state.response_mode)
            };
            guard.finished = true;
            if !response_mode {
                return match result? {
                    Payload::Bytes(bytes) => Ok(LuaValue::String(lua.create_string(bytes)?)),
                    _ => unreachable!(),
                };
            }
            let out = lua.create_table()?;
            out.set("key", key)?;
            match result {
                Ok(Payload::Response(response)) => out.set("response", response)?,
                Err(error) => out.set("error", error)?,
                _ => unreachable!(),
            }
            Ok(LuaValue::Table(out))
        });
    }
}
