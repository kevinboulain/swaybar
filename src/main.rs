#![allow(clippy::upper_case_acronyms)]

use async_stream::stream;
use bytes::{Buf, BytesMut};
use chrono::{self, Duration};
use serde::{Deserialize, Serialize};
use serde_json::{json, Deserializer};
use std::{io, pin::pin, str};
use tokio::{
    signal::unix::{signal, SignalKind},
    sync::watch,
    time::{interval, MissedTickBehavior},
};
use tokio_stream::StreamExt as _;
use tokio_util::codec::{Decoder, FramedRead};

mod dbus;
mod process;
mod prometheus;

macro_rules! fetch {
    ($interval:expr, $expression:expr, $forced:expr) => {{
        let mut interval = interval($interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut forced = $forced.clone();
        stream! {
          loop {
            tokio::select! {
              _ = interval.tick() => (),
              Ok(_) = forced.changed() => (),
            }
            yield $expression.await;
          }
        }
    }};
    ($interval:expr, $expression:expr) => {{
        let mut interval = interval($interval);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        stream! {
          loop {
            interval.tick().await;
            yield $expression.await;
          }
        }
    }};
}

#[derive(Debug)]
enum Select {
    Audio(f64),
    AudioError(String),
    Batteries(Vec<(String, f64)>),
    BatteriesError(String),
    Bluetooth((bool, Vec<(String, bool, String)>)),
    BluetoothError(String),
    ClickEvent(ClickEvent),
    ClickEventError(String),
    CPU([Option<f64>; prometheus::POINTS]),
    CPUError(String),
    Download([Option<f64>; prometheus::POINTS]),
    DownloadError(String),
    Refresh,
    Sigint,
    Temperature([Option<f64>; prometheus::POINTS]),
    TemperatureError(String),
    Upload([Option<f64>; prometheus::POINTS]),
    UploadError(String),
    WiFi(Vec<(String, f64)>),
    WiFiError(String),
}
use Select::*;

#[derive(Debug, Deserialize)]
struct ClickEvent {
    name: String,
    instance: Option<String>,
    button: i32,
}

#[derive(Default)]
struct ClickEventDecoder {}

impl Decoder for ClickEventDecoder {
    type Item = ClickEvent;
    type Error = io::Error; // Tokio requires an instance of io::Error...

    fn decode(&mut self, source: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        eprintln!("decode: {:?}", source);
        if source.starts_with(b"[\n") || source.starts_with(b"\n,") {
            source.advance(2);
        }
        let mut deserializer = Deserializer::from_slice(source).into_iter::<ClickEvent>();
        match deserializer.next() {
            Some(Ok(click_event)) => {
                // Tell Tokio some input has been consumed.
                source.advance(deserializer.byte_offset());
                Ok(Some(click_event))
            }
            None => Ok(None),                               // Done.
            Some(Err(error)) if error.is_eof() => Ok(None), // Should retry.
            Some(Err(_)) => Err(io::Error::new(io::ErrorKind::InvalidData, "")), // Abort
        }
    }
}

fn interpolate(minimum: f64, maximum: f64, value: f64) -> f64 {
    let value = f64::max(minimum, f64::min(maximum, value));
    (value - minimum) / (maximum - minimum)
}

// Can't yet const-concat arrays.
const NON_EMPTY_BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
const BARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

fn bars0(minimum: f64, maximum: f64, value: f64) -> char {
    let interpolated = interpolate(minimum, maximum, value);
    BARS[(interpolated * (BARS.len() - 1) as f64) as usize]
}

fn bars1(minimum: f64, maximum: f64, value: f64) -> char {
    let interpolated = interpolate(minimum, maximum, value);
    NON_EMPTY_BARS[(interpolated * (NON_EMPTY_BARS.len() - 1) as f64) as usize]
}

enum Color {
    Unspecified,
    Orange,
    Red,
}
use Color::*;

fn color<TS: ToString>(string: TS, color: Color) -> String {
    format!(
        r#"<span color="{}">{}</span>"#,
        match color {
            Unspecified => return string.to_string(),
            Orange => "orange",
            Red => "red",
        },
        string.to_string()
    )
}

#[derive(Debug, Serialize)]
struct Block {
    name: Option<String>,
    instance: Option<String>,
    full_text: String,
    markup: String,
}

impl Block {
    fn new(text: String) -> Self {
        Self {
            name: None,
            instance: None,
            full_text: text,
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let client = &prometheus::client();

    let mut sigint = signal(SignalKind::interrupt()).unwrap();

    // https://github.com/tokio-rs/tokio/issues/2466
    // This has a number of downsides, like breaking the stdin redirection (can be worked around
    // with a pipe).
    let stdin = tokio_fd::AsyncFd::try_from(libc::STDIN_FILENO).unwrap();
    let mut click_events = FramedRead::new(stdin, ClickEventDecoder::default());

    let (force, forced) = watch::channel(());

    let every_30s = Duration::seconds(30).to_std().unwrap();
    let every_5s = Duration::seconds(5).to_std().unwrap();
    let every_1s = Duration::seconds(1).to_std().unwrap();

    let mut refresh = interval(every_1s);
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut audio = pin!(fetch!(every_5s, process::audio(), forced));
    let mut batteries = pin!(dbus::power().await);
    let mut bluetooth = pin!(dbus::bluetooth().await);
    let mut cpu = pin!(fetch!(every_30s, prometheus::cpu(client)));
    let mut download = pin!(fetch!(every_30s, prometheus::download(client)));
    let mut temperature = pin!(fetch!(every_30s, prometheus::temperature(client)));
    let mut upload = pin!(fetch!(every_30s, prometheus::upload(client)));
    let mut wifi = pin!(fetch!(every_30s, prometheus::wifi(client)));

    let mut audio_block = None;
    let mut battery_blocks = Vec::new();
    let mut bluetooth_blocks = Vec::new();
    let mut cpu_block = None;
    let mut download_block = None;
    let mut errors = Vec::new();
    let mut temperature_block = None;
    let mut upload_block = None;
    let mut wifi_blocks = Vec::new();

    // For simplicity I assume stdout and stderr are never going to block.
    println!(
        "{}\n[",
        json!({
          "version": 1,
          "click_events": true,
        })
    );

    loop {
        // I couldn't manage to get StreamMap working (closure issue, as usual).
        // https://docs.rs/tokio/latest/tokio/macro.select.html
        // > Waits on multiple concurrent branches, returning when the first branch completes,
        // > cancelling the remaining branches.
        // https://github.com/tokio-rs/tokio/discussions/4416
        // Streams are always cancellation-safe.
        let next = tokio::select! {
          Some(result) = audio.next() => match result {
            Ok(result) => Audio(result),
            Err(error) => AudioError(error.to_string()),
          },
          Some(result) = batteries.next() => match result {
            Ok(result) => Batteries(result),
            Err(error) => BatteriesError(error.to_string()),
          },
          Some(result) = bluetooth.next() => match result {
            Ok(result) => Bluetooth(result),
            Err(error) => BluetoothError(error.to_string()),
          },
          Some(result) = click_events.next() => match result {
            Ok(result) => ClickEvent(result),
            Err(error) => ClickEventError(error.to_string()),
          },
          Some(result) = cpu.next() => match result {
            Ok(result) => CPU(result),
            Err(error) => CPUError(error.to_string()),
          },
          Some(result) = download.next() => match result {
            Ok(result) => Download(result),
            Err(error) => DownloadError(error.to_string()),
          },
          _ = refresh.tick() => Refresh,
          Some(_) = sigint.recv() => Sigint,
          Some(result) = temperature.next() => match result {
            Ok(result) => Temperature(result),
            Err(error) => TemperatureError(error.to_string()),
          },
          Some(result) = upload.next() => match result {
            Ok(result) => Upload(result),
            Err(error) => UploadError(error.to_string()),
          },
          Some(result) = wifi.next() => match result {
            Ok(result) => WiFi(result),
            Err(error) => WiFiError(error.to_string()),
          },
        };

        match next {
            Audio(volume) => {
                audio_block =
                    Some(Block::new(format!("{} Volume", bars0(0., 1., volume))).name("audio"))
            }
            Batteries(batteries) => {
                battery_blocks = batteries
                    .iter()
                    .map(|(name, charge)| {
                        let bar = bars1(0., 100., *charge); // The battery can't really reach 0.
                        let bar = color(
                            bar,
                            match *charge {
                                charge if charge <= 10. => Red,
                                charge if charge <= 30. => Orange,
                                _ => Unspecified,
                            },
                        );
                        Block::new(format!("{} {:.00}% {}", bar, *charge, name.to_lowercase()))
                    })
                    .collect()
            }
            Bluetooth((powered, connections)) => {
                bluetooth_blocks = if !powered {
                    vec![Block::new(format!("{} Bluetooth", BARS[0])).name("bluetooth")]
                } else if connections.is_empty() {
                    vec![Block::new(format!("{} Bluetooth", BARS[BARS.len() - 1])).name("bluetooth")]
                } else {
                    connections
                        .iter()
                        .map(|(name, connected, path)| {
                            // https://stackoverflow.com/a/73301647
                            // 0 means the device is within the golden range.
                            // Something around -25 (dBm?) results in a crappy connection (audio
                            // stops before the device disconnects for example) so use that as a
                            // lower threshold.
                            // let signal = interpolate(-25., 0., *rssi);
                            // let bar = color(
                            //     bars0(0., 1., signal),
                            //     match signal {
                            //         charge if charge <= 0.3 => Red,
                            //         charge if charge <= 0.5 => Orange,
                            //         _ => Unspecified,
                            //     },
                            // );
                            let bar = if *connected {
                                BARS[BARS.len() - 1]
                            } else {
                                BARS[0]
                            };
                            Block::new(format!("{} {}", bar, name.to_lowercase()))
                                .name("bluetooth")
                                .instance(path)
                        })
                        .collect()
                }
            }
            ClickEvent(event) => {
                eprintln!("click event: {:?}", event);
                match event.name.as_str() {
                    "audio" => {
                        let status = match event.button {
                            1 => process::volume_mute().await,
                            4 => process::volume_up().await,
                            5 => process::volume_down().await,
                            _ => continue,
                        };
                        eprintln!("click event status: {:?}", status);
                        force.send(()).unwrap() // TODO
                    }
                    "bluetooth" if event.button == 1 => {
                        let status = dbus::bluetooth_toggle(event.instance).await;
                        eprintln!("click event status: {:?}", status);
                    }
                    _ => (),
                }
            }
            CPU(utilization) => {
                let bars: Vec<String> = utilization
                    .iter()
                    .map(|option| {
                        option.map_or(BARS[0].to_string(), |utilization| {
                            color(
                                bars0(0., 1., utilization),
                                match utilization {
                                    utilization if utilization >= 0.7 => Red,
                                    utilization if utilization >= 0.3 => Orange,
                                    _ => Unspecified,
                                },
                            )
                        })
                    })
                    .collect();
                cpu_block = Some(Block::new(format!(
                    "{} {:.00}% CPU",
                    bars.join(""),
                    utilization[utilization.len() - 1].unwrap_or(0.) * 100.
                )));
            }
            Refresh => {
                let mut blocks = Vec::new();
                let error_block = errors.pop().map(|error| Block::new(color(error, Red)));
                if let Some(block) = error_block.as_ref() {
                    blocks.push(block);
                }
                if let Some(block) = upload_block.as_ref() {
                    blocks.push(block);
                }
                if let Some(block) = download_block.as_ref() {
                    blocks.push(block);
                }
                for block in &wifi_blocks {
                    blocks.push(block);
                }
                if let Some(block) = temperature_block.as_ref() {
                    blocks.push(block);
                }
                if let Some(block) = cpu_block.as_ref() {
                    blocks.push(block);
                }
                for block in &battery_blocks {
                    blocks.push(block);
                }
                if let Some(block) = audio_block.as_ref() {
                    blocks.push(block);
                }
                for block in &bluetooth_blocks {
                    blocks.push(block);
                }
                let date_block =
                    Block::new(chrono::offset::Local::now().format("%T %A %F").to_string());
                blocks.push(&date_block);
                println!("{},", serde_json::to_string(&blocks).unwrap()) // unwrap: why would it fail?
            }
            Download(bytes) => {
                let bars: String = bytes
                    .iter()
                    .map(|option| option.map_or(BARS[0], |bytes| bars0(0., 1_000_000., bytes))) // 1M is interesting but not too large
                    .collect();
                download_block = Some(Block::new(format!(
                    "{} {:.02}M Download",
                    bars,
                    bytes[bytes.len() - 1].unwrap_or(0.) / 1_000_000.
                )));
            }
            Sigint => break,
            Temperature(degrees) => {
                let bars: Vec<String> = degrees
                    .iter()
                    .map(|option| {
                        option.map_or(BARS[0].to_string(), |degrees| {
                            color(
                                bars0(30., 100., degrees), // Unlikely to be less than 30°C.
                                match degrees {
                                    degrees if degrees >= 70. => Red,
                                    degrees if degrees >= 50. => Orange,
                                    _ => Unspecified,
                                },
                            )
                        })
                    })
                    .collect();
                temperature_block = Some(Block::new(format!(
                    "{} {:.00}°C",
                    bars.join(""),
                    degrees[degrees.len() - 1].unwrap_or(0.)
                )));
            }
            Upload(bytes) => {
                let bars: String = bytes
                    .iter()
                    .map(|option| option.map_or(BARS[0], |bytes| bars0(0., 1_000_000., bytes))) // 1M is interesting but not too large
                    .collect();
                upload_block = Some(Block::new(format!(
                    "{} {:.02}M Upload",
                    bars,
                    bytes[bytes.len() - 1].unwrap_or(0.) / 1_000_000.
                )));
            }
            WiFi(ssids) => {
                wifi_blocks = ssids
                    .iter()
                    .map(|(name, dbm)| {
                        // TODO: device specific
                        //  https://github.com/bmegli/wifi-scan/issues/18
                        //  https://www.intuitibits.com/2016/03/23/dbm-to-percent-conversion/
                        let signal = interpolate(-110., -40., *dbm);
                        let bar = color(
                            bars0(0., 1., signal),
                            match signal {
                                charge if charge <= 0.3 => Red,
                                charge if charge <= 0.5 => Orange,
                                _ => Unspecified,
                            },
                        );
                        Block::new(format!("{} {}", bar, name))
                    })
                    .collect()
            }
            ref whole @ (AudioError(ref error)
            | BatteriesError(ref error)
            | BluetoothError(ref error)
            | ClickEventError(ref error)
            | CPUError(ref error)
            | DownloadError(ref error)
            | TemperatureError(ref error)
            | UploadError(ref error)
            | WiFiError(ref error)) => {
                eprintln!("update error: {:?}", whole);
                errors.push(error.to_owned())
            }
        }
    }

    eprintln!("stopping");
}
