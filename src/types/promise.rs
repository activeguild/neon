use std::ptr;
#[cfg(feature = "napi-6")]
use std::sync::Arc;

use neon_runtime::no_panic::FailureBoundary;
#[cfg(feature = "napi-6")]
use neon_runtime::tsfn::ThreadsafeFunction;
use neon_runtime::{napi, raw};

use crate::context::{internal::Env, Context};
use crate::handle::{internal::TransparentNoCopyWrapper, Managed};
#[cfg(feature = "napi-6")]
use crate::lifecycle::{DropData, InstanceData};
use crate::result::JsResult;
use crate::types::{private::ValueInternal, Handle, Object, Value};
#[cfg(feature = "channel-api")]
use crate::{
    context::TaskContext,
    event::{Channel, JoinHandle, SendError},
};

#[cfg(feature = "futures")]
use {
    crate::context::FunctionContext,
    crate::event::JoinError,
    crate::result::NeonResult,
    crate::types::{JsFunction, JsValue},
    futures_rs::channel::oneshot,
    std::future::Future,
    std::pin::Pin,
    std::sync::Mutex,
    std::task::{self, Poll},
};

const BOUNDARY: FailureBoundary = FailureBoundary {
    both: "A panic and exception occurred while resolving a `neon::types::Deferred`",
    exception: "An exception occurred while resolving a `neon::types::Deferred`",
    panic: "A panic occurred while resolving a `neon::types::Deferred`",
};

#[cfg_attr(docsrs, doc(cfg(feature = "promise-api")))]
#[derive(Debug)]
#[repr(transparent)]
/// The JavaScript [`Promise`](https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Promise) value.
///
/// [`JsPromise`] may be constructed with [`Context::promise`].
pub struct JsPromise(raw::Local);

impl JsPromise {
    pub(crate) fn new<'a, C: Context<'a>>(cx: &mut C) -> (Deferred, Handle<'a, Self>) {
        let (deferred, promise) = unsafe { napi::promise::create(cx.env().to_raw()) };
        let deferred = Deferred {
            internal: Some(NodeApiDeferred(deferred)),
            #[cfg(feature = "napi-6")]
            drop_queue: InstanceData::drop_queue(cx),
        };

        (deferred, Handle::new_internal(JsPromise(promise)))
    }

    #[cfg(feature = "futures")]
    pub fn to_future<'a, O, C, F>(&self, cx: &mut C, f: F) -> NeonResult<JsFuture<O>>
    where
        O: Send + 'static,
        C: Context<'a>,
        for<'cx> F: FnOnce(
                &mut FunctionContext<'cx>,
                Result<Handle<'cx, JsValue>, Handle<'cx, JsValue>>,
            ) -> NeonResult<O>
            + Send
            + 'static,
    {
        let then = self.get::<JsFunction, _, _>(cx, "then")?;
        let catch = self.get::<JsFunction, _, _>(cx, "catch")?;

        let (tx, rx) = oneshot::channel();
        let state = Arc::new(Mutex::new(Some((f, tx))));

        let resolve = JsFunction::new(cx, {
            let state = state.clone();

            move |mut cx| {
                if let Ok(mut guard) = state.lock() {
                    if let Some((f, tx)) = guard.take() {
                        let v = cx.argument::<JsValue>(0)?;

                        let _ = tx.send(f(&mut cx, Ok(v)));
                    }
                };

                Ok(cx.undefined())
            }
        })?;

        let reject = JsFunction::new(cx, {
            let state = state.clone();

            move |mut cx| {
                if let Ok(mut guard) = state.lock() {
                    if let Some((f, tx)) = guard.take() {
                        let v = cx.argument::<JsValue>(0)?;

                        let _ = tx.send(f(&mut cx, Err(v)));
                    }
                };

                Ok(cx.undefined())
            }
        })?;

        then.exec(cx, Handle::new_internal(Self(self.0)), [resolve.upcast()])?;
        catch.exec(cx, Handle::new_internal(Self(self.0)), [reject.upcast()])?;

        Ok(JsFuture { rx })
    }
}

#[cfg(feature = "futures")]
/// A [`Future`](std::future::Future) created from a [`JsPromise`].
///
/// Unlike typical `Future`, `JsFuture` are eagerly executed because they
/// are backed by a `Promise`.
pub struct JsFuture<T> {
    rx: oneshot::Receiver<NeonResult<T>>,
}

#[cfg(feature = "futures")]
impl<T> Future for JsFuture<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Self::Output> {
        JoinError::map_poll(&mut self.rx, cx)
    }
}

unsafe impl TransparentNoCopyWrapper for JsPromise {
    type Inner = raw::Local;

    fn into_inner(self) -> Self::Inner {
        self.0
    }
}

impl Managed for JsPromise {
    fn to_raw(&self) -> raw::Local {
        self.0
    }

    fn from_raw(_env: Env, h: raw::Local) -> Self {
        Self(h)
    }
}

impl ValueInternal for JsPromise {
    fn name() -> String {
        "Promise".to_string()
    }

    fn is_typeof<Other: Value>(env: Env, other: &Other) -> bool {
        unsafe { neon_runtime::tag::is_promise(env.to_raw(), other.to_raw()) }
    }
}

impl Value for JsPromise {}

impl Object for JsPromise {}

