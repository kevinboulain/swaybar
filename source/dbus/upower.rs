use smol::stream::StreamExt as _;

#[zbus::proxy(
  default_service = "org.freedesktop.UPower",
  default_path = "/org/freedesktop/UPower",
  interface = "org.freedesktop.UPower"
)]
trait UPower {
  fn enumerate_devices(&self) -> zbus::Result<Vec<zvariant::OwnedObjectPath>>;
  #[zbus(signal)]
  fn device_added(&self) -> zbus::Result<()>;
  #[zbus(signal)]
  fn device_removed(&self) -> zbus::Result<()>;
}

#[zbus::proxy(default_service = "org.freedesktop.UPower", interface = "org.freedesktop.UPower.Device")]
trait Device {
  #[zbus(property)]
  fn is_present(&self) -> zbus::Result<bool>;
  #[zbus(property)]
  fn is_rechargeable(&self) -> zbus::Result<bool>;
  #[zbus(property)]
  fn model(&self) -> zbus::Result<String>;
  #[zbus(property)]
  fn percentage(&self) -> zbus::Result<f64>;
}

#[derive(Debug)]
pub struct Status {
  pub device: String,
  pub percentage: f64,
}

pub async fn statuses() -> impl smol::stream::Stream<Item = zbus::Result<Vec<Status>>> {
  async_stream::try_stream! {
  let connection = zbus::Connection::system().await?;
  let upower = UPowerProxy::new(&connection).await?;

  // signal time=1664639124.613631 sender=:1.3 -> destination=(null destination) serial=759 path=/org/freedesktop/UPower; interface=org.freedesktop.UPower; member=DeviceAdded
  //    object path "/org/freedesktop/UPower/devices/..."
  let mut device_added = upower.receive_device_added().await?.map(|device| {
    log::trace!("Device added:\n{:?}", device.message());
    true
  }).boxed_local();
  // signal time=1664648770.774883 sender=:1.3 -> destination=(null destination) serial=1718 path=/org/freedesktop/UPower; interface=org.freedesktop.UPower; member=DeviceRemoved
  //    object path "/org/freedesktop/UPower/devices/..."
  let mut device_removed = upower.receive_device_removed().await?.map(|device| {
    log::trace!("Device removed:\n{:?}", device.message());
    true
  }).boxed_local();

  let mut devices = std::collections::HashMap::new();
  let mut devices_properties = std::collections::HashMap::new();

  loop {
    // Crawl all the devices.
    // UPower makes it difficult for us for two reasons:
    //  - devices are under /org/freedesktop/UPower/devices, which, apparently, requires a
    //    dummy Proxy in zbus to handle,
    //  - the tree is updated only after DeviceRemoved is sent so we can't trust Introspect
    //    anyway.
    // Fortunately, by using EnumerateDevices we can work around that and we don't have to
    // ignore the fake DisplayDevice (https://upower.freedesktop.org/docs/UPower.html).
    let paths: std::collections::HashSet<_> = upower.enumerate_devices().await?.iter().cloned().collect();

    let previous_paths: std::collections::HashSet<_> = devices.keys().cloned().collect();
    for path in previous_paths.difference(&paths) {
      devices.remove(path);
      devices_properties.remove(path);
    }
    for path in paths.difference(&previous_paths) {
      let device = DeviceProxy::builder(&connection).path(path.clone())?.build().await?;
      if !device.is_present().await? {
        continue;
      }
      // signal time=1664653462.331600 sender=:1.3 -> destination=(null destination) serial=2110 path=/org/freedesktop/UPower/devices/...; interface=org.freedesktop.DBus.Properties; member=PropertiesChanged
      //    string "org.freedesktop.UPower.Device"
      //    array [
      //       dict entry(
      //          string "TimeToEmpty"
      //          variant             int64 28227
      //       )
      //       dict entry(
      //          string "Percentage"
      //          variant             double 98
      //       )
      //       ...
      //    ]
      //    array [
      //    ]
      let properties = zbus::fdo::PropertiesProxy::builder(&connection)
        .destination(device.0.destination())?
        .path(device.0.path())?
        .build()
        .await?;
      devices_properties.insert(path.clone(), properties.receive_properties_changed().await?.map(|properties| {
        log::trace!("Device properties:\n{:?}", properties.message());
        false
      }).boxed_local());
      devices.insert(path.clone(), device);
    }

    loop {
      let mut statuses = Vec::new();
      for device in devices.values() {
        statuses.push(Status {
          device: device.model().await?,
          percentage: device.percentage().await?,
        })
      }
      statuses.sort_by(|status1, status2| status1.device.cmp(&status2.device));
      yield statuses;

      let (rebuild, index, _) = futures_util::future::select_all(
        [device_added.next(), device_removed.next()]
        .into_iter()
        .chain(devices_properties.values_mut().map(|device_properties| device_properties.next())),
      )
      .await;
      assert!(rebuild.is_some(), "End of stream {index}");
      if let Some(true) = rebuild {
        break;
      }
    }
  }
  }
}
