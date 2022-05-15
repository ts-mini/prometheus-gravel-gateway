use std::{collections::HashMap, sync::Arc};

use warp::{Filter, http::HeaderValue, hyper::{HeaderMap, body::Bytes}, path::Tail, reject::Reject};

use crate::{aggregator::{AggregationError, Aggregator}, auth::Authenticator};

#[cfg(feature="clustering")]
use crate::clustering::ClusterConfig;

#[derive(Debug)]
enum GravelError {
    Error(String),
    AuthError,
    AggregationError(AggregationError)
}

impl Reject for GravelError {}

pub struct RoutesConfig {
    pub authenticator: Box<dyn Authenticator + Send + Sync>,
    #[cfg(feature="clustering")]
    pub cluster_conf: Option<ClusterConfig>
}

async fn auth(config: Arc<RoutesConfig>, header: String) -> Result<(), warp::Rejection> {
    if config.authenticator.authenticate(&header) {
        return Ok(());
    }

    return Err(warp::reject::custom(GravelError::AuthError));
}

pub fn get_routes(aggregator: Aggregator, config: RoutesConfig) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
    let default_auth = warp::any().map(|| {
        return String::new();
    });

    let config = Arc::new(config);
    let auth_config = Arc::clone(&config);

    let auth = warp::header::<String>("authorization").or(default_auth).unify().and_then(move |header| auth(auth_config.clone(), header)).untuple_one();

    let push_metrics_path = warp::path("metrics")
        .and(warp::post())
        .and(auth)
        .and(warp::filters::body::bytes())
        .and(warp::path::tail())
        .and(with_aggregator(aggregator.clone()))
        .and(with_config(Arc::clone(&config)))
        .and_then(ingest_metrics);

    let mut get_metrics_headers = HeaderMap::new();
    get_metrics_headers.insert("Content-Type", HeaderValue::from_static("text/plain; version=0.0.4"));

    let get_metrics_path = warp::path!("metrics")
        .and(warp::get())
        .and(with_aggregator(aggregator.clone()))
        .and_then(get_metrics)
        .with(warp::reply::with::headers(get_metrics_headers));

    return push_metrics_path.or(get_metrics_path);
}

fn with_aggregator(
    agg: Aggregator,
) -> impl Filter<Extract = (Aggregator,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || agg.clone())
}

fn with_config(
    conf: Arc<RoutesConfig>,
) -> impl Filter<Extract = (Arc<RoutesConfig>,), Error = std::convert::Infallible> + Clone {
    warp::any().map(move || Arc::clone(&conf))
}

async fn forward_to_peer(peer: &str, data: Bytes, url_tail: Tail) -> Result<(), GravelError> {
    let client = reqwest::Client::new();
    return match client.post(peer.to_owned() + "/" + url_tail.as_str()).body(data).send().await {
        Ok(o) => {
            if o.status().is_success() {
                return Ok(());
            }

            return Err(GravelError::Error(format!("Failed to forward to peer. Got status: {}", 200)));
        },
        Err(e) => Err(GravelError::Error(e.to_string()))
    }
}

/// The routes for POST /metrics requests - takes a Prometheus exposition format
/// and merges it into the existing metrics. Also supports push gateway syntax - /metrics/job/foo
/// adds a job="foo" label to all the metrics
async fn ingest_metrics(
    data: Bytes,
    url_tail: Tail,
    mut agg: Aggregator,
    conf: Arc<RoutesConfig>
) -> Result<impl warp::Reply, warp::Rejection> {
    let labels = {
        let mut labelset = HashMap::new();
        let mut labels = url_tail.as_str().split("/").peekable();
        while labels.peek().is_some() {
            let name = labels.next().unwrap();
            if name.is_empty() {
                break;
            }
            let value = labels.next().unwrap_or_default();
            labelset.insert(name, value);
        }
        labelset
    };

    // We're clustering, so might need to forward the metrics
    if let Some(cluster_conf) = conf.cluster_conf.as_ref() {
        let job = labels.get("job").unwrap_or(&"");
        if let Some(peer) = cluster_conf.get_peer_for_key(job) {
            if !cluster_conf.is_self(peer) {
                match forward_to_peer(peer, data, url_tail).await {
                    Ok(_) => return Ok(""),
                    Err(e) => return Err(warp::reject::custom(e))
                }
            }
        }
    }

    let body = match String::from_utf8(data.to_vec()) {
        Ok(s) => s,
        Err(_) => {
            return Err(warp::reject::custom(GravelError::Error("Invalid UTF-8 in body".into())));
        }
    };

    match agg.parse_and_merge(&body, &labels).await {
        Ok(_) => Ok(""),
        Err(e) => Err(warp::reject::custom(GravelError::AggregationError(e))),
    }
}

async fn get_metrics(agg: Aggregator) -> Result<impl warp::Reply, warp::Rejection> {
    Ok(agg.to_string().await)
}