#[cfg_attr(docsrs, doc(cfg(feature = "promise-api")))]
/// [`Deferred`] is a handle that can be used to resolve or reject a [`JsPromise`]
///
/// It is recommended to settle a [`Deferred`] with [`Deferred::settle_with`] to ensure
/// exceptions are caught.
///
/// On Node-API versions less than 6, dropping a [`Deferred`] without settling will
/// cause a panic. On Node-API 6+, the associated [`JsPromise`] will be automatically
/// rejected.
pub struct Deferred {
    internal: Option<NodeApiDeferred>,
    #[cfg(feature = "napi-6")]
    drop_queue: Arc<ThreadsafeFunction<DropData>>,
}

impl Deferred {
    /// Resolve a [`JsPromise`] with a JavaScript value
    pub fn resolve<'a, V, C>(self, cx: &mut C, value: Handle<V>)
    where
        V: Value,
        C: Context<'a>,
    {
        unsafe {
            napi::promise::resolve(cx.env().to_raw(), self.into_inner(), value.to_raw());
        }
    }

    /// Reject a [`JsPromise`] with a JavaScript value
    pub fn reject<'a, V, C>(self, cx: &mut C, value: Handle<V>)
    where
        V: Value,
        C: Context<'a>,
    {
        unsafe {
            napi::promise::reject(cx.env().to_raw(), self.into_inner(), value.to_raw());
        }
    }

    #[cfg_attr(docsrs, doc(cfg(feature = "channel-api")))]
    #[cfg(feature = "channel-api")]
    /// Settle the [`JsPromise`] by sending a closure across a [`Channel`][`crate::event::Channel`]
    /// to be executed on the main JavaScript thread.
    ///
    /// Usage is identical to [`Deferred::settle_with`].
    ///
    /// Returns a [`SendError`][crate::event::SendError] if sending the closure to the main JavaScript thread fails.
    /// See [`Channel::try_send`][crate::event::Channel::try_send] for more details.
    pub fn try_settle_with<V, F>(
        self,
        channel: &Channel,
        complete: F,
    ) -> Result<JoinHandle<()>, SendError>
    where
        V: Value,
        F: FnOnce(TaskContext) -> JsResult<V> + Send + 'static,
    {
        channel.try_send(move |cx| {
            self.try_catch_settle(cx, move |cx| complete(cx));
            Ok(())
        })
    }

    #[cfg_attr(docsrs, doc(cfg(feature = "channel-api")))]
    #[cfg(feature = "channel-api")]
    /// Settle the [`JsPromise`] by sending a closure across a [`Channel`][crate::event::Channel]
    /// to be executed on the main JavaScript thread.
    ///
    /// Panics if there is a libuv error.
    ///
    /// ```
    /// # #[cfg(feature = "channel-api")] {
    /// # use neon::prelude::*;
    /// # fn example(mut cx: FunctionContext) -> JsResult<JsPromise> {
    /// let channel = cx.channel();
    /// let (deferred, promise) = cx.promise();
    ///
    /// deferred.settle_with(&channel, move |cx| Ok(cx.number(42)));
    ///
    /// # Ok(promise)
    /// # }
    /// # }
    /// ```
    pub fn settle_with<V, F>(self, channel: &Channel, complete: F) -> JoinHandle<()>
    where
        V: Value,
        F: FnOnce(TaskContext) -> JsResult<V> + Send + 'static,
    {
        self.try_settle_with(channel, complete).unwrap()
    }

    pub(crate) fn try_catch_settle<'a, C, V, F>(self, cx: C, f: F)
    where
        C: Context<'a>,
        V: Value,
        F: FnOnce(C) -> JsResult<'a, V>,
    {
        unsafe {
            BOUNDARY.catch_failure(
                cx.env().to_raw(),
                Some(self.into_inner()),
                move |_| match f(cx) {
                    Ok(value) => value.to_raw(),
                    Err(_) => ptr::null_mut(),
                },
            );
        }
    }

    pub(crate) fn into_inner(mut self) -> napi::Deferred {
        self.internal.take().unwrap().0
    }
}

#[repr(transparent)]
pub(crate) struct NodeApiDeferred(napi::Deferred);

unsafe impl Send for NodeApiDeferred {}

#[cfg(feature = "napi-6")]
impl NodeApiDeferred {
    pub(crate) unsafe fn leaked(self, env: raw::Env) {
        napi::promise::reject_err_message(
            env,
            self.0,
            "`neon::types::Deferred` was dropped without being settled",
        );
    }
}

impl Drop for Deferred {
    #[cfg(not(feature = "napi-6"))]
    fn drop(&mut self) {
        // If `None`, the `Deferred` has already been settled
        if self.internal.is_none() {
            return;
        }

        // Destructors are called during stack unwinding, prevent a double
        // panic and instead prefer to leak.
        if std::thread::panicking() {
            eprintln!("Warning: neon::types::JsPromise leaked during a panic");
            return;
        }

        // Only panic if the event loop is still running
        if let Ok(true) = crate::context::internal::IS_RUNNING.try_with(|v| *v.borrow()) {
            panic!("Must settle a `neon::types::JsPromise` with `neon::types::Deferred`");
        }
    }

    #[cfg(feature = "napi-6")]
    fn drop(&mut self) {
        // If `None`, the `Deferred` has already been settled
        if let Some(internal) = self.internal.take() {
            let _ = self.drop_queue.call(DropData::Deferred(internal), None);
        }
    }
}
