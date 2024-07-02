// Copyright 2023-2024 The NativeLink Authors. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::borrow::{Borrow, Cow};
use std::cell::UnsafeCell;
use std::io::SeekFrom;
use std::iter;
use std::ops::{Bound, Deref, DerefMut};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::future::{try_join, Future};
use nativelink_config::stores::ErrorCode;
use nativelink_error::{Code, Error, ResultExt};
use pin_project_lite::pin_project;
use prometheus_client::registry::Registry;
use rand::rngs::OsRng;
use rand::Rng;
use tokio::io::AsyncSeekExt;
use tonic::async_trait;
use tracing::{event, Level};

use crate::buf_channel::{make_buf_channel_pair, DropCloserReadHalf, DropCloserWriteHalf};
use crate::fs::{self, open_file, ResumeableFileSlot};
use crate::health_utils::{HealthRegistryBuilder, HealthStatus, HealthStatusIndicator};
use crate::store_trait::{
    StoreDriver, StoreKey, StoreOptimizations, StoreSubscription, UploadSizeInfo,
};

struct ExponentialBackoff<'f, J> {
    jitter_fn: &'f J,
    jitter_amt: f32,

    max_retries: usize,
    num_retries: usize,

    current_delay: Duration,
}

impl<J: JitterFn> Iterator for ExponentialBackoff<'_, J> {
    type Item = Duration;

    fn next(&mut self) -> Option<Self::Item> {
        if self.num_retries >= self.max_retries {
            None
        } else {
            let delay = self.current_delay;
            let jittered_delay = self.jitter_fn.jitter(delay, self.jitter_amt);

            self.current_delay *= 2;
            self.num_retries += 1;

            Some(jittered_delay)
        }
    }
}

impl<'f, J: JitterFn> ExponentialBackoff<'f, J> {
    fn new(config: &nativelink_config::stores::Retry, jitter_fn: &'f J) -> Self {
        ExponentialBackoff {
            jitter_fn,
            jitter_amt: config.jitter,

            max_retries: config.max_retries,
            num_retries: 0,
            current_delay: Duration::from_millis(config.delay as u64),
        }
    }
}

pub trait SleepFn {
    type Fut: Future<Output = ()> + Send;

    fn sleep(&self, duration: Duration) -> Self::Fut;
}

impl<F, Fut> SleepFn for F
where
    F: Fn(Duration) -> Fut,
    Fut: Future<Output = ()> + Send,
{
    type Fut = Fut;

    fn sleep(&self, duration: Duration) -> Self::Fut {
        (self)(duration)
    }
}

struct TokioSleepFn;

impl SleepFn for TokioSleepFn {
    type Fut = tokio::time::Sleep;

    fn sleep(&self, duration: Duration) -> Self::Fut {
        tokio::time::sleep(duration)
    }
}

pub trait JitterFn {
    fn jitter(&self, delay: Duration, jitter_amt: f32) -> Duration;
}

impl<F> JitterFn for F
where
    F: Fn(Duration, f32) -> Duration,
{
    fn jitter(&self, delay: Duration, jitter_amt: f32) -> Duration {
        (self)(delay, jitter_amt)
    }
}

struct DefaultJitterFn;

impl JitterFn for DefaultJitterFn {
    fn jitter(&self, delay: Duration, jitter_amt: f32) -> Duration {
        if jitter_amt == 0. {
            return delay;
        }

        let min = 1. - (jitter_amt / 2.);
        let max = 1. + (jitter_amt / 2.);
        delay.mul_f32(OsRng.gen_range(min..max))
    }
}

struct RetryCell<T: ?Sized>(
    // Invariants:
    // 1. Data must be accessed only inside of a closure argument to [`RetryWrapper::retry`].
    // 2. Data must be accessed only a single time in the same closure argument to [`RetryWrapper::retry`]
    UnsafeCell<T>,
);

impl<'a, T: ?Sized> RetryCell<&'a mut T> {
    #[inline(always)]
    fn new(val: &'a mut T) -> Self {
        Self(UnsafeCell::new(val))
    }
}

impl<T> RetryCell<T> {
    #[inline(always)]
    fn owned(val: T) -> Self {
        Self(UnsafeCell::new(val))
    }
}

