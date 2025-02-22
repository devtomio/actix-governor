//! A middleware for [actionable](https://github.com/actix/actix-web) that provides
//! rate-limiting backed by [governor](https://github.com/antifuchs/governor).
//!
//! # Features:
//!
//! + Simple to use
//! + High performance
//! + Robust yet flexible API
//! + Actively maintained as part of the [Triox-Project](https://github.com/AaronErhardt/Triox)
//!
//! # How does it work?
//!
//! Each governor middleware has a configuration that stores a quota.
//! The quota specifies how many requests can be sent from an IP address
//! before the middleware starts blocking further requests.
//!
//! For example if the quota allowed ten requests a client could send a burst of
//! ten requests in short time before the middleware starts blocking.
//!
//! Once at least one element of the quota was used the elements of the quota
//! will be replenished after a specified period.
//!
//! For example if this period was 2 seconds and the quota was empty
//! it would take 2 seconds to replenish one element of the quota.
//! This means you could send one request every two seconds on average.
//!
//! If there was a quota that allowed ten requests with the same period
//! a client could again send a burst of ten requests and then had to wait
//! two seconds before sending further requests or 20 seconds before the full
//! quota would be replenished and he could send another burst.
//!
//! # Example
//! ```rust,no_run
//! use actix_governor::{Governor, GovernorConfigBuilder};
//! use actix_web::{web, App, HttpServer, Responder};
//!
//! async fn index() -> impl Responder {
//!     "Hello world!"
//! }
//!
//! #[actix_web::main]
//! async fn main() -> std::io::Result<()> {
//!     // Allow bursts with up to five requests per IP address
//!     // and replenishes one element every two seconds
//!     let governor_conf = GovernorConfigBuilder::default()
//!         .per_second(2)
//!         .burst_size(5)
//!         .finish()
//!         .unwrap();
//!
//!     HttpServer::new(move || {
//!         App::new()
//!             // Enable Governor middleware
//!             .wrap(Governor::new(&governor_conf))
//!             // Route hello world service
//!             .route("/", web::get().to(index))
//!    })
//!     .bind("127.0.0.1:8080")?
//!     .run()
//!     .await
//! }
//! ```
//!
//! # Configuration presets
//!
//! Instead of using the configuration builder you can use predefined presets.
//!
//! + [`GovernorConfig::default()`]: The default configuration which is suitable for most services.
//! Allows bursts with up to eight requests and replenishes one element after 500ms, based on peer IP.
//!
//! + [`GovernorConfig::secure()`]: A default configuration for security related services.
//! Allows bursts with up to two requests and replenishes one element after four seconds, based on peer IP.
//!
//! For example the secure configuration can be used as a short version of this code:
//!
//! ```rust
//! use actix_governor::GovernorConfigBuilder;
//!
//! let config = GovernorConfigBuilder::default()
//!     .per_second(4)
//!     .burst_size(2)
//!     .finish()
//!     .unwrap();
//! ```
//!
//! # Customize rate limiting key
//!
//! By default, rate limiting is done using the peer IP address (i.e. the IP address of the HTTP client that requested your app: either your user or a reverse proxy, depending on your deployment setup).
//! You can configure a different behavior which:
//! 1. can be useful in itself
//! 2. allows you to setup multiple instances of this middleware based on different keys (for example, if you want to apply rate limiting with different rates on IP and API keys at the same time)
//!
//! This is achieved by defining a [KeyExtractor] and giving it to a [Governor] instance.
//! Two ready-to-use key extractors are provided:
//! - [PeerIpKeyExtractor]: this is the default
//! - [GlobalKeyExtractor]: uses the same key for all incoming requests
//!
//! Check out the [custom_key](https://github.com/AaronErhardt/actix-governor/blob/main/examples/custom_key.rs) example to see how a custom key extractor can be implemented.
//!
//! # Add x-ratelimit headers
//!
//! By default, `x-ratelimit-after` is enabled but if you want to enable `x-ratelimit-limit`, `x-ratelimit-whitelisted` and `x-ratelimit-remaining` use [`use_headers`] method
//!
//! [`use_headers`]: crate::GovernorConfigBuilder::use_headers()
//!
//! # Common pitfalls
//!
//! Do not construct the same configuration multiple times, unless explicitly wanted!
//! This will create an independent rate limiter for each configuration!
//!
//! Instead pass the same configuration reference into [`Governor::new()`],
//! like it is described in the example.

