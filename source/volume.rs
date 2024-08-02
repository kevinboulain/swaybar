// https://gitlab.freedesktop.org/pipewire/wireplumber/-/issues/651
// No D-Bus interface exists to listen for changes in volume.
// Sometimes, PipeWire is acting up so I rely on PulseAudio tools for the times I need to fallback.

use smol::{io::AsyncBufReadExt as _, stream::StreamExt as _};

#[derive(Debug)]
pub enum Status {
  Mute,
  Volume(u8),
}

// https://github.com/pulseaudio/pulseaudio/blob/c1990dd02647405b0c13aab59f75d05cbb202336/src/utils/pactl.c#L2205
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Type {
  Change,
  #[serde(other)]
  Unknown,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Facility {
  Sink,
  #[serde(other)]
  Unknown,
}

#[derive(Debug, serde::Deserialize)]
struct Event {
  index: u32,
  event: Type,
  on: Facility,
}

#[derive(Debug, serde::Deserialize)]
struct Volume {
  value_percent: String,
}

#[derive(Debug, serde::Deserialize)]
struct Sink {
  index: u32,
  mute: bool,
  volume: std::collections::HashMap<String, Volume>,
}

pub async fn statuses() -> impl smol::stream::Stream<Item = std::io::Result<Status>> {
  let stream = async_stream::try_stream! {
  let mut pactl = smol::process::Command::new("pactl")
    .args(["--format", "json", "subscribe"])
    .stdout(smol::process::Stdio::piped())
    .spawn()?;
  let mut lines = smol::io::BufReader::new(
    pactl.stdout.take().unwrap(), // Unwrap: stdout is piped?
  )
  .split(b'\n');
  while let Some(line) = lines.next().await {
    let line = line?;
    log::trace!("pactl subscribe:\n{:?}", String::from_utf8_lossy(&line));
    if let Event {
      index,
      // It's important to aggressively filter upfront because running pactl will trigger another
      // new/remove client event, which will be fed back...
      event: Type::Change,
      on: Facility::Sink,
    } = serde_json::from_slice(&line)?
    {
      let sinks = smol::process::Command::new("pactl")
        .args(["--format", "json", "list", "sinks"])
        .output()
        .await?;
      log::trace!("pactl list sinks:\n{sinks:?}");
      for sink in serde_json::from_slice::<Vec<Sink>>(&sinks.stdout)?
        .iter()
        .filter(|sink| sink.index == index)
      {
        match sink.mute {
          true => {
            yield Status::Mute;
            break;
          }
          false => {
            if let Some(volume) = sink.volume.iter().next() {
              if let Some(volume) = volume.1.value_percent.strip_suffix('%') {
                if let Ok(volume) = volume.parse() {
                  yield Status::Volume(volume);
                  break;
                }
              }
            }
            log::debug!("pactl list sinks: no match for {index}");
          }
        }
      }
    }
  }
  // Push an error in case the stream unexpectedly ends (e.g.: the command exits because the
  // connection to the server is broken).
  let status = pactl.status().await?;
  Err(std::io::Error::new(
    std::io::ErrorKind::Other,
    format!("pactl subscribe ended: {status:?}"),
  ))?; // try_stream! doesn't allow to yield or return an error directly.
  };
  smol::stream::once(status().await) // Push the starting value.
    .chain(stream)
}

async fn status() -> std::io::Result<Status> {
  let mute = smol::process::Command::new("bash").args(["-c", "volume_mute_get"]).output().await?;
  match String::from_utf8_lossy(&mute.stdout).trim() {
    "no" => {
      let volume = smol::process::Command::new("bash").args(["-c", "volume_get"]).output().await?;
      match String::from_utf8_lossy(&volume.stdout).trim().parse() {
        Ok(volume) => Ok(Status::Volume(volume)),
        Err(error) => Err(std::io::Error::new(
          std::io::ErrorKind::Other,
          format!("couldn't get volume ({volume:?}, {error:?})"),
        )),
      }
    }
    "yes" => Ok(Status::Mute),
    _ => Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("couldn't get mute ({mute:?})"),
    )),
  }
}

pub async fn mute() -> std::io::Result<()> {
  let status = smol::process::Command::new("bash").args(["-c", "volume_mute"]).status().await?;
  match status.success() {
    true => Ok(()),
    false => Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("couldn't mute volume ({status:?})"),
    )),
  }
}

pub async fn up() -> std::io::Result<()> {
  let status = smol::process::Command::new("bash").args(["-c", "volume_up"]).status().await?;
  match status.success() {
    true => Ok(()),
    false => Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("couldn't up volume ({status:?})"),
    )),
  }
}

pub async fn down() -> std::io::Result<()> {
  let status = smol::process::Command::new("bash").args(["-c", "volume_down"]).status().await?;
  match status.success() {
    true => Ok(()),
    false => Err(std::io::Error::new(
      std::io::ErrorKind::Other,
      format!("couldn't down volume ({status:?})"),
    )),
  }
}