impl<T: ?Sized> RetryCell<T> {
    /// Get a mutable reference to the data contained in this [`RetryCell`]
    ///
    /// ## Safety
    ///
    /// It is unsound to use this function to obtain multiple mutable references to the same underlying data. Concretely:
    /// 1. It is unsound to call this method anywhere outside of a closure argument to [`RetryWrapper::retry`].
    /// 2. It is unsound to call this method multiple times in the same closure argument to [`RetryWrapper::retry`]
    ///
    /// More specifically, this method exists because [`RetryWrapper::retry`] takes a [`FnMut`] as an argument,
    /// which in practice is typically a closure. The borrow checker can't follow what happens to a mutable reference
    /// inside of a closure, so it errs on the side of caution -- it assumes that the closure can and will be called
    /// multiple times, from different threads, in parallel, etc. This means that these closures cannot mutate external
    /// state -- what if two different instances of the closure are invoked with the same mutable reference?
    /// That would violate the borrow checker's rules.
    ///
    /// This is an issue for methods like [`has_with_results`](`StoreDriver::has_with_results`), which need to mutate external
    /// state. We know that we won't be mutating the same state from multiple places within a single call to `retry`, so we use
    /// this method to manually enforce the borrow checker's rules.
    // The `mut_from_ref` lint exists for exactly the reasons described above: it's unsound to derive multiple mutable references
    // that point to the same underlying data. We won't be doing that so long as this type's safety contract is upheld, so we
    // disable the lint.
    #[allow(clippy::mut_from_ref)]
    #[inline(always)]
    unsafe fn get(&self) -> &mut T {
        &mut *self.0.get()
    }
}

unsafe impl<T: Sync + Send> Send for RetryCell<T> {}
unsafe impl<C: Sync> Sync for RetryCell<C> {}

struct RetriableReader<'a> {
    inner: RetryCell<&'a mut DropCloserReadHalf>,
    cache: RetryCell<Vec<Bytes>>,
}

impl<'a> RetriableReader<'a> {
    fn new(rx: &'a mut DropCloserReadHalf) -> Self {
        Self {
            inner: RetryCell::new(rx),
            cache: RetryCell::owned(Vec::new()),
        }
    }

    /// Get a [`DropCloserReadHalf`] that is guaranteed to receive all data sent across multiple retries.
    ///
    /// The first item in the tuple returned by this function is a guard; in order to drive the returned reader,
    /// the guard future must be awaited concurrently with other operations (i.e. with [`try_join`]).
    /// Failing to await this future may cause the program to hang, or for data to be lost.
    ///
    /// In general, you should avoid using this method directly and use [`RetryWrapper::retry_with_reader`] instead,
    /// which wraps this function and corrently handles buffered data.
    ///
    /// # Safety
    ///
    /// The same invariants as [`RetryCell`] apply; call this method exactly once per invocation of `retry`, and not
    /// outside of it. Furthermore, the returned future must be awaited within the same invocation.
    //
    // The lifetimes here are a new wrinkle in the already wrinkly invariants, but they basically specify the same thing
    // in a different way: the constraint of `'b: 'a` specifies that the borrow of `&'b self` will always outlive the lifetime
    // of the contained `&'a mut` reader. In reality, the order of lifetimes is different: 'a will always outlive 'b,
    // because 'a is the lifetime of the contained data. The only way these conditions can both be satisfied is
    // if 'a is exactly 'b.
    //
    // The only way for 'a to equal 'b exactly is for the borrow of self and returned future to be dropped at the same time,
    // which will happen if this method is invoked exactly once per `retry`, and the returned future is also run to completion
    // in the same closure. In short: if `RetryCell`'s invariants are upheld.
    async unsafe fn get<'b: 'a>(
        &'b self,
    ) -> (
        impl Future<Output = Result<(), Error>> + 'a,
        DropCloserReadHalf,
    ) {
        // Safety: The invariant of `RetryCell` must be upheld by the caller -- if it is, this is safe.
        let cache = unsafe { self.cache.get() };

        let (mut tx, rx) = make_buf_channel_pair();
        for buf in cache.iter().filter(|buf| !buf.is_empty()).cloned() {
            // Pre-send any buffers that were sent on a previous iteration.
            //
            // Unwrap safety:
            //
            // There are four error conditions where this operation can fail.
            // 1. `tx`'s internal writer has been dropped
            // 2. `buf.len()` cannot be converted to `u64`
            // 3. `buf.len()` is zero
            // 4. `rx`'s internal reader has been dropped
            //
            // We should never hit these conditions because:
            // 1. We just instantiated `tx` and it hasn't gone out of scope.
            // 2. `buf.len()` is a `usize`, which is only ever 32 or 64 bits long. For now!
            // 3. We pre-filter all zero-length buffers
            // 4. We just instantiated `rx` and it hasn't gone out of scope.
            tx.send(buf)
                .await
                .expect("Hit unexpected error condition sending bytes");
        }

        // Retrieve the reader that's actually wired up to the outside world.
        // Safety: If the caller upholds `RetryCell`'s invariants, this is safe.
        let real_rx = unsafe { self.inner.get() };

        let guard = async move { tx.bind_with(real_rx, |chunk| cache.push(chunk)).await };

        (guard, rx)
    }
}