#[cfg(test)]
mod tests;

use governor::{
    clock::{DefaultClock, QuantaInstant},
    middleware::{NoOpMiddleware, RateLimitingMiddleware, StateInformationMiddleware},
    state::keyed::DefaultKeyedStateStore,
    Quota, RateLimiter,
};

use std::{cell::RefCell, marker::PhantomData, num::NonZeroU32, rc::Rc, sync::Arc, time::Duration};

use actix_web::dev::{Service, ServiceRequest, ServiceResponse, Transform};
use actix_web::http::Method;
use actix_web::{body::MessageBody, Error};
use futures::future;

mod key_extractor;
mod service;

type SharedRateLimiter<Key, M> =
    Arc<RateLimiter<Key, DefaultKeyedStateStore<Key>, DefaultClock, M>>;

pub use key_extractor::{GlobalKeyExtractor, KeyExtractor, PeerIpKeyExtractor};

const DEFAULT_PERIOD: Duration = Duration::from_millis(500);
const DEFAULT_BURST_SIZE: u32 = 8;

/// Helper struct for building a configuration for the governor middleware.
///
/// # Example
///
/// Create a configuration with a quota of ten requests per IP address
/// that replenishes one element every minute.
///
/// ```rust
/// use actix_governor::GovernorConfigBuilder;
///
/// let config = GovernorConfigBuilder::default()
///     .per_second(60)
///     .burst_size(10)
///     .finish()
///     .unwrap();
/// ```
///
/// with x-ratelimit headers
///
/// ```rust
/// use actix_governor::GovernorConfigBuilder;
///
/// let config = GovernorConfigBuilder::default()
///     .per_second(60)
///     .burst_size(10)
///     .use_headers() // Add this
///     .finish()
///     .unwrap();
/// ```
#[derive(Debug, Eq)]
pub struct GovernorConfigBuilder<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> {
    period: Duration,
    burst_size: u32,
    methods: Option<Vec<Method>>,
    key_extractor: K,
    middleware: PhantomData<M>,
}

impl<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> Clone
    for GovernorConfigBuilder<K, M>
{
    fn clone(&self) -> Self {
        Self {
            period: self.period,
            burst_size: self.burst_size,
            methods: self.methods.clone(),
            key_extractor: self.key_extractor.clone(),
            middleware: self.middleware,
        }
    }
}

impl<K: KeyExtractor + PartialEq, M: RateLimitingMiddleware<QuantaInstant>> PartialEq
    for GovernorConfigBuilder<K, M>
{
    fn eq(&self, other: &Self) -> bool {
        self.period == other.period
            && self.burst_size == other.burst_size
            && self.methods == other.methods
            && self.key_extractor == other.key_extractor
    }
}

impl Default for GovernorConfigBuilder<PeerIpKeyExtractor, NoOpMiddleware> {
    /// The default configuration which is suitable for most services.
    /// Allows burst with up to eight requests and replenishes one element after 500ms, based on peer IP.
    /// The values can be modified by calling other methods on this struct.
    fn default() -> Self {
        Self::const_default()
    }
}

impl<M: RateLimitingMiddleware<QuantaInstant>> GovernorConfigBuilder<PeerIpKeyExtractor, M> {
    pub fn const_default() -> Self {
        GovernorConfigBuilder {
            period: DEFAULT_PERIOD,
            burst_size: DEFAULT_BURST_SIZE,
            methods: None,
            key_extractor: PeerIpKeyExtractor,
            middleware: PhantomData,
        }
    }
    /// Set the interval after which one element of the quota is replenished.
    ///
    /// **The interval must not be zero.**
    pub fn const_period(mut self, duration: Duration) -> Self {
        self.period = duration;
        self
    }
    /// Set the interval after which one element of the quota is replenished in seconds.
    ///
    /// **The interval must not be zero.**
    pub fn const_per_second(mut self, seconds: u64) -> Self {
        self.period = Duration::from_secs(seconds);
        self
    }
    /// Set the interval after which one element of the quota is replenished in milliseconds.
    ///
    /// **The interval must not be zero.**
    pub fn const_per_millisecond(mut self, milliseconds: u64) -> Self {
        self.period = Duration::from_millis(milliseconds);
        self
    }
    /// Set the interval after which one element of the quota is replenished in nanoseconds.
    ///
    /// **The interval must not be zero.**
    pub fn const_per_nanosecond(mut self, nanoseconds: u64) -> Self {
        self.period = Duration::from_nanos(nanoseconds);
        self
    }
    /// Set quota size that defines how many requests can occur
    /// before the governor middleware starts blocking requests from an IP address and
    /// clients have to wait until the elements of the quota are replenished.
    ///
    /// **The burst_size must not be zero.**
    pub fn const_burst_size(mut self, burst_size: u32) -> Self {
        self.burst_size = burst_size;
        self
    }
}

