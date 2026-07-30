//! Ecobee thermostats and SmartSensors over the cloud API.
//!
//! Two things make this driver different from the lighting ones:
//!
//! - **It is a cloud API**, so it is the one driver here that stops working when the internet
//!   does. Juno is offline-first; this is a device limitation, not a design choice, and the
//!   thermostat keeps running its own schedule regardless.
//! - **Ecobee rate-limits hard.** Polling is deliberately slow (3 min default) and setpoint
//!   writes are absolute, never read-modify-write, so a throttled read cannot corrupt a write.
//!
//! Ecobee speaks Fahrenheit×10 internally. The proxy contract is Celsius, so the conversion
//! lives here — which is exactly where a unit conversion should live.

use driver_sdk::*;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

const API: &str = "https://api.ecobee.com/1/thermostat";
const AUTHORIZE_API: &str = "https://api.ecobee.com/authorize";
const TOKEN_API: &str = "https://api.ecobee.com/token";

#[derive(Default)]
pub struct EcobeeThermostat;
#[derive(Default)]
pub struct EcobeeRemoteSensor;

/// Celsius -> ecobee's tenths-of-Fahrenheit.
pub fn c_to_f10(c: f64) -> i64 {
    (c * 9.0 / 5.0 + 32.0).round() as i64 * 10
}

/// Ecobee's tenths-of-Fahrenheit -> Celsius, to one decimal.
pub fn f10_to_c(f10: i64) -> f64 {
    let c = (f10 as f64 / 10.0 - 32.0) * 5.0 / 9.0;
    (c * 10.0).round() / 10.0
}