pin_project! {
    #[project_ref = RetryProjectionRef]
    #[project = RetryProjectionMut]
    pub struct Retry<T, S = TokioSleepFn, J = DefaultJitterFn> {
        #[pin]
        inner: T,
        config: nativelink_config::stores::Retry,
        sleep_fn: S,
        jitter_fn: J,
    }
}

impl<T, S, J> Deref for Retry<T, S, J> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T, S, J> DerefMut for Retry<T, S, J> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<T, S, J, U> AsRef<U> for Retry<T, S, J>
where
    U: ?Sized,
    <Retry<T, S, J> as Deref>::Target: AsRef<U>,
{
    #[inline(always)]
    fn as_ref(&self) -> &U {
        self.deref().as_ref()
    }
}

impl<T, S, J, U> AsMut<U> for Retry<T, S, J>
where
    U: ?Sized,
    <Retry<T, S, J> as Deref>::Target: AsMut<U>,
{
    #[inline(always)]
    fn as_mut(&mut self) -> &mut U {
        self.deref_mut().as_mut()
    }
}

pub trait Retriable: Sized {
    fn with_retry(self, config: nativelink_config::stores::Retry) -> Retry<Self> {
        Retry::new(config, self)
    }
}

impl<T> Retriable for T {}

impl<T, S, J> RetryProjectionRef<'_, T, S, J>
where
    T: Unpin,
    S: SleepFn + Send + Sync,
    J: JitterFn + Send + Sync,
{
    async fn retry<F, Fut, Out>(&self, mut f: F) -> Result<Out, Error>
    where
        F: FnMut(Pin<&T>) -> Fut,
        Fut: Future<Output = Result<Out, Error>>,
    {
        #[inline]
        fn should_retry(config: &nativelink_config::stores::Retry, error: &Error) -> bool {
            let code = to_error_code(&error.code);
            config
                .retry_on_errors
                .as_ref()
                .is_some_and(|errors| errors.contains(&code))
                || !(error.code.is_ok() || error.code.is_unrecoverable_error())
        }

        let Self {
            inner,
            config,
            sleep_fn,
            jitter_fn,
        } = self;

        let mut backoffs = ExponentialBackoff::new(config, *jitter_fn);
        let mut attempt = 1;

        loop {
            match (f)(inner.as_ref()).await {
                Ok(t) => return Ok(t),
                Err(error) if should_retry(config, &error) => {
                    if let Some(duration) = backoffs.next() {
                        sleep_fn.sleep(duration).await;
                        attempt += 1;
                    } else {
                        event!(
                            Level::ERROR,
                            ?attempt,
                            ?error,
                            "Not retrying error after max number of retries reached"
                        );
                        return Err(error);
                    }
                }
                Err(error) => {
                    event!(
                        Level::ERROR,
                        ?attempt,
                        ?error,
                        "Not retrying permanent error"
                    );
                    return Err(error);
                }
            }
        }
    }
}

