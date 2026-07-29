// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Customation AS
//! The single gnubg context and the level catalog.
//!
//! gnubg's evaluator is one global neural net with static caches; the
//! daemon owns exactly one initialized context and serializes every call
//! on it. Levels are gnubg-convention plies (0-ply = raw NN) plus the
//! position-only rollout — gnubgapi exposes no move/cube rollout, so the
//! rollout level declares `methods: [evaluatePosition]` (spec §7).

use std::ffi::CString;
use std::path::Path;
use std::sync::Arc;

use bep_protocol::contract::{
    kinds, methods, Conventions, Describe, EngineIdentity, Level, RolloutParams,
};
use serde::Deserialize;

use crate::ffi::{
    CubeDecisionResult, GnubgApi, GnubgApiError, RolloutSettings, ScoredMove, MAX_MOVES,
    NUM_OUTPUTS,
};

pub const PROTOCOL_VERSION: &str = "0.1";
pub const ENGINE_FAMILY: &str = "gnubg";
pub const ENGINE_DISPLAY_NAME: &str = "GNU Backgammon";
pub const MAX_PARALLEL: u32 = 1;
/// gnubg counts plies the gnubg way: 0-ply = raw NN.
pub const PLY_COUNTING: &str = "gnubg";
pub const EQUITY_CONVENTION: &str = "contract";

pub mod levels {
    pub const PLY_0: &str = "0ply";
    pub const PLY_1: &str = "1ply";
    pub const PLY_2: &str = "2ply";
    pub const PLY_3: &str = "3ply";
    pub const PLY_4: &str = "4ply";
    pub const ROLLOUT: &str = "rollout";
}

/// gnubg ply-parity rule for the cube: in gnubg's numbering, odd-ply
/// trees leave the OPPONENT on roll at the leaves, biasing cube
/// equities — only even plies (0, 2, 4) produce sensible cube
/// decisions. 4-ply exists AS the deep-cube level; its full-movelist
/// checker scoring would be pathological, so it answers position and
/// cube only.
fn ply_methods(depth: u32) -> Option<&'static [&'static str]> {
    match depth {
        1 | 3 => Some(&[
            methods::EVALUATE_POSITION,
            methods::EVALUATE_MOVES,
            methods::ANALYZE_MOVE,
        ]),
        4 => Some(&[methods::EVALUATE_POSITION, methods::EVALUATE_CUBE]),
        _ => None,
    }
}

pub const WEIGHTS_FILENAME: &str = "gnubg.weights";
pub const WEIGHTS_BINARY_FILENAME: &str = "gnubg.wd";

/// The level a request resolved to.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Ply(u32),
    Rollout(RolloutOptions),
}

impl Resolved {
    /// Contract Plies stamp: ply levels carry their gnubg-convention
    /// depth; the rollout level stamps 0 (the platform's dedup identity
    /// for non-ply engines).
    pub fn plies_stamp(&self) -> i32 {
        match self {
            Resolved::Ply(depth) => *depth as i32,
            Resolved::Rollout(_) => 0,
        }
    }
}

/// levelOptions for the configurable rollout level — mirrors
/// gnubgapi_rollout_settings. Unknown keys are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RolloutOptions {
    #[serde(default)]
    pub trials: Option<u32>,
    #[serde(default)]
    pub cubeful: Option<bool>,
    #[serde(default)]
    pub variance_reduction: Option<bool>,
    #[serde(default)]
    pub chequer_plies: Option<u32>,
    #[serde(default)]
    pub cube_plies: Option<u32>,
    #[serde(default)]
    pub seed: Option<u32>,
    #[serde(default)]
    pub truncate: Option<bool>,
    #[serde(default)]
    pub truncate_plies: Option<u32>,
}

#[derive(Debug)]
pub enum LevelError {
    UnknownLevel(String),
    InvalidOptions(String),
    /// Method not offered by the level (rollout answers evaluatePosition only).
    MethodNotSupported { level: String, method: String },
}

