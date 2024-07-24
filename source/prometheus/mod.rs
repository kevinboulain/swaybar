mod query;
use query::{instant, range};
pub use query::{Error, MatrixResult, Value, VectorResult};

pub const STEP: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn cpu(
  executor: &smol::Executor<'static>,
  start: chrono::DateTime<chrono::offset::Local>,
  end: chrono::DateTime<chrono::offset::Local>,
) -> Result<Vec<MatrixResult>, Error> {
  range(
    executor,
    r#"avg (sum (rate(node_cpu_seconds_total{mode!="idle"}[1m])) without (mode)) without (cpu)"#,
    start.timestamp(),
    end.timestamp(),
    STEP.as_secs_f64(),
  )
  .await
}

pub async fn download(
  executor: &smol::Executor<'static>,
  start: chrono::DateTime<chrono::offset::Local>,
  end: chrono::DateTime<chrono::offset::Local>,
) -> Result<Vec<MatrixResult>, Error> {
  range(
    executor,
    r#"rate(node_network_receive_bytes_total{device="wlan0"}[1m])"#,
    start.timestamp(),
    end.timestamp(),
    STEP.as_secs_f64(),
  )
  .await
}

pub async fn temperature(
  executor: &smol::Executor<'static>,
  start: chrono::DateTime<chrono::offset::Local>,
  end: chrono::DateTime<chrono::offset::Local>,
) -> Result<Vec<MatrixResult>, Error> {
  range(
    executor,
    r#"max (max_over_time(node_thermal_zone_temp[1m])) without (type, zone)"#,
    start.timestamp(),
    end.timestamp(),
    STEP.as_secs_f64(),
  )
  .await
}

pub async fn upload(
  executor: &smol::Executor<'static>,
  start: chrono::DateTime<chrono::offset::Local>,
  end: chrono::DateTime<chrono::offset::Local>,
) -> Result<Vec<MatrixResult>, Error> {
  range(
    executor,
    r#"rate(node_network_transmit_bytes_total{device="wlan0"}[1m])"#,
    start.timestamp(),
    end.timestamp(),
    STEP.as_secs_f64(),
  )
  .await
}

pub async fn wifi(executor: &smol::Executor<'static>, end: chrono::DateTime<chrono::offset::Local>) -> Result<Vec<VectorResult>, Error> {
  // iwd exposes a dbus interface but not the signal strength.
  instant(
    executor,
    r#"
      # {bssid="00:11:22:33:44:55", device="wlan0", instance="localhost:9100", job="node_exporter", ssid="SSID"} 0
      0 * sum(node_wifi_station_info{mode="client"}) without (mode)
      + ignoring (ssid) group_left
      # {bssid="00:11:22:33:44:55", device="wlan0", instance="localhost:9100", job="node_exporter"} -47
      sum(label_replace(node_wifi_station_signal_dbm, "bssid","$1","mac_address", "(.+)")) without (mac_address)
      # {bssid="00:11:22:33:44:55", device="wlan0", instance="localhost:9100", job="node_exporter", ssid="SSID"} -47
    "#,
    end.timestamp(),
  )
  .await
}
