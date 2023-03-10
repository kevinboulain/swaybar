use lazy_static::lazy_static;
use regex::bytes::Regex;
use std::{io, process::ExitStatus, str};
use tokio::process::Command;

pub async fn audio() -> io::Result<f64> {
    let volume = Command::new("wpctl")
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .output();

    lazy_static! {
        static ref VOLUME: Regex = Regex::new(r"Volume: (\d\.\d+)( \[MUTED\])?").unwrap();
    }

    let volume = if let Some(matches) = VOLUME.captures(&volume.await?.stdout[..]) {
        if matches.get(2).is_some() {
            0.
        } else {
            str::from_utf8(&matches[1]).unwrap().parse().unwrap() // unwrap: mostly validated by the regex
        }
    } else {
        return Err(io::Error::new(io::ErrorKind::InvalidData, ""));
    };
    Ok(volume)
}

pub async fn volume_mute() -> io::Result<ExitStatus> {
    Command::new("bash")
        .args(["-c", "volume_mute"])
        .status()
        .await
}

pub async fn volume_up() -> io::Result<ExitStatus> {
    Command::new("bash")
        .args(["-c", "volume_up"])
        .status()
        .await
}

pub async fn volume_down() -> io::Result<ExitStatus> {
    Command::new("bash")
        .args(["-c", "volume_down"])
        .status()
        .await
}

pub async fn bluetooth_toggle() -> io::Result<ExitStatus> {
    Command::new("bash")
        .args([
            "-c",
            r#"(if bluetoothctl show | grep 'Powered: no'; then
                  bluetoothctl power on
                else
                  bluetoothctl power off
               fi) &> /dev/null
            "#,
        ])
        .status()
        .await
}

// pub async fn bluetooth_rssi(address: &str) -> io::Result<Option<f64>> {
//     let rssi = Command::new("hcitool").args(&["rssi", address]).output();
//
//     lazy_static! {
//         static ref RSSI: Regex = Regex::new(r"RSSI return value: (-?\d+)").unwrap();
//     }
//
//     Ok(
//         if let Some(matches) = RSSI.captures(&rssi.await?.stdout[..]) {
//             Some(str::from_utf8(&matches[1]).unwrap().parse().unwrap()) // unwrap: mostly validated by the regex
//         } else {
//             None
//         },
//     )
// }
