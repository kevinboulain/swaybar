use smol::stream::StreamExt as _;

#[zbus::proxy(default_service = "org.bluez", default_path = "/org/bluez/hci0", interface = "org.bluez.Adapter1")]
trait Adapter1 {
  #[zbus(property)]
  fn powered(&self) -> zbus::Result<bool>;
}

#[zbus::proxy(default_service = "org.bluez", interface = "org.bluez.Device1")]
trait Device1 {
  fn connect(&self) -> zbus::Result<()>;
  fn disconnect(&self) -> zbus::Result<()>;
  #[zbus(property)]
  fn name(&self) -> zbus::Result<String>;
  #[zbus(property)]
  fn paired(&self) -> zbus::Result<bool>;
  #[zbus(property)]
  fn connected(&self) -> zbus::Result<bool>;
  #[zbus(property)]
  fn address(&self) -> zbus::Result<String>;
  // The RSSI property is spotty. It does appear during a scan, for some devices.
  // Worst case, 'hcitool rssi' can retrieve it.
}

#[derive(Debug)]
pub struct DeviceStatus {
  pub name: String,
  pub connected: bool,
  pub path: zvariant::OwnedObjectPath,
}

#[derive(Debug)]
pub struct Status {
  pub powered: bool,
  pub devices: Vec<DeviceStatus>,
}

pub async fn statuses() -> impl smol::stream::Stream<Item = zbus::Result<Status>> {
  async_stream::try_stream! {
  let connection = zbus::Connection::system().await?;
  let adapter = Adapter1Proxy::new(&connection).await?;

  let properties = zbus::fdo::PropertiesProxy::builder(&connection)
    .destination(adapter.0.destination())?
    .path(adapter.0.path())?
    .build()
    .await?;
  // signal time=1664648181.086398 sender=:1.298 -> destination=(null destination) serial=3655 path=/org/bluez/hci0; interface=org.freedesktop.DBus.Properties; member=PropertiesChanged
  //    string "org.bluez.Adapter1"
  //    array [
  //       dict entry(
  //          string "Powered"
  //          variant             boolean true
  //       )
  //    ]
  //    array [
  //    ]
  let mut adapter_properties = properties
    .receive_properties_changed()
    .await?
    .map(|properties| {
      log::trace!("Adapter properties:\n{:?}", properties.message());
      false
    })
    .boxed_local();

  let object_manager = zbus::fdo::ObjectManagerProxy::builder(&connection)
    .destination(adapter.0.destination())?
    .path("/")?
    .build()
    .await?;
  // During a scan, BlueZ adds all devices to the tree, firing InterfacesAdded signals.
  //
  // signal time=1664647927.437581 sender=:1.298 -> destination=(null destination) serial=3608 path=/; interface=org.freedesktop.DBus.ObjectManager; member=InterfacesAdded
  //    object path "/org/bluez/hci0/dev_00_11_22_33_44_55
  //    array [
  //       ...
  //    ]
  let mut interfaces_added = object_manager.receive_interfaces_added().await?.map(|interface| {
    log::trace!("Interface added:\n{:?}", interface.message());
    true
  }).boxed_local();

  // Some time after a scan or a device disconnection, the InterfacesRemoved signal is fired.
  // BlueZ never forgets trusted devices.
  // Note we could ignore the removal of connected devices to ease the reconnection but for
  // untrusted devices, BlueZ won't want to reconnect (because it doesn't remember it), so
  // the best of course of action is really to remove it.
  //
  // signal time=1664647957.904215 sender=:1.298 -> destination=(null destination) serial=3637 path=/; interface=org.freedesktop.DBus.ObjectManager; member=InterfacesRemoved
  //    object path "/org/bluez/hci0/dev_00_11_22_33_44_55"
  //    array [
  //       ...
  //    ]
  let mut interfaces_removed = object_manager.receive_interfaces_removed().await?.map(|interface| {
    log::trace!("Interface removed:\n{:?}", interface.message());
    true
  }).boxed_local();

  let mut devices = std::collections::HashMap::new();
  let mut devices_properties = std::collections::HashMap::new();

  loop {
    let mut paths = std::collections::HashSet::new();

    // Crawl all the devices under the adapter.
    // Trusted devices are always present.
    // https://git.kernel.org/pub/scm/bluetooth/bluez.git/tree/test/list-devices
    for (path, interface) in object_manager.get_managed_objects().await? {
      for (interface, _) in interface {
        if interface == "org.bluez.Device1" {
          paths.insert(path.clone());
        }
      }
    }

    let previous_paths: std::collections::HashSet<_> = devices.keys().cloned().collect();
    for path in previous_paths.difference(&paths) {
      devices_properties.remove(path);
      devices.remove(path);
    }
    for path in paths.difference(&previous_paths) {
      let device = Device1Proxy::builder(&connection).path(path.to_string())?.build().await?;
      if !device.paired().await? {
        // I only care about connecting and disconnecting known devices.
        // For everything else, it's probably best to use the CLI.
        continue;
      }
      // signal time=1664648294.292049 sender=:1.298 -> destination=(null destination) serial=3658 path=/org/bluez/hci0/dev_00_11_22_33_44_55; interface=org.freedesktop.DBus.Properties; member=PropertiesChanged
      //    string "org.bluez.Device1"
      //    array [
      //       dict entry(
      //          string "Connected"
      //          variant             boolean true
      //       )
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
      let mut device_statuses = Vec::new();
      for device in devices.values() {
        device_statuses.push(DeviceStatus {
          name: device.name().await?,
          connected: device.connected().await?,
          path: device.0.path().to_owned().into(),
        });
      }
      let status = Status {
        powered: adapter.powered().await?,
        devices: device_statuses,
      };
      yield status;

      // In the Tokio version I was using a StreamMap. zbus also depends on futures-util so I don't
      // feel too bad to use select_all here (it allocates every time, though).
      let (rebuild, index, _) = futures_util::future::select_all(
        // A signal might fire multiple times in a row:
        //  - more than one property can change,
        //  - more entries can be added under a device.
        // TODO: the streams need to be boxed(_local) to make the type checker happy here, can I do otherwise?
        [adapter_properties.next(), interfaces_added.next(), interfaces_removed.next()]
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

pub async fn toggle(device: Option<&zvariant::ObjectPath<'_>>) -> zbus::Result<()> {
  if let Some(path) = device {
    let device = Device1Proxy::builder(&zbus::Connection::system().await?)
      .path(path)?
      .build()
      .await?;
    return if device.connected().await? {
      device.disconnect().await
    } else {
      device.connect().await
    };
  }

  // There is no method on the adapter to do that.
  smol::process::Command::new("bash")
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
    .map(|_| ())
    .map_err(|error| zbus::Error::InputOutput(std::sync::Arc::new(error)))
}
