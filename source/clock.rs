use smol::stream::StreamExt as _;

pub async fn statuses() -> impl smol::stream::Stream<Item = String> {
  smol::Timer::interval_at(std::time::Instant::now(), std::time::Duration::from_secs(1))
    .map(|_| chrono::offset::Local::now().format("%T %A %F").to_string())
}
