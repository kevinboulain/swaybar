use smol::{
  io::{AsyncBufReadExt as _, AsyncWriteExt as _},
  stream::StreamExt as _,
};

mod clock;
mod dbus;
mod prometheus;
mod volume;

fn interpolate(minimum: f64, maximum: f64, value: f64) -> f64 {
  let value = minimum.max(maximum.min(value));
  (value - minimum) / (maximum - minimum)
}

const BARS0: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const BARS1: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn bars0(minimum: f64, maximum: f64, value: f64) -> char {
  let interpolated = interpolate(minimum, maximum, value);
  BARS0[(interpolated * (BARS0.len() - 1) as f64) as usize]
}

fn bars1(minimum: f64, maximum: f64, value: f64) -> char {
  let interpolated = interpolate(minimum, maximum, value);
  BARS1[(interpolated * (BARS1.len() - 1) as f64) as usize]
}

enum Color {
  Unspecified,
  Orange,
  Red,
}

fn color<TS: ToString>(string: TS, color: Color) -> String {
  format!(
    r#"<span color="{}">{}</span>"#,
    match color {
      Color::Unspecified => return string.to_string(),
      Color::Orange => "orange",
      Color::Red => "red",
    },
    string.to_string()
  )
}

#[derive(Clone, Debug, serde::Serialize)]
struct Block {
  name: Option<String>,
  instance: Option<String>,
  full_text: String,
  markup: String,
}

impl Block {
  fn new(full_text: &str) -> Self {
    Self {
      name: None,
      instance: None,
      full_text: full_text.to_string(),
      markup: "pango".to_string(),
    }
  }

  fn name(mut self, name: &str) -> Self {
    self.name = Some(name.to_string());
    self
  }

  fn instance(mut self, instance: &str) -> Self {
    assert!(self.name.is_some());
    self.instance = Some(instance.to_string());
    self
  }
}

#[derive(Debug, Default)]
struct Blocks {
  bluez: Vec<Block>,
  clock: Option<Block>,
  cpu: Option<Block>,
  download: Option<Block>,
  error: Option<Block>,
  temperature: Option<Block>,
  upload: Option<Block>,
  upower: Vec<Block>,
  volume: Option<Block>,
  wifi: Option<Block>,
}

// TODO: This workaround is unfortunate, is that really necessary?
// https://stackoverflow.com/a/78410928
pub struct RefCellGuard<T: ?Sized>(std::cell::RefCell<T>);

impl<T> RefCellGuard<T> {
  pub const fn new(value: T) -> Self {
    Self(std::cell::RefCell::new(value))
  }
}

impl<T: ?Sized> RefCellGuard<T> {
  pub fn borrow<R, F: FnOnce(&T) -> R>(&self, function: F) -> R {
    function(&mut self.0.borrow())
  }
  pub fn borrow_mut<R, F: FnOnce(&mut T) -> R>(&self, function: F) -> R {
    function(&mut self.0.borrow_mut())
  }
}