impl<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> GovernorConfigBuilder<K, M> {
    /// Set the interval after which one element of the quota is replenished.
    ///
    /// **The interval must not be zero.**
    pub fn period(&mut self, duration: Duration) -> &mut Self {
        self.period = duration;
        self
    }
    /// Set the interval after which one element of the quota is replenished in seconds.
    ///
    /// **The interval must not be zero.**
    pub fn per_second(&mut self, seconds: u64) -> &mut Self {
        self.period = Duration::from_secs(seconds);
        self
    }
    /// Set the interval after which one element of the quota is replenished in milliseconds.
    ///
    /// **The interval must not be zero.**
    pub fn per_millisecond(&mut self, milliseconds: u64) -> &mut Self {
        self.period = Duration::from_millis(milliseconds);
        self
    }
    /// Set the interval after which one element of the quota is replenished in nanoseconds.
    ///
    /// **The interval must not be zero.**
    pub fn per_nanosecond(&mut self, nanoseconds: u64) -> &mut Self {
        self.period = Duration::from_nanos(nanoseconds);
        self
    }
    /// Set quota size that defines how many requests can occur
    /// before the governor middleware starts blocking requests from an IP address and
    /// clients have to wait until the elements of the quota are replenished.
    ///
    /// **The burst_size must not be zero.**
    pub fn burst_size(&mut self, burst_size: u32) -> &mut Self {
        self.burst_size = burst_size;
        self
    }

    /// Set the HTTP methods this configuration should apply to.
    /// By default this is all methods.
    pub fn methods(&mut self, methods: Vec<Method>) -> &mut Self {
        self.methods = Some(methods);
        self
    }

    /// Set the key extractor this configuration should use.
    /// By default this is using the [PeerIpKeyExtractor].
    pub fn key_extractor<K2: KeyExtractor>(
        &mut self,
        key_extractor: K2,
    ) -> GovernorConfigBuilder<K2, M> {
        GovernorConfigBuilder {
            period: self.period,
            burst_size: self.burst_size,
            methods: self.methods.to_owned(),
            key_extractor,
            middleware: PhantomData,
        }
    }

    /// Set x-ratelimit headers to response, the headers is
    /// - `x-ratelimit-limit`       - Request limit
    /// - `x-ratelimit-remaining`   - The number of requests left for the time window
    /// - `x-ratelimit-after`       - Number of seconds in which the API will become available after its rate limit has been exceeded
    /// - `x-ratelimit-whitelisted` - If the request method not in methods, this header will be add it, use [`methods`] to add methods
    ///
    /// By default `x-ratelimit-after` is enabled, with [`use_headers`] will enable `x-ratelimit-limit`, `x-ratelimit-whitelisted` and `x-ratelimit-remaining`
    ///
    /// [`methods`]: crate::GovernorConfigBuilder::methods()
    pub fn use_headers(&mut self) -> GovernorConfigBuilder<K, StateInformationMiddleware> {
        GovernorConfigBuilder {
            period: self.period,
            burst_size: self.burst_size,
            methods: self.methods.to_owned(),
            key_extractor: self.key_extractor.clone(),
            middleware: PhantomData,
        }
    }

