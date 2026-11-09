#![cfg_attr(windows, windows_subsystem = "windows")]

mod protocol;

use serde::Deserialize;
use serde_json::Value;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, BufReader};

use input::{create, InputBackend, InputEvent, InputOptions};

/// The backend, kept alive across command calls.
#[derive(Default)]
struct State(Mutex<Option<Box<dyn InputBackend>>>);

#[derive(Deserialize)]
struct Inject {
    events: Vec<InputEvent>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StartInput {
    options: InputOptions,
}

#[derive(Deserialize)]
struct ConfigureInput {
    options: InputOptions,
}

#[tokio::main]
async fn main() {
    let out = protocol::stdout();
    protocol::register(&out).await;

    let state = State::default();
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<protocol::Incoming>(&line) else {
            continue;
        };
        match msg {
            protocol::Incoming::Call {
                id,
                method,
                payload,
            } => {
                let result = dispatch(&state, &method, payload);
                protocol::reply(&out, id, result).await;
            }
        }
    }
}

/// Route one call to the matching input operation.
fn dispatch(state: &State, method: &str, payload: Value) -> Result<Value, String> {
    match method {
        "input_start" => {
            let args: Option<StartInput> = serde_json::from_value(payload).map_err(|e| e.to_string())?;
            let options = args.map(|a| a.options).unwrap_or_default();
            let mut held = state.0.lock().unwrap();
            if held.is_none() {
                *held = Some(create()?);
            }
            held.as_mut().unwrap().configure(&options)?;
            Ok(Value::Null)
        }
        "input_configure" => {
            let ConfigureInput { options } = serde_json::from_value(payload).map_err(|e| e.to_string())?;
            let mut held = state.0.lock().unwrap();
            held.as_mut().ok_or("input backend not connected")?.configure(&options)?;
            Ok(Value::Null)
        }
        "input_inject" => {
            let Inject { events } = serde_json::from_value(payload).map_err(|e| e.to_string())?;
            let mut held = state.0.lock().unwrap();
            let backend = held.as_mut().ok_or("input backend not connected")?;
            backend.handle(&events)?;
            Ok(Value::Null)
        }
        "input_release" => {
            if let Some(backend) = state.0.lock().unwrap().as_mut() {
                backend.release();
            }
            Ok(Value::Null)
        }
        "input_stop" => {
            let mut held = state.0.lock().unwrap();
            if let Some(backend) = held.as_mut() {
                backend.release();
            }
            *held = None;
            Ok(Value::Null)
        }
        other => Err(format!("unknown method: {other}")),
    }
}