#[derive(Debug, thiserror::Error)]
enum BlockUpdateError {
  #[error("IO error")]
  IO(#[from] std::io::Error),
  #[error("JSON error")]
  JSON(#[from] serde_json::Error),
  #[error("Prometheus error")]
  Prometheus(#[from] prometheus::Error),
  #[error("ZBus error")]
  ZBus(#[from] zbus::Error),
}

#[derive(Debug)]
enum BlockUpdate {
  Click(Click),
  Error(BlockUpdateError),
  Publish,
  Rebuild,
}

type BlockUpdateStream<'b> = std::pin::Pin<Box<dyn smol::stream::Stream<Item = BlockUpdate> + 'b>>;
type BlocksGuard = RefCellGuard<Blocks>;

async fn bluez(blocks: &BlocksGuard) -> BlockUpdateStream<'_> {
  dbus::bluez::statuses()
    .await
    .map(|status| {
      let (bluez, update) = match status {
        Ok(status) => {
          let block_name = "bluez";
          (
            match status {
              dbus::bluez::Status { powered: false, .. } => vec![Block::new(&format!("{} Bluetooth", BARS0[0])).name(block_name)],
              dbus::bluez::Status {
                powered: true, devices, ..
              } if devices.is_empty() => vec![Block::new(&format!("{} Bluetooth", BARS0[BARS0.len() - 1])).name(block_name)],
              dbus::bluez::Status { devices, .. } => devices
                .iter()
                .map(|dbus::bluez::DeviceStatus { name, connected, path }| {
                  let bar = if *connected { BARS0[BARS0.len() - 1] } else { BARS0[0] }; // https://stackoverflow.com/a/73301647
                  Block::new(&format!("{bar} {name}")).name(block_name).instance(path.as_str())
                })
                .collect(),
            },
            BlockUpdate::Publish,
          )
        }
        Err(error) => (Vec::new(), BlockUpdate::Error(error.into())),
      };
      blocks.borrow_mut(|blocks| blocks.bluez = bluez);
      update
    })
    .boxed_local()
}

#[derive(Debug, serde::Deserialize)]
struct Click {
  name: String,
  instance: Option<String>,
  button: i32,
}

async fn clicks<'b>() -> BlockUpdateStream<'b> {
  futures_util::io::BufReader::new(smol::Unblock::new(std::io::stdin()))
    .lines()
    .filter_map(|line| {
      log::debug!("From sway: {line:?}");
      match line {
        Ok(line) if line == "[" => None, // Start of the infinite array.
        Ok(line) => match serde_json::from_str(
          line
            // All elements after the first one start with ','.
            .strip_prefix(',')
            .unwrap_or(&line),
        ) {
          Ok(click) => Some(BlockUpdate::Click(click)),
          Err(error) => Some(BlockUpdate::Error(error.into())),
        },
        Err(error) => Some(BlockUpdate::Error(error.into())),
      }
    })
    .boxed_local()
}

async fn clock(blocks: &BlocksGuard) -> BlockUpdateStream<'_> {
  clock::statuses()
    .await
    .map(|clock| {
      blocks.borrow_mut(|blocks| blocks.clock = Some(Block::new(&clock)));
      BlockUpdate::Publish
    })
    .boxed_local()
}

struct ErrorSender(async_channel::Sender<BlockUpdateError>);

impl ErrorSender {
  // Because it's not asynchronous and because the queue is bounded, only
  // async_channel::Sender::force_send should be used to avoid any deadlock.
  // It doesn't matter much if an error is lost when too many are fired at once.
  pub fn force_send(&self, error: BlockUpdateError) -> Result<(), async_channel::SendError<BlockUpdateError>> {
    if let Some(error) = self.0.force_send(error)? {
      log::debug!("Dropped unreceived error: {error:?}");
    }
    Ok(())
  }
}

async fn error(blocks: &BlocksGuard) -> (ErrorSender, BlockUpdateStream<'_>) {
  let (sender, receiver) = async_channel::bounded(1);
  let stream = async_stream::stream! {
    #[derive(Debug)]
    enum Event {
      Receive(BlockUpdateError),
      Tick,
    }

    let mut receiver = receiver.map(Event::Receive).boxed_local();
    let mut timer = smol::Timer::interval_at(std::time::Instant::now(), std::time::Duration::from_secs(1))
    .map(|_| Event::Tick)
    .boxed_local();
    let mut ticks = 0;
    loop {
      let (event, index, _) = futures_util::future::select_all([receiver.next(), timer.next()]).await;
      assert!(event.is_some(), "End of stream {index}");
      if let Some(event) = event {
        match event {
          Event::Receive(error) => {
            ticks = 0;
            blocks.borrow_mut(|blocks| blocks.error = Some(Block::new(&color(format!("{error:?}"), Color::Red))));
            yield BlockUpdate::Publish;
          }
          Event::Tick => {
            ticks += <bool as Into<u32>>::into(blocks.borrow_mut(|blocks| blocks.error.is_some()));
            if ticks >= 10 {
              ticks = 0;
              blocks.borrow_mut(|blocks| blocks.error = None);
              yield BlockUpdate::Rebuild;
            }
          }
        };
      }
    }
  }
  .boxed_local();
  (ErrorSender(sender), stream)
}

async fn prometheus<'b, const POINTS: usize>(executor: &'b smol::Executor<'static>, blocks: &'b BlocksGuard) -> BlockUpdateStream<'b> {
  async_stream::stream! {
  let mut timer = smol::Timer::interval(std::time::Duration::from_secs(30));
  loop {
    let end = chrono::offset::Local::now();
    let start = end
      - (<usize as TryInto<u32>>::try_into(POINTS - 1).unwrap() // Unwrap: ack.
        * prometheus::STEP);
    // All the calls could be made in parallel with a type wrapper and
    // futures_util::future::try_join_all but latency or fine error handling doesn't matter much
    // here.
    yield Ok::<
      (
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::VectorResult>,
      ),
      prometheus::Error,
    >((
      prometheus::cpu(executor, start, end).await?,
      prometheus::download(executor, start, end).await?,
      prometheus::temperature(executor, start, end).await?,
      prometheus::upload(executor, start, end).await?,
      prometheus::wifi(executor, end).await?,
    ));

    timer.next().await;
  }
  }
  .map(
    move |statuses: Result<
      (
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::MatrixResult>,
        Vec<prometheus::VectorResult>,
      ),
      _,
    >| {
      let (cpu, download, temperature, upload, wifi, update) = match statuses {
        Ok((cpu, download, temperature, upload, wifi)) => {
          let pad = |matrix: Vec<prometheus::MatrixResult>, default| -> Option<[f64; POINTS]> {
            let matrix = matrix
              .first() // It's assumed there's only one metric.
              .map(|result| &result.values[..])
              .unwrap_or(&[]);
            if matrix.is_empty() {
              return None;
            }
            let mut padded = std::iter::once((0, default)).cycle().take(POINTS - matrix.len()).chain(
              matrix
                .iter()
                .map(|prometheus::Value(timestamp, value)| (*timestamp, value.parse().unwrap_or(default))),
            );
            Some(std::array::from_fn(|_| padded.next().unwrap().1)) // Unwrap: padded iterator.
          };
          let cpu = pad(cpu, 0.).map(|cpu| {
            Block::new(&format!(
              "{} {:.00}% CPU",
              cpu
                .map(|utilization| {
                  color(
                    bars0(0., 1., utilization),
                    match utilization {
                      utilization if utilization >= 0.7 => Color::Red,
                      utilization if utilization >= 0.3 => Color::Orange,
                      _ => Color::Unspecified,
                    },
                  )
                })
                .join(""),
              cpu[cpu.len() - 1] * 100.
            ))
          });
          let download = pad(download, 0.).map(|download| {
            Block::new(&format!(
              "{} {:.02}M Download",
              // 1M is interesting but not too large.
              download.map(|bytes| bars0(0., 1_000_000., bytes)).iter().collect::<String>(),
              download[download.len() - 1] / 1_000_000.
            ))
          });
          let temperature = pad(temperature, 0.).map(|temperature| {
            Block::new(&format!(
              "{} {:.00}°C",
              temperature
                .map(|degrees| {
                  color(
                    bars0(30., 100., degrees), // Unlikely to be less than 30°C.
                    match degrees {
                      degrees if degrees >= 70. => Color::Red,
                      degrees if degrees >= 50. => Color::Orange,
                      _ => Color::Unspecified,
                    },
                  )
                })
                .join(""),
              temperature[temperature.len() - 1]
            ))
          });
          let upload = pad(upload, 0.).map(|upload| {
            Block::new(&format!(
              "{} {:.02}M Upload",
              // 1M is interesting but not too large.
              upload.map(|bytes| bars0(0., 1_000_000., bytes)).iter().collect::<String>(),
              upload[upload.len() - 1] / 1_000_000.
            ))
          });
          let wifi = wifi
            .first() // It's assumed there's only one metric.
            .and_then(|result| {
              // TODO: device specific
              //  https://github.com/bmegli/wifi-scan/issues/18
              //  https://www.intuitibits.com/2016/03/23/dbm-to-percent-conversion/
              let signal = interpolate(-110., -40., result.value.1.parse().ok()?);
              let bar = color(
                bars0(0., 1., signal),
                match signal {
                  signal if signal <= 0.3 => Color::Red,
                  signal if signal <= 0.5 => Color::Orange,
                  _ => Color::Unspecified,
                },
              );
              let ssid = result.metric.get("ssid")?;
              Some(Block::new(&format!("{bar} {ssid}")))
            });
          (cpu, download, temperature, upload, wifi, BlockUpdate::Publish)
        }
        Err(error) => (None, None, None, None, None, BlockUpdate::Error(error.into())),
      };
      blocks.borrow_mut(|blocks| {
        blocks.cpu = cpu;
        blocks.download = download;
        blocks.temperature = temperature;
        blocks.upload = upload;
        blocks.wifi = wifi;
      });
      update
    },
  )
  .boxed_local()
}

