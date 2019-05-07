// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::{
    result,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use futures::compat::Compat01As03;
use futures::prelude::*;
use log::*;
use tokio_timer::timer::Handle;

use crate::{
    compat::{loop_fn, Loop},
    rpc::util::GLOBAL_TIMER_HANDLE,
    Result,
};

pub const RECONNECT_INTERVAL_SEC: u64 = 1; // 1s

/// The context of sending requets.
pub struct Request<Resp, Func, Cli, Reconnect> {
    reconnect_count: usize,
    request_sent: usize,

    client: Arc<RwLock<Cli>>,
    timer: Handle,

    resp: Option<Result<Resp>>,
    func: Func,
    reconnect: Reconnect,
}

const MAX_REQUEST_COUNT: usize = 3;

impl<Resp, Func, Cli, Reconnect, RespFuture> Request<Resp, Func, Cli, Reconnect>
where
    Resp: Send + 'static,
    Func: FnMut(&RwLock<Cli>) -> RespFuture + Send + 'static,
    Cli: Send + Sync + 'static,
    Reconnect: FnMut(&Arc<RwLock<Cli>>, u64) -> Result<()> + Send + 'static,
    RespFuture: Future<Output = Result<Resp>> + Send + 'static,
{
    pub fn new(func: Func, client: Arc<RwLock<Cli>>, reconnect: Reconnect, retry: usize) -> Self {
        Request {
            reconnect_count: retry,
            request_sent: 0,
            client,
            timer: GLOBAL_TIMER_HANDLE.clone(),
            resp: None,
            func,
            reconnect,
        }
    }

    fn reconnect_if_needed(
        mut self,
    ) -> impl Future<Output = std::result::Result<Self, Self>> + Send {
        debug!("reconnect remains: {}", self.reconnect_count);

        if self.request_sent < MAX_REQUEST_COUNT {
            return future::Either::Left(future::ok(self));
        }

        // Updating client.
        self.reconnect_count -= 1;

        // FIXME: should not block the core.
        match (self.reconnect)(&self.client, RECONNECT_INTERVAL_SEC) {
            Ok(_) => {
                self.request_sent = 0;
                future::Either::Left(future::ok(self))
            }
            Err(_) => future::Either::Right(
                Compat01As03::new(
                    self.timer
                        .delay(Instant::now() + Duration::from_secs(RECONNECT_INTERVAL_SEC)),
                )
                .map(|_| Err(self)),
            ),
        }
    }

    fn send_and_receive(mut self) -> impl Future<Output = std::result::Result<Self, Self>> + Send {
        self.request_sent += 1;
        debug!("request sent: {}", self.request_sent);

        future::ok(self).and_then(|mut ctx| {
            let req = (ctx.func)(&ctx.client);
            req.map(|resp| match resp {
                Ok(resp) => {
                    ctx.resp = Some(Ok(resp));
                    Ok(ctx)
                }
                Err(e) => {
                    error!("request failed: {:?}", e);
                    Err(ctx)
                }
            })
        })
    }

    fn break_or_continue(ctx: result::Result<Self, Self>) -> Result<Loop<Self, Self>> {
        let ctx = match ctx {
            Ok(ctx) | Err(ctx) => ctx,
        };
        let done = ctx.reconnect_count == 0 || ctx.resp.is_some();
        if done {
            Ok(Loop::Break(ctx))
        } else {
            Ok(Loop::Continue(ctx))
        }
    }

    fn post_loop(self) -> Result<Resp> {
        self.resp
            .unwrap_or_else(|| Err(internal_err!("fail to request")))
    }

    /// Returns a Future, it is resolves once a future returned by the closure
    /// is resolved successfully, otherwise it repeats `retry` times.
    pub fn execute(self) -> impl Future<Output = Result<Resp>> {
        let ctx = self;
        loop_fn(ctx, |ctx| {
            ctx.reconnect_if_needed()
                .and_then(Self::send_and_receive)
                .map(Self::break_or_continue)
        })
        .and_then(|r| future::ready(r.post_loop()))
    }
}