    /// Finish building the configuration and return the configuration for the middleware.
    /// Returns `None` if either burst size or period interval are zero.
    pub fn finish(&mut self) -> Option<GovernorConfig<K, M>> {
        if self.burst_size != 0 && self.period.as_nanos() != 0 {
            Some(GovernorConfig {
                key_extractor: self.key_extractor.clone(),
                limiter: Arc::new(
                    RateLimiter::keyed(
                        Quota::with_period(self.period)
                            .unwrap()
                            .allow_burst(NonZeroU32::new(self.burst_size).unwrap()),
                    )
                    .with_middleware::<M>(),
                ),
                methods: self.methods.clone(),
            })
        } else {
            None
        }
    }
}

#[derive(Debug)]
/// Configuration for the Governor middleware.
pub struct GovernorConfig<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> {
    key_extractor: K,
    limiter: SharedRateLimiter<K::Key, M>,
    methods: Option<Vec<Method>>,
}

impl<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> Clone for GovernorConfig<K, M> {
    fn clone(&self) -> Self {
        GovernorConfig {
            key_extractor: self.key_extractor.clone(),
            limiter: self.limiter.clone(),
            methods: self.methods.clone(),
        }
    }
}

impl Default for GovernorConfig<PeerIpKeyExtractor, NoOpMiddleware> {
    /// The default configuration which is suitable for most services.
    /// Allows bursts with up to eight requests and replenishes one element after 500ms, based on peer IP.
    fn default() -> Self {
        GovernorConfigBuilder::default().finish().unwrap()
    }
}

impl<M: RateLimitingMiddleware<QuantaInstant>> GovernorConfig<PeerIpKeyExtractor, M> {
    /// A default configuration for security related services.
    /// Allows bursts with up to two requests and replenishes one element after four seconds, based on peer IP.
    ///
    /// This prevents brute-forcing passwords or security tokens
    /// yet allows to quickly retype a wrong password once before the quota is exceeded.
    pub fn secure() -> Self {
        GovernorConfigBuilder {
            period: Duration::from_secs(4),
            burst_size: 2,
            methods: None,
            key_extractor: PeerIpKeyExtractor,
            middleware: PhantomData,
        }
        .finish()
        .unwrap()
    }
}

/// Governor middleware factory.
pub struct Governor<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> {
    key_extractor: K,
    limiter: SharedRateLimiter<K::Key, M>,
    methods: Option<Vec<Method>>,
}

impl<K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> Governor<K, M> {
    /// Create new governor middleware factory from configuration.
    pub fn new(config: &GovernorConfig<K, M>) -> Self {
        Governor {
            key_extractor: config.key_extractor.clone(),
            limiter: config.limiter.clone(),
            methods: config.methods.clone(),
        }
    }
}

impl<S, B, K> Transform<S, ServiceRequest> for Governor<K, NoOpMiddleware>
where
    K: KeyExtractor,
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    B: MessageBody,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = GovernorMiddleware<S, K, NoOpMiddleware>;
    type InitError = ();
    type Future = future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(GovernorMiddleware::<S, K, NoOpMiddleware> {
            service: Rc::new(RefCell::new(service)),
            key_extractor: self.key_extractor.clone(),
            limiter: self.limiter.clone(),
            methods: self.methods.clone(),
        })
    }
}

impl<S, B, K> Transform<S, ServiceRequest> for Governor<K, StateInformationMiddleware>
where
    K: KeyExtractor,
    S: Service<ServiceRequest, Response = ServiceResponse<B>, Error = Error>,
    B: MessageBody,
    <S as Service<ServiceRequest>>::Future: Unpin,
{
    type Response = ServiceResponse<B>;
    type Error = Error;
    type Transform = GovernorMiddleware<S, K, StateInformationMiddleware>;
    type InitError = ();
    type Future = future::Ready<Result<Self::Transform, Self::InitError>>;

    fn new_transform(&self, service: S) -> Self::Future {
        future::ok(GovernorMiddleware::<S, K, StateInformationMiddleware> {
            service: Rc::new(RefCell::new(service)),
            key_extractor: self.key_extractor.clone(),
            limiter: self.limiter.clone(),
            methods: self.methods.clone(),
        })
    }
}

pub struct GovernorMiddleware<S, K: KeyExtractor, M: RateLimitingMiddleware<QuantaInstant>> {
    service: std::rc::Rc<std::cell::RefCell<S>>,
    key_extractor: K,
    limiter: SharedRateLimiter<K::Key, M>,
    methods: Option<Vec<Method>>,
}