async fn upower(blocks: &BlocksGuard) -> BlockUpdateStream<'_> {
  dbus::upower::statuses()
    .await
    .map(|statuses| {
      let (upower, update) = match statuses {
        Ok(statuses) => {
          (
            statuses
              .iter()
              .map(|dbus::upower::Status { device, percentage }| {
                let bar = bars1(0., 100., *percentage); // The battery can't really reach 0.
                let bar = color(
                  bar,
                  match *percentage {
                    percentage if percentage <= 10. => Color::Red,
                    percentage if percentage <= 30. => Color::Orange,
                    _ => Color::Unspecified,
                  },
                );
                Block::new(&format!("{bar} {percentage:.00}% {device}"))
              })
              .collect(),
            BlockUpdate::Publish,
          )
        }
        Err(error) => (Vec::new(), BlockUpdate::Error(error.into())),
      };
      blocks.borrow_mut(|blocks| blocks.upower = upower);
      update
    })
    .boxed_local()
}

async fn volume(blocks: &BlocksGuard) -> BlockUpdateStream<'_> {
  volume::statuses()
    .await
    .map(|status| {
      let (volume, update) = match status {
        Ok(status) => (
          Some(
            Block::new(&format!(
              "{} Volume",
              bars0(
                0.,
                100.,
                match status {
                  volume::Status::Mute => 0.,
                  volume::Status::Volume(volume) => volume.into(),
                }
              )
            ))
            .name("volume"),
          ),
          BlockUpdate::Publish,
        ),
        Err(error) => (None, BlockUpdate::Error(error.into())),
      };
      blocks.borrow_mut(|blocks| blocks.volume = volume);
      update
    })
    .boxed_local()
}

