#![feature(generic_associated_types)]
#![feature(type_alias_impl_trait)]

use axum::{extract::MatchedPath, http::Request, response::IntoResponse, routing::get, Router, http};
use axum::middleware::{FromFnLayer, Next};
use std::{fmt, time::{Instant}};
use std::convert::Infallible;
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::task::Poll::Ready;
use axum::extract::State;
use axum::http::Response;
use axum::routing::Route;
use axum_core::body::BoxBody;
use axum_core::Error;

use opentelemetry_prometheus::PrometheusExporter;

use prometheus::{Encoder, TextEncoder};

use opentelemetry::{
    KeyValue,Key,Value,
};
use axum_macros::debug_handler;

use opentelemetry::{Context as OtelContext, global};
use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::sdk::export::metrics::aggregation;
use opentelemetry::sdk::metrics::{controllers, processors, selectors};

use tower::{Layer, Service};
use tower::layer::{layer_fn, LayerFn};

use futures_util::future::BoxFuture;
use futures_util::ready;
use pin_project_lite::pin_project;


#[derive(Clone)]
pub struct Metric {
    pub cx: OtelContext,
    pub http_counter: Counter<u64>,

    // migrate from ValueRecorder to Histogram if opentelemetry 0.18.0 released
    pub http_req_histogram: Histogram<f64>,
}

#[derive(Clone)]
pub struct MetricState {
    exporter: PrometheusExporter,
    pub metric: Metric,
}

#[derive(Clone)]
pub struct PromMetrics<S> {
    pub(crate) state: Arc<MetricState>,
    service: S,
}

#[derive(Clone)]
pub struct PromMetricsLayer {
    pub(crate) state: Arc<MetricState>,
}

impl PromMetricsLayer {

    pub fn new() -> Self {
        Self {
            state: Arc::new(Self::new_state()),
        }
    }

    pub fn new_state() -> MetricState {

        let meter = global::meter("my-app");

        // init global meter provider and prometheus exporter
        let controller = controllers::basic(
            processors::factory(
                selectors::simple::histogram([0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0]),
                aggregation::cumulative_temporality_selector(),
            )
                .with_memory(true),
        )
            .build();
        let exporter = opentelemetry_prometheus::exporter(controller).
            init();

        let app_state = MetricState {
            exporter,
            metric: Metric {
                cx: Default::default(),
                http_counter: meter.u64_counter("http.counter")
                    .with_description("Counts http request")
                    .init(),
                http_req_histogram:  meter.f64_histogram("http.histogram")
                    .with_description("Counts http request latency")
                    .init()
            } };

        app_state
    }

    pub fn routes(&self) -> Router<Arc<MetricState>> {
        Router::with_state(self.state.clone())
            .route("/metrics", get(exporter_handler))
    }
}

impl<S> Layer<S> for PromMetricsLayer {
    type Service = PromMetrics<S>;

    fn layer(&self, service: S) -> Self::Service {
        PromMetrics {
            state: self.state.clone(),
            service
        }
    }
}

pin_project! {
    /// Response future for [`PromMetrics`].
    pub struct ResponseFuture<F> {
        #[pin]
        inner: F,
        #[pin]
        start: Instant,
        #[pin]
        state: Arc<MetricState>,
        #[pin]
        path: String,
        #[pin]
        method: String,
    }
}

impl<S, R, ResBody> Service<Request<R>> for PromMetrics<S>
    where
        S: Service<Request<R>, Response = Response<ResBody>>,
{
    type Response = Response<ResBody>;
    type Error = S::Error;
    type Future = ResponseFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.service.poll_ready(cx)
    }

    fn call(&mut self, req: Request<R>) -> Self::Future {
        // axum::middleware::from_fn_with_state(self.state.clone(), track_metrics)

        let start = Instant::now();
        let method = req.method().clone().to_string();
        let path = if let Some(matched_path) = req.extensions().get::<MatchedPath>() {
            matched_path.as_str().to_owned()
        } else {
            req.uri().path().to_owned()
        };

        ResponseFuture {
            inner: self.service.call(req),
            start,
            method,
            path,
            state: self.state.clone()
        }
    }
}

impl<F, B, E> Future for ResponseFuture<F>
    where
        F: Future<Output = Result<Response<B>, E>>,
{
    type Output = Result<Response<B>, E>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        let response = ready!(this.inner.poll(cx))?;

        if this.path.clone() == "/metrics" {
            return Ready(Ok(response));
        }

        let latency = this.start.elapsed().as_secs_f64();
        let status = response.status().as_u16().to_string();

        let labels = [
            KeyValue{key: Key::from("method"), value: Value::from(this.method.clone()) },
            KeyValue::new("path", this.path.clone()),
            KeyValue::new("status", status.clone()),
        ];

        this.state.metric.http_counter.add(& this.state.metric.cx, 1, &labels);

        this.state.metric.http_req_histogram.record(&this.state.metric.cx, latency, &labels);

        tracing::info!("method={} latency={} status={} labels={:?}", &this.method, &latency, &status, &labels);
        println!("ResponseFuture::poll");

        Poll::Ready(Ok(response))
    }
}


#[debug_handler]
pub async fn exporter_handler(state: State<Arc<MetricState>>) -> impl IntoResponse {
    println!("metrics api");
    let mut buffer = Vec::new();
    let encoder = TextEncoder::new();
    encoder.encode(&state.exporter.registry().gather(), &mut buffer).unwrap();
    let metrics = String::from_utf8(buffer).unwrap();
    metrics
}
