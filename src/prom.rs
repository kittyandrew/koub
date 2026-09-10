use lazy_static::lazy_static;
use prometheus::{self, Encoder, GaugeVec, HistogramVec, IntCounter, IntCounterVec, IntGauge, IntGaugeVec, TextEncoder};
use rocket::{Data, Request, Response, State, fairing::Fairing, fairing::Info, fairing::Kind};
use std::time::Instant;

lazy_static! {
    // Request-level metrics
    pub static ref TOTAL_REQUESTS_SERVED: IntCounter = prometheus::register_int_counter!(
        "oubot_requests_total",
        "Total number of requests served (including rejected)"
    )
    .unwrap();
    pub static ref ENDPOINTS_REQUESTS_SERVED: IntCounterVec = prometheus::register_int_counter_vec!(
        "oubot_endpoint_requests_total",
        "Total number of requests served per endpoint",
        &["method", "endpoint"]
    )
    .unwrap();
    pub static ref ENDPOINT_REQUESTS_DURATION: HistogramVec = prometheus::register_histogram_vec!(
        "oubot_request_duration_seconds",
        "Endpoint request handling latencies in seconds",
        &["method", "endpoint"]
    )
    .unwrap();

    // Domain-level metrics
    // @NOTE: State encoding: 0=Uninitialized, 1=Up, 2=Down, 3=Paused
    pub static ref UPTIME_STATE: IntGaugeVec = prometheus::register_int_gauge_vec!(
        "oubot_uptime_state",
        "Current uptime state per user (0=uninit, 1=up, 2=down, 3=paused)",
        &["user_id"]
    )
    .unwrap();
    pub static ref LAST_SEEN_TIMESTAMP: GaugeVec = prometheus::register_gauge_vec!(
        "oubot_last_seen_timestamp",
        "Unix timestamp of last heartbeat per user",
        &["user_id"]
    )
    .unwrap();
    pub static ref AUTH_FAILURES: IntCounterVec = prometheus::register_int_counter_vec!(
        "oubot_auth_failures_total",
        "Authentication failures by reason",
        &["reason"]
    )
    .unwrap();
    pub static ref NOTIFICATIONS: IntCounterVec = prometheus::register_int_counter_vec!(
        "oubot_notifications_total",
        "Notifications sent by type and result",
        &["type", "result"]
    )
    .unwrap();
    pub static ref ACTIVE_USERS: IntGauge = prometheus::register_int_gauge!(
        "oubot_active_users",
        "Number of registered users"
    )
    .unwrap();
}

#[derive(Copy, Clone, Debug)]
struct DurationTimer(Option<Instant>);

pub struct PrometheusCollection;

#[rocket::async_trait]
impl Fairing for PrometheusCollection {
    fn info(&self) -> Info {
        Info { name: "Prometheus metrics", kind: Kind::Request | Kind::Response }
    }

    async fn on_request(&self, request: &mut Request<'_>, _: &mut Data<'_>) {
        // Note(andrew): Setting timer value here to the local request cache, since this
        //     is executed prior to entering our endpoints, which means we will be able
        //     to time request latency.
        request.local_cache(|| DurationTimer(Some(Instant::now())));
    }

    async fn on_response<'r>(&self, request: &'r Request<'_>, response: &mut Response<'r>) {
        let method = request.method().to_string();
        let endpoint = request.uri().path().to_string();
        if endpoint != "/api/v1/metrics" {
            // Record total request count.
            TOTAL_REQUESTS_SERVED.inc();

            // Measuring request latency (internal processing time). Doing this first, so any
            // code below adds minimal overhead to the processing time. Just make sure that the
            // code below actually isn't artificially slow.
            let duration_timer = request.local_cache(|| DurationTimer(None));
            if let Some(duration) = duration_timer.0.map(|st| st.elapsed()) {
                ENDPOINTS_REQUESTS_SERVED.local().with_label_values(&[&method, &endpoint]).inc();

                let latency_ms = duration.as_micros() as f64 / 1000.;
                ENDPOINT_REQUESTS_DURATION.local().with_label_values(&[&method, &endpoint]).observe(latency_ms / 1000.);

                // While we can, lets add response header with timing as well.
                response.set_raw_header("X-Response-Time", format!("{latency_ms} ms"));
            };
        }
    }
}

// @WARNING: This endpoint is unauthenticated. User-level metrics (oubot_uptime_state,
//  oubot_last_seen_timestamp) expose user IDs and device status. In production, block
//  this path in the reverse proxy so it's only reachable from the internal monitoring
//  network. See the ntfy pattern in kittyos for reference.
#[get("/api/v1/metrics")]
pub async fn get_metrics(_rl: crate::bauth::RateLimitGuard, encoder: &State<TextEncoder>) -> String {
    let mut buffer = Vec::new();
    encoder.encode(&prometheus::gather(), &mut buffer).unwrap();
    String::from_utf8(buffer).unwrap()
}