fn now_s() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn api_key(inst: &Instance) -> Option<String> {
    inst.property("API key")
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A live access token, if the OAuth path (an `API key`) is configured and one is on hand
/// that has not expired.
fn access_token(inst: &Instance) -> Option<String> {
    let tok = inst.scratch.get("access_token").and_then(Value::as_str)?;
    let exp = inst
        .scratch
        .get("access_expires_at")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    (exp > now_s() + 30).then(|| tok.to_string())
}

/// `None` means "not ready yet, refresh first" — only possible on the OAuth path. Without an
/// `API key` this is the manual/demo path: `Refresh token` is used directly as the bearer
/// token, exactly as before.
fn auth(inst: &Instance, req: HttpRequest) -> Option<HttpRequest> {
    let token = match api_key(inst) {
        None => inst.property("Refresh token").as_str().unwrap_or("").to_string(),
        Some(_) => access_token(inst)?,
    };
    Some(req.header("authorization", format!("Bearer {token}")))
}

/// True only on the OAuth path, when the access token has expired or was never fetched.
fn needs_refresh(inst: &Instance) -> bool {
    api_key(inst).is_some() && access_token(inst).is_none()
}

fn refresh_token_value(inst: &Instance) -> Option<String> {
    inst.scratch
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            inst.property("Refresh token")
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

/// Exchange the (possibly rotated) refresh token for a fresh access token. Ecobee answers
/// this the same shape whether it is a first exchange or a renewal.
fn refresh_request(inst: &Instance) -> Option<HostCall> {
    let key = api_key(inst)?;
    let refresh = refresh_token_value(inst)?;
    Some(HostCall::Http(HttpRequest::new(
        "POST",
        format!("{TOKEN_API}?grant_type=refresh_token&code={refresh}&client_id={key}"),
    )))
}

impl EcobeeThermostat {
    fn identifier(inst: &Instance) -> Option<String> {
        inst.property("Thermostat identifier")
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    /// Every write is a `setHold` with both setpoints stated absolutely. Ecobee requires both
    /// even when only one is changing, so we carry the other from last known state.
    fn hold(inst: &Instance, heat_c: f64, cool_c: f64) -> Option<HostCall> {
        let id = Self::identifier(inst)?;
        let body = json!({
            "selection": { "selectionType": "thermostats", "selectionMatch": id },
            "functions": [{
                "type": "setHold",
                "params": {
                    "holdType": "nextTransition",
                    "heatHoldTemp": c_to_f10(heat_c),
                    "coolHoldTemp": c_to_f10(cool_c),
                }
            }]
        });
        let req = auth(inst, HttpRequest::new("POST", API).json(body.to_string()))?;
        Some(HostCall::Http(req))
    }

    fn setpoints(inst: &Instance) -> (f64, f64) {
        (
            inst.scratch
                .get("heat_c")
                .and_then(Value::as_f64)
                .unwrap_or(20.0),
            inst.scratch
                .get("cool_c")
                .and_then(Value::as_f64)
                .unwrap_or(24.0),
        )
    }
}

impl DriverModule for EcobeeThermostat {
    fn on_command(
        &self,
        inst: &mut Instance,
        _proxy: LocalId,
        cmd: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        // The OAuth path needs a live access token before any of these can go out. Stash
        // what was asked for and come back to it once the refresh response arrives.
        if matches!(
            cmd,
            "set_heat_setpoint" | "set_cool_setpoint" | "set_mode" | "set_fan_mode" | "set_hold"
        ) && needs_refresh(inst)
        {
            let Some(refresh) = refresh_request(inst) else {
                return vec![HostCall::warn(
                    "ecobee: set the API key and Refresh token on this device first",
                )];
            };
            inst.scratch
                .insert("pending_action".into(), json!({ "cmd": cmd, "args": args }));
            return vec![refresh];
        }

        let (heat, cool) = Self::setpoints(inst);

        match cmd {
            "set_heat_setpoint" | "set_cool_setpoint" => {
                let Some(c) = args.get("celsius").and_then(Value::as_f64) else {
                    return vec![HostCall::warn("ecobee: no celsius value")];
                };
                let heating = cmd == "set_heat_setpoint";
                // Ecobee rejects a hold whose setpoints are closer than its deadband, so
                // push the other one out of the way rather than sending a request that fails.
                let (h, c2) = if heating {
                    (c, cool.max(c + 2.0))
                } else {
                    (heat.min(c - 2.0), c)
                };
                inst.scratch.insert("heat_c".into(), json!(h));
                inst.scratch.insert("cool_c".into(), json!(c2));

                let Some(req) = Self::hold(inst, h, c2) else {
                    return vec![HostCall::warn(
                        "ecobee: set the Thermostat identifier on this device first",
                    )];
                };
                let mut a = Args::new();
                a.insert("which".into(), json!(if heating { "heat" } else { "cool" }));
                a.insert("celsius".into(), json!(c));
                vec![req, HostCall::notify(1, "setpoint_changed", a)]
            }
            "set_mode" => {
                let Some(mode) = args.get("mode").and_then(Value::as_str) else {
                    return vec![HostCall::warn("ecobee: no mode")];
                };
                let Some(id) = Self::identifier(inst) else {
                    return vec![HostCall::warn("ecobee: no Thermostat identifier")];
                };
                let body = json!({
                    "selection": { "selectionType": "thermostats", "selectionMatch": id },
                    "thermostat": { "settings": { "hvacMode": mode } }
                });
                let Some(req) = auth(inst, HttpRequest::new("POST", API).json(body.to_string()))
                else {
                    return vec![HostCall::warn("ecobee: token not ready, try again shortly")];
                };
                let mut a = Args::new();
                a.insert("mode".into(), json!(mode));
                vec![HostCall::Http(req), HostCall::notify(1, "mode_changed", a)]
            }
            "set_fan_mode" => {
                let Some(mode) = args.get("mode").and_then(Value::as_str) else {
                    return vec![HostCall::warn("ecobee: no fan mode")];
                };
                let Some(id) = Self::identifier(inst) else {
                    return vec![HostCall::warn("ecobee: no Thermostat identifier")];
                };
                let body = json!({
                    "selection": { "selectionType": "thermostats", "selectionMatch": id },
                    "functions": [{
                        "type": "setHold",
                        "params": {
                            "holdType": "nextTransition",
                            "heatHoldTemp": c_to_f10(heat),
                            "coolHoldTemp": c_to_f10(cool),
                            "fan": mode,
                        }
                    }]
                });
                let Some(req) = auth(inst, HttpRequest::new("POST", API).json(body.to_string()))
                else {
                    return vec![HostCall::warn("ecobee: token not ready, try again shortly")];
                };
                vec![HostCall::Http(req)]
            }
            "set_hold" => {
                let on = args.get("hold").and_then(Value::as_bool).unwrap_or(false);
                let Some(id) = Self::identifier(inst) else {
                    return vec![HostCall::warn("ecobee: no Thermostat identifier")];
                };
                let body = if on {
                    json!({
                        "selection": { "selectionType": "thermostats", "selectionMatch": id },
                        "functions": [{ "type": "setHold", "params": {
                            "holdType": "indefinite",
                            "heatHoldTemp": c_to_f10(heat),
                            "coolHoldTemp": c_to_f10(cool) }}]
                    })
                } else {
                    json!({
                        "selection": { "selectionType": "thermostats", "selectionMatch": id },
                        "functions": [{ "type": "resumeProgram", "params": { "resumeAll": false }}]
                    })
                };
                let Some(req) = auth(inst, HttpRequest::new("POST", API).json(body.to_string()))
                else {
                    return vec![HostCall::warn("ecobee: token not ready, try again shortly")];
                };
                vec![HostCall::Http(req)]
            }
            other => vec![HostCall::warn(format!("ecobee: unhandled `{other}`"))],
        }
    }

    /// A poll came back. Ecobee returns the thermostat, its runtime, and its remote sensors
    /// in one document — we fan that out to the right bindings.
    fn on_event(
        &self,
        inst: &mut Instance,
        _control: LocalId,
        note: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        if note != "http_response" {
            return Vec::new();
        }
        let Some(doc) = args.get("body") else {
            return Vec::new();
        };

        // A token (re)fresh reply, not a poll. Bank it, then run whatever was waiting on it.
        if let Some(access) = doc.get("access_token").and_then(Value::as_str) {
            let ttl = doc.get("expires_in").and_then(Value::as_i64).unwrap_or(3600);
            inst.scratch.insert("access_token".into(), json!(access));
            inst.scratch.insert("access_expires_at".into(), json!(now_s() + ttl));
            if let Some(refresh) = doc.get("refresh_token").and_then(Value::as_str) {
                inst.scratch.insert("refresh_token".into(), json!(refresh));
            }
            let Some(pending) = inst.scratch.remove("pending_action") else {
                return Vec::new();
            };
            let cmd = pending.get("cmd").and_then(Value::as_str).unwrap_or("").to_string();
            if cmd == "__bind__" {
                return self.on_bind(inst);
            }
            let pargs: Args = pending
                .get("args")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            return self.on_command(inst, 1, &cmd, &pargs);
        }

        let Some(t) = doc
            .get("thermostatList")
            .and_then(|l| l.as_array())
            .and_then(|l| l.first())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();

        if let Some(temp) = t.pointer("/runtime/actualTemperature").and_then(Value::as_i64) {
            let c = f10_to_c(temp);
            let mut a = Args::new();
            a.insert("celsius".into(), json!(c));
            out.push(HostCall::notify(1, "temperature_changed", a));
            let mut s = Args::new();
            s.insert("value".into(), json!(c));
            out.push(HostCall::notify(3, "value_changed", s));
        }
        if let Some(h) = t.pointer("/runtime/actualHumidity").and_then(Value::as_i64) {
            let mut a = Args::new();
            a.insert("percent".into(), json!(h as f64));
            out.push(HostCall::notify(1, "humidity_changed", a));
        }
        if let Some(heat) = t.pointer("/runtime/desiredHeat").and_then(Value::as_i64) {
            inst.scratch.insert("heat_c".into(), json!(f10_to_c(heat)));
        }
        if let Some(cool) = t.pointer("/runtime/desiredCool").and_then(Value::as_i64) {
            inst.scratch.insert("cool_c".into(), json!(f10_to_c(cool)));
        }
        if let Some(mode) = t.pointer("/settings/hvacMode").and_then(Value::as_str) {
            let mut a = Args::new();
            a.insert("mode".into(), json!(mode));
            out.push(HostCall::notify(1, "mode_changed", a));
        }
        // The thermostat's own occupancy sensor lives in the remoteSensors list too, as the
        // one whose type is "thermostat".
        if let Some(sensors) = t.get("remoteSensors").and_then(Value::as_array)
            && let Some(built_in) = sensors
                .iter()
                .find(|s| s.get("type").and_then(Value::as_str) == Some("thermostat"))
            && let Some(occ) = read_occupancy(built_in)
        {
            let mut a = Args::new();
            a.insert("detected".into(), json!(occ));
            out.push(HostCall::notify(2, "detected_changed", a));
        }
        out
    }

    fn on_bind(&self, inst: &mut Instance) -> Vec<HostCall> {
        let Some(id) = Self::identifier(inst) else {
            return vec![HostCall::warn(
                "ecobee: set the Thermostat identifier and Refresh token on this device",
            )];
        };
        if needs_refresh(inst) {
            let Some(refresh) = refresh_request(inst) else {
                return vec![HostCall::warn(
                    "ecobee: set the API key and Refresh token on this device first",
                )];
            };
            inst.scratch
                .insert("pending_action".into(), json!({ "cmd": "__bind__" }));
            return vec![refresh];
        }
        let selection = json!({
            "selection": {
                "selectionType": "thermostats",
                "selectionMatch": id,
                "includeRuntime": true,
                "includeSettings": true,
                "includeSensors": true,
            }
        });
        let Some(req) = auth(inst, HttpRequest::new("GET", format!("{API}?json={selection}")))
        else {
            return vec![HostCall::warn("ecobee: token not ready, try again shortly")];
        };
        vec![HostCall::Http(req)]
    }

    /// The ecobee PIN authorization wizard: ask for the developer API key, get a PIN,
    /// wait for it to be entered on ecobee.com, exchange it for tokens, then list what is on
    /// the account so the installer can pick which thermostats and sensors to adopt.
    ///
    /// Driven entirely by `state`, so core can replay any step (a retry after `Wait`, a fetch
    /// response coming back) without this needing to remember anything itself.
    fn discover(&self, _driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        let stage = state.get("stage").and_then(Value::as_str).unwrap_or("start");
        let note = input.get("note").and_then(Value::as_str);

        match stage {
            "authorizing" if note == Some("authorize") => {
                let key = state.get("api_key").and_then(Value::as_str).unwrap_or("").to_string();
                let body = input.get("response").cloned().unwrap_or(Value::Null);
                let (Some(pin), Some(code)) = (
                    body.get("ecobeePin").and_then(Value::as_str),
                    body.get("code").and_then(Value::as_str),
                ) else {
                    return (
                        Self::failure("ecobee did not return a PIN — check the API key"),
                        Value::Null,
                    );
                };
                let interval = body.get("interval").and_then(Value::as_i64).unwrap_or(30);
                (
                    SetupStep::Instruct {
                        title: "Authorize Juno on ecobee.com".into(),
                        body: format!(
                            "On the ecobee web portal, open Menu \u{2192} My Apps \u{2192} Add \
                             Application, and enter this PIN:\n\n{pin}\n\n\
                             Then come back and continue."
                        ),
                        continue_label: "I've entered the PIN".into(),
                    },
                    json!({ "stage": "pending", "api_key": key, "code": code, "interval": interval }),
                )
            }
            "pending" if note != Some("token") => {
                let key = state.get("api_key").and_then(Value::as_str).unwrap_or("").to_string();
                let code = state.get("code").and_then(Value::as_str).unwrap_or("").to_string();
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "POST",
                            format!("{TOKEN_API}?grant_type=ecobeePin&code={code}&client_id={key}"),
                        ),
                        note: "token".into(),
                    },
                    state.clone(),
                )
            }
            "pending" => {
                let key = state.get("api_key").and_then(Value::as_str).unwrap_or("").to_string();
                let body = input.get("response").cloned().unwrap_or(Value::Null);
                if let Some(access) = body.get("access_token").and_then(Value::as_str) {
                    let refresh = body.get("refresh_token").and_then(Value::as_str).unwrap_or("");
                    let selection = json!({
                        "selection": { "selectionType": "registered", "includeSensors": true }
                    });
                    return (
                        SetupStep::Fetch {
                            request: HttpRequest::new("GET", format!("{API}?json={selection}"))
                                .header("authorization", format!("Bearer {access}")),
                            note: "list".into(),
                        },
                        json!({ "stage": "listing", "api_key": key, "refresh_token": refresh }),
                    );
                }
                // Ecobee answers `authorization_pending` while waiting for the PIN to be
                // entered — that is the expected state, not a failure.
                let interval = state.get("interval").and_then(Value::as_i64).unwrap_or(30).max(5);
                (
                    SetupStep::Wait {
                        title: "Waiting for the PIN to be entered on ecobee.com".into(),
                        body: String::new(),
                        retry_ms: interval as u32 * 1000,
                    },
                    state.clone(),
                )
            }
            "listing" if note == Some("list") => {
                let key = state.get("api_key").and_then(Value::as_str).unwrap_or("").to_string();
                let refresh = state.get("refresh_token").and_then(Value::as_str).unwrap_or("").to_string();
                let list = input
                    .get("response")
                    .and_then(|b| b.get("thermostatList"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();

                let mut options = Vec::new();
                for t in &list {
                    let Some(id) = t.get("identifier").and_then(Value::as_str) else { continue };
                    let name = t.get("name").and_then(Value::as_str).unwrap_or(id).to_string();
                    let mut props = BTreeMap::new();
                    props.insert("API key".into(), json!(key));
                    props.insert("Refresh token".into(), json!(refresh));
                    props.insert("Thermostat identifier".into(), json!(id));
                    options.push(Candidate {
                        label: name,
                        kind: "thermostat".into(),
                        driver_id: "ecobee.thermostat".into(),
                        properties: props,
                        verified: "confirmed by the ecobee API".into(),
                    });

                    let Some(sensors) = t.get("remoteSensors").and_then(Value::as_array) else {
                        continue;
                    };
                    for s in sensors {
                        // The thermostat's own built-in sensor ships as part of the
                        // thermostat itself, not as something separate to adopt.
                        if s.get("type").and_then(Value::as_str) == Some("thermostat") {
                            continue;
                        }
                        let Some(sid) = s.get("id").and_then(Value::as_str) else { continue };
                        let sname = s.get("name").and_then(Value::as_str).unwrap_or(sid).to_string();
                        let mut sp = BTreeMap::new();
                        sp.insert("Thermostat identifier".into(), json!(id));
                        sp.insert("Sensor id".into(), json!(sid));
                        options.push(Candidate {
                            label: sname,
                            kind: "sensor".into(),
                            driver_id: "ecobee.remote_sensor".into(),
                            properties: sp,
                            verified: "confirmed by the ecobee API".into(),
                        });
                    }
                }
                (
                    SetupStep::Choose {
                        title: "Add these ecobee devices".into(),
                        body: String::new(),
                        options,
                        multiple: true,
                    },
                    Value::Null,
                )
            }
            _ => {
                // `start`, or anything unrecognised: ask for the API key. Submitting the
                // form re-enters here with it in `input`.
                let Some(key) = input.get("api_key").and_then(Value::as_str) else {
                    return (Self::ask_api_key(), json!({ "stage": "start" }));
                };
                (
                    SetupStep::Fetch {
                        request: HttpRequest::new(
                            "GET",
                            format!(
                                "{AUTHORIZE_API}?response_type=ecobeePin&client_id={key}&scope=smartWrite"
                            ),
                        ),
                        note: "authorize".into(),
                    },
                    json!({ "stage": "authorizing", "api_key": key }),
                )
            }
        }
    }
}

impl EcobeeThermostat {
    fn ask_api_key() -> SetupStep {
        SetupStep::Form {
            title: "Connect an ecobee account".into(),
            body: "Create an app at ecobee.com/developer (Add Application \u{2192} My apps) to \
                   get an API key."
                .into(),
            fields: vec![Field {
                name: "api_key".into(),
                label: "API key".into(),
                kind: "password".into(),
                help: "From the ecobee developer portal".into(),
                default: None,
                options: Vec::new(),
                required: true,
            }],
        }
    }

    fn failure(msg: &str) -> SetupStep {
        SetupStep::Instruct {
            title: "Could not connect to ecobee".into(),
            body: msg.into(),
            continue_label: "Try again".into(),
        }
    }
}

/// Ecobee reports every sensor value as a `{type, value}` pair in a list, with occupancy as
/// the *string* "true"/"false" rather than a bool.
fn read_occupancy(sensor: &Value) -> Option<bool> {
    sensor
        .get("capability")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("occupancy"))
        .and_then(|c| c.get("value"))
        .and_then(Value::as_str)
        .map(|v| v == "true")
}

fn read_temperature_c(sensor: &Value) -> Option<f64> {
    let raw = sensor
        .get("capability")?
        .as_array()?
        .iter()
        .find(|c| c.get("type").and_then(Value::as_str) == Some("temperature"))
        .and_then(|c| c.get("value"))
        .and_then(Value::as_str)?;
    // An unreachable sensor reports "unknown" rather than omitting the field.
    raw.parse::<i64>().ok().map(f10_to_c)
}

impl DriverModule for EcobeeRemoteSensor {
    fn on_command(
        &self,
        _inst: &mut Instance,
        _proxy: LocalId,
        cmd: &str,
        _args: &Args,
    ) -> Vec<HostCall> {
        vec![HostCall::warn(format!(
            "ecobee sensor is read-only, got `{cmd}`"
        ))]
    }

    fn on_event(
        &self,
        inst: &mut Instance,
        _control: LocalId,
        note: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        if note != "http_response" {
            return Vec::new();
        }
        let want = inst.property("Sensor id").as_str().unwrap_or("").to_string();
        let Some(sensor) = args
            .get("body")
            .and_then(|d| d.get("thermostatList"))
            .and_then(Value::as_array)
            .and_then(|l| l.first())
            .and_then(|t| t.get("remoteSensors"))
            .and_then(Value::as_array)
            .and_then(|s| {
                s.iter()
                    .find(|s| s.get("id").and_then(Value::as_str) == Some(&want))
            })
        else {
            return Vec::new();
        };

        let mut out = Vec::new();
        if let Some(occ) = read_occupancy(sensor)
            && inst.scratch.get("occupied").and_then(Value::as_bool) != Some(occ)
        {
            inst.scratch.insert("occupied".into(), json!(occ));
            let mut a = Args::new();
            a.insert("detected".into(), json!(occ));
            out.push(HostCall::notify(1, "detected_changed", a));
        }
        if let Some(c) = read_temperature_c(sensor) {
            let mut a = Args::new();
            a.insert("value".into(), json!(c));
            out.push(HostCall::notify(2, "value_changed", a));
        }
        out
    }
}

// ---------------------------------------------------------------------------------------
// One payload, two drivers
// ---------------------------------------------------------------------------------------

/// The thermostat and its remote sensors ship together because they are the same API, the
/// same credentials, and the same poll. Splitting them into two payloads would buy nothing
/// and create a version skew nobody can see.
///
/// Setup is already shared — `discover` offers both kinds of candidate from one flow. Only
/// the live calls need routing, and a sensor is simply the instance that was given a
/// `Sensor id`; a thermostat never has one.
#[derive(Default)]
pub struct Ecobee;

fn is_sensor(inst: &Instance) -> bool {
    !inst.property("Sensor id").as_str().unwrap_or("").is_empty()
}

impl DriverModule for Ecobee {
    fn on_command(
        &self,
        inst: &mut Instance,
        proxy: LocalId,
        cmd: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        if is_sensor(inst) {
            EcobeeRemoteSensor.on_command(inst, proxy, cmd, args)
        } else {
            EcobeeThermostat.on_command(inst, proxy, cmd, args)
        }
    }

    fn on_event(
        &self,
        inst: &mut Instance,
        control: LocalId,
        note: &str,
        args: &Args,
    ) -> Vec<HostCall> {
        if is_sensor(inst) {
            EcobeeRemoteSensor.on_event(inst, control, note, args)
        } else {
            EcobeeThermostat.on_event(inst, control, note, args)
        }
    }

    fn on_bind(&self, inst: &mut Instance) -> Vec<HostCall> {
        if is_sensor(inst) {
            EcobeeRemoteSensor.on_bind(inst)
        } else {
            EcobeeThermostat.on_bind(inst)
        }
    }

    fn unsupported(&self) -> Vec<String> {
        EcobeeThermostat.unsupported()
    }

    fn discover(&self, driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        EcobeeThermostat.discover(driver_id, state, input)
    }

    fn setup(&self, driver_id: &str, state: &Value, input: &Args) -> (SetupStep, Value) {
        EcobeeThermostat.setup(driver_id, state, input)
    }
}

export_driver!(Ecobee);
