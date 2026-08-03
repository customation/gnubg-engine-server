// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! gnubg-engine-server — GNU Backgammon as a Backgammon Engine Protocol
//! daemon: JSON-RPC 2.0 over stdio, evaluations via libgnubgapi.
//!
//! Release layout next to the executable:
//!   data/  gnubg.weights, gnubg.wd, gnubg_os0.bd, gnubg_ts0.bd, met/
//!   the engine library (libgnubgapi.so / libgnubgapi.dll / .dylib)

mod engine;
mod ffi;
mod mapping;

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bep_protocol::contract::{error_codes, methods, CancelParams, EvaluateParams};
use bep_protocol::jsonrpc::{self, codes, FrameSink, Incoming};
use serde_json::Value;

use engine::{resolve_level, Engine, LevelError};
use ffi::GnubgApi;

const ENV_GNUBGAPI_LIB: &str = "GNUBGAPI_LIB";
/// Set to anything but 0/false/empty to log one line per request. Shared
/// spelling with sage-engine-server on purpose — one switch, both engines.
const ENV_LOG_REQUESTS: &str = "BEP_LOG_REQUESTS";

const FLAG_GNUBGAPI_LIB: &str = "--gnubgapi-lib";
const FLAG_DATA_DIR: &str = "--data-dir";
const FLAG_WEIGHTS: &str = "--weights";
const FLAG_WEIGHTS_BINARY: &str = "--weights-binary";
const FLAG_NO_BEAROFF: &str = "--no-bearoff";
const FLAG_HELP: &str = "--help";

const DEFAULT_DATA_SUBDIR: &str = "data";

#[cfg(target_os = "windows")]
const GNUBGAPI_LIB_FILENAME: &str = "libgnubgapi.dll";
#[cfg(target_os = "macos")]
const GNUBGAPI_LIB_FILENAME: &str = "libgnubgapi.dylib";
#[cfg(all(unix, not(target_os = "macos")))]
const GNUBGAPI_LIB_FILENAME: &str = "libgnubgapi.so";

const BUILD_NAME: &str = concat!("gnubg-engine-server ", env!("CARGO_PKG_VERSION"));

const EXIT_CONFIG_ERROR: i32 = 2;

struct Args {
    gnubgapi_lib: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    weights: Option<PathBuf>,
    weights_binary: Option<PathBuf>,
    no_bearoff: bool,
}

fn usage() -> String {
    format!(
        "usage: gnubg-engine-server [{FLAG_GNUBGAPI_LIB} <path>] [{FLAG_DATA_DIR} <dir>] \
         [{FLAG_WEIGHTS} <path>] [{FLAG_WEIGHTS_BINARY} <path>] [{FLAG_NO_BEAROFF}]\n\
         Defaults: engine library {GNUBGAPI_LIB_FILENAME} next to the executable (or \
         ${ENV_GNUBGAPI_LIB}), data in <exe dir>/{DEFAULT_DATA_SUBDIR} with \
         {}/{} inside it",
        engine::WEIGHTS_FILENAME,
        engine::WEIGHTS_BINARY_FILENAME
    )
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        gnubgapi_lib: None,
        data_dir: None,
        weights: None,
        weights_binary: None,
        no_bearoff: false,
    };
    let mut iter = std::env::args().skip(1);
    while let Some(flag) = iter.next() {
        let mut value_for = |name: &str| {
            iter.next().ok_or_else(|| format!("{name} requires a value\n{}", usage()))
        };
        match flag.as_str() {
            FLAG_GNUBGAPI_LIB => {
                args.gnubgapi_lib = Some(PathBuf::from(value_for(FLAG_GNUBGAPI_LIB)?))
            }
            FLAG_DATA_DIR => args.data_dir = Some(PathBuf::from(value_for(FLAG_DATA_DIR)?)),
            FLAG_WEIGHTS => args.weights = Some(PathBuf::from(value_for(FLAG_WEIGHTS)?)),
            FLAG_WEIGHTS_BINARY => {
                args.weights_binary = Some(PathBuf::from(value_for(FLAG_WEIGHTS_BINARY)?))
            }
            FLAG_NO_BEAROFF => args.no_bearoff = true,
            FLAG_HELP => return Err(usage()),
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }
    Ok(args)
}

fn exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot resolve executable path: {e}"))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "executable has no parent directory".to_string())
}

