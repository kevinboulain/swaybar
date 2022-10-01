use crate::process;
use async_stream::try_stream;
use std::collections::{HashMap, HashSet};
use tokio_stream::Stream;
use tokio_stream::{StreamExt, StreamMap};
use zbus::{
    dbus_proxy,
    fdo::{ObjectManagerProxy, PropertiesProxy},
    Connection, Error, Result,
};

#[dbus_proxy(
    default_service = "org.bluez",
    default_path = "/org/bluez/hci0",
    interface = "org.bluez.Adapter1"
)]
trait Adapter1 {
    #[dbus_proxy(property)]
    fn powered(&self) -> Result<bool>;
}

#[dbus_proxy(default_service = "org.bluez", interface = "org.bluez.Device1")]
trait Device1 {
    fn connect(&self) -> Result<()>;
    fn disconnect(&self) -> Result<()>;

    #[dbus_proxy(property)]
    fn name(&self) -> Result<String>;
    #[dbus_proxy(property)]
    fn paired(&self) -> Result<bool>;
    #[dbus_proxy(property)]
    fn connected(&self) -> Result<bool>;
    #[dbus_proxy(property)]
    fn address(&self) -> Result<String>;
    // The RSSI property is spotty. It does appear during a scan, for some devices.
    // Worst case, 'hcitool rssi' can retrieve it.
}

async fn bluetooth_status(
    adapter: &'_ Adapter1Proxy<'_>,
    devices: &HashMap<String, Device1Proxy<'_>>,
) -> Result<(bool, Vec<(String, bool, String)>)> {
    let powered = adapter.powered().await?;
    let mut connections = Vec::new();
    for (_, device) in devices {
        let name = device.name().await?;
        let connected = device.connected().await?;
        connections.push((name, connected, device.path().to_string()));
    }
    connections.sort_by(|(name1, _, _), (name2, _, _)| name1.cmp(name2));
    Ok((powered, connections))
}

pub async fn bluetooth() -> impl Stream<Item = Result<(bool, Vec<(String, bool, String)>)>> {
    try_stream! {
        // I don't believe this nicely handles a disconnection (like a restart of BlueZ)...
        let connection = Connection::system().await?;
        let adapter = Adapter1Proxy::new(&connection).await?;
        let properties = PropertiesProxy::builder(&connection)
            .destination(adapter.destination())?
            .path(adapter.path())?
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
        let mut adapter_properties = properties.receive_properties_changed().await?;

        let object_manager = ObjectManagerProxy::builder(&connection)
            .destination(adapter.destination())? // This seems necessary (empty string is refused).
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
        let mut interfaces_added = object_manager.receive_interfaces_added().await?;
        // Some time after a scan or a device disconnection, the InterfacesRemoved signal is fired.
        // BlueZ never forgets trusted devices.
        // Note we could ignore the removal of connected devices to eease the reconnection but for
        // untrusted devices, BlueZ won't want to reconnect (because it doesn't remember it), so
        // the best of course of action is really to remove it.
        //
        // signal time=1664647957.904215 sender=:1.298 -> destination=(null destination) serial=3637 path=/; interface=org.freedesktop.DBus.ObjectManager; member=InterfacesRemoved
        //    object path "/org/bluez/hci0/dev_00_11_22_33_44_55"
        //    array [
        //       ...
        //    ]
        let mut interfaces_removed = object_manager.receive_interfaces_removed().await?;

        let mut devices = HashMap::new();
        let mut device_properties = StreamMap::new();

        loop {
            let mut paths = HashSet::new();

            // Crawl all the devices under the adapter.
            // Trusted devices are always present.
            // https://git.kernel.org/pub/scm/bluetooth/bluez.git/tree/test/list-devices
            for (path, interface) in object_manager.get_managed_objects().await? {
                for (interface, _) in interface {
                    if interface == "org.bluez.Device1" {
                        paths.insert(path.as_str().to_string());
                    }
                }
            }

            let previous_paths: HashSet<String> = devices.keys().cloned().collect();
            for path in previous_paths.difference(&paths) {
                devices.remove(path);
                device_properties.remove(path);
            }
            for path in paths.difference(&previous_paths) {
                let device = Device1Proxy::builder(&connection)
                    .path(path.to_string())?
                    .build()
                    .await?;
                if !device.paired().await? {
                    // I only care about connecting and disconnecting known devices.
                    // For everything else, it's probably best to use the CLI.
                    eprintln!("skipping unpaired device {}", device.path());
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
                let properties = PropertiesProxy::builder(&connection)
                    .destination(device.destination().to_owned())?
                    .path(device.path().to_owned())?
                    .build()
                    .await?;
                devices.insert(path.to_string(), device);
                device_properties.insert(
                    path.to_string(),
                    properties.receive_properties_changed().await?,
                );
            }

            loop {
                yield bluetooth_status(&adapter, &devices).await?;

                // A signal might fire multiple times in a row:
                //  - more than one property can change,
                //  - more entries can be added under a device.
                let rebuild = tokio::select! {
                    // Device change, rebuild the maps.
                    Some(_) = interfaces_added.next() => true,
                    Some(_) = interfaces_removed.next() => true,
                    // Property change, continue polling.
                    Some(_) = adapter_properties.next() => false,
                    Some(_) = device_properties.next() => false,
                };

                if rebuild {
                    break;
                }
            }
        }
    }
}

pub async fn bluetooth_toggle(device: Option<String>) -> Result<()> {
    if let Some(path) = device {
        let connection = Connection::system().await?;
        let device = Device1Proxy::builder(&connection)
            .path(path.to_string())?
            .build()
            .await?;
        if device.connected().await? {
            device.disconnect().await?;
        } else {
            device.connect().await?;
        }
        return Ok(());
    }

    // There is no method on the adapter to do that.
    if let Err(_) = process::bluetooth_toggle().await {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::Other,
            "",
        )));
    }

    // I guess we could power off the adapter when no devices are left?
    Ok(())
}
