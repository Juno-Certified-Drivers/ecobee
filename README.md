# Ecobee

Ecobee thermostats and SmartSensors, over HomeKit on the LAN.

| Package | Drivers | Path |
| --- | --- | --- |
| [`hap/`](hap) | `ecobee.hap.thermostat`, `ecobee.hap.sensor` | HomeKit, on the LAN |

A sensor names the thermostat as its `parent`, so it reads the thermostat's connection rather
than carrying its own — pair once, and the sensors come with it.

There was a second package, `cloud/`, against api.ecobee.com, shipping `ecobee.thermostat` and
`ecobee.remote_sensor`. It shared nothing with this one but a vendor, and it was the only
driver here that stopped working when the internet did. It is gone, and remote access and the
cloud-only scheduling went with it; HomeKit is local and needs no developer account. Those two
driver ids are retired.

Ecobee speaks Fahrenheit×10 internally. The proxy contract is Celsius, so the conversion lives
in the driver — which is exactly where a unit conversion belongs.

## Building

```bash
cargo build --release -p juno-driver-ecobee-hap
```

Releases are built by [`junohouse/driver-ci`](https://github.com/junohouse/driver-ci): push to
`main` for a beta, tag `v1.2.0` for a release.
