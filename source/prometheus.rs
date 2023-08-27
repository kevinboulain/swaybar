use chrono::Duration;
use prometheus_http_query::{response::InstantVector, response::PromqlResult, Client, Error};

// Assume a 1m window everywhere.
pub const POINTS: usize = 5;

pub fn client() -> Client {
    Client::default()
}

async fn instant_query(client: &Client, query: &str) -> Result<PromqlResult, Error> {
    client.query(query).post().await
}

async fn range_query(client: &Client, query: &str) -> Result<PromqlResult, Error> {
    let end = chrono::offset::Local::now();
    let start = end - Duration::minutes(POINTS.try_into().unwrap()); // unwrap: global constant
    let step = 60.;
    client
        .query_range(query, start.timestamp(), end.timestamp(), step)
        .post()
        .await
}

fn array_from_matrix(result: PromqlResult) -> [Option<f64>; POINTS] {
    let mut array: [Option<f64>; POINTS] = Default::default();
    let values = result.data().as_matrix().unwrap(); // unwrap: known query
    if values.is_empty() {
        return array;
    }
    let mut values = values[0]
        .samples()
        .iter()
        .map(|sample| sample.value())
        .rev();
    for index in (0..array.len()).rev() {
        array[index] = values.next()
    }
    array
}

fn vector_from_instant<Map>(result: PromqlResult, map: Map) -> Vec<(String, f64)>
where
    Map: FnMut(&InstantVector) -> (String, f64),
{
    result.data().as_vector().unwrap().iter().map(map).collect() // unwrap: known query
}

pub async fn cpu(client: &Client) -> Result<[Option<f64>; POINTS], Error> {
    const QUERY: &str = r#"
        avg (sum (rate(node_cpu_seconds_total{mode!="idle"}[1m])) without (mode)) without (cpu)
    "#;
    Ok(array_from_matrix(range_query(client, QUERY).await?))
}

pub async fn download(client: &Client) -> Result<[Option<f64>; POINTS], Error> {
    const QUERY: &str = r#"
        rate(node_network_receive_bytes_total{device="wlan0"}[1m])
    "#;
    Ok(array_from_matrix(range_query(client, QUERY).await?))
}

pub async fn temperature(client: &Client) -> Result<[Option<f64>; POINTS], Error> {
    const QUERY: &str = r#"
        max (max_over_time(node_thermal_zone_temp[1m])) without (type, zone)
    "#;
    Ok(array_from_matrix(range_query(client, QUERY).await?))
}

pub async fn upload(client: &Client) -> Result<[Option<f64>; POINTS], Error> {
    const QUERY: &str = r#"
        rate(node_network_transmit_bytes_total{device="wlan0"}[1m])
    "#;
    Ok(array_from_matrix(range_query(client, QUERY).await?))
}

pub async fn wifi(client: &Client) -> Result<Vec<(String, f64)>, Error> {
    const QUERY: &str = r#"
        # {bssid="00:11:22:33:44:55", device="wlan0", instance="localhost:9100", job="node_exporter", ssid="SSID"} 0
        0 * sum(node_wifi_station_info{mode="client"}) without (mode)
        + ignoring (ssid) group_left
        # {bssid="00:11:22:33:44:55", device="wlan0", instance="localhost:9100", job="node_exporter"} -47
        sum(label_replace(node_wifi_station_signal_dbm, "bssid","$1","mac_address", "(.+)")) without (mac_address)
        # {bssid="00:11:22:33:44:55", device="wlan0", instance="localhost:9100", job="node_exporter", ssid="SSID"} -47
    "#;
    let unknown = "unknown SSID".to_string();
    let mut ssids = vector_from_instant(instant_query(client, QUERY).await?, |vector| {
        (
            vector.metric().get("ssid").unwrap_or(&unknown).to_string(),
            vector.sample().value(),
        )
    });
    ssids.sort_by(|(name1, _), (name2, _)| name1.cmp(name2));
    Ok(ssids)
}
