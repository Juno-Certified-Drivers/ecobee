# Ecobee

Ecobee thermostats and SmartSensors, two ways.

**Two packages, because they are two protocols that share nothing but a vendor.** Install one
or the other, not both, for the same physical thermostat.

| Package | Drivers | Path |
| --- | --- | --- |
| [`cloud/`](cloud) | `ecobee.thermostat`, `ecobee.remote_sensor` | api.ecobee.com |
| [`hap/`](hap) | `ecobee.hap.thermostat`, `ecobee.hap.sensor` | HomeKit, on the LAN |

Within `cloud`, the thermostat and its remote sensors *are* bundled into one payload: same API,
same credentials, same poll. Setup is one flow that offers both, and a sensor is simply the
instance that was given a `Sensor id`.

Across the two packages there is no such sharing, so they stay apart. `on_command` is not given
a driver id, so a single payload would have to guess from instance state which protocol a
command meant — fragile, for no gain.

## Which one

**`hap`** unless you need remote access. It is local, works when the internet is down, and
needs no developer account.

**`cloud`** reaches the thermostat from anywhere, and is the only path that exposes some
scheduling. It is also the one driver here that stops working when the internet does — a device
limitation, not a design choice. The thermostat keeps running its own schedule regardless.

Ecobee rate-limits hard, so polling is deliberately slow (3 min default) and setpoint writes
are absolute, never read-modify-write: a throttled read cannot corrupt a write.

Ecobee speaks Fahrenheit×10 internally. The proxy contract is Celsius, so the conversion lives
in the driver — which is exactly where a unit conversion belongs.

## Building

```bash
cargo build --release            # both packages
cargo build --release -p juno-driver-ecobee
```

Releases are built by [`junohouse/driver-ci`](https://github.com/junohouse/driver-ci): push to
`main` for a beta, tag `v1.2.0` for a release. Both packages release together and share a
version; they ship as separate artifacts.