fn build_engine(args: &Args) -> Result<Engine, String> {
    let exe_dir = exe_dir()?;

    let lib_path = match &args.gnubgapi_lib {
        Some(path) => path.clone(),
        None => match std::env::var_os(ENV_GNUBGAPI_LIB) {
            Some(value) => PathBuf::from(value),
            None => exe_dir.join(GNUBGAPI_LIB_FILENAME),
        },
    };
    if !lib_path.is_file() {
        return Err(format!(
            "engine library not found: {} (set {FLAG_GNUBGAPI_LIB} or {ENV_GNUBGAPI_LIB})",
            lib_path.display()
        ));
    }
    // libgnubgapi ships with dependency DLLs beside it (glib, iconv, …);
    // Windows resolves those via the process search path, not the loaded
    // library's own directory — make its home directory searchable.
    #[cfg(windows)]
    if let Some(lib_dir) = lib_path.parent() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut paths: Vec<PathBuf> = std::env::split_paths(&path).collect();
        paths.insert(0, lib_dir.to_path_buf());
        match std::env::join_paths(paths) {
            Ok(joined) => std::env::set_var("PATH", joined),
            Err(e) => eprintln!("cannot extend PATH with {}: {e}", lib_dir.display()),
        }
    }
    let api = GnubgApi::load(&lib_path)
        .map_err(|e| format!("cannot load engine library {}: {e}", lib_path.display()))?;

    let data_dir = args.data_dir.clone().unwrap_or_else(|| exe_dir.join(DEFAULT_DATA_SUBDIR));
    let weights =
        args.weights.clone().unwrap_or_else(|| data_dir.join(engine::WEIGHTS_FILENAME));
    let weights_binary = args
        .weights_binary
        .clone()
        .unwrap_or_else(|| data_dir.join(engine::WEIGHTS_BINARY_FILENAME));
    for (label, path) in
        [("weights", &weights), ("weights binary", &weights_binary)]
    {
        if !path.is_file() {
            return Err(format!("{label} file not found: {}", path.display()));
        }
    }
    if !data_dir.is_dir() {
        return Err(format!("data directory not found: {}", data_dir.display()));
    }

    let api = Arc::new(api);
    eprintln!(
        "gnubg-engine-server {}: gnubgapi {} from {}, data {}",
        env!("CARGO_PKG_VERSION"),
        api.version(),
        lib_path.display(),
        data_dir.display()
    );
    Engine::new(api, &weights, &weights_binary, &data_dir, args.no_bearoff)
}

fn id_key(id: &Value) -> String {
    id.to_string()
}

fn send_or_log<W: io::Write + Send>(sink: &FrameSink<W>, message: &Value) {
    if let Err(e) = sink.send(message) {
        eprintln!("cannot write to stdout ({e}); exiting");
        std::process::exit(1);
    }
}

fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(EXIT_CONFIG_ERROR);
        }
    };
    let engine = match build_engine(&args) {
        Ok(engine) => Arc::new(engine),
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(EXIT_CONFIG_ERROR);
        }
    };

    let sink = FrameSink::new(io::stdout());
    // Tracked so cancel notifications can be answered honestly (logged as
    // unsupported — gnubgapi cannot abort a running evaluation).
    let in_flight: Arc<Mutex<HashMap<String, ()>>> = Arc::new(Mutex::new(HashMap::new()));

    let stdin = io::stdin();
    let mut reader = stdin.lock();
    loop {
        let message = match jsonrpc::read_message(&mut reader) {
            Ok(Some(Ok(message))) => message,
            Ok(Some(Err(parse_error))) => {
                send_or_log(
                    &sink,
                    &jsonrpc::error(None, codes::PARSE_ERROR, &parse_error.to_string()),
                );
                continue;
            }
            Ok(None) => break,
            Err(io_error) => {
                eprintln!("stdin read failed: {io_error}");
                break;
            }
        };
        dispatch(message, &engine, &sink, &in_flight);
    }
}

