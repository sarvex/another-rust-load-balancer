use crate::server::BackendPool;
use arc_swap::{access::Access, ArcSwap};
use futures::future::join_all;
use hyper::{
  client::HttpConnector,
  http::uri::{self, Authority},
  Client, StatusCode, Uri,
};
use hyper_timeout::TimeoutConnector;
use log::info;
use serde::Deserialize;
use std::time::Duration;
use std::time::SystemTime;
use std::{convert::TryFrom, ops::Deref};
use std::{fmt, sync::Arc};

// Amount of time in seconds to pass until the next health check is started
const CHECK_INTERVAL: i64 = 20;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct HealthConfig {
  pub slow_threshold: i64,
  pub interval: i64,
  pub timeout: u64,
  pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Healthiness {
  Healthy,
  Slow(i64),
  Unresponsive(Option<StatusCode>),
}

impl fmt::Display for Healthiness {
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    match self {
      Healthiness::Healthy => write!(f, "Healthy"),
      Healthiness::Slow(respone_time) => write!(f, "Slow {}", respone_time),
      Healthiness::Unresponsive(Some(status_code)) => write!(f, "Unresponsive, status: {}", status_code),
      Healthiness::Unresponsive(None) => write!(f, "Unresponsive"),
    }
  }
}

pub async fn watch_health<A, G>(backend_pools: A)
where
  A: Access<Vec<Arc<BackendPool>>, Guard = G> + Send + Sync + 'static,
  G: Deref<Target = Vec<Arc<BackendPool>>> + Send + Sync,
{
  let backend_pools = Arc::new(backend_pools);
  let mut interval_timer = tokio::time::interval(chrono::Duration::seconds(CHECK_INTERVAL).to_std().unwrap());
  let backend_pools = backend_pools.load();
  loop {
    interval_timer.tick().await;
    let backend_pools = backend_pools.clone();
    let mut checks = Vec::new();

    for pool in backend_pools.deref() {
      for (server_address, healthiness) in &pool.addresses {
        let future = check_server_health_once(server_address.clone(), healthiness, &pool.health_config);
        checks.push(future);
      }
    }
    join_all(checks).await;
  }
}

async fn check_server_health_once(
  server_address: String,
  healthiness: &ArcSwap<Healthiness>,
  health_config: &HealthConfig,
) {
  let uri = uri::Uri::builder()
    .scheme("http")
    .path_and_query(&health_config.path)
    .authority(Authority::from_maybe_shared(server_address.clone()).unwrap())
    .build()
    .unwrap();

  let previous_healthiness = healthiness.load();
  let result = contact_server(uri, health_config.slow_threshold, health_config.timeout).await;

  if previous_healthiness.as_ref() != &result {
    info!("new healthiness for {}: {}", &server_address, &result);
    healthiness.store(Arc::new(result));
  }
}

async fn contact_server(server_address: Uri, slow_threshold: i64, timeout: u64) -> Healthiness {
  let http_connector = HttpConnector::new();
  let mut connector = TimeoutConnector::new(http_connector);
  connector.set_connect_timeout(Some(Duration::from_millis(timeout)));
  connector.set_read_timeout(Some(Duration::from_millis(timeout)));
  connector.set_write_timeout(Some(Duration::from_millis(timeout)));
  let client = Client::builder().build::<_, hyper::Body>(connector);

  let before_request = SystemTime::now();
  // Await the response...
  if let Ok(response) = client.get(server_address).await {
    if response.status().is_success() {
      // elapsed() only fails when system time is later than "self"
      let time_to_respond = before_request.elapsed().unwrap().as_millis();
      let response_time = i64::try_from(time_to_respond);
      if response_time.unwrap() > slow_threshold {
        Healthiness::Slow(response_time.unwrap())
      } else {
        Healthiness::Healthy
      }
    } else {
      Healthiness::Unresponsive(Some(response.status()))
    }
  } else {
    Healthiness::Unresponsive(None)
  }
}
