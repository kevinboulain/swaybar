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
use zvariant::OwnedObjectPath;

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
    for device in devices.values() {
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
    if let Err(error) = process::bluetooth_toggle().await {
        return Err(Error::InputOutput(
            std::io::Error::new(std::io::ErrorKind::Other, error.to_string()).into(),
        ));
    }

    // I guess we could power off the adapter when no devices are left?
    Ok(())
}

#[dbus_proxy(
    default_service = "org.freedesktop.UPower",
    default_path = "/org/freedesktop/UPower",
    interface = "org.freedesktop.UPower"
)]
trait UPower {
    fn enumerate_devices(&self) -> Result<Vec<OwnedObjectPath>>;

    #[dbus_proxy(signal)]
    fn device_added(&self) -> Result<()>;
    #[dbus_proxy(signal)]
    fn device_removed(&self) -> Result<()>;
}

#[dbus_proxy(
    default_service = "org.freedesktop.UPower",
    interface = "org.freedesktop.UPower.Device"
)]
trait Device {
    #[dbus_proxy(property)]
    fn is_present(&self) -> Result<bool>;
    #[dbus_proxy(property)]
    fn is_rechargeable(&self) -> Result<bool>;
    #[dbus_proxy(property)]
    fn model(&self) -> Result<String>;
    #[dbus_proxy(property)]
    fn percentage(&self) -> Result<f64>;
}

async fn power_status(devices: &HashMap<String, DeviceProxy<'_>>) -> Result<Vec<(String, f64)>> {
    let mut batteries = Vec::new();
    for device in devices.values() {
        let model = device.model().await?;
        let percentage = device.percentage().await?;
        batteries.push((model, percentage));
    }
    batteries.sort_by(|(name1, _), (name2, _)| name1.cmp(name2));
    Ok(batteries)
}

pub async fn power() -> impl Stream<Item = Result<Vec<(String, f64)>>> {
    try_stream! {
        // I don't believe this nicely handles a disconnection (like a restart of UPower)...
        let connection = Connection::system().await?;
        let upower = UPowerProxy::new(&connection).await?;
        // signal time=1664639124.613631 sender=:1.3 -> destination=(null destination) serial=759 path=/org/freedesktop/UPower; interface=org.freedesktop.UPower; member=DeviceAdded
        //    object path "/org/freedesktop/UPower/devices/..."
        let mut device_added = upower.receive_device_added().await?;
        // signal time=1664648770.774883 sender=:1.3 -> destination=(null destination) serial=1718 path=/org/freedesktop/UPower; interface=org.freedesktop.UPower; member=DeviceRemoved
        //    object path "/org/freedesktop/UPower/devices/..."
        let mut device_removed = upower.receive_device_removed().await?;

        let mut devices = HashMap::new();
        let mut device_properties = StreamMap::new();

        loop {
            // Crawl all the devices.
            // UPower makes it difficult for us for two reasons:
            //  - devices are under /org/freedesktop/UPower/devices, which, apparently, requires a
            //    dummy Proxy in zbus to handle,
            //  - the tree is updated only after DeviceRemoved is sent so we can't trust Introspect
            //    anyway.
            // Fortunately, by using EnumerateDevices we can work around that and we don't have to
            // ignore the fake DisplayDevice (https://upower.freedesktop.org/docs/UPower.html).
            let paths: HashSet<String> = upower.enumerate_devices().await?.iter().map(|path| path.to_string()).collect();

            let previous_paths: HashSet<String> = devices.keys().cloned().collect();
            for path in previous_paths.difference(&paths) {
                devices.remove(path);
                device_properties.remove(path);
            }
            for path in paths.difference(&previous_paths) {
                let device = DeviceProxy::builder(&connection)
                    .path(path.to_string())?
                    .build()
                    .await?;
                if !device.is_present().await? {
                    eprintln!("skipping power device {}", device.path());
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
                yield power_status(&devices).await?;

                // A signal might fire multiple times in a row:
                //  - more than one property can change,
                //  - more entries can be added under a device.
                let rebuild = tokio::select! {
                    // Device change, rebuild the maps.
                    Some(_) = device_added.next() => true,
                    Some(_) = device_removed.next() => true,
                    // Property change, continue polling.
                    Some(_) = device_properties.next() => false,
                };

                if rebuild {
                    break;
                }
            }
        }
    }
}