impl<T, S, J> RetryProjectionMut<'_, T, S, J>
where
    T: Unpin,
    S: SleepFn + Send + Sync,
    J: JitterFn + Send + Sync,
{
    async fn retry_mut<F, Fut, Out>(&mut self, mut f: F) -> Result<Out, Error>
    where
        F: FnMut(Pin<&mut T>) -> Fut,
        Fut: Future<Output = Result<Out, Error>>,
    {
        #[inline]
        fn should_retry(config: &nativelink_config::stores::Retry, error: &Error) -> bool {
            let code = to_error_code(&error.code);
            config
                .retry_on_errors
                .as_ref()
                .is_some_and(|errors| errors.contains(&code))
                || !(error.code.is_ok() || error.code.is_unrecoverable_error())
        }

        let Self {
            inner,
            config,
            sleep_fn,
            jitter_fn,
        } = self;

        let mut backoffs = ExponentialBackoff::new(config, *jitter_fn);
        let mut attempt = 1;

        loop {
            match (f)(inner.as_mut()).await {
                Ok(t) => return Ok(t),
                Err(error) if should_retry(config, &error) => {
                    if let Some(duration) = backoffs.next() {
                        sleep_fn.sleep(duration).await;
                        attempt += 1;
                    } else {
                        event!(
                            Level::ERROR,
                            ?attempt,
                            ?error,
                            "Not retrying error after max number of retries reached"
                        );
                        return Err(error);
                    }
                }
                Err(error) => {
                    event!(
                        Level::ERROR,
                        ?attempt,
                        ?error,
                        "Not retrying permanent error"
                    );
                    return Err(error);
                }
            }
        }
    }
}