pub fn resolve_level(
    level_id: &str,
    method: &str,
    options: Option<&serde_json::Value>,
) -> Result<Resolved, LevelError> {
    let ply = |depth: u32| -> Result<Resolved, LevelError> {
        if options.is_some() {
            return Err(LevelError::InvalidOptions(format!(
                "level {level_id:?} is not configurable"
            )));
        }
        if let Some(allowed) = ply_methods(depth) {
            if !allowed.contains(&method) {
                return Err(LevelError::MethodNotSupported {
                    level: level_id.to_string(),
                    method: method.to_string(),
                });
            }
        }
        Ok(Resolved::Ply(depth))
    };
    match level_id {
        levels::PLY_0 => ply(0),
        levels::PLY_1 => ply(1),
        levels::PLY_2 => ply(2),
        levels::PLY_3 => ply(3),
        levels::PLY_4 => ply(4),
        levels::ROLLOUT => {
            if method != methods::EVALUATE_POSITION {
                return Err(LevelError::MethodNotSupported {
                    level: level_id.to_string(),
                    method: method.to_string(),
                });
            }
            let parsed = match options {
                None => RolloutOptions::default(),
                Some(value) => serde_json::from_value(value.clone())
                    .map_err(|e| LevelError::InvalidOptions(e.to_string()))?,
            };
            Ok(Resolved::Rollout(parsed))
        }
        other => Err(LevelError::UnknownLevel(other.to_string())),
    }
}

/// The engine thread's private state — the context never leaves the
/// thread that created and initialized it. This is a hard requirement:
/// gnubg's move scoring (FindnSaveBestMoves) drives thread-local task
/// machinery that must run on the initializing thread; calling it from
/// any other thread segfaults (proven in the container E2E).
struct Inner {
    api: Arc<GnubgApi>,
    ctx: *mut crate::ffi::ContextOpaque,
}

type Job = Box<dyn FnOnce(&Inner) + Send>;

/// The one gnubg engine: a dedicated executor thread owns the context,
/// performs init, and runs every evaluation; callers block on a reply
/// channel. Serialization falls out of the single thread.
pub struct Engine {
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    thread: Option<std::thread::JoinHandle<()>>,
    rollout_defaults: RolloutSettings,
    version: String,
}

impl Engine {
    pub fn new(
        api: Arc<GnubgApi>,
        weights_path: &Path,
        weights_binary_path: &Path,
        data_dir: &Path,
        no_bearoff: bool,
    ) -> Result<Engine, String> {
        let to_cstring = |path: &Path| -> Result<CString, String> {
            CString::new(path.to_string_lossy().into_owned())
                .map_err(|_| format!("path contains a NUL byte: {}", path.display()))
        };
        let weights = to_cstring(weights_path)?;
        let weights_binary = to_cstring(weights_binary_path)?;
        let data = to_cstring(data_dir)?;

        let (job_sender, job_receiver) = std::sync::mpsc::channel::<Job>();
        let (startup_sender, startup_receiver) =
            std::sync::mpsc::channel::<Result<(RolloutSettings, String), String>>();

        let thread = std::thread::Builder::new()
            .name("gnubg-engine".to_string())
            .spawn(move || {
                let ctx = match api.create() {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        let _ = startup_sender.send(Err(format!("gnubgapi_create failed: {e}")));
                        return;
                    }
                };
                if let Err(e) =
                    unsafe { api.init(ctx, &weights, &weights_binary, &data, no_bearoff) }
                {
                    let _ = startup_sender.send(Err(format!("gnubgapi_init failed: {e}")));
                    return;
                }
                let defaults = api.rollout_settings_defaults();
                let version = api.version();
                if startup_sender.send(Ok((defaults, version))).is_err() {
                    unsafe { api.shutdown_and_destroy(ctx) };
                    return;
                }
                let inner = Inner { api, ctx };
                while let Ok(job) = job_receiver.recv() {
                    job(&inner);
                }
                unsafe { inner.api.shutdown_and_destroy(inner.ctx) };
            })
            .map_err(|e| format!("cannot spawn engine thread: {e}"))?;