fn dispatch(
    message: Incoming,
    engine: &Arc<Engine>,
    sink: &FrameSink<io::Stdout>,
    in_flight: &Arc<Mutex<HashMap<String, ()>>>,
) {
    match message.method.as_str() {
        methods::DESCRIBE => {
            if let Some(id) = &message.id {
                match serde_json::to_value(engine.describe(BUILD_NAME)) {
                    Ok(result) => send_or_log(sink, &jsonrpc::success(id, result)),
                    Err(e) => send_or_log(
                        sink,
                        &jsonrpc::error(Some(id), codes::INTERNAL_ERROR, &e.to_string()),
                    ),
                }
            }
        }
        methods::SHUTDOWN => {
            if let Some(id) = &message.id {
                send_or_log(sink, &jsonrpc::success(id, Value::Null));
            }
            std::process::exit(0);
        }
        methods::CANCEL => {
            match serde_json::from_value::<CancelParams>(message.params.unwrap_or(Value::Null)) {
                Ok(cancel) => {
                    let known = {
                        let map = in_flight.lock().unwrap_or_else(|p| p.into_inner());
                        map.contains_key(&id_key(&cancel.id))
                    };
                    // The spec allows engines that cannot abort to complete
                    // normally (§6); gnubgapi has no cancellation.
                    eprintln!(
                        "cancel for request {} ignored: gnubg evaluations cannot abort{}",
                        cancel.id,
                        if known { "" } else { " (request not in flight)" }
                    );
                }
                Err(e) => eprintln!("malformed cancel notification: {e}"),
            }
        }
        methods::EVALUATE_POSITION
        | methods::EVALUATE_CUBE
        | methods::EVALUATE_MOVES
        | methods::ANALYZE_MOVE => {
            let Some(id) = message.id.clone() else {
                eprintln!("{} sent as a notification — ignored (no id to answer)", message.method);
                return;
            };
            let params = match serde_json::from_value::<EvaluateParams>(
                message.params.unwrap_or(Value::Null),
            ) {
                Ok(params) => params,
                Err(e) => {
                    send_or_log(
                        sink,
                        &jsonrpc::error(Some(&id), codes::INVALID_PARAMS, &e.to_string()),
                    );
                    return;
                }
            };
            {
                let mut map = in_flight.lock().unwrap_or_else(|p| p.into_inner());
                map.insert(id_key(&id), ());
            }

            let engine = Arc::clone(engine);
            let sink = sink.clone();
            let in_flight = Arc::clone(in_flight);
            let method = message.method.clone();
            std::thread::spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    evaluate(&method, &params, &engine, &id)
                }));
                let response = match outcome {
                    Ok(response) => response,
                    Err(panic) => {
                        let detail = panic
                            .downcast_ref::<&str>()
                            .map(|s| s.to_string())
                            .or_else(|| panic.downcast_ref::<String>().cloned())
                            .unwrap_or_else(|| "panic in evaluation thread".to_string());
                        eprintln!("evaluation panicked: {detail}");
                        jsonrpc::error(Some(&id), codes::INTERNAL_ERROR, &detail)
                    }
                };
                {
                    let mut map = in_flight.lock().unwrap_or_else(|p| p.into_inner());
                    map.remove(&id_key(&id));
                }
                send_or_log(&sink, &response);
            });
        }
        other => {
            if let Some(id) = &message.id {
                send_or_log(
                    sink,
                    &jsonrpc::error(
                        Some(id),
                        codes::METHOD_NOT_FOUND,
                        &format!("unknown method {other:?}"),
                    ),
                );
            }
        }
    }
}

fn level_error_response(id: &Value, error: LevelError) -> Value {
    match error {
        LevelError::UnknownLevel(level) => jsonrpc::error(
            Some(id),
            error_codes::UNKNOWN_LEVEL,
            &format!("unknown level {level:?}"),
        ),
        LevelError::InvalidOptions(message) => {
            jsonrpc::error(Some(id), codes::INVALID_PARAMS, &message)
        }
    }
}