fn main() -> std::io::Result<()> {
  // Technically, stderr could block... For simplicity's sake (and because I don't want to roll my
  // own logging framework), let's ignore it.
  env_logger::init();

  let executor = smol::Executor::new();
  smol::block_on(executor.run(async {
    let mut stdout = smol::Unblock::new(std::io::stdout());
    stdout
      .write_all(&serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "click_events": true,
      }))?)
      .await?;
    stdout.write_all(b"\n[").await?;

    let blocks = BlocksGuard::new(Blocks::default());

    let (error_sender, error) = error(&blocks).await;
    let mut infallible_streams = [clock(&blocks).await, error];
    let fallible_futures: &[Box<
      dyn for<'b> Fn(
        &'b smol::Executor<'static>,
        &'b BlocksGuard,
      ) -> std::pin::Pin<Box<dyn std::future::Future<Output = BlockUpdateStream<'b>> + 'b>>,
    >] = &[
      Box::new(|_, blocks| Box::pin(bluez(blocks))),
      Box::new(|_, _| Box::pin(clicks())),
      Box::new(|executor, blocks| Box::pin(prometheus::<5>(executor, blocks))),
      Box::new(|_, blocks| Box::pin(upower(blocks))),
      Box::new(|_, blocks| Box::pin(volume(blocks))),
    ];
    let mut fallible_streams = futures_util::future::join_all(fallible_futures.iter().map(|future| future(&executor, &blocks))).await;
    // It doesn't seem possible to use a single pending stream or future.
    let mut pending_streams = (0..fallible_streams.len())
      .map(|_| smol::stream::pending().boxed_local())
      .collect::<Vec<_>>();
    let mut failed_streams = vec![false; fallible_streams.len()];

    loop {
      let (refresh, index, _) = futures_util::future::select_all(
        fallible_streams
          .iter_mut()
          .chain(infallible_streams.iter_mut())
          .map(|stream| stream.next()),
      )
      .await;
      match refresh {
        Some(BlockUpdate::Click(
          ref click @ Click {
            ref name,
            ref instance,
            button,
          },
        )) => {
          log::trace!("Click from stream {index:?}: {click:?}");
          match name.as_str() {
            "bluez" => match button {
              // TODO: Toggling an unreachable device might block for a little while.
              1 => match dbus::bluez::toggle(
                instance
                  .clone()
                  .map(|path| {
                    zvariant::ObjectPath::try_from(path)
                      // Unwrap: the ObjectPath was converted to a string when declaring the block.
                      .unwrap()
                  })
                  .as_ref(),
              )
              .await
              {
                Ok(()) => (),
                Err(error) => {
                  log::warn!("Failed to handle event: {error:?}");
                  error_sender.force_send(error.into()).unwrap(); // Unwrap: the receiver won't close.
                }
              },
              button => log::trace!("Unhandled button: {button:?}"),
            },
            "volume" => match match button {
              1 => volume::mute().await,
              4 => volume::up().await,
              5 => volume::down().await,
              button => {
                log::trace!("Unhandled button: {button:?}");
                Ok(())
              }
            } {
              Ok(()) => (),
              Err(error) => {
                log::warn!("Failed to handle event: {error:?}");
                error_sender.force_send(error.into()).unwrap(); // Unwrap: the receiver won't close.
              }
            },
            name => log::warn!("Unhandled event: {name:?}"),
          }
        }
        Some(BlockUpdate::Error(error)) => {
          // A fallible stream will end right after an error.
          log::warn!("Error from stream {index:?}: {error:?}");
          error_sender.force_send(error).unwrap(); // Unwrap: the receiver won't close.
        }
        Some(BlockUpdate::Publish) => {
          // A stream published something.
          log::trace!("Publish from stream {index:?}");
          // TODO: Avoid bursts with some caching (sway CPU usage spikes a bit).
          let bar = blocks.borrow(
            |Blocks {
               bluez,
               clock,
               cpu,
               download,
               error,
               temperature,
               upload,
               upower,
               volume,
               wifi,
             }| {
              error
                .iter()
                .chain(upload.iter())
                .chain(download.iter())
                .chain(wifi.iter())
                .chain(temperature.iter())
                .chain(cpu.iter())
                .chain(upower.iter())
                .chain(volume.iter())
                .chain(bluez.iter())
                .chain(clock.iter())
                .cloned()
                .collect::<Vec<_>>()
            },
          );
          stdout.write_all(&serde_json::to_vec(&bar)?).await?;
          stdout.write_all(b",\n").await?;
        }
        Some(BlockUpdate::Rebuild) => {
          // After a backoff period, an ended fallible stream is rebuilt.
          log::trace!("Rebuild from stream {index:?}");
          for index in 0..fallible_streams.len() {
            if failed_streams[index] {
              pending_streams[index] = fallible_futures[index](&executor, &blocks).await;
              failed_streams[index] = false;
              std::mem::swap(&mut fallible_streams[index], &mut pending_streams[index]);
            }
          }
        }
        None => {
          // A fallible stream has ended, it shouldn't be polled anymore (or it will immediately
          // return None again).
          log::trace!("End of stream {index:?}");
          assert!(index < fallible_streams.len() && !failed_streams[index]);
          failed_streams[index] = true;
          std::mem::swap(&mut fallible_streams[index], &mut pending_streams[index]);
        }
      }
    }
  }))
}