        let (rollout_defaults, version) = startup_receiver
            .recv()
            .map_err(|_| "engine thread died during startup".to_string())??;
        Ok(Engine { jobs: Some(job_sender), thread: Some(thread), rollout_defaults, version })
    }

    /// Run one call on the engine thread and wait for its result.
    fn run<R: Send + 'static>(
        &self,
        call: impl FnOnce(&Inner) -> R + Send + 'static,
    ) -> Result<R, GnubgApiError> {
        let (reply_sender, reply_receiver) = std::sync::mpsc::channel::<R>();
        let job: Job = Box::new(move |inner| {
            let _ = reply_sender.send(call(inner));
        });
        let sender = self.jobs.as_ref().expect("jobs sender lives until Drop");
        sender.send(job).map_err(|_| GnubgApiError {
            code: crate::ffi::GNUBGAPI_OK,
            message: "engine thread is gone".to_string(),
        })?;
        reply_receiver.recv().map_err(|_| GnubgApiError {
            code: crate::ffi::GNUBGAPI_OK,
            message: "engine thread died during the call".to_string(),
        })
    }

    pub fn evaluate_position(
        &self,
        position_id: &CString,
        match_id: &CString,
        resolved: &Resolved,
    ) -> Result<[f64; NUM_OUTPUTS], GnubgApiError> {
        let position_id = position_id.clone();
        let match_id = match_id.clone();
        match resolved {
            Resolved::Ply(depth) => {
                let depth = *depth;
                self.run(move |inner| unsafe {
                    inner.api.evaluate_position_full_plied(inner.ctx, &position_id, &match_id, depth)
                })?
            }
            Resolved::Rollout(options) => {
                let mut settings = self.rollout_defaults;
                if let Some(trials) = options.trials {
                    settings.n_trials = trials;
                }
                if let Some(cubeful) = options.cubeful {
                    settings.cubeful = cubeful as i32;
                }
                if let Some(vr) = options.variance_reduction {
                    settings.variance_reduction = vr as i32;
                }
                if let Some(chequer) = options.chequer_plies {
                    settings.chequer_plies = chequer;
                }
                if let Some(cube) = options.cube_plies {
                    settings.cube_plies = cube;
                }
                if let Some(seed) = options.seed {
                    settings.seed = seed;
                }
                if let Some(truncate) = options.truncate {
                    settings.truncate = truncate as i32;
                }
                if let Some(truncate_plies) = options.truncate_plies {
                    settings.truncate_plies = truncate_plies;
                }
                self.run(move |inner| unsafe {
                    inner.api.rollout_position(inner.ctx, &position_id, &match_id, &settings)
                })?
            }
        }
    }

    pub fn evaluate_cube(
        &self,
        position_id: &CString,
        match_id: &CString,
        n_plies: u32,
    ) -> Result<CubeDecisionResult, GnubgApiError> {
        let position_id = position_id.clone();
        let match_id = match_id.clone();
        self.run(move |inner| unsafe {
            inner.api.evaluate_cube_decision(inner.ctx, &position_id, &match_id, n_plies)
        })?
    }

    pub fn scored_moves(
        &self,
        position_id: &CString,
        match_id: &CString,
        die1: i32,
        die2: i32,
        n_plies: u32,
    ) -> Result<Vec<ScoredMove>, GnubgApiError> {
        let position_id = position_id.clone();
        let match_id = match_id.clone();
        self.run(move |inner| {
            let mut buffer = vec![ScoredMove::zeroed(); MAX_MOVES];
            let count = unsafe {
                inner.api.generate_moves_with_eval(
                    inner.ctx, &position_id, &match_id, die1, die2, n_plies, &mut buffer,
                )
            }?;
            buffer.truncate(count);
            Ok(buffer)
        })?
    }

    pub fn describe(&self, build: &str) -> Describe {
        let defaults = &self.rollout_defaults;
        let ply_level = |id: &str, depth: u32| Level {
            id: id.to_string(),
            kind: kinds::PLY.to_string(),
            display_name: None,
            ply_depth: Some(depth),
            rollout: None,
            // The cube ply-parity rule (see ply_methods): odd plies never
            // answer evaluateCube; 4-ply answers position/cube only.
            methods: ply_methods(depth)
                .map(|allowed| allowed.iter().map(|m| m.to_string()).collect()),
            configurable: false,
            supports_progress: false,
            supports_cancel: false,
        };
        Describe {
            protocol_version: PROTOCOL_VERSION.to_string(),
            engine: EngineIdentity {
                family: ENGINE_FAMILY.to_string(),
                display_name: ENGINE_DISPLAY_NAME.to_string(),
                version: self.version.clone(),
                build: build.to_string(),
            },
            max_parallel: MAX_PARALLEL,
            conventions: Conventions {
                ply_counting: PLY_COUNTING.to_string(),
                equity: EQUITY_CONVENTION.to_string(),
            },
            levels: vec![
                ply_level(levels::PLY_0, 0),
                ply_level(levels::PLY_1, 1),
                ply_level(levels::PLY_2, 2),
                ply_level(levels::PLY_3, 3),
                ply_level(levels::PLY_4, 4),
                Level {
                    id: levels::ROLLOUT.to_string(),
                    kind: kinds::ROLLOUT.to_string(),
                    display_name: None,
                    ply_depth: None,
                    rollout: Some(RolloutParams {
                        trials: defaults.n_trials,
                        truncation: if defaults.truncate != 0 {
                            defaults.truncate_plies
                        } else {
                            0
                        },
                        variance_reduction: defaults.variance_reduction != 0,
                        // gnubg convention: 0-ply = raw NN.
                        checker_ply: Some(defaults.chequer_plies),
                        cube_ply: Some(defaults.cube_plies),
                    }),
                    // gnubgapi has no move/cube rollout entry points.
                    methods: Some(vec![methods::EVALUATE_POSITION.to_string()]),
                    configurable: true,
                    // No progress callback or cancel in the gnubgapi ABI.
                    supports_progress: false,
                    supports_cancel: false,
                },
            ],
        }
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        // Closing the job channel ends the executor loop; the thread
        // shuts the context down before exiting.
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("gnubg engine thread panicked during shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ply_levels_resolve_and_stamp_gnubg_convention() {
        let resolved = resolve_level(levels::PLY_0, methods::EVALUATE_MOVES, None).unwrap();
        assert_eq!(resolved, Resolved::Ply(0));
        assert_eq!(resolved.plies_stamp(), 0);
        let resolved = resolve_level(levels::PLY_2, methods::EVALUATE_CUBE, None).unwrap();
        assert_eq!(resolved.plies_stamp(), 2);
    }

    #[test]
    fn cube_only_on_even_plies() {
        // gnubg parity rule: odd plies never answer evaluateCube.
        for level in [levels::PLY_1, levels::PLY_3] {
            assert!(matches!(
                resolve_level(level, methods::EVALUATE_CUBE, None),
                Err(LevelError::MethodNotSupported { .. })
            ));
            assert!(resolve_level(level, methods::EVALUATE_MOVES, None).is_ok());
        }
        for level in [levels::PLY_0, levels::PLY_2, levels::PLY_4] {
            assert!(resolve_level(level, methods::EVALUATE_CUBE, None).is_ok());
        }
        // 4-ply is the deep-cube level: no full-movelist checker scoring.
        assert!(matches!(
            resolve_level(levels::PLY_4, methods::EVALUATE_MOVES, None),
            Err(LevelError::MethodNotSupported { .. })
        ));
    }

    #[test]
    fn rollout_answers_evaluate_position_only() {
        assert!(matches!(
            resolve_level(levels::ROLLOUT, methods::EVALUATE_POSITION, None),
            Ok(Resolved::Rollout(_))
        ));
        assert!(matches!(
            resolve_level(levels::ROLLOUT, methods::EVALUATE_MOVES, None),
            Err(LevelError::MethodNotSupported { .. })
        ));
    }

    #[test]
    fn rollout_options_reject_unknown_keys_and_ply_levels_reject_options() {
        let options = serde_json::json!({"trials": 108, "bogus": true});
        assert!(matches!(
            resolve_level(levels::ROLLOUT, methods::EVALUATE_POSITION, Some(&options)),
            Err(LevelError::InvalidOptions(_))
        ));
        let options = serde_json::json!({"trials": 108});
        assert!(matches!(
            resolve_level(levels::PLY_2, methods::EVALUATE_POSITION, Some(&options)),
            Err(LevelError::InvalidOptions(_))
        ));
        let options = serde_json::json!({"trials": 108, "cubePlies": 2});
        let resolved =
            resolve_level(levels::ROLLOUT, methods::EVALUATE_POSITION, Some(&options)).unwrap();
        assert_eq!(
            resolved,
            Resolved::Rollout(RolloutOptions {
                trials: Some(108),
                cube_plies: Some(2),
                ..RolloutOptions::default()
            })
        );
    }
}