impl<T, S, J> Retry<T, S, J>
where
    T: Unpin,
    S: SleepFn + Send + Sync,
    J: JitterFn + Send + Sync,
{
    async fn retry_mut<F, Fut, Out>(&mut self, mut f: F) -> Result<Out, Error>
    where
        F: FnMut(&mut T) -> Fut,
        Fut: Future<Output = Result<Out, Error>>,
    {
        #[inline]
        fn should_retry(config: &nativelink_config::stores::Retry, error: &Error) -> bool {
            let code = to_error_code(&error.code);
            config
                .retry_on_errors
                .as_ref()
                .is_some_and(|errors| errors.contains(&code))
                || !(error.code.is_ok() || error.code.is_unrecoverable_error())
        }

        let Self {
            inner,
            config,
            sleep_fn,
            jitter_fn,
        } = self;

        let mut backoffs = ExponentialBackoff::new(config, jitter_fn);
        let mut attempt = 1;

        loop {
            match (f)(inner).await {
                Ok(t) => return Ok(t),
                Err(error) if should_retry(config, &error) => {
                    if let Some(duration) = backoffs.next() {
                        sleep_fn.sleep(duration).await;
                        attempt += 1;
                    } else {
                        event!(
                            Level::ERROR,
                            ?attempt,
                            ?error,
                            "Not retrying error after max number of retries reached"
                        );
                        return Err(error);
                    }
                }
                Err(error) => {
                    event!(
                        Level::ERROR,
                        ?attempt,
                        ?error,
                        "Not retrying permanent error"
                    );
                    return Err(error);
                }
            }
        }
    }

    async fn retry<F, Fut, Out>(&self, mut f: F) -> Result<Out, Error>
    where
        F: FnMut(&T) -> Fut,
        Fut: Future<Output = Result<Out, Error>>,
    {
        #[inline]
        fn should_retry(config: &nativelink_config::stores::Retry, error: &Error) -> bool {
            let code = to_error_code(&error.code);
            config
                .retry_on_errors
                .as_ref()
                .is_some_and(|errors| errors.contains(&code))
                || !(error.code.is_ok() || error.code.is_unrecoverable_error())
        }

        let Self {
            inner,
            config,
            sleep_fn,
            jitter_fn,
        } = self;

        let mut backoffs = ExponentialBackoff::new(config, jitter_fn);
        let mut attempt = 1;

        loop {
            match (f)(inner).await {
                Ok(t) => return Ok(t),
                Err(error) if should_retry(config, &error) => {
                    if let Some(duration) = backoffs.next() {
                        sleep_fn.sleep(duration).await;
                        attempt += 1;
                    } else {
                        event!(
                            Level::ERROR,
                            ?attempt,
                            ?error,
                            "Not retrying error after max number of retries reached"
                        );
                        return Err(error);
                    }
                }
                Err(error) => {
                    event!(
                        Level::ERROR,
                        ?attempt,
                        ?error,
                        "Not retrying permanent error"
                    );
                    return Err(error);
                }
            }
        }
    }

    #[inline]
    async fn retry_pinned<F, Fut, Out>(self: Pin<&Self>, f: F) -> Result<Out, Error>
    where
        F: FnMut(Pin<&T>) -> Fut,
        Fut: Future<Output = Result<Out, Error>>,
    {
        self.project_ref().retry(f).await
    }

    #[inline]
    async fn retry_pinned_with_reader<F, Fut, Out>(
        self: Pin<&Self>,
        reader: &mut DropCloserReadHalf,
        mut f: F,
    ) -> Result<Out, Error>
    where
        F: FnMut(Pin<&T>, DropCloserReadHalf) -> Fut,
        Fut: Future<Output = Result<Out, Error>>,
    {
        let retriable_reader = RetriableReader::new(reader);
        let retriable_f = RetryCell::new(&mut f);

        self.retry_pinned(|inner| {
            // Safety: we uphold `RetriableReader`'s invariants.
            let rdr_fut = unsafe { retriable_reader.get() };
            // Safety: we uphold `RetryCell`'s invariants
            let f = unsafe { retriable_f.get() };

            async move {
                let (guard, rx) = rdr_fut.await;
                let ((), output) = try_join(guard, (f)(inner, rx)).await?;
                Ok(output)
            }
        })
        .await
    }

    #[inline]
    async fn retry_pinned_with_file<F, Fut, Out, U>(
        self: Pin<&Self>,
        mut file: ResumeableFileSlot,
        mut f: F,
    ) -> Result<Out, Error>
    where
        T: Borrow<U>,
        U: Unpin + ?Sized,
        F: FnMut(Pin<&U>, ResumeableFileSlot) -> Fut,
        Fut: Future<Output = Result<Out, Error>>,
    {
        let reader = file
            .as_reader()
            .await
            .err_tip(|| "in RetryWrapper::retry_with_file")?;
        let limit = reader.limit();
        let start = reader
            .get_mut()
            .stream_position()
            .await
            .err_tip(|| "in RetryWrapper::retry_with_file")?;

        file.close_file()
            .await
            .err_tip(|| "in RetryWrapper::retry_with_file")?;
        let path = file.get_path();

        let get_file_handle = || async {
            let mut file = open_file(path, u64::MAX).await?;

            let reader = file.as_reader().await?;
            let offset = reader.get_mut().seek(SeekFrom::Start(start)).await?;
            debug_assert_eq!(offset, start);

            reader.set_limit(limit);

            Ok::<_, Error>(file)
        };

        let retriable_f = RetryCell::new(&mut f);

        self.retry_pinned(|inner| {
            // Safety: `RetryCell`'s invariant is upheld.
            let f = unsafe { retriable_f.get() };
            async move {
                let file = get_file_handle().await?;
                (f)(inner, file).await
            }
        })
        .await
    }
}

impl<T> Retry<T, TokioSleepFn, DefaultJitterFn> {
    #[inline]
    pub fn new(config: nativelink_config::stores::Retry, inner: T) -> Self {
        Retry::new_with_jitter(config, inner, DefaultJitterFn)
    }
}

impl<T, J: JitterFn + Send + Sync> Retry<T, TokioSleepFn, J> {
    #[inline]
    pub fn new_with_jitter(
        config: nativelink_config::stores::Retry,
        inner: T,
        jitter_fn: J,
    ) -> Self {
        Retry::new_with_jitter_and_sleep(config, inner, TokioSleepFn, jitter_fn)
    }
}

impl<T, S: SleepFn + Send + Sync> Retry<T, S, DefaultJitterFn> {
    #[inline]
    pub fn new_with_sleep(config: nativelink_config::stores::Retry, inner: T, sleep_fn: S) -> Self {
        Retry::new_with_jitter_and_sleep(config, inner, sleep_fn, DefaultJitterFn)
    }
}

