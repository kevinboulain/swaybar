use smol::stream::StreamExt as _;

pub type Metric = std::collections::HashMap<String, String>;

#[derive(Debug, serde::Deserialize)]
pub struct Value(pub i64, pub String);

// { "status": "success",
//   "data": { "resultType": "matrix",
//             "result": [ { "metric": { "instance": "localhost:9100", "job": "node_exporter" },
//                           "values": [ [ 1720256580, "83" ],
//                                       [ 1720256640, "48" ],
//                                       [ 1720256700, "47" ],
//                                       [ 1720256760, "47" ],
//                                       [ 1720256820, "45" ],
//                                       [ 1720256880, "45" ] ] } ]
//           }
// }
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MatrixResult {
  pub metric: Metric,
  pub values: Vec<Value>,
}

// { "status": "success",
//   "data": { "resultType": "vector",
//             "result": [ { "metric": { "bssid": "00:11:22:33:44:55", "device": "wlan0", "instance": "localhost:9100", "job": "node_exporter", "ssid":"SSID" },
//                           "value": [ 1720256880.334, "-50" ] } ]
//           }
// }
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VectorResult {
  pub metric: Metric,
  pub value: Value,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(tag = "resultType", content = "result")]
enum Data {
  Matrix(Vec<MatrixResult>),
  Vector(Vec<VectorResult>),
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
enum Status {
  Success,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Response {
  #[allow(dead_code)] // For deserialization only.
  status: Status,
  data: Data,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
  #[error("HTTP error")]
  HTTP(#[from] http::Error),
  #[error("Hyper error")]
  Hyper(#[from] hyper::Error),
  #[error("IO error")]
  IO(#[from] std::io::Error),
  #[error("JSON error")]
  JSON(#[from] serde_json::Error),
}

async fn common(
  executor: &smol::Executor<'static>,
  query: &str,
  uri: http::uri::Builder,
  body: &mut form_urlencoded::Serializer<'_, String>,
) -> std::result::Result<Response, Error> {
  let uri = uri.scheme("http").authority("localhost:9090").build()?;
  let request = http::Request::builder()
    .uri(&uri)
    .header(
      http::header::HOST,
      uri
        .authority()
        .unwrap() // Unwrap: set above.
        .as_str(),
    )
    .header(http::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
    .method(http::method::Method::POST)
    .body(body.append_pair("query", query).finish())?;

  // https://github.com/smol-rs/smol/blob/master/examples/hyper-client.rs
  assert_eq!(request.uri().scheme(), Some(&http::uri::Scheme::HTTP));
  let host = request.uri().host().unwrap(); // Unwrap: set above.
  let stream = {
    let port = request.uri().port_u16().unwrap_or(80);
    smol::net::TcpStream::connect((host, port)).await?
  };
  let (mut sender, connection) = hyper::client::conn::http1::handshake(smol_hyper::rt::FuturesIo::new(stream)).await?;
  executor
    .spawn(async move {
      // From my understanding, this is weird but okay: internally, hyper uses a channel to
      // communicate between the sender and the connection, so if the connection gets closed
      // the channel should too, which would bubble up to the sender.
      if let Err(e) = connection.await {
        log::warn!("Connection failed: {:?}", e);
      }
    })
    .detach();
  let response = sender.send_request(request).await?;
  log::trace!("HTTP response: {response:?}");
  let body: std::result::Result<Vec<u8>, _> = http_body_util::BodyStream::new(response.into_body())
    .try_fold(Vec::new(), |mut body, chunk| {
      if let Some(chunk) = chunk.data_ref() {
        body.extend_from_slice(chunk);
      }
      Ok(body)
    })
    .await;
  Ok(serde_json::from_slice(&body?)?)
}

pub async fn instant(executor: &smol::Executor<'static>, query: &str, time: i64) -> std::result::Result<Vec<VectorResult>, Error> {
  Ok(
    match common(
      executor,
      query,
      http::uri::Builder::new().path_and_query("/api/v1/query"),
      form_urlencoded::Serializer::new(String::new()).append_pair("time", &time.to_string()),
    )
    .await?
    .data
    {
      Data::Vector(results) => results,
      _ => unreachable!(),
    },
  )
}

pub async fn range(
  executor: &smol::Executor<'static>,
  query: &str,
  start: i64,
  end: i64,
  step: f64,
) -> std::result::Result<Vec<MatrixResult>, Error> {
  Ok(
    match common(
      executor,
      query,
      http::uri::Builder::new().path_and_query("/api/v1/query_range"),
      form_urlencoded::Serializer::new(String::new())
        .append_pair("start", &start.to_string())
        .append_pair("end", &end.to_string())
        .append_pair("step", &step.to_string()),
    )
    .await?
    .data
    {
      Data::Matrix(results) => results,
      _ => unreachable!(),
    },
  )
}
