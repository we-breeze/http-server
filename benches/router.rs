#![recursion_limit = "512"]

use std::hint::black_box;
use std::time::Instant;

use brz_http_server::__private::{RouteDescriptor, decode_path, match_route};
use brz_http_server::{
    ApiMetrics, Handler, NoAuthenticator, Request, Response, Router, StatusCode,
};

#[path = "support/legacy_router.rs"]
mod legacy;

#[derive(Clone)]
struct Api {
    descriptors: &'static [RouteDescriptor],
}

fn metrics() -> ApiMetrics {
    static METRICS: std::sync::LazyLock<ApiMetrics> = std::sync::LazyLock::new(|| {
        ApiMetrics::new([
            "/router-bench_2xx",
            "/router-bench_3xx",
            "/router-bench_4xx",
            "/router-bench_5xx",
        ])
    });
    *METRICS
}

impl Api {
    fn new(id: usize) -> Self {
        let routes = [
            format!("/api/group-{id:04}/status"),
            format!("/api/group-{id:04}/:id"),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, path)| RouteDescriptor {
            path: Box::leak(path.into_boxed_str()),
            methods: 2,
            priority: (1 << 24) + (4 - index) * 1024 + 4,
            metrics,
        })
        .collect::<Vec<_>>();
        Self {
            descriptors: Box::leak(routes.into_boxed_slice()),
        }
    }
}

impl Handler for Api {
    fn routes(&self) -> &'static [RouteDescriptor] {
        self.descriptors
    }
    fn route_priority(&self, path: &str, method: &str) -> Option<usize> {
        let path = decode_path(path);
        self.descriptors
            .iter()
            .find(|route| method == "GET" && match_route(&path, route.path).is_some())
            .map(|route| route.priority)
    }
    fn route_metrics(&self, path: &str, _method: &str) -> Option<(usize, ApiMetrics)> {
        let path = decode_path(path);
        self.descriptors
            .iter()
            .find(|route| match_route(&path, route.path).is_some())
            .map(|route| (route.priority, metrics()))
    }
    fn route_methods(&self, path: &str) -> u16 {
        let path = decode_path(path);
        self.descriptors
            .iter()
            .filter(|route| match_route(&path, route.path).is_some())
            .fold(0, |bits, route| bits | route.methods)
    }
    async fn call(&self, _: Request<'_>, _: &NoAuthenticator) -> Response {
        Response::empty(StatusCode::OK)
    }
}

fn legacy_24(apis: &[Api]) -> impl Handler {
    legacy::Router::new(apis[0].clone())
        .merge(apis[1].clone())
        .merge(apis[2].clone())
        .merge(apis[3].clone())
        .merge(apis[4].clone())
        .merge(apis[5].clone())
        .merge(apis[6].clone())
        .merge(apis[7].clone())
        .merge(apis[8].clone())
        .merge(apis[9].clone())
        .merge(apis[10].clone())
        .merge(apis[11].clone())
        .merge(apis[12].clone())
        .merge(apis[13].clone())
        .merge(apis[14].clone())
        .merge(apis[15].clone())
        .merge(apis[16].clone())
        .merge(apis[17].clone())
        .merge(apis[18].clone())
        .merge(apis[19].clone())
        .merge(apis[20].clone())
        .merge(apis[21].clone())
        .merge(apis[22].clone())
        .merge(apis[23].clone())
}

fn measure(handler: &impl Handler, path: &str) -> f64 {
    const ITERATIONS: u32 = 20_000;
    for _ in 0..1000 {
        black_box(handler.route_metrics(black_box(path), "GET"));
    }
    let mut samples = [0.0; 3];
    for sample in &mut samples {
        let start = Instant::now();
        for _ in 0..ITERATIONS {
            black_box(handler.route_metrics(black_box(path), "GET"));
        }
        *sample = start.elapsed().as_secs_f64() * 1e9 / f64::from(ITERATIONS);
    }
    samples.sort_by(f64::total_cmp);
    samples[1]
}

fn main() {
    let apis = (0..512).map(Api::new).collect::<Vec<_>>();
    let legacy = legacy_24(&apis);
    let flat = apis[..24]
        .iter()
        .cloned()
        .fold(Router::default(), Router::merge);
    let _ = flat.prepare("/api/group-0000/status", "GET");
    println!("Routing + metric selection only; median ns/op; no I/O or handler Future invocation.");
    println!("case\tlegacy(24 APIs)\tbuckets(24 APIs)");
    for (case, path) in [
        ("static-first", "/api/group-0000/status"),
        ("static-last", "/api/group-0023/status"),
        ("parameter-first", "/api/group-0000/123"),
        ("parameter-last", "/api/group-0023/123"),
        ("miss", "/api/missing/no-match"),
    ] {
        println!(
            "{case}\t{:.0}\t{:.0}",
            measure(&legacy, path),
            measure(&flat, path)
        );
    }
    for count in [128, 512] {
        let flat = apis[..count]
            .iter()
            .cloned()
            .fold(Router::default(), Router::merge);
        let path = format!("/api/group-{:04}/123", count - 1);
        println!(
            "buckets({count} APIs), parameter-last\t{:.0}",
            measure(&flat, &path)
        );
    }
}