fn evaluate(method: &str, params: &EvaluateParams, engine: &Engine, id: &Value) -> Value {
    // gnubgapi decodes the ids natively; validate shape here so bad ids
    // get INVALID_ID rather than a generic engine failure.
    if let Err(e) = bep_protocol::gnubg_ids::decode_position_id(&params.position_id) {
        return jsonrpc::error(Some(id), error_codes::INVALID_ID, &e.to_string());
    }
    if let Err(e) = bep_protocol::gnubg_ids::decode_match_id(&params.match_id) {
        return jsonrpc::error(Some(id), error_codes::INVALID_ID, &e.to_string());
    }
    let (position_id, match_id) = match (
        CString::new(params.position_id.clone()),
        CString::new(params.match_id.clone()),
    ) {
        (Ok(p), Ok(m)) => (p, m),
        _ => {
            return jsonrpc::error(Some(id), error_codes::INVALID_ID, "id contains a NUL byte")
        }
    };

    let resolved = match resolve_level(&params.level, params.level_options.as_ref()) {
        Ok(resolved) => resolved,
        Err(error) => return level_error_response(id, error),
    };

    // One line in, one line out, on stderr — stdout is the JSON-RPC channel.
    // Same wording as the cloud workers' Recv/Done so the four engines'
    // logs read alike, and the same env var as sage-engine-server so
    // turning engine logging on does not mean remembering which engine.
    let log_requests = request_logging_enabled();
    let started = std::time::Instant::now();
    if log_requests {
        let dice = match (params.die1, params.die2) {
            (Some(die1), Some(die2)) => format!(" Die={die1},{die2}"),
            _ => String::new(),
        };
        eprintln!(
            "Recv {method} Level={} {}{dice} Pos={} Match={}",
            params.level,
            engine.config_summary(&resolved),
            params.position_id,
            params.match_id
        );
    }

    let result: Result<Value, String> = match method {
        methods::EVALUATE_POSITION => engine
            .evaluate_position(&position_id, &match_id, &resolved)
            .map_err(|e| e.to_string())
            .map(|output| mapping::position_payload(&output, &params.position_id))
            .and_then(to_result_value),
        methods::EVALUATE_CUBE => engine
            .evaluate_cube(&position_id, &match_id, &resolved)
            .map_err(|e| e.to_string())
            .and_then(|r| mapping::cube_payload(&r, &params.position_id))
            .and_then(to_result_value),
        methods::EVALUATE_MOVES | methods::ANALYZE_MOVE => {
            let (die1, die2) = match (params.die1, params.die2) {
                (Some(die1), Some(die2)) if (1..=6).contains(&die1) && (1..=6).contains(&die2) => {
                    (die1, die2)
                }
                _ => {
                    return jsonrpc::error(
                        Some(id),
                        codes::INVALID_PARAMS,
                        "die1 and die2 are required and must be 1-6",
                    )
                }
            };
            let moves = engine
                .scored_moves(&position_id, &match_id, die1, die2, &resolved)
                .map_err(|e| e.to_string())
                .and_then(|scored| {
                    mapping::move_hints(
                        &scored,
                        &params.position_id,
                        &params.match_id,
                        die1,
                        die2,
                        resolved.plies_stamp(),
                    )
                });
            if method == methods::EVALUATE_MOVES {
                moves.and_then(to_result_value)
            } else {
                let Some(played) = params.played_move.as_deref() else {
                    return jsonrpc::error(
                        Some(id),
                        codes::INVALID_PARAMS,
                        "analyzeMove requires a move",
                    );
                };
                moves
                    .and_then(|m| mapping::analyze_payload(m, played))
                    .and_then(to_result_value)
            }
        }
        other => {
            return jsonrpc::error(
                Some(id),
                codes::METHOD_NOT_FOUND,
                &format!("unknown method {other:?}"),
            )
        }
    };

    if log_requests {
        let outcome = match &result {
            Ok(_) => "ok".to_string(),
            Err(message) => format!("FAILED {message}"),
        };
        eprintln!(
            "Done {method} Level={} {outcome} in {} ms",
            params.level,
            started.elapsed().as_millis()
        );
    }

    match result {
        Ok(value) => jsonrpc::success(id, value),
        Err(message) => jsonrpc::error(Some(id), error_codes::EVALUATION_FAILED, &message),
    }
}

/// Per-request logging is opt-in for the same reason as sage: the desktop
/// host runs a pool of daemons, and a 36-roll pass across eight instances
/// is 288 lines nobody asked for. Off by default, on when you are asking
/// what the engine actually did.
fn request_logging_enabled() -> bool {
    match std::env::var(ENV_LOG_REQUESTS) {
        Ok(value) => {
            let value = value.trim();
            !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false")
        }
        Err(_) => false,
    }
}

fn to_result_value<T: serde::Serialize>(payload: T) -> Result<Value, String> {
    serde_json::to_value(payload).map_err(|e| format!("cannot serialize result: {e}"))
}