impl<T, S, J> Retry<T, S, J>
where
    S: SleepFn + Send + Sync,
    J: JitterFn + Send + Sync,
{
    #[inline]
    pub fn new_with_jitter_and_sleep(
        config: nativelink_config::stores::Retry,
        inner: T,
        sleep_fn: S,
        jitter_fn: J,
    ) -> Self {
        Self {
            sleep_fn,
            jitter_fn,
            config,
            inner,
        }
    }
}

#[async_trait]
impl<T, S, J> HealthStatusIndicator for Retry<Arc<T>, S, J>
where
    S: SleepFn + Sync + Send + Unpin,
    J: JitterFn + Send + Sync + Unpin,
    T: HealthStatusIndicator + ?Sized,
{
    #[inline]
    fn get_name(&self) -> &'static str {
        self.inner.get_name()
    }

    #[inline]
    fn struct_name(&self) -> &'static str {
        self.inner.struct_name()
    }

    #[inline]
    async fn check_health(&self, namespace: Cow<'static, str>) -> HealthStatus {
        self.inner.check_health(namespace).await
    }
}

#[async_trait]
impl<T, S, J> StoreDriver for Retry<Arc<T>, S, J>
where
    S: SleepFn + Sync + Send + Unpin + 'static,
    J: JitterFn + Send + Sync + Unpin + 'static,
    T: StoreDriver + ?Sized,
{
    #[inline]
    async fn has(self: Pin<&Self>, key: StoreKey<'_>) -> Result<Option<usize>, Error> {
        self.retry_pinned(|store: Pin<&T>| store.has(key.borrow()))
            .await
    }

    #[inline]
    async fn has_many(
        self: Pin<&Self>,
        digests: &[StoreKey<'_>],
    ) -> Result<Vec<Option<usize>>, Error> {
        self.retry_pinned(|store: Pin<&T>| store.has_many(digests))
            .await
    }

    #[inline]
    async fn has_with_results(
        self: Pin<&Self>,
        digests: &[StoreKey<'_>],
        results: &mut [Option<usize>],
    ) -> Result<(), Error> {
        let retriable_results = RetryCell::new(results);
        // Safety: `RetryCell`'s invariants are upheld.
        self.retry_pinned(|store: Pin<&T>| {
            store.has_with_results(digests, unsafe { retriable_results.get() })
        })
        .await
    }

    #[inline]
    async fn list(
        self: Pin<&Self>,
        range: (Bound<StoreKey<'_>>, Bound<StoreKey<'_>>),
        handler: &mut (dyn for<'a> FnMut(&'a StoreKey) -> bool + Send + Sync + '_),
    ) -> Result<usize, Error> {
        let retriable_handler = RetryCell::new(handler);

        // we can cheaply clone the `range` argument by calling `StoreKey::borrow` on the inner keys.
        let retriable_range = || {
            let a = range.0.as_ref().map(StoreKey::borrow);
            let b = range.1.as_ref().map(StoreKey::borrow);
            (a, b)
        };

        // Safety: `RetryCell`'s invariants are upheld.
        self.retry_pinned(|store: Pin<&T>| {
            store.list(retriable_range(), unsafe { retriable_handler.get() })
        })
        .await
    }

    #[inline]
    async fn update(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        mut reader: DropCloserReadHalf,
        upload_size: UploadSizeInfo,
    ) -> Result<(), Error> {
        self.retry_pinned_with_reader(&mut reader, |store: Pin<&T>, rx| {
            store.update(key.borrow(), rx, upload_size)
        })
        .await
    }

    #[inline]
    fn optimized_for(&self, optimization: StoreOptimizations) -> bool {
        self.inner.optimized_for(optimization)
    }

    #[inline]
    async fn update_with_whole_file(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        file: fs::ResumeableFileSlot,
        upload_size: UploadSizeInfo,
    ) -> Result<Option<fs::ResumeableFileSlot>, Error> {
        self.retry_pinned_with_file(file, |store: Pin<&T>, file| {
            store.update_with_whole_file(key.borrow(), file, upload_size)
        })
        .await
    }

    #[inline]
    async fn update_oneshot(self: Pin<&Self>, key: StoreKey<'_>, data: Bytes) -> Result<(), Error> {
        self.retry_pinned(|store: Pin<&T>| store.update_oneshot(key.borrow(), data.clone()))
            .await
    }

    #[inline]
    async fn get_part(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        writer: &mut DropCloserWriteHalf,
        offset: usize,
        length: Option<usize>,
    ) -> Result<(), Error> {
        let retriable_writer = RetryCell::new(writer);

        self.retry_pinned(|store: Pin<&T>| {
            store.get_part(
                key.borrow(),
                // Safety: we uphold `RetryCell`'s invariants
                unsafe { retriable_writer.get() },
                offset,
                length,
            )
        })
        .await
    }

    #[inline]
    async fn get(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        mut writer: DropCloserWriteHalf,
    ) -> Result<(), Error> {
        let writer = RetryCell::new(&mut writer);

        self.retry_pinned(|store: Pin<&T>| {
            // Safety: we uphold `RetryCell`'s invariant
            let outer_tx = unsafe { writer.get() };

            let (inner_tx, mut rx) = make_buf_channel_pair();
            let key = key.borrow();

            async move {
                let channel_future = outer_tx.bind(&mut rx);
                let update_future = store.get(key, inner_tx);

                try_join(channel_future, update_future)
                    .await
                    .map(|((), ())| ())
            }
        })
        .await
    }

    #[inline]
    async fn get_part_unchunked(
        self: Pin<&Self>,
        key: StoreKey<'_>,
        offset: usize,
        length: Option<usize>,
    ) -> Result<Bytes, Error> {
        self.retry_pinned(|store: Pin<&T>| store.get_part_unchunked(key.borrow(), offset, length))
            .await
    }

    #[inline]
    async fn subscribe(self: Arc<Self>, key: StoreKey<'_>) -> Box<dyn StoreSubscription> {
        // TODO: put retry logic around this subscription
        self.inner.clone().subscribe(key).await
    }

    #[inline]
    async fn check_health(self: Pin<&Self>, namespace: Cow<'static, str>) -> HealthStatus {
        let this = self.project_ref();
        this.inner.check_health(namespace).await
    }

    #[inline]
    fn inner_store(&self, digest: Option<StoreKey<'_>>) -> &dyn StoreDriver {
        self.inner.inner_store(digest)
    }

    #[inline]
    fn as_any(&self) -> &(dyn std::any::Any + Sync + Send + 'static) {
        self
    }

    #[inline]
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn std::any::Any + Sync + Send + 'static> {
        self
    }

    /// Register any metrics that this store wants to expose to the Prometheus.
    fn register_metrics(self: Arc<Self>, registry: &mut Registry) {
        self.inner.clone().register_metrics(registry)
    }

    // Register health checks used to monitor the store.
    fn register_health(self: Arc<Self>, registry: &mut HealthRegistryBuilder) {
        self.inner.clone().register_health(registry)
    }
}

fn to_error_code(code: &Code) -> ErrorCode {
    match code {
        Code::Cancelled => ErrorCode::Cancelled,
        Code::Unknown => ErrorCode::Unknown,
        Code::InvalidArgument => ErrorCode::InvalidArgument,
        Code::DeadlineExceeded => ErrorCode::DeadlineExceeded,
        Code::NotFound => ErrorCode::NotFound,
        Code::AlreadyExists => ErrorCode::AlreadyExists,
        Code::PermissionDenied => ErrorCode::PermissionDenied,
        Code::ResourceExhausted => ErrorCode::ResourceExhausted,
        Code::FailedPrecondition => ErrorCode::FailedPrecondition,
        Code::Aborted => ErrorCode::Aborted,
        Code::OutOfRange => ErrorCode::OutOfRange,
        Code::Unimplemented => ErrorCode::Unimplemented,
        Code::Internal => ErrorCode::Internal,
        Code::Unavailable => ErrorCode::Unavailable,
        Code::DataLoss => ErrorCode::DataLoss,
        Code::Unauthenticated => ErrorCode::Unauthenticated,
        _ => ErrorCode::Unknown,
    }
}